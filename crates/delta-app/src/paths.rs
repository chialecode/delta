use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildChannel {
    Release,
    Dev,
    Source,
}

impl BuildChannel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Dev => "dev",
            Self::Source => "source",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "release" => Some(Self::Release),
            "dev" => Some(Self::Dev),
            "source" => Some(Self::Source),
            _ => None,
        }
    }

    /// Detect build channel from compile-time `DELTA_CHANNEL`, defaulting to `source`.
    /// Per S-11 (`D-26`), `cargo run` / `cargo test` default to `source` and NEVER touch
    /// `%LOCALAPPDATA%\delta` (`release`).
    pub fn current() -> Self {
        option_env!("DELTA_CHANNEL")
            .and_then(Self::parse)
            .unwrap_or(Self::Source)
    }

    /// Lowercase directory leaf name: `delta` for `release`, `delta-dev` for `dev` and `source`.
    pub fn dir_name(&self) -> &'static str {
        match self {
            Self::Release => "delta",
            Self::Dev | Self::Source => "delta-dev",
        }
    }

    /// Windows Credential Manager service prefix: `delta:` for `release`, `delta-dev:` for `dev`/`source`.
    pub fn credential_prefix(&self) -> &'static str {
        match self {
            Self::Release => "delta",
            Self::Dev | Self::Source => "delta-dev",
        }
    }
}

/// Resolve existing ancestors before comparing, so `..`, junctions and case variants
/// cannot route a development build into the release library.
pub fn ensure_channel_data_isolation(
    path: &Path,
    channel: BuildChannel,
) -> Result<(), PathResolutionError> {
    let fail = || PathResolutionError::DataDirNotWritable {
        path: "requested data directory".into(),
        reason: "development build cannot access the release channel or data root".into(),
    };
    if channel == BuildChannel::Release && BuildChannel::current() != BuildChannel::Release {
        return Err(fail());
    }
    if channel == BuildChannel::Release {
        return Ok(());
    }
    fn resolve(path: &Path) -> Result<PathBuf, std::io::Error> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let mut clean = PathBuf::new();
        for component in absolute.components() {
            match component {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    clean.pop();
                }
                other => clean.push(other.as_os_str()),
            }
        }
        let mut ancestor = clean.clone();
        let mut suffix = Vec::new();
        while !ancestor.exists() {
            let name = ancestor
                .file_name()
                .ok_or_else(|| std::io::Error::other("missing path ancestor"))?
                .to_os_string();
            suffix.push(name);
            if !ancestor.pop() {
                return Err(std::io::Error::other("missing path ancestor"));
            }
        }
        let mut resolved = std::fs::canonicalize(ancestor)?;
        for name in suffix.into_iter().rev() {
            resolved.push(name);
        }
        Ok(resolved)
    }
    let normalize = |p: &Path| -> Result<String, PathResolutionError> {
        Ok(resolve(p)
            .map_err(|_| fail())?
            .to_string_lossy()
            .replace('\\', "/")
            .to_lowercase())
    };
    // Both D-26 release roots: the application data root and the user-file root. Test
    // install identities are additionally barred from the developer's `delta-dev` roots.
    let mut leaves = vec!["delta"];
    if crate::identity::InstallIdentity::current().is_some_and(|identity| identity.is_test()) {
        leaves.push("delta-dev");
    }
    let mut release_roots = Vec::new();
    for leaf in leaves {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            release_roots.push(PathBuf::from(local).join(leaf));
        }
        if let Ok(documents) = system_documents_root() {
            release_roots.push(documents.join(leaf));
        }
    }
    if release_roots.is_empty() {
        return Ok(());
    }
    let requested = normalize(path)?;
    for root in release_roots {
        let release = normalize(&root)?;
        if requested == release || requested.starts_with(&format!("{release}/")) {
            return Err(fail());
        }
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum PathResolutionError {
    #[error("data directory '{path}' is not writable or invalid: {reason} (no fallback applied)")]
    DataDirNotWritable { path: String, reason: String },
    #[error("missing required environment or known folder path: {0}")]
    MissingBaseFolder(String),
    #[error("another delta instance holds exclusive lock on '{0}'")]
    InstanceLocked(String),
    #[error("lock error on '{path}': {reason}")]
    LockFailed { path: String, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedAppPaths {
    pub channel: BuildChannel,
    pub data_dir: PathBuf,
    pub documents_dir: PathBuf,
    pub default_export_dir: PathBuf,
    pub default_backup_dir: PathBuf,
    pub credential_prefix: String,
}

/// Resolve Windows `FOLDERID_Documents` via `SHGetKnownFolderPath`, with `DELTA_DOCUMENTS_DIR`
/// override for isolated testing (S-11).
pub fn resolve_user_documents_root() -> Result<PathBuf, PathResolutionError> {
    if let Ok(override_dir) = std::env::var("DELTA_DOCUMENTS_DIR") {
        let trimmed = override_dir.trim();
        if !trimmed.is_empty() {
            return Ok(PathBuf::from(trimmed));
        }
    }
    system_documents_root()
}

/// The registered system Documents folder, ignoring test overrides. The channel guard uses
/// it so a source/dev build can never target the release user-file root `<Documents>/delta`.
pub(crate) fn system_documents_root() -> Result<PathBuf, PathResolutionError> {
    #[cfg(windows)]
    if let Some(path) = known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_Documents) {
        return Ok(path);
    }

    #[cfg(not(windows))]
    if let Ok(profile) = std::env::var("USERPROFILE") {
        return Ok(PathBuf::from(profile).join("Documents"));
    }
    Err(PathResolutionError::MissingBaseFolder(
        "FOLDERID_Documents / USERPROFILE".to_string(),
    ))
}

/// The current user's Desktop folder (`FOLDERID_Desktop`, Inno Setup's `{userdesktop}`), where
/// per-user installers put the desktop shortcut. `None` off Windows or when it is not registered.
pub fn user_desktop_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        known_folder(&windows_sys::Win32::UI::Shell::FOLDERID_Desktop)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// Resolve a Windows known folder through `SHGetKnownFolderPath`.
#[cfg(windows)]
fn known_folder(id: &windows_sys::core::GUID) -> Option<PathBuf> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{SHGetKnownFolderPath, KF_FLAG_DONT_VERIFY};
    // SAFETY: SHGetKnownFolderPath writes a NUL-terminated CoTaskMem string (or null) that is
    // read once and freed here.
    unsafe {
        let mut psz_path: windows_sys::core::PWSTR = std::ptr::null_mut();
        let hr = SHGetKnownFolderPath(
            id,
            KF_FLAG_DONT_VERIFY as u32,
            std::ptr::null_mut(),
            &mut psz_path,
        );
        let mut found = None;
        if hr == 0 && !psz_path.is_null() {
            let mut len = 0usize;
            while *psz_path.add(len) != 0 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(psz_path, len);
            found = Some(PathBuf::from(std::ffi::OsString::from_wide(slice)));
        }
        if !psz_path.is_null() {
            CoTaskMemFree(psz_path as *const std::ffi::c_void);
        }
        found
    }
}

#[cfg(windows)]
use std::os::windows::ffi::OsStringExt;

/// Resolve application data and documents paths according to S-11 (`D-26`):
/// Priority for `data_dir`: `cli_data_dir` (`--data-dir`) > `DELTA_DATA_DIR` > `%LOCALAPPDATA%\<channel_dir>`.
/// Validates writability of `data_dir` and refuses to fall back to `%LOCALAPPDATA%\delta` or `%TEMP%` on failure.
pub fn resolve_app_paths(
    channel_override: Option<BuildChannel>,
    cli_data_dir: Option<&Path>,
) -> Result<ResolvedAppPaths, PathResolutionError> {
    let channel = channel_override.unwrap_or_else(BuildChannel::current);
    let data_dir = if let Some(cli_p) = cli_data_dir {
        cli_p.to_path_buf()
    } else if let Ok(env_p) = std::env::var("DELTA_DATA_DIR") {
        let trimmed = env_p.trim();
        if !trimmed.is_empty() {
            PathBuf::from(trimmed)
        } else {
            default_channel_data_dir(channel)?
        }
    } else {
        default_channel_data_dir(channel)?
    };

    let docs_root = resolve_user_documents_root()?;
    let documents_dir = docs_root.join(identity_dir_name(channel));
    let default_export_dir = documents_dir.join("exports");
    let default_backup_dir = documents_dir.join("backups");

    Ok(ResolvedAppPaths {
        channel,
        data_dir,
        documents_dir,
        default_export_dir,
        default_backup_dir,
        credential_prefix: identity_dir_name(channel).to_string(),
    })
}

/// Test install identities (`S-26`) keep their own data, document and credential roots even
/// though they compile as the `dev` channel, so they never share `delta-dev` state.
fn identity_dir_name(channel: BuildChannel) -> &'static str {
    match crate::identity::InstallIdentity::current() {
        Some(identity) if identity.is_test() && channel == BuildChannel::current() => {
            identity.name()
        }
        _ => channel.dir_name(),
    }
}

fn default_channel_data_dir(channel: BuildChannel) -> Result<PathBuf, PathResolutionError> {
    let local_app_data = std::env::var("LOCALAPPDATA")
        .map_err(|_| PathResolutionError::MissingBaseFolder("LOCALAPPDATA".to_string()))?;
    if local_app_data.trim().is_empty() {
        return Err(PathResolutionError::MissingBaseFolder(
            "LOCALAPPDATA is empty".to_string(),
        ));
    }
    Ok(PathBuf::from(local_app_data).join(identity_dir_name(channel)))
}

/// Ensure `data_dir` is a writable directory; if it is a file or cannot be created/written,
/// return `PathResolutionError::DataDirNotWritable` immediately without any fallback (S-11).
pub fn ensure_writable_data_dir(data_dir: &Path) -> Result<(), PathResolutionError> {
    if data_dir.exists() && !data_dir.is_dir() {
        return Err(PathResolutionError::DataDirNotWritable {
            path: data_dir.display().to_string(),
            reason: "path exists and is not a directory".to_string(),
        });
    }
    fs::create_dir_all(data_dir).map_err(|e| PathResolutionError::DataDirNotWritable {
        path: data_dir.display().to_string(),
        reason: e.to_string(),
    })?;

    let probe_path = data_dir.join(".write_probe.tmp");
    match OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&probe_path)
    {
        Ok(_) => {
            let _ = fs::remove_file(&probe_path);
            Ok(())
        }
        Err(e) => Err(PathResolutionError::DataDirNotWritable {
            path: data_dir.display().to_string(),
            reason: e.to_string(),
        }),
    }
}

/// Exclusive OS-level file lock (`delta.lock`) using Windows `LockFileEx(LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY)`.
/// Automatically released by OS if the process crashes or exits (S-11).
pub struct InstanceLock {
    lock_path: PathBuf,
    file: File,
}

impl InstanceLock {
    pub fn acquire(data_dir: &Path) -> Result<Self, PathResolutionError> {
        ensure_writable_data_dir(data_dir)?;
        let lock_path = data_dir.join("delta.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|e| PathResolutionError::LockFailed {
                path: lock_path.display().to_string(),
                reason: e.to_string(),
            })?;

        #[cfg(windows)]
        unsafe {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
            };
            use windows_sys::Win32::System::IO::OVERLAPPED;

            let handle = file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
            let mut overlapped: OVERLAPPED = std::mem::zeroed();
            let ok = LockFileEx(
                handle,
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                1,
                0,
                &mut overlapped,
            );
            if ok == 0 {
                return Err(PathResolutionError::InstanceLocked(
                    lock_path.display().to_string(),
                ));
            }
        }

        Ok(Self { lock_path, file })
    }

    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
            use windows_sys::Win32::System::IO::OVERLAPPED;

            let handle = self.file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
            let mut overlapped: OVERLAPPED = std::mem::zeroed();
            let _ = UnlockFileEx(handle, 0, 1, 0, &mut overlapped);
        }
    }
}

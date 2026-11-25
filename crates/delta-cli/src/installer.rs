//! NSIS installers, local dev updates, signed release artifacts and M2 migration
//! (`S-25`..`S-34`). Installers are built only from a clean, fingerprinted workspace; the
//! signing key never reaches logs or artifacts.

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use delta_app::{
    run_quiet_command, run_quiet_command_with_timeout,
    update::{self, UpdateManifest},
    validate_package_version, ControlCommand, ControlStatus, ErrorEnvelope, InstallIdentity,
};
use delta_data::sha256_file;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::packaging::{
    collect_paths, collect_workspace_licenses, copy_tree, failure, inspect_product_binary,
    pe_imports, reject_ancestors_links, reject_link, relative_file, run_static_crt_release_build,
    CollectedLicenses, InstallManifest,
};

/// `fs::canonicalize` without the `\\?\` prefix, so paths read normally in logs and NSIS.
pub(crate) fn plain_canonical(path: &Path) -> std::io::Result<PathBuf> {
    let resolved = fs::canonicalize(path)?;
    let text = resolved.to_string_lossy();
    Ok(match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => resolved,
    })
}

/// Workspace-relative directory for user-facing installers (ignored by Git).
pub(crate) const DIST_DIR: &str = "dist";
const RELEASE_KEY_FILE: &str = "update-signing.key";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct InstallerArtifact {
    pub identity: String,
    pub version: String,
    pub setup_path: PathBuf,
    pub setup_sha256: String,
    pub size: u64,
    pub source_fingerprint: String,
    pub nsis: String,
    pub built_at: String,
    pub binary_imports: Value,
}

impl InstallerArtifact {
    pub fn sidecar(setup: &Path) -> PathBuf {
        setup.with_extension("package.json")
    }

    /// Read the sidecar and confirm it still describes the installer bytes.
    pub fn load(setup: &Path) -> Result<Self, ErrorEnvelope> {
        let artifact: Self = serde_json::from_slice(
            &fs::read(Self::sidecar(setup)).map_err(|e| failure("INSTALLER_SIDECAR", e))?,
        )
        .map_err(|e| failure("INSTALLER_SIDECAR", e))?;
        if sha256_file(setup).map_err(|e| failure("INSTALLER_HASH", e))? != artifact.setup_sha256 {
            return Err(failure(
                "INSTALLER_HASH",
                "installer bytes differ from their package description",
            ));
        }
        Ok(artifact)
    }

    pub fn identity(&self) -> Result<InstallIdentity, ErrorEnvelope> {
        InstallIdentity::parse(&self.identity)
            .ok_or_else(|| failure("INSTALLER_IDENTITY", "unknown installer identity"))
    }
}

/// The installer template, relative to the repository root.
pub(crate) const NSIS_TEMPLATE: &str = "crates/delta-cli/installer/delta.nsi";
/// The pinned NSIS build (`S-25`), unpacked by `scripts/fetch_nsis.py` (download and SHA-256
/// fixed there) under the ignored `.local/`. The template refuses any other NSIS version.
pub(crate) const NSIS_DIR: &str = ".local/tools/nsis-3.13";
pub(crate) const NSIS_VERSION: &str = "3.13";

/// Locate the pinned NSIS compiler: `DELTA_MAKENSIS`, else the copy `scripts/fetch_nsis.py`
/// unpacked. A missing compiler fails the build with the command that fetches it.
pub(crate) fn find_makensis(root: &Path) -> Result<PathBuf, ErrorEnvelope> {
    let candidate = match std::env::var_os("DELTA_MAKENSIS") {
        Some(explicit) if !explicit.is_empty() => PathBuf::from(explicit),
        _ => root.join(NSIS_DIR).join("makensis.exe"),
    };
    if candidate.is_file() {
        return Ok(candidate);
    }
    Err(failure(
        "NSIS_MISSING",
        format!(
            "NSIS {NSIS_VERSION} (makensis.exe) not found at {}; run `python -B scripts/fetch_nsis.py` (pinned download, SHA-256 checked) or set DELTA_MAKENSIS. No ZIP fallback is produced.",
            candidate.display()
        ),
    ))
}

/// Escape text for an NSIS double-quoted string (`$` starts a variable).
fn nsis_escape(text: &str) -> String {
    text.replace('$', "$$")
}

/// The last lines makensis printed (errors go to stdout), for a failure message.
fn output_tail(stdout: &str, stderr: &str) -> String {
    let lines: Vec<&str> = stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let tail = lines[lines.len().saturating_sub(6)..].join(" | ");
    if tail.is_empty() {
        "no output".into()
    } else {
        tail
    }
}

/// The `DELTA_REMOVE_INSTALLED_FILES` macro: every staged file, then its folders deepest
/// first. The uninstaller (and the next version's installer, through `/UPDATE`) removes
/// exactly these, so files a later version drops do not linger and user files stay.
/// `DELTA_CHECK_FILES_WRITABLE` checks the same files before the installer changes anything.
pub(crate) fn render_remove_files_nsh(stage: &Path) -> Result<String, ErrorEnvelope> {
    let mut files = Vec::new();
    collect_paths(stage, stage, &mut files)?;
    if files.is_empty() {
        return Err(failure("PACKAGE_STAGE", "no files to install"));
    }
    files.sort();
    let mut folders = std::collections::BTreeSet::new();
    for file in &files {
        let mut parent = Path::new(file).parent();
        while let Some(folder) = parent.filter(|p| !p.as_os_str().is_empty()) {
            folders.insert(folder.to_string_lossy().replace('\\', "/"));
            parent = folder.parent();
        }
    }
    let mut folders: Vec<String> = folders.into_iter().collect();
    folders.sort_by(|a, b| {
        b.matches('/')
            .count()
            .cmp(&a.matches('/').count())
            .then_with(|| a.cmp(b))
    });
    let windows = |relative: &str| nsis_escape(&relative.replace('/', "\\"));
    let mut lines = vec![
        "; Generated by delta-cli package-build: the files this version installs. Do not edit."
            .to_string(),
        "!macro DELTA_REMOVE_INSTALLED_FILES".to_string(),
    ];
    lines.extend(
        files
            .iter()
            .map(|file| format!("  Delete \"$INSTDIR\\{}\"", windows(file))),
    );
    lines.extend(
        folders
            .iter()
            .map(|folder| format!("  RMDir \"$INSTDIR\\{}\"", windows(folder))),
    );
    lines.push("!macroend".into());
    lines.push("!macro DELTA_CHECK_FILES_WRITABLE".into());
    lines.extend(
        files
            .iter()
            .map(|file| format!("  !insertmacro DELTA_CHECK_WRITABLE \"{}\"", windows(file))),
    );
    lines.push("!macroend".into());
    Ok(lines.join("\n") + "\n")
}

fn git_tags(root: &Path) -> Result<Vec<String>, ErrorEnvelope> {
    let result = run_quiet_command(
        OsStr::new("git"),
        ["tag", "--list"],
        Some(root),
        &BTreeMap::new(),
    )
    .map_err(|e| failure("GIT_TAGS", e))?;
    if result.status_code != Some(0) {
        return Err(failure("GIT_TAGS", "git tag --list failed"));
    }
    Ok(result.stdout.lines().map(str::to_string).collect())
}

/// Next development version for `identity` from the repository tags.
pub(crate) fn default_dev_version(root: &Path, at: DateTime<Utc>) -> Result<String, ErrorEnvelope> {
    delta_app::dev_version(&git_tags(root)?, at)
        .map(|v| v.to_string())
        .map_err(|e| failure(e.code(), e))
}

fn numeric_version(version: &semver::Version) -> String {
    format!("{}.{}.{}.0", version.major, version.minor, version.patch)
}

/// Icon of an identity's setup and uninstall entry, relative to the repository root: the
/// development identities show `icon-dev.ico` (the release icon with a "DEV" badge), the same file
/// the `delta-desktop` and `delta-cli` build scripts embed for them.
fn identity_icon(identity: InstallIdentity) -> &'static str {
    if identity.is_dev_like() {
        "crates/delta-desktop/assets/icon-dev.ico"
    } else {
        "crates/delta-desktop/assets/icon.ico"
    }
}

/// The `-D` values `package-build` passes to `delta.nsi` (each one the template requires).
fn nsis_defines(
    identity: InstallIdentity,
    version: &semver::Version,
    stage: &Path,
    setup_path: &Path,
    icon: &Path,
    files_nsh: &Path,
) -> Vec<(&'static str, String)> {
    vec![
        ("DELTA_IDENTITY", identity.name().to_string()),
        ("DELTA_APP_ID", identity.app_id().to_string()),
        ("DELTA_DISPLAY_NAME", identity.display_name().to_string()),
        ("DELTA_VERSION", version.to_string()),
        ("DELTA_NUMERIC_VERSION", numeric_version(version)),
        ("DELTA_DESKTOP_EXE", identity.desktop_exe().to_string()),
        ("DELTA_CLI_EXE", identity.cli_exe().to_string()),
        ("DELTA_MUTEX", identity.mutex_name()),
        ("DELTA_AUMID", identity.app_user_model_id().to_string()),
        // Dev-like identities put the desktop shortcut back on every silent over-install,
        // a visible sign that a new local build was installed.
        (
            "DELTA_DESKTOP_ON_UPDATE",
            if identity.is_dev_like() { "1" } else { "0" }.to_string(),
        ),
        ("DELTA_SOURCE_DIR", stage.display().to_string()),
        ("DELTA_OUT_FILE", setup_path.display().to_string()),
        // The setup and the uninstall entry show the same icon as the installed binaries.
        ("DELTA_ICON", icon.display().to_string()),
        ("DELTA_FILES_NSH", files_nsh.display().to_string()),
    ]
}

/// Shared, once-per-invocation preparation: workspace checks, license inventory, compiler.
pub(crate) struct PackagingContext {
    root: PathBuf,
    output: PathBuf,
    licenses: CollectedLicenses,
    makensis: PathBuf,
    tags: Vec<String>,
    pub fingerprint: String,
}

impl PackagingContext {
    pub fn prepare(root: &Path, output: &Path) -> Result<Self, ErrorEnvelope> {
        let root = plain_canonical(root).map_err(|e| failure("PACKAGE_ROOT", e))?;
        if !root.join(NSIS_TEMPLATE).is_file() {
            return Err(failure(
                "PACKAGE_ROOT",
                "build requires the DELTA workspace",
            ));
        }
        let makensis = find_makensis(&root)?;
        let fingerprint = crate::verify::source_fingerprint(&root)?;
        fs::create_dir_all(output).map_err(|e| failure("PACKAGE_OUTPUT", e))?;
        let output = plain_canonical(output).map_err(|e| failure("PACKAGE_OUTPUT", e))?;
        let licenses = collect_workspace_licenses(&root, &output, false)?;
        let tags = git_tags(&root)?;
        Ok(Self {
            root,
            output,
            licenses,
            makensis,
            tags,
            fingerprint,
        })
    }

    pub fn output(&self) -> &Path {
        &self.output
    }

    pub fn validate_version(
        &self,
        identity: InstallIdentity,
        version: &str,
    ) -> Result<semver::Version, ErrorEnvelope> {
        validate_package_version(identity, version, &self.tags).map_err(|e| failure(e.code(), e))
    }

    /// Release build with static CRT, staged files and an NSIS `setup.exe` in `target_dir`.
    pub fn build(
        &self,
        identity: InstallIdentity,
        version: &str,
        target_dir: &Path,
    ) -> Result<InstallerArtifact, ErrorEnvelope> {
        let parsed = self.validate_version(identity, version)?;
        let version = parsed.to_string();
        let channel = identity.channel().as_str();
        let target = self.root.join(".local/target-packages");
        let environment = BTreeMap::from([
            ("DELTA_CHANNEL".into(), channel.to_string()),
            ("DELTA_IDENTITY".into(), identity.name().to_string()),
            ("DELTA_VERSION".into(), version.clone()),
            ("DELTA_SOURCE_FINGERPRINT".into(), self.fingerprint.clone()),
        ]);
        let started = Instant::now();
        let build = run_static_crt_release_build(&self.root, &target, environment)?;
        fs::create_dir_all(target_dir).map_err(|e| failure("PACKAGE_OUTPUT", e))?;
        let log_name = format!("build-{}-{version}-log.json", identity.name());
        fs::write(
            self.output.join(&log_name),
            serde_json::to_vec_pretty(&build).map_err(|e| failure("PACKAGE_BUILD", e))?,
        )
        .map_err(|e| failure("PACKAGE_BUILD", e))?;
        if build.status_code != Some(0) || build.timed_out {
            return Err(failure(
                "PACKAGE_BUILD",
                format!(
                    "{} release build failed after {:?}; see {log_name}",
                    identity.name(),
                    started.elapsed()
                ),
            ));
        }
        let built = target.join("x86_64-pc-windows-msvc/release");
        // A short name: makensis cannot open files past MAX_PATH (260), and the gate stages
        // under %TEMP% with long third-party-licenses names below it.
        let stage = self.output.join(format!(
            "stage-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        fs::create_dir(&stage).map_err(|e| failure("PACKAGE_STAGE", e))?;
        let result = self.stage_and_compile(identity, &parsed, &built, &stage, target_dir);
        let _ = fs::remove_dir_all(&stage);
        result
    }

    fn stage_and_compile(
        &self,
        identity: InstallIdentity,
        version: &semver::Version,
        built: &Path,
        stage: &Path,
        target_dir: &Path,
    ) -> Result<InstallerArtifact, ErrorEnvelope> {
        let version_text = version.to_string();
        let desktop = stage.join(identity.desktop_exe());
        let cli = stage.join(identity.cli_exe());
        fs::copy(built.join("delta.exe"), &desktop).map_err(|e| failure("PACKAGE_COPY", e))?;
        fs::copy(built.join("delta-cli.exe"), &cli).map_err(|e| failure("PACKAGE_COPY", e))?;
        for (path, product) in [(&desktop, "delta-desktop"), (&cli, "delta-cli")] {
            verify_identity_binary(path, product, identity, &version_text)?;
        }
        let imports = json!({
            "target": "x86_64-pc-windows-msvc", "crt_static": true,
            "desktop": pe_imports(&desktop)?, "cli": pe_imports(&cli)?,
        });
        for name in ["LICENSE", "THIRD-PARTY-NOTICES.txt"] {
            fs::copy(self.root.join(name), stage.join(name))
                .map_err(|e| failure("PACKAGE_COPY", e))?;
        }
        copy_tree(&self.licenses.dir, &stage.join("third-party-licenses"))?;
        fs::write(stage.join("README.txt"), readme(identity, &version_text))
            .map_err(|e| failure("PACKAGE_README", e))?;
        let built_at = Utc::now().to_rfc3339();
        fs::write(
            stage.join("build-info.json"),
            serde_json::to_vec_pretty(&json!({
                "identity": identity.name(), "version": version_text, "channel": identity.channel().as_str(),
                "source_fingerprint": self.fingerprint, "built_at": built_at, "binary_imports": imports,
            }))
            .map_err(|e| failure("PACKAGE_INFO", e))?,
        )
        .map_err(|e| failure("PACKAGE_INFO", e))?;
        let setup_name = identity.setup_file_name(&version_text);
        let setup_path = target_dir.join(&setup_name);
        // The file list lives beside the stage, not in it: it is not installed.
        let mut files_nsh = stage.as_os_str().to_owned();
        files_nsh.push(".nsh");
        let files_nsh = PathBuf::from(files_nsh);
        fs::write(&files_nsh, render_remove_files_nsh(stage)?)
            .map_err(|e| failure("NSIS_COMPILE", e))?;
        let icon = self.root.join(identity_icon(identity));
        let defines = nsis_defines(identity, version, stage, &setup_path, &icon, &files_nsh);
        // `-WX`: a warning fails the build, so the template stays warning-free.
        let mut args: Vec<String> = [
            "-V2",
            "-WX",
            "-INPUTCHARSET",
            "UTF8",
            "-OUTPUTCHARSET",
            "UTF8",
        ]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
        args.extend(
            defines
                .iter()
                .map(|(name, value)| format!("-D{name}={value}")),
        );
        args.push(self.root.join(NSIS_TEMPLATE).display().to_string());
        let compiled = run_quiet_command_with_timeout(
            self.makensis.as_os_str(),
            &args,
            Some(&self.root),
            &BTreeMap::new(),
            Duration::from_secs(300),
        );
        let _ = fs::remove_file(&files_nsh);
        let compiled = compiled.map_err(|e| failure("NSIS_COMPILE", e))?;
        let log_name = format!("makensis-{}-{version_text}-log.json", identity.name());
        fs::write(
            self.output.join(&log_name),
            serde_json::to_vec_pretty(&compiled).map_err(|e| failure("NSIS_COMPILE", e))?,
        )
        .map_err(|e| failure("NSIS_COMPILE", e))?;
        if compiled.status_code != Some(0) || compiled.timed_out || !setup_path.is_file() {
            // The log may sit in a temporary folder a gate removes, so the reason goes in the message too.
            return Err(failure(
                "NSIS_COMPILE",
                format!(
                    "NSIS compilation failed (exit {:?}, timed out: {}); see {log_name}: {}",
                    compiled.status_code,
                    compiled.timed_out,
                    output_tail(&compiled.stdout, &compiled.stderr)
                ),
            ));
        }
        let artifact = InstallerArtifact {
            identity: identity.name().into(),
            version: version_text,
            setup_sha256: sha256_file(&setup_path).map_err(|e| failure("INSTALLER_HASH", e))?,
            size: fs::metadata(&setup_path)
                .map_err(|e| failure("INSTALLER_HASH", e))?
                .len(),
            setup_path: setup_path.clone(),
            source_fingerprint: self.fingerprint.clone(),
            nsis: format!("{NSIS_VERSION} (template-enforced)"),
            built_at,
            binary_imports: imports,
        };
        fs::write(
            InstallerArtifact::sidecar(&setup_path),
            serde_json::to_vec_pretty(&artifact).map_err(|e| failure("INSTALLER_SIDECAR", e))?,
        )
        .map_err(|e| failure("INSTALLER_SIDECAR", e))?;
        if crate::verify::source_fingerprint(&self.root)? != self.fingerprint {
            return Err(failure(
                "PACKAGE_SOURCE_CHANGED",
                "source changed during packaging; artifacts are not accepted",
            ));
        }
        Ok(artifact)
    }
}

fn readme(identity: InstallIdentity, version: &str) -> String {
    format!(
        "{name} {version}\r\n\r\n\
         Windows x64, installed for the current user without administrator rights.\r\n\
         Application data stays in %LOCALAPPDATA%\\{id}; exports and backups in Documents\\{id}.\r\n\
         Uninstalling (Settings > Apps) removes the installed files only and keeps all data.\r\n\
         The installer is not Authenticode-signed: Windows SmartScreen may show \"Windows protected your PC\";\r\n\
         choose \"More info\" > \"Run anyway\" only for a file downloaded from the official DELTA release page.\r\n\
         Release updates are verified with an Ed25519 signature before they are installed.\r\n",
        name = identity.display_name(),
        id = identity.name(),
    )
}

/// Check `--build-info` of a packaged or installed binary against its identity and version.
pub(crate) fn verify_identity_binary(
    path: &Path,
    product: &str,
    identity: InstallIdentity,
    version: &str,
) -> Result<Value, ErrorEnvelope> {
    let info = inspect_product_binary(path)?;
    if info["product"] != product
        || info["identity"] != identity.name()
        || info["channel"] != identity.channel().as_str()
        || info["version"] != version
    {
        return Err(failure(
            "PACKAGE_BINARY",
            format!("binary identity or version differs: {info}"),
        ));
    }
    Ok(info)
}

// ---------------------------------------------------------------------------
// Installed state (HKCU uninstall key written by the NSIS installer)
// ---------------------------------------------------------------------------

pub(crate) fn installed_value(identity: InstallIdentity, name: &str) -> Option<String> {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Registry::{
            RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ,
        };
        let key: Vec<u16> = identity
            .uninstall_key()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let value: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let mut size = 0u32;
        if RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        ) != 0
            || size == 0
        {
            return None;
        }
        let mut buffer = vec![0u16; (size as usize).div_ceil(2)];
        let mut out_size = (buffer.len() * 2) as u32;
        let status = RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut out_size,
        );
        if status != 0 {
            return None;
        }
        let used = (out_size as usize / 2).min(buffer.len());
        let length = buffer[..used].iter().position(|c| *c == 0).unwrap_or(used);
        Some(String::from_utf16_lossy(&buffer[..length]))
    }
    #[cfg(not(windows))]
    {
        let _ = (identity, name);
        None
    }
}

/// `(ProductVersion string, FileVersion numbers)` from a PE `VERSIONINFO` resource.
pub(crate) fn file_version_info(path: &Path) -> Option<(String, [u16; 4])> {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO,
        };
        let wide = |text: &str| -> Vec<u16> { text.encode_utf16().chain(Some(0)).collect() };
        let file = wide(&path.to_string_lossy());
        let size = GetFileVersionInfoSizeW(file.as_ptr(), std::ptr::null_mut());
        if size == 0 {
            return None;
        }
        // Allocate 4-byte aligned storage for Win32 VERSIONINFO structs; still use unaligned
        // reads below so any internal offset returned by VerQueryValueW is guaranteed sound.
        let mut data = vec![0u32; (size as usize).div_ceil(4)];
        if GetFileVersionInfoW(file.as_ptr(), 0, size, data.as_mut_ptr().cast()) == 0 {
            return None;
        }
        let mut pointer: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut length = 0u32;
        if VerQueryValueW(
            data.as_ptr().cast(),
            wide("\\").as_ptr(),
            &mut pointer,
            &mut length,
        ) == 0
            || pointer.is_null()
            || (length as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>()
        {
            return None;
        }
        let fixed = std::ptr::read_unaligned(pointer as *const VS_FIXEDFILEINFO);
        let numbers = [
            (fixed.dwFileVersionMS >> 16) as u16,
            (fixed.dwFileVersionMS & 0xffff) as u16,
            (fixed.dwFileVersionLS >> 16) as u16,
            (fixed.dwFileVersionLS & 0xffff) as u16,
        ];
        pointer = std::ptr::null_mut();
        length = 0;
        if VerQueryValueW(
            data.as_ptr().cast(),
            wide("\\VarFileInfo\\Translation").as_ptr(),
            &mut pointer,
            &mut length,
        ) == 0
            || pointer.is_null()
            || length < 4
        {
            return None;
        }
        let lang = std::ptr::read_unaligned(pointer as *const u16);
        let codepage = std::ptr::read_unaligned((pointer as *const u16).add(1));
        let query = format!(
            "\\StringFileInfo\\{:04x}{:04x}\\ProductVersion",
            lang, codepage
        );
        pointer = std::ptr::null_mut();
        length = 0;
        if VerQueryValueW(
            data.as_ptr().cast(),
            wide(&query).as_ptr(),
            &mut pointer,
            &mut length,
        ) == 0
            || pointer.is_null()
            || length == 0
        {
            return None;
        }
        let mut text = Vec::with_capacity(length as usize);
        let base = pointer as *const u16;
        for i in 0..(length as usize) {
            let unit = std::ptr::read_unaligned(base.add(i));
            if unit == 0 {
                break;
            }
            text.push(unit);
        }
        Some((String::from_utf16_lossy(&text), numbers))
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

pub(crate) fn installed_version(identity: InstallIdentity) -> Option<String> {
    installed_value(identity, "DisplayVersion")
}

pub(crate) fn install_dir(identity: InstallIdentity) -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(|base| PathBuf::from(base).join("Programs").join(identity.name()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SetupRun {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub elapsed_ms: u64,
    pub log: PathBuf,
}

/// Run an installer silently (`/S /LOG=<log>`). The quiet runner hides consoles and aborts if
/// any window becomes visible (`D-25`).
pub(crate) fn run_setup_silently(setup: &Path, log: &Path) -> Result<SetupRun, ErrorEnvelope> {
    let started = Instant::now();
    let result = run_quiet_command_with_timeout(
        setup.as_os_str(),
        update::silent_installer_args(log, None),
        None,
        &BTreeMap::new(),
        Duration::from_secs(300),
    )
    .map_err(|e| failure("SETUP_RUN", e))?;
    Ok(SetupRun {
        exit_code: result.status_code,
        timed_out: result.timed_out,
        elapsed_ms: started.elapsed().as_millis() as u64,
        log: log.to_path_buf(),
    })
}

/// Append `line` to an installer's `/LOG=` file the way the installer itself does. Logging is
/// best effort: a log that cannot be written never fails the installation.
pub(crate) fn append_setup_log(log: &Path, line: &str) {
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(log) {
        let _ = write!(file, "DELTA: {line}\r\n");
    }
}

/// Line the uninstaller appends to its `/LOG=` file when it is done.
const UNINSTALL_DONE_MARKER: &str = "DELTA: uninstall done";

/// Run the identity's uninstaller silently and wait until its key and files are gone.
pub(crate) fn run_uninstall_silently(
    identity: InstallIdentity,
    log: &Path,
) -> Result<SetupRun, ErrorEnvelope> {
    let started = Instant::now();
    let directory =
        install_dir(identity).ok_or_else(|| failure("UNINSTALL", "LOCALAPPDATA is not set"))?;
    let uninstaller = directory.join("Uninstall.exe");
    if !uninstaller.is_file() {
        return Err(failure("UNINSTALL", "uninstaller is missing"));
    }
    // An NSIS uninstaller copies itself to %TEMP% and exits; the copy does the work. Inside a
    // kill-on-close job that copy would be cut off before it removes Uninstall.exe, so it is
    // started outside the job and the wait is for its own completion line in the log.
    delta_app::spawn_detached_app(
        uninstaller.as_os_str(),
        ["/S".to_string(), delta_app::update::installer_log_arg(log)],
        &BTreeMap::new(),
        false,
    )
    .map_err(|e| failure("UNINSTALL", e))?;
    let finished = |log: &Path| {
        installed_version(identity).is_none()
            && !uninstaller.exists()
            && fs::read(log)
                .map(|bytes| String::from_utf8_lossy(&bytes).contains(UNINSTALL_DONE_MARKER))
                .unwrap_or(false)
    };
    while started.elapsed() < Duration::from_secs(180) && !finished(log) {
        std::thread::sleep(Duration::from_millis(200));
    }
    let done = finished(log);
    Ok(SetupRun {
        exit_code: done.then_some(0),
        timed_out: !done,
        elapsed_ms: started.elapsed().as_millis() as u64,
        log: log.to_path_buf(),
    })
}

// ---------------------------------------------------------------------------
// control / install-dev
// ---------------------------------------------------------------------------

/// `control <focus|quit-for-update>`: exit 0 ok/not running, 3 busy or save failed,
/// 4 timeout (the app keeps running), 6 rejected.
pub(crate) fn control(
    identity: InstallIdentity,
    command: ControlCommand,
    timeout: Duration,
) -> (u8, Value) {
    if !delta_app::identity_is_running(identity) {
        return (
            0,
            json!({"status": "not-running", "identity": identity.name()}),
        );
    }
    match delta_app::send_control_command(identity, command, timeout) {
        Ok(reply) => {
            let code = match reply.status {
                ControlStatus::Ok => 0,
                ControlStatus::Busy | ControlStatus::SaveFailed => 3,
                ControlStatus::Rejected => 6,
            };
            if code == 0
                && command == ControlCommand::QuitForUpdate
                && !delta_app::wait_for_exit(&identity.mutex_name(), timeout)
            {
                return (4, json!({"status": "timeout", "reply": reply}));
            }
            (code, json!({"status": reply.status, "reply": reply}))
        }
        Err(error) => (
            if matches!(error, delta_app::InstanceError::Timeout(_)) {
                4
            } else {
                3
            },
            json!({"status": "error", "code": error.code(), "message": error.to_string()}),
        ),
    }
}

/// Newest built package of `identity` in `directory` whose sidecar still matches.
pub(crate) fn latest_package(
    directory: &Path,
    identity: InstallIdentity,
) -> Result<PathBuf, ErrorEnvelope> {
    let prefix = format!("{}-setup-", identity.name());
    let mut best: Option<(semver::Version, PathBuf)> = None;
    for entry in fs::read_dir(directory).map_err(|e| failure("INSTALL_DEV_PACKAGE", e))? {
        let path = entry.map_err(|e| failure("INSTALL_DEV_PACKAGE", e))?.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if !name.starts_with(&prefix) || !name.ends_with("-x64.exe") {
            continue;
        }
        let Ok(artifact) = InstallerArtifact::load(&path) else {
            continue;
        };
        let Ok(version) = semver::Version::parse(&artifact.version) else {
            continue;
        };
        if artifact.identity == identity.name()
            && best.as_ref().is_none_or(|(current, _)| version > *current)
        {
            best = Some((version, path));
        }
    }
    best.map(|(_, path)| path).ok_or_else(|| {
        failure(
            "INSTALL_DEV_PACKAGE",
            format!(
                "no verified {} package found; run package-build first",
                identity.name()
            ),
        )
    })
}

/// Keep only `keep` among this identity's installers in `directory`: remove older
/// `<identity>-setup-*-x64.exe` files with their `.package.json` sidecars. Other identities
/// and unrelated files are never touched. Returns the removed installer names.
pub(crate) fn prune_older_packages(
    directory: &Path,
    identity: InstallIdentity,
    keep: &Path,
) -> Result<Vec<String>, ErrorEnvelope> {
    let prefix = format!("{}-setup-", identity.name());
    let keep_name = keep
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let mut removed = Vec::new();
    for entry in fs::read_dir(directory).map_err(|e| failure("PACKAGE_PRUNE", e))? {
        let path = entry.map_err(|e| failure("PACKAGE_PRUNE", e))?.path();
        let Some(name) = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        // `delta-dev-setup-…` must not match the `delta-setup-` prefix of another identity:
        // the version that follows the prefix always starts with a digit.
        let is_installer = name
            .strip_prefix(&prefix)
            .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
            && name.ends_with("-x64.exe");
        if !is_installer || name == keep_name || !path.is_file() {
            continue;
        }
        fs::remove_file(&path).map_err(|e| failure("PACKAGE_PRUNE", e))?;
        let sidecar = InstallerArtifact::sidecar(&path);
        if sidecar.is_file() {
            fs::remove_file(&sidecar).map_err(|e| failure("PACKAGE_PRUNE", e))?;
        }
        removed.push(name);
    }
    removed.sort();
    Ok(removed)
}

/// `install-dev` receipt (`S-30`). The fields are the same camelCase set the author's other
/// desktop apps write, plus the DELTA-specific ones after `at`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InstallDevReceipt {
    pub identity: String,
    pub installed: bool,
    /// Nothing changed on purpose (`reason`: `disabled` or `window_open`); not a failure.
    pub skipped: bool,
    pub reason: Option<String>,
    pub error: Option<String>,
    pub first_install: bool,
    pub old_version: Option<String>,
    pub new_version: String,
    pub install_location: String,
    pub was_running: bool,
    /// A window of the running app was open when `install-dev` started.
    pub window_open: bool,
    /// `install-dev` waited for that window to close.
    pub deferred: bool,
    pub defer_ms: u64,
    pub quit_ms: u64,
    pub relaunched: bool,
    /// Started with `--background` (tray icon only without showing a window; D-36).
    pub background: bool,
    /// The started app could not leave the caller's job object (an agent's command sandbox)
    /// and may end with the command; open it once from the Start menu (D-36).
    pub in_job: bool,
    pub elapsed_ms: u64,
    pub at: String,
    pub installer: String,
    pub verified_registry_version: Option<String>,
    pub verified_exe_version: Option<String>,
    pub restart_process_id: Option<u32>,
    /// `desktop_shell` (started by `explorer.exe`, outside any caller job; D-36) or `detached`.
    pub launch_method: Option<String>,
    pub guidance: Option<String>,
    pub setup: Option<SetupRun>,
}

impl InstallDevReceipt {
    /// A receipt with what is known before anything is installed.
    pub fn start(identity: &str, package: &Path, new_version: &str) -> Self {
        Self {
            identity: identity.into(),
            new_version: new_version.into(),
            installer: package
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            at: Utc::now().to_rfc3339(),
            ..Self::default()
        }
    }
}

/// Flag that starts the desktop app in the system tray without showing a window (D-36).
pub(crate) const BACKGROUND_FLAG: &str = "--background";

/// `DELTA_DEV_AUTO_INSTALL=0` turns `install-dev` of the real `delta-dev` off; the gates'
/// `delta-dev-test` runs are never affected.
pub(crate) const AUTO_INSTALL_SWITCH: &str = "DELTA_DEV_AUTO_INSTALL";

fn auto_install_disabled(identity: InstallIdentity, switch: Option<&str>) -> bool {
    identity == InstallIdentity::Dev && switch.is_some_and(|value| value.trim() == "0")
}

/// While a window of the running app is open the person is using it: `install-dev` checks
/// every 2 s and gives up after 10 minutes (the install is then skipped, not failed).
pub(crate) const WINDOW_POLL: Duration = Duration::from_secs(2);
pub(crate) const WINDOW_WAIT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WindowWait {
    pub was_open: bool,
    pub still_open: bool,
    pub waited: Duration,
}

/// Wait while `is_open` says a window is open, polling every `poll` for at most `limit`.
/// The clock and the sleep are passed in so the decision is tested without waiting.
pub(crate) fn wait_while_window_open(
    limit: Duration,
    poll: Duration,
    mut is_open: impl FnMut() -> bool,
    mut sleep: impl FnMut(Duration),
    mut elapsed: impl FnMut() -> Duration,
) -> WindowWait {
    if !is_open() {
        return WindowWait {
            was_open: false,
            still_open: false,
            waited: Duration::ZERO,
        };
    }
    loop {
        let waited = elapsed();
        if waited >= limit {
            return WindowWait {
                was_open: true,
                still_open: true,
                waited,
            };
        }
        sleep(poll.min(limit - waited));
        if !is_open() {
            return WindowWait {
                was_open: true,
                still_open: false,
                waited: elapsed(),
            };
        }
    }
}

/// Arguments for the app started after `install-dev`: its previous arguments when it was
/// running, plus `--background` once (D-36).
fn background_launch_args(mut previous: Vec<String>) -> Vec<String> {
    if !previous.iter().any(|arg| arg == BACKGROUND_FLAG) {
        previous.push(BACKGROUND_FLAG.into());
    }
    previous
}

/// Whether `install-dev` should ask the desktop shell (`explorer.exe`) to launch the app.
///
/// Only the real `delta-dev` identity uses the desktop shell (and only when `delta-cli` runs
/// inside a job object): test identities (`--test-identity` in `M3-A-06`) must inherit the
/// gate's isolated `DELTA_DATA_DIR` / `DELTA_DOCUMENTS_DIR` / `LOCALAPPDATA` environment.
fn should_use_desktop_shell(identity: InstallIdentity, caller_in_job: bool) -> bool {
    !identity.is_test() && caller_in_job
}

/// `install-dev` (`S-30`): only dev-channel packages; never kills a running app. Installs on
/// first use too. While a window of the running app is open it waits (`WINDOW_WAIT`) and
/// then skips; after every successful install the app is started in the background, whether
/// it was running or not (D-36: the person keeps the dev build running).
/// A failed `install-dev`: the error and the receipt as far as it got.
#[derive(Debug)]
pub(crate) struct InstallDevFailure {
    pub error: ErrorEnvelope,
    pub receipt: Box<InstallDevReceipt>,
}

/// Where `install-dev` keeps its receipt: `install-dev.json` for `delta-dev`, a name of its
/// own for the gate's test identity, so a gate run never overwrites the real receipt.
pub(crate) fn install_dev_receipt_name(identity: InstallIdentity) -> String {
    if identity == InstallIdentity::Dev {
        "install-dev.json".into()
    } else {
        format!("install-dev-{}.json", identity.name())
    }
}

pub(crate) fn install_dev(
    package: &Path,
    expected: InstallIdentity,
    log_dir: &Path,
) -> Result<InstallDevReceipt, InstallDevFailure> {
    let started = Instant::now();
    let mut receipt = InstallDevReceipt::start(expected.name(), package, "");
    receipt.install_location = install_dir(expected)
        .map(|dir| dir.display().to_string())
        .unwrap_or_default();
    match install_dev_into(&mut receipt, package, expected, log_dir) {
        Ok(()) => {
            receipt.elapsed_ms = started.elapsed().as_millis() as u64;
            Ok(receipt)
        }
        Err(error) => {
            receipt.reason = Some(error.code.to_ascii_lowercase());
            receipt.error = Some(error.message.clone());
            receipt.elapsed_ms = started.elapsed().as_millis() as u64;
            Err(InstallDevFailure {
                error,
                receipt: Box::new(receipt),
            })
        }
    }
}

fn install_dev_into(
    receipt: &mut InstallDevReceipt,
    package: &Path,
    expected: InstallIdentity,
    log_dir: &Path,
) -> Result<(), ErrorEnvelope> {
    let artifact = InstallerArtifact::load(package)?;
    let identity = artifact.identity()?;
    if identity != expected || !identity.is_dev_like() {
        return Err(failure(
            "INSTALL_DEV_IDENTITY",
            format!(
                "install-dev only installs {} packages; refused {}",
                expected.name(),
                artifact.identity
            ),
        ));
    }
    let validated_version = semver::Version::parse(&artifact.version)
        .map_err(|e| {
            failure(
                "INSTALL_DEV_VERSION",
                format!("invalid package version: {e}"),
            )
        })?
        .to_string();
    let directory = install_dir(identity)
        .ok_or_else(|| failure("INSTALL_DEV_DIR", "LOCALAPPDATA is not set"))?;
    receipt.new_version = validated_version.clone();
    receipt.install_location = directory.display().to_string();
    let previous = installed_version(identity);
    receipt.first_install = previous.is_none();
    receipt.old_version = previous.clone();
    if auto_install_disabled(identity, std::env::var(AUTO_INSTALL_SWITCH).ok().as_deref()) {
        receipt.skipped = true;
        receipt.reason = Some("disabled".into());
        receipt.guidance = Some(format!(
            "{AUTO_INSTALL_SWITCH}=0 turns install-dev off; unset it to install {}",
            identity.name()
        ));
        return Ok(());
    }
    if let Some(previous) = &previous {
        if !delta_app::is_upgrade(previous, &validated_version) {
            return Err(failure(
                "INSTALL_DEV_DOWNGRADE",
                format!(
                    "installed {previous} is not older than package {validated_version}; nothing changed",
                ),
            ));
        }
    }
    let desktop_exe = directory.join(identity.desktop_exe());
    if delta_app::identity_is_running(identity) {
        let clock = Instant::now();
        let wait = wait_while_window_open(
            WINDOW_WAIT,
            WINDOW_POLL,
            || delta_app::exe_has_visible_window(&desktop_exe),
            std::thread::sleep,
            || clock.elapsed(),
        );
        receipt.window_open = wait.was_open;
        receipt.deferred = wait.was_open;
        receipt.defer_ms = wait.waited.as_millis() as u64;
        if wait.still_open {
            receipt.was_running = true;
            receipt.skipped = true;
            receipt.reason = Some("window_open".into());
            receipt.guidance = Some(format!(
                "a {} window stayed open for {} minutes; close it (the tray icon may stay) and run install-dev again",
                identity.display_name(),
                WINDOW_WAIT.as_secs() / 60
            ));
            return Ok(());
        }
    }
    // The person may have quit the app while install-dev waited.
    let was_running = delta_app::identity_is_running(identity);
    receipt.was_running = was_running;
    let mut relaunch_args = Vec::new();
    if was_running {
        let quit_started = Instant::now();
        let reply = delta_app::send_control_command(
            identity,
            ControlCommand::QuitForUpdate,
            Duration::from_secs(30),
        )
        .map_err(|e| {
            failure(
                "INSTALL_DEV_APP_RUNNING",
                format!(
                    "{} did not answer quit-for-update ({e}); nothing changed",
                    identity.name()
                ),
            )
        })?;
        if reply.status != ControlStatus::Ok {
            return Err(failure(
                "INSTALL_DEV_APP_BUSY",
                format!(
                    "{} answered {:?}: {}; nothing changed",
                    identity.name(),
                    reply.status,
                    reply.reason
                ),
            ));
        }
        if !delta_app::wait_for_exit(&identity.mutex_name(), Duration::from_secs(30)) {
            return Err(failure(
                "INSTALL_DEV_APP_RUNNING",
                "the application did not exit within 30 s; nothing changed",
            ));
        }
        receipt.quit_ms = quit_started.elapsed().as_millis() as u64;
        relaunch_args = reply.relaunch_args;
    }
    fs::create_dir_all(log_dir).map_err(|e| failure("INSTALL_DEV_LOG", e))?;
    let log = log_dir.join(format!(
        "install-dev-{}-{validated_version}.log",
        identity.name(),
    ));
    let setup = run_setup_silently(package, &log)?;
    receipt.setup = Some(setup.clone());
    let installed = installed_version(identity).unwrap_or_default();
    if setup.exit_code != Some(0) || setup.timed_out || installed != validated_version {
        // Setup stops before it changes anything when it can (exit codes 3-9): start again
        // whatever is installed, so the app this command quit does not stay closed.
        let mut restarted = String::new();
        if was_running && desktop_exe.is_file() {
            if let Ok((process_id, in_job, method)) = start_in_background(
                identity,
                &desktop_exe,
                background_launch_args(relaunch_args),
            ) {
                receipt.relaunched = true;
                receipt.background = true;
                receipt.restart_process_id = Some(process_id);
                receipt.launch_method = Some(method.into());
                receipt.in_job = in_job;
                restarted = "; started the installed copy again in the background".into();
            }
        }
        return Err(failure(
            "INSTALL_DEV_FAILED",
            format!(
                "setup exit {:?}; installed version {installed:?}{restarted}; see {}",
                setup.exit_code,
                log.display()
            ),
        ));
    }
    receipt.verified_registry_version = Some(installed.clone());
    let info = verify_identity_binary(&desktop_exe, "delta-desktop", identity, &validated_version)?;
    receipt.verified_exe_version = info["version"].as_str().map(str::to_string);
    receipt.installed = true;
    // Always start it again in the background (D-36: tray icon only, no main window). Inside a
    // job object (an agent's command sandbox may forbid breaking away and end every process when
    // the command returns) the desktop shell starts `delta-dev` first, so it is explorer's child
    // and outside any caller job; otherwise (or for isolated `--test-identity` gate runs, or when
    // the shell cannot), it breaks away when allowed. A job that forbids both still gets the app,
    // reported as `inJob` instead of failing the install.
    let (process_id, in_job, method) = start_in_background(
        identity,
        &desktop_exe,
        background_launch_args(relaunch_args),
    )
    .map_err(|e| {
        failure(
            "INSTALL_DEV_RESTART",
            format!("installed {installed}, but could not start it: {e}"),
        )
    })?;
    receipt.relaunched = true;
    receipt.background = true;
    receipt.restart_process_id = Some(process_id);
    receipt.launch_method = Some(method.into());
    receipt.in_job = in_job;
    if in_job {
        receipt.guidance = Some(format!(
            "{} was started inside this command's job object and may end with the command; open it once from the Start menu",
            identity.name()
        ));
    }
    Ok(())
}

/// Start the desktop app in the background (tray only): through the desktop shell inside a
/// job object, otherwise detached. Answers its process id, whether it stayed in a job, and how.
fn start_in_background(
    identity: InstallIdentity,
    desktop_exe: &Path,
    launch_args: Vec<String>,
) -> Result<(u32, bool, &'static str), String> {
    // Pass `SW_SHOWNORMAL` (1): `delta-dev --background` opens no window at startup (`show: false`),
    // and `SW_SHOWNORMAL` avoids overriding the first tray-click reveal with a minimized state.
    const SW_SHOWNORMAL: i32 = 1;
    let shell_pid = if should_use_desktop_shell(identity, delta_app::process_in_job(None)) {
        delta_app::spawn_via_desktop_shell(desktop_exe, &launch_args, SW_SHOWNORMAL).ok()
    } else {
        None
    };
    match shell_pid {
        // Launched by `explorer.exe`, so it is outside any caller sandbox job. Check against
        // `ACTIVE_JOB` rather than `process_in_job(Some(pid))`, because `delta-desktop` joins its
        // own `QuietJobObject` at startup and Windows 10/11 PCA/Shell routinely assigns GUI apps
        // launched via `ShellExecute` to a system job, both of which would falsely make
        // `IsProcessInJob(pid, NULL)` return true.
        Some(pid) => Ok((pid, delta_app::process_in_active_job(pid), "desktop_shell")),
        None => {
            let launch = delta_app::spawn_detached_app(
                desktop_exe.as_os_str(),
                launch_args,
                &BTreeMap::new(),
                true,
            )
            .map_err(|e| e.to_string())?;
            Ok((launch.process_id, !launch.broke_away_from_job, "detached"))
        }
    }
}

// ---------------------------------------------------------------------------
// Release signing (`S-32`)
// ---------------------------------------------------------------------------

/// Where the release signing key lives (`D-32`): a plain hex file in the user profile,
/// outside the repository and outside every application data root:
/// `%APPDATA%/delta-release/update-signing.key`.
pub(crate) fn default_release_key_file() -> Result<PathBuf, ErrorEnvelope> {
    std::env::var_os("APPDATA")
        .filter(|value| !value.is_empty())
        .map(|base| {
            PathBuf::from(base)
                .join("delta-release")
                .join(RELEASE_KEY_FILE)
        })
        .ok_or_else(|| failure("RELEASE_KEY_FILE", "APPDATA is not set; pass --key-file"))
}

/// Normalize a path (canonicalizing when present, otherwise resolving `.`/`..` against CWD)
/// so key-file comparisons cannot be bypassed via slashes, relative segments, or Windows casing.
fn normalized_path_key(path: &Path) -> String {
    let resolved = plain_canonical(path).unwrap_or_else(|_| {
        let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let mut clean = PathBuf::new();
        for comp in abs.components() {
            match comp {
                Component::CurDir => {}
                Component::ParentDir => {
                    clean.pop();
                }
                other => clean.push(other.as_os_str()),
            }
        }
        clean
    });
    let text = resolved.to_string_lossy();
    let stripped = text
        .strip_prefix(r"\\?\")
        .filter(|rest| !rest.starts_with("UNC\\"))
        .unwrap_or(&text);
    if cfg!(windows) {
        stripped.replace('/', "\\").to_ascii_lowercase()
    } else {
        stripped.to_string()
    }
}

fn same_path_normalized(a: &Path, b: &Path) -> bool {
    normalized_path_key(a) == normalized_path_key(b)
}

/// Whether `path` would land inside `repository` (including its enclosing git root if `repository`
/// is a subdirectory) or inside any git repository discovered along `path`'s existing ancestors.
fn inside_repository(path: &Path, repository: &Path) -> bool {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let repo_root = fs::canonicalize(repository).ok().map(|canon| {
        canon
            .ancestors()
            .find(|a| a.join(".git").exists())
            .map(Path::to_path_buf)
            .unwrap_or(canon)
    });
    let existing = absolute
        .ancestors()
        .find_map(|ancestor| fs::canonicalize(ancestor).ok());
    let Some(existing) = existing else {
        return false;
    };
    if repo_root
        .as_ref()
        .is_some_and(|root| existing.starts_with(root))
    {
        return true;
    }
    existing
        .ancestors()
        .any(|ancestor| ancestor.join(".git").exists())
}

/// Load the signing key. When `allow_environment` is true (release builds without an explicit
/// `--key-file`), accepts `UPDATE_SIGNING_KEY_PEM` or `DELTA_RELEASE_SIGNING_KEY`; otherwise reads
/// `key_file`. Supports both 32-byte hex and PKCS#8 Ed25519 PEM. The key never reaches logs or
/// artifacts.
pub(crate) fn load_signing_key(
    key_file: Option<&Path>,
    allow_environment: bool,
) -> Result<ed25519_dalek::SigningKey, ErrorEnvelope> {
    let env_key = if allow_environment {
        ["UPDATE_SIGNING_KEY_PEM", "DELTA_RELEASE_SIGNING_KEY"]
            .into_iter()
            .find_map(|name| std::env::var(name).ok().filter(|v| !v.trim().is_empty()))
    } else {
        None
    };
    let raw_key = match env_key {
        Some(value) => value,
        None => {
            let path = match key_file {
                Some(path) => path.to_path_buf(),
                None => default_release_key_file()?,
            };
            fs::read_to_string(&path).map_err(|_| {
                failure(
                    "RELEASE_KEY_MISSING",
                    format!(
                        "no signing key at {}; nothing was built or published",
                        path.display()
                    ),
                )
            })?
        }
    };
    update::parse_signing_key(raw_key.trim()).map_err(|_| {
        failure(
            "RELEASE_KEY_INVALID",
            "the signing key is not a valid Ed25519 hex or PKCS#8 PEM key",
        )
    })
}

/// Generate a new key file; refuses to overwrite or to write inside the repository. Prints
/// only the path and the public key.
pub(crate) fn release_keygen(key_file: &Path, repository: &Path) -> Result<Value, ErrorEnvelope> {
    if inside_repository(key_file, repository) {
        return Err(failure(
            "RELEASE_KEY_STORE",
            "refusing to write the signing key inside the repository",
        ));
    }
    if let Some(parent) = key_file.parent() {
        fs::create_dir_all(parent).map_err(|e| failure("RELEASE_KEY_STORE", e))?;
    }
    let key = update::generate_signing_key().map_err(|e| failure(e.code(), e))?;
    // `create_new` refuses an existing key atomically (no check-then-write window).
    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(key_file)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(failure(
                "RELEASE_KEY_EXISTS",
                format!("{} already exists; it was not replaced", key_file.display()),
            ))
        }
        Err(e) => return Err(failure("RELEASE_KEY_STORE", e)),
    };
    file.write_all(hex::encode(key.to_bytes()).as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| failure("RELEASE_KEY_STORE", e))?;
    Ok(json!({
        "status": "generated",
        "key_file": key_file,
        "public_key_hex": hex::encode(key.verifying_key().to_bytes()),
        "next": "Back up the key file offline (ACT-10), then set public_key_hex in crates/delta-app/src/release-signing-key.json",
    }))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ReleaseArtifacts {
    pub directory: PathBuf,
    pub setup: PathBuf,
    pub manifest: UpdateManifest,
    pub signature_file: PathBuf,
    pub public_key_hex: String,
    pub binary_imports: Value,
}

/// `release-build` (`S-32`): key first, then build, hash, sign, verify, publish atomically.
pub(crate) fn release_build(
    root: &Path,
    output: &Path,
    identity: InstallIdentity,
    version: &str,
    key_file: Option<&Path>,
) -> Result<ReleaseArtifacts, ErrorEnvelope> {
    let channel = identity.update_channel().ok_or_else(|| {
        failure(
            "RELEASE_IDENTITY",
            "release-build produces release or test-release identities only",
        )
    })?;
    let is_release = identity == InstallIdentity::Release;
    // Test rehearsals use their own throw-away key; the release key never signs them.
    if !is_release {
        let Some(path) = key_file else {
            return Err(failure(
                "RELEASE_KEY_FILE",
                "test releases must pass an explicit --key-file, never the release signing key",
            ));
        };
        if default_release_key_file().is_ok_and(|default| same_path_normalized(&default, path)) {
            return Err(failure(
                "RELEASE_KEY_FILE",
                "test releases must use a separate --key-file, never the release signing key",
            ));
        }
    }
    let key = load_signing_key(key_file, is_release && key_file.is_none())?;
    if identity == InstallIdentity::Release {
        let embedded = update::embedded_release_public_key().ok_or_else(|| {
            failure(
                "RELEASE_PUBLIC_KEY_NOT_EMBEDDED",
                "release-signing-key.json has no public key; installed releases could not verify updates",
            )
        })?;
        if embedded != key.verifying_key() {
            return Err(failure(
                "RELEASE_PUBLIC_KEY_MISMATCH",
                "the signing key does not match the embedded public key",
            ));
        }
    }
    let context = PackagingContext::prepare(root, output)?;
    let validated_version = context.validate_version(identity, version)?.to_string();
    let staging = context.output().join(format!(
        ".staging-{}-{}",
        identity.name(),
        uuid::Uuid::new_v4()
    ));
    let final_directory =
        context
            .output()
            .join(format!("{}-{}", identity.name(), validated_version));
    if final_directory.exists() {
        return Err(failure(
            "RELEASE_EXISTS",
            format!("{} already exists", final_directory.display()),
        ));
    }
    let result = (|| -> Result<ReleaseArtifacts, ErrorEnvelope> {
        let artifact = context.build(identity, &validated_version, &staging)?;
        let file = identity.setup_file_name(&artifact.version);
        let manifest = UpdateManifest {
            schema_version: 1,
            channel: channel.into(),
            version: artifact.version.clone(),
            file: file.clone(),
            size: artifact.size,
            sha256: artifact.setup_sha256.clone(),
            published_at: Utc::now().to_rfc3339(),
        };
        let signature = update::sign_manifest(&key, &manifest);
        update::verify_manifest_signature(&key.verifying_key(), &manifest, &signature)
            .map_err(|e| failure(e.code(), e))?;
        fs::write(
            staging.join(update::MANIFEST_FILE),
            serde_json::to_vec_pretty(&manifest).map_err(|e| failure("RELEASE_MANIFEST", e))?,
        )
        .map_err(|e| failure("RELEASE_MANIFEST", e))?;
        fs::write(staging.join(format!("{file}.sig")), &signature)
            .map_err(|e| failure("RELEASE_SIGNATURE", e))?;
        fs::write(
            staging.join("public-key.txt"),
            hex::encode(key.verifying_key().to_bytes()),
        )
        .map_err(|e| failure("RELEASE_SIGNATURE", e))?;
        fs::rename(&staging, &final_directory).map_err(|e| failure("RELEASE_PUBLISH", e))?;
        Ok(ReleaseArtifacts {
            setup: final_directory.join(&file),
            signature_file: final_directory.join(format!("{file}.sig")),
            directory: final_directory.clone(),
            manifest,
            public_key_hex: hex::encode(key.verifying_key().to_bytes()),
            binary_imports: artifact.binary_imports,
        })
    })();
    if result.is_err() && staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn plain_file_name(name: &str) -> Result<&str, ErrorEnvelope> {
    let path = Path::new(name);
    let mut comps = path.components();
    match (comps.next(), comps.next()) {
        (Some(Component::Normal(_)), None)
            if !name.is_empty() && !name.contains(['/', '\\', ':']) =>
        {
            Ok(name)
        }
        _ => Err(failure(
            "UPDATE_TEST_SOURCE",
            "manifest.file must be a plain filename without path separators",
        )),
    }
}

/// Serve a test-identity release from `127.0.0.1` for manual update checks (`H-04`). Only
/// `channel: test` artifacts are served; the release identity never accepts loopback HTTP.
pub(crate) fn serve_test_release(
    directory: &Path,
    duration: Duration,
) -> Result<(), ErrorEnvelope> {
    let manifest: UpdateManifest = serde_json::from_slice(
        &fs::read(directory.join(update::MANIFEST_FILE))
            .map_err(|e| failure("UPDATE_TEST_SOURCE", e))?,
    )
    .map_err(|e| failure("UPDATE_TEST_SOURCE", e))?;
    if manifest.channel != "test" {
        return Err(failure(
            "UPDATE_TEST_SOURCE",
            "only test-identity releases may be served from loopback",
        ));
    }
    let safe_file = plain_file_name(&manifest.file)?;
    let installer =
        fs::read(directory.join(safe_file)).map_err(|e| failure("UPDATE_TEST_SOURCE", e))?;
    let signature = fs::read_to_string(directory.join(format!("{safe_file}.sig")))
        .map_err(|e| failure("UPDATE_TEST_SOURCE", e))?;
    let server = update::UpdateLoopbackServer::start().map_err(|e| failure(e.code(), e))?;
    server.publish(&manifest, installer, Some(signature));
    let public_key = fs::read_to_string(directory.join("public-key.txt")).unwrap_or_default();
    crate::cli_args::print_json(&json!({
        "status": "serving",
        "version": manifest.version,
        "seconds": duration.as_secs(),
        "DELTA_UPDATE_SOURCE": server.latest_url(),
        "DELTA_UPDATE_PUBLIC_KEY": public_key.trim(),
    }));
    std::thread::sleep(duration);
    Ok(())
}

/// Verify a signed release directory using only the embedded release public key (never a private
/// key): checks manifest metadata, Ed25519 signature, installer size/SHA-256, tamper rejection,
/// and `SHA256SUMS` (when present).
pub(crate) fn release_verify(
    directory: &Path,
    expected_version: &str,
) -> Result<Value, ErrorEnvelope> {
    let embedded = update::embedded_release_public_key().ok_or_else(|| {
        failure(
            "RELEASE_PUBLIC_KEY_NOT_EMBEDDED",
            "release-signing-key.json has no public key",
        )
    })?;
    let manifest: UpdateManifest = serde_json::from_slice(
        &fs::read(directory.join(update::MANIFEST_FILE))
            .map_err(|e| failure("RELEASE_VERIFY", e))?,
    )
    .map_err(|e| failure("RELEASE_VERIFY", e))?;
    if manifest.version != expected_version {
        return Err(failure(
            "RELEASE_VERIFY",
            format!(
                "manifest version '{}' does not match expected '{expected_version}'",
                manifest.version
            ),
        ));
    }
    let safe_file = plain_file_name(&manifest.file)?;
    let installer = directory.join(safe_file);
    let sig_name = format!("{safe_file}.sig");
    let signature = fs::read_to_string(directory.join(&sig_name))
        .map_err(|e| failure("RELEASE_VERIFY", e))?
        .trim()
        .to_string();
    let policy = update::UpdatePolicy::release(Some(embedded));
    let offer = update::UpdateOffer {
        manifest: manifest.clone(),
        signature_hex: signature.clone(),
        installer_url: format!(
            "https://github.com/chialecode/delta/releases/download/v{expected_version}/{safe_file}"
        ),
    };
    update::verify_installer(&policy, &offer, &installer).map_err(|e| failure(e.code(), e))?;

    // Tamper-rejection checks against the verified release manifest & signature.
    let mut bad_hash = manifest.clone();
    bad_hash.sha256 = if bad_hash.sha256.starts_with('0') {
        format!("1{}", &bad_hash.sha256[1..])
    } else {
        format!("0{}", &bad_hash.sha256[1..])
    };
    if update::verify_manifest_signature(&embedded, &bad_hash, &signature).is_ok() {
        return Err(failure(
            "RELEASE_VERIFY_TAMPER",
            "tampered sha256 was not rejected",
        ));
    }
    let mut bad_version = manifest.clone();
    bad_version.version = format!("{expected_version}.9");
    if update::verify_manifest_signature(&embedded, &bad_version, &signature).is_ok() {
        return Err(failure(
            "RELEASE_VERIFY_TAMPER",
            "tampered version was not rejected",
        ));
    }
    let mut bad_channel = manifest.clone();
    bad_channel.channel = "test".into();
    let bad_channel_offer = update::UpdateOffer {
        manifest: bad_channel,
        signature_hex: signature.clone(),
        installer_url: offer.installer_url.clone(),
    };
    if !matches!(
        update::verify_installer(&policy, &bad_channel_offer, &installer),
        Err(update::UpdateError::ChannelMismatch { .. })
    ) {
        return Err(failure(
            "RELEASE_VERIFY_TAMPER",
            "cross-channel manifest was not rejected",
        ));
    }
    let missing_sig_offer = update::UpdateOffer {
        manifest: manifest.clone(),
        signature_hex: String::new(),
        installer_url: offer.installer_url.clone(),
    };
    if !matches!(
        update::verify_installer(&policy, &missing_sig_offer, &installer),
        Err(update::UpdateError::SignatureMissing)
    ) {
        return Err(failure(
            "RELEASE_VERIFY_TAMPER",
            "missing signature was not rejected",
        ));
    }
    let wrong_key = update::generate_signing_key().map_err(|e| failure(e.code(), e))?;
    if update::verify_manifest_signature(&wrong_key.verifying_key(), &manifest, &signature).is_ok()
    {
        return Err(failure(
            "RELEASE_VERIFY_TAMPER",
            "wrong public key was not rejected",
        ));
    }

    let sums_path = directory.join("SHA256SUMS");
    let mut sha256sums_checked = 0usize;
    if sums_path.is_file() {
        let content = fs::read_to_string(&sums_path).map_err(|e| failure("RELEASE_VERIFY", e))?;
        let mut seen = BTreeMap::new();
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            let Some((expected_hash, name)) = line.split_once("  ") else {
                return Err(failure(
                    "RELEASE_VERIFY_SUMS",
                    format!("malformed SHA256SUMS line: {line}"),
                ));
            };
            let safe_name = plain_file_name(name)?;
            let actual = sha256_file(&directory.join(safe_name))
                .map_err(|e| failure("RELEASE_VERIFY_SUMS", e))?;
            if actual != expected_hash {
                return Err(failure(
                    "RELEASE_VERIFY_SUMS",
                    format!("SHA256SUMS mismatch for {safe_name}"),
                ));
            }
            seen.insert(safe_name.to_string(), actual);
        }
        for required in [safe_file, sig_name.as_str(), update::MANIFEST_FILE] {
            if !seen.contains_key(required) {
                return Err(failure(
                    "RELEASE_VERIFY_SUMS",
                    format!("SHA256SUMS does not cover {required}"),
                ));
            }
        }
        sha256sums_checked = seen.len();
    }

    Ok(json!({
        "status": "verified",
        "version": manifest.version,
        "channel": manifest.channel,
        "file": manifest.file,
        "size": manifest.size,
        "sha256": manifest.sha256,
        "tamper_checks_passed": 5,
        "sha256sums_checked": sha256sums_checked,
    }))
}

fn test_shortcuts(identity: InstallIdentity) -> Vec<PathBuf> {
    let mut links = Vec::new();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        links.push(
            PathBuf::from(appdata)
                .join("Microsoft/Windows/Start Menu/Programs")
                .join(format!("{}.lnk", identity.display_name())),
        );
    }
    if let Some(desktop) = delta_app::user_desktop_dir() {
        links.push(desktop.join(format!("{}.lnk", identity.display_name())));
    }
    links
}

/// CI release rehearsal: using only `delta-test` and a throw-away key in `%TEMP%`, builds v1 and
/// signed test-release v2, proves tamper rejection (missing sig, wrong key, tampered manifest,
/// cross-channel, tampered installer bytes, downgrade), then silently installs v1, downloads v2
/// with Range resume, upgrades v1 -> v2, verifies data preservation, and uninstalls cleanly.
pub(crate) fn release_rehearse(root: &Path) -> Result<Value, ErrorEnvelope> {
    let identity = InstallIdentity::Test;
    let started = Instant::now();
    find_makensis(root)?;
    if delta_app::identity_is_running(identity) {
        return Err(failure(
            "REHEARSE_RUNNING",
            "delta-test is running; close it before running release-rehearse",
        ));
    }

    let run_id = format!("{}-{}", std::process::id(), Utc::now().timestamp_millis());
    let temp = std::env::temp_dir().join(format!("delta_release_rehearse_{run_id}"));
    fs::create_dir_all(&temp).map_err(|e| failure("REHEARSE_TEMP", e))?;

    struct RehearsalCleanup {
        identity: InstallIdentity,
        temp: PathBuf,
    }
    impl Drop for RehearsalCleanup {
        fn drop(&mut self) {
            if installed_version(self.identity).is_some() {
                let log = self.temp.join("cleanup-uninstall.log");
                let _ = run_uninstall_silently(self.identity, &log);
            }
            if let Some(dir) = install_dir(self.identity).filter(|d| d.exists()) {
                let _ = fs::remove_dir_all(&dir);
            }
            for link in test_shortcuts(self.identity) {
                if link.exists() {
                    let _ = fs::remove_file(&link);
                }
            }
            let _ = fs::remove_dir_all(&self.temp);
        }
    }
    let _cleanup = RehearsalCleanup {
        identity,
        temp: temp.clone(),
    };

    // Clean any leftover delta-test install before starting.
    if installed_version(identity).is_some() {
        let log = temp.join("preflight-uninstall.log");
        let run = run_uninstall_silently(identity, &log)?;
        if run.exit_code != Some(0) || run.timed_out {
            return Err(failure(
                "REHEARSE_PREFLIGHT",
                format!("preflight uninstall failed: exit {:?}", run.exit_code),
            ));
        }
    }
    if let Some(dir) = install_dir(identity).filter(|d| d.exists()) {
        fs::remove_dir_all(&dir).map_err(|e| failure("REHEARSE_PREFLIGHT", e))?;
    }

    let now = Utc::now();
    let v1 = default_dev_version(root, now)?;
    let v2 = default_dev_version(root, now + chrono::Duration::seconds(61))?;
    let pkg_dir = temp.join("packages");
    let context = PackagingContext::prepare(root, &pkg_dir)?;
    let pkg_v1 = context.build(identity, &v1, &pkg_dir)?;

    let key_file = temp.join("rehearsal-signing.key");
    let keygen = release_keygen(&key_file, root)?;
    let public_hex = keygen["public_key_hex"]
        .as_str()
        .ok_or_else(|| failure("REHEARSE_KEY", "missing public_key_hex"))?
        .to_string();
    let public_key = update::parse_public_key(&public_hex).map_err(|e| failure(e.code(), e))?;

    let release_out = temp.join("release");
    let rel_v2 = release_build(root, &release_out, identity, &v2, Some(&key_file))?;
    for part in ["desktop", "cli"] {
        let imports = rel_v2.binary_imports[part].to_string().to_ascii_lowercase();
        if imports.contains("vcruntime")
            || imports.contains("msvcp")
            || imports.contains("ucrtbase")
        {
            return Err(failure(
                "REHEARSE_IMPORTS",
                format!("{part} imports an external MSVC runtime"),
            ));
        }
    }

    let installer_bytes = fs::read(&rel_v2.setup).map_err(|e| failure("REHEARSE_IO", e))?;
    let signature = fs::read_to_string(&rel_v2.signature_file)
        .map_err(|e| failure("REHEARSE_IO", e))?
        .trim()
        .to_string();
    let v1_semver = semver::Version::parse(&v1).map_err(|e| failure("REHEARSE_VERSION", e))?;
    let v2_semver = semver::Version::parse(&v2).map_err(|e| failure("REHEARSE_VERSION", e))?;

    let server = update::UpdateLoopbackServer::start().map_err(|e| failure(e.code(), e))?;
    let policy = update::UpdatePolicy::test(server.latest_url(), Some(public_key));
    let client = update::UpdateClient::new(policy.clone()).map_err(|e| failure(e.code(), e))?;
    let cancel = std::sync::atomic::AtomicBool::new(false);

    // 1. Missing signature rejected before download.
    server.publish(&rel_v2.manifest, installer_bytes.clone(), None);
    let err = client
        .check(&v1_semver)
        .expect_err("missing signature must fail");
    if err.code() != "UPDATE_SIGNATURE_MISSING" {
        return Err(failure(
            "REHEARSE_TAMPER",
            format!("expected UPDATE_SIGNATURE_MISSING, got {}", err.code()),
        ));
    }

    // 2. Wrong signing key rejected.
    let wrong_key = update::generate_signing_key().map_err(|e| failure(e.code(), e))?;
    let wrong_sig = update::sign_manifest(&wrong_key, &rel_v2.manifest);
    server.publish(&rel_v2.manifest, installer_bytes.clone(), Some(wrong_sig));
    let err = client.check(&v1_semver).expect_err("wrong key must fail");
    if err.code() != "UPDATE_SIGNATURE_INVALID" {
        return Err(failure(
            "REHEARSE_TAMPER",
            format!("expected UPDATE_SIGNATURE_INVALID, got {}", err.code()),
        ));
    }

    // 3. Tampered manifest hash rejected.
    let mut bad_manifest = rel_v2.manifest.clone();
    bad_manifest.sha256 = if bad_manifest.sha256.starts_with('0') {
        format!("1{}", &bad_manifest.sha256[1..])
    } else {
        format!("0{}", &bad_manifest.sha256[1..])
    };
    server.publish(
        &bad_manifest,
        installer_bytes.clone(),
        Some(signature.clone()),
    );
    let err = client
        .check(&v1_semver)
        .expect_err("tampered manifest must fail");
    if err.code() != "UPDATE_SIGNATURE_INVALID" {
        return Err(failure(
            "REHEARSE_TAMPER",
            format!("expected UPDATE_SIGNATURE_INVALID, got {}", err.code()),
        ));
    }

    // 4. Cross-channel manifest rejected.
    let mut cross_channel = rel_v2.manifest.clone();
    cross_channel.channel = "release".into();
    server.publish(
        &cross_channel,
        installer_bytes.clone(),
        Some(signature.clone()),
    );
    let err = client
        .check(&v1_semver)
        .expect_err("cross-channel manifest must fail");
    if err.code() != "UPDATE_CHANNEL_MISMATCH" {
        return Err(failure(
            "REHEARSE_TAMPER",
            format!("expected UPDATE_CHANNEL_MISMATCH, got {}", err.code()),
        ));
    }

    // 5. Tampered installer bytes rejected with UPDATE_HASH_MISMATCH and .partial removed.
    let mut tampered_bytes = installer_bytes.clone();
    let mid = tampered_bytes.len() / 2;
    tampered_bytes[mid] ^= 0xff;
    server.publish(&rel_v2.manifest, tampered_bytes, Some(signature.clone()));
    let offer = client
        .check(&v1_semver)
        .map_err(|e| failure(e.code(), e))?
        .ok_or_else(|| failure("REHEARSE_UPDATE", "expected offer for v2"))?;
    let dl_tamper_dir = temp.join("dl-tamper");
    let err = client
        .download(&offer, &dl_tamper_dir, &cancel)
        .expect_err("tampered installer bytes must fail");
    if err.code() != "UPDATE_HASH_MISMATCH" {
        return Err(failure(
            "REHEARSE_TAMPER",
            format!("expected UPDATE_HASH_MISMATCH, got {}", err.code()),
        ));
    }
    if dl_tamper_dir
        .join(format!("{}.partial", rel_v2.manifest.file))
        .exists()
    {
        return Err(failure(
            "REHEARSE_TAMPER",
            "partial file remained after hash mismatch",
        ));
    }

    // 6. Genuine release: check(v2) returns None (no re-offer), downgrade rejected.
    server.publish(
        &rel_v2.manifest,
        installer_bytes.clone(),
        Some(signature.clone()),
    );
    if client
        .check(&v2_semver)
        .map_err(|e| failure(e.code(), e))?
        .is_some()
    {
        return Err(failure("REHEARSE_DOWNGRADE", "same version was offered"));
    }
    let key = load_signing_key(Some(&key_file), false)?;
    let v1_manifest = UpdateManifest {
        schema_version: 1,
        channel: "test".into(),
        version: v1.clone(),
        file: identity.setup_file_name(&v1),
        size: pkg_v1.size,
        sha256: pkg_v1.setup_sha256.clone(),
        published_at: Utc::now().to_rfc3339(),
    };
    let v1_sig = update::sign_manifest(&key, &v1_manifest);
    if !matches!(
        policy.evaluate(&v2_semver, &v1_manifest, Some(&v1_sig)),
        Err(update::UpdateError::NotNewer { .. })
    ) {
        return Err(failure("REHEARSE_DOWNGRADE", "downgrade was not rejected"));
    }

    // 7. Real silent install v1 -> Range-resumed download v2 -> silent upgrade v1 -> v2 -> uninstall.
    let v1_log = temp.join("install-v1.log");
    let run_v1 = run_setup_silently(&pkg_v1.setup_path, &v1_log)?;
    if run_v1.exit_code != Some(0) || run_v1.timed_out {
        return Err(failure(
            "REHEARSE_INSTALL_V1",
            format!("v1 silent install failed: exit {:?}", run_v1.exit_code),
        ));
    }
    if installed_version(identity).as_deref() != Some(v1.as_str()) {
        return Err(failure(
            "REHEARSE_INSTALL_V1",
            "DisplayVersion mismatch after v1 install",
        ));
    }
    let dir = install_dir(identity).ok_or_else(|| failure("REHEARSE_DIR", "no LOCALAPPDATA"))?;
    verify_identity_binary(
        &dir.join(identity.desktop_exe()),
        "delta-desktop",
        identity,
        &v1,
    )?;

    // Isolated data sentinel must survive upgrade and uninstall.
    let data_dir = temp.join("data");
    fs::create_dir_all(&data_dir).map_err(|e| failure("REHEARSE_DATA", e))?;
    let sentinel = data_dir.join("sentinel.txt");
    fs::write(&sentinel, b"must survive upgrade and uninstall")
        .map_err(|e| failure("REHEARSE_DATA", e))?;

    // Download v2 with a forced 1/3 interruption to verify Range resume.
    server.interrupt_next_download_after(installer_bytes.len() / 3);
    let dl_dir = temp.join("dl-v2");
    let first_err = client
        .download(&offer, &dl_dir, &cancel)
        .expect_err("interrupted first download must fail cleanly");
    if first_err.code() != "UPDATE_NETWORK" {
        return Err(failure(
            "REHEARSE_RESUME",
            format!("expected UPDATE_NETWORK, got {}", first_err.code()),
        ));
    }
    let downloaded_v2 = client
        .download(&offer, &dl_dir, &cancel)
        .map_err(|e| failure(e.code(), e))?;
    update::verify_installer(&policy, &offer, &downloaded_v2).map_err(|e| failure(e.code(), e))?;
    let ranges = server
        .ranges_seen
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if !ranges.iter().any(|r| r.ends_with('-')) {
        return Err(failure(
            "REHEARSE_RESUME",
            format!("download did not resume with HTTP Range: {ranges:?}"),
        ));
    }

    let v2_log = temp.join("upgrade-v2.log");
    let run_v2 = run_setup_silently(&downloaded_v2, &v2_log)?;
    if run_v2.exit_code != Some(0) || run_v2.timed_out {
        return Err(failure(
            "REHEARSE_UPGRADE_V2",
            format!("v2 silent upgrade failed: exit {:?}", run_v2.exit_code),
        ));
    }
    if installed_version(identity).as_deref() != Some(v2.as_str()) {
        return Err(failure(
            "REHEARSE_UPGRADE_V2",
            "DisplayVersion mismatch after v2 upgrade",
        ));
    }
    verify_identity_binary(
        &dir.join(identity.desktop_exe()),
        "delta-desktop",
        identity,
        &v2,
    )?;
    if fs::read(&sentinel).ok().as_deref() != Some(b"must survive upgrade and uninstall".as_slice())
    {
        return Err(failure(
            "REHEARSE_DATA",
            "sentinel data changed during upgrade",
        ));
    }

    let uninst_log = temp.join("uninstall.log");
    let run_uninst = run_uninstall_silently(identity, &uninst_log)?;
    if run_uninst.exit_code != Some(0)
        || run_uninst.timed_out
        || installed_version(identity).is_some()
        || dir.exists()
    {
        return Err(failure(
            "REHEARSE_UNINSTALL",
            "silent uninstall left registry key or install directory",
        ));
    }
    if fs::read(&sentinel).ok().as_deref() != Some(b"must survive upgrade and uninstall".as_slice())
    {
        return Err(failure(
            "REHEARSE_DATA",
            "sentinel data changed during uninstall",
        ));
    }

    // Confirm the ephemeral private key hex never leaked into logs or release outputs.
    let secret_hex = hex::encode(key.to_bytes());
    let mut files = Vec::new();
    collect_paths(&release_out, &release_out, &mut files)?;
    for rel in files {
        let full = release_out.join(&rel);
        if full.extension().is_some_and(|e| e == "exe") {
            continue;
        }
        if fs::read_to_string(&full)
            .map(|text| text.contains(&secret_hex))
            .unwrap_or(false)
        {
            return Err(failure(
                "REHEARSE_KEY_LEAK",
                format!("ephemeral private key leaked into {rel}"),
            ));
        }
    }

    Ok(json!({
        "status": "passed",
        "identity": identity.name(),
        "v1": v1,
        "v2": v2,
        "tamper_checks_passed": 6,
        "range_resume_headers": ranges,
        "elapsed_ms": started.elapsed().as_millis() as u64,
    }))
}

// ---------------------------------------------------------------------------
// M2 portable migration (`S-34`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct MigrationReport {
    pub removed: Vec<String>,
    pub kept_modified: Vec<String>,
    pub missing: Vec<String>,
    pub kept_unknown: Vec<String>,
}

/// Remove only manifest-owned files whose hashes still match, plus the two manifests.
pub(crate) fn migrate_portable(
    install_dir: &Path,
    identity: InstallIdentity,
) -> Result<MigrationReport, ErrorEnvelope> {
    reject_link(install_dir)?;
    let directory = fs::canonicalize(install_dir).map_err(|e| failure("MIGRATE_ROOT", e))?;
    if directory.file_name().and_then(|n| n.to_str()) != Some(identity.name()) {
        return Err(failure(
            "MIGRATE_ROOT",
            "installation directory does not belong to this identity",
        ));
    }
    let manifest_path = directory.join("install-manifest.json");
    reject_link(&manifest_path)?;
    let manifest: InstallManifest = serde_json::from_slice(
        &fs::read(&manifest_path).map_err(|e| failure("MIGRATE_MANIFEST", e))?,
    )
    .map_err(|e| failure("MIGRATE_MANIFEST", e))?;
    if manifest.schema_version != 1 || manifest.product != identity.name() {
        return Err(failure(
            "MIGRATE_MANIFEST",
            "portable manifest belongs to another product; nothing removed",
        ));
    }
    let mut report = MigrationReport::default();
    // Classify everything first and make sure every verified file can be removed (a running
    // M2 portable app keeps its executables open) before changing anything, so a failed
    // migration leaves the portable installation intact.
    let mut verified = BTreeMap::new();
    for (name, expected) in &manifest.files {
        if matches!(
            name.as_str(),
            "package-manifest.json" | "install-manifest.json"
        ) {
            continue;
        }
        let path = directory.join(relative_file(name)?);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        // Reject links/junctions anywhere along the ancestor chain before hashing or mutating.
        reject_ancestors_links(&directory, &path)?;
        let matches = metadata.is_file()
            && sha256_file(&path).map_err(|e| failure("MIGRATE_HASH", e))? == *expected;
        if matches {
            if let Err(error) = fs::OpenOptions::new().write(true).open(&path) {
                return Err(failure(
                    "MIGRATE_IN_USE",
                    format!("{name} is in use ({error}); close DELTA and run Setup again; nothing removed"),
                ));
            }
        }
        verified.insert(name.clone(), matches);
    }
    for name in manifest.files.keys() {
        if matches!(
            name.as_str(),
            "package-manifest.json" | "install-manifest.json"
        ) {
            continue;
        }
        let path = directory.join(relative_file(name)?);
        let Some(&matches) = verified.get(name) else {
            report.missing.push(name.clone());
            continue;
        };
        if !matches {
            // The new installation may ship a file with the same name; keep the user's copy
            // beside it instead of letting the installer overwrite it.
            let mut kept = path.with_file_name(format!(
                "{}.m2-modified",
                path.file_name().and_then(|n| n.to_str()).unwrap_or("file")
            ));
            let mut counter = 1;
            while kept.exists() {
                counter += 1;
                kept = path.with_file_name(format!(
                    "{}.m2-modified-{counter}",
                    path.file_name().and_then(|n| n.to_str()).unwrap_or("file")
                ));
            }
            fs::rename(&path, &kept).map_err(|e| failure("MIGRATE_KEEP", e))?;
            report.kept_modified.push(format!(
                "{name} -> {}",
                kept.strip_prefix(&directory)
                    .unwrap_or(&kept)
                    .to_string_lossy()
                    .replace('\\', "/")
            ));
            continue;
        }
        fs::remove_file(&path).map_err(|e| failure("MIGRATE_REMOVE", e))?;
        report.removed.push(name.clone());
    }
    for manifest_file in ["package-manifest.json", "install-manifest.json"] {
        let path = directory.join(manifest_file);
        if fs::symlink_metadata(&path).is_ok() {
            reject_link(&path)?;
            if path.is_file() {
                fs::remove_file(&path).map_err(|e| failure("MIGRATE_REMOVE", e))?;
                if !report.removed.iter().any(|n| n == manifest_file) {
                    report.removed.push(manifest_file.into());
                }
            }
        }
    }
    let mut remaining = Vec::new();
    collect_paths(&directory, &directory, &mut remaining)?;
    remaining.sort();
    report.kept_unknown = remaining
        .into_iter()
        .filter(|name| {
            !report
                .kept_modified
                .iter()
                .any(|kept| kept.ends_with(&format!("-> {name}")))
        })
        .collect();
    remove_empty_directories(&directory)?;
    Ok(report)
}

fn remove_empty_directories(directory: &Path) -> Result<(), ErrorEnvelope> {
    for entry in fs::read_dir(directory).map_err(|e| failure("MIGRATE_REMOVE", e))? {
        let path = entry.map_err(|e| failure("MIGRATE_REMOVE", e))?.path();
        if fs::symlink_metadata(&path)
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            remove_empty_directories(&path)?;
            if fs::read_dir(&path)
                .map(|mut items| items.next().is_none())
                .unwrap_or(false)
            {
                fs::remove_dir(&path).map_err(|e| failure("MIGRATE_REMOVE", e))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nsis_file_list_removes_every_file_then_folders_deepest_first() {
        let stage = std::env::var_os("DELTA_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("delta_nsis_files_{}", uuid::Uuid::new_v4()));
        for name in [
            "delta-dev.exe",
            "third-party-licenses/a/LICENSE",
            "third-party-licenses/b.txt",
            "odd$name.txt",
        ] {
            let path = stage.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"x").unwrap();
        }
        let text = render_remove_files_nsh(&stage).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[1], "!macro DELTA_REMOVE_INSTALLED_FILES");
        assert_eq!(
            &lines[2..],
            &[
                r#"  Delete "$INSTDIR\delta-dev.exe""#,
                r#"  Delete "$INSTDIR\odd$$name.txt""#,
                r#"  Delete "$INSTDIR\third-party-licenses\a\LICENSE""#,
                r#"  Delete "$INSTDIR\third-party-licenses\b.txt""#,
                r#"  RMDir "$INSTDIR\third-party-licenses\a""#,
                r#"  RMDir "$INSTDIR\third-party-licenses""#,
                "!macroend",
                "!macro DELTA_CHECK_FILES_WRITABLE",
                r#"  !insertmacro DELTA_CHECK_WRITABLE "delta-dev.exe""#,
                r#"  !insertmacro DELTA_CHECK_WRITABLE "odd$$name.txt""#,
                r#"  !insertmacro DELTA_CHECK_WRITABLE "third-party-licenses\a\LICENSE""#,
                r#"  !insertmacro DELTA_CHECK_WRITABLE "third-party-licenses\b.txt""#,
                "!macroend",
            ]
        );
        fs::remove_dir_all(&stage).unwrap();
        assert!(render_remove_files_nsh(&stage).is_err());
    }

    #[test]
    fn nsis_template_gets_every_value_it_requires() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let template = fs::read_to_string(root.join(NSIS_TEMPLATE)).unwrap();
        let required: std::collections::BTreeSet<&str> = template
            .lines()
            .filter_map(|line| line.trim().strip_prefix("!insertmacro DELTA_REQUIRE "))
            .map(|rest| rest.split_whitespace().next().unwrap())
            .collect();
        let version = semver::Version::parse("0.1.1-dev.20261125000000").unwrap();
        let p = Path::new("x");
        let defines = nsis_defines(InstallIdentity::DevTest, &version, p, p, p, p);
        let passed: std::collections::BTreeSet<&str> =
            defines.iter().map(|(name, _)| *name).collect();
        assert_eq!(required, passed);
        let value = |name: &str| {
            defines
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.as_str())
                .unwrap()
        };
        assert_eq!(value("DELTA_APP_ID"), "io.github.chialecode.delta-dev-test");
        assert_eq!(value("DELTA_DESKTOP_ON_UPDATE"), "1");
        assert_eq!(value("DELTA_NUMERIC_VERSION"), "0.1.1.0");
        assert_eq!(
            InstallIdentity::Dev.uninstall_key(),
            r"Software\Microsoft\Windows\CurrentVersion\Uninstall\io.github.chialecode.delta-dev"
        );
        // NSIS's LZMA module is CPL-1.0, outside the licence allowlist.
        assert!(template.contains("SetCompressor zlib"));
        assert!(!template.to_ascii_lowercase().contains("setcompressor lzma"));
        assert!(!template.to_ascii_lowercase().contains("/solid lzma"));
    }

    #[test]
    fn window_defer_waits_while_open_and_skips_after_the_limit() {
        use std::cell::Cell;
        let run = |open_polls: u32| {
            let now = Cell::new(Duration::ZERO);
            let polls = Cell::new(0u32);
            let wait = wait_while_window_open(
                WINDOW_WAIT,
                WINDOW_POLL,
                || {
                    polls.set(polls.get() + 1);
                    polls.get() <= open_polls
                },
                |step| now.set(now.get() + step),
                || now.get(),
            );
            (wait, polls.get())
        };
        // Not open: no wait at all.
        let (wait, polls) = run(0);
        assert_eq!(
            wait,
            WindowWait {
                was_open: false,
                still_open: false,
                waited: Duration::ZERO
            }
        );
        assert_eq!(polls, 1);
        // Closed after three checks: deferred 4 s, then installs.
        let (wait, _) = run(3);
        assert!(wait.was_open && !wait.still_open);
        assert_eq!(wait.waited, Duration::from_secs(6));
        // Open for good: gives up at 10 minutes.
        let (wait, polls) = run(u32::MAX);
        assert!(wait.was_open && wait.still_open);
        assert_eq!(wait.waited, WINDOW_WAIT);
        assert_eq!(polls, 301);
    }

    #[test]
    fn auto_install_switch_only_turns_off_the_real_dev_build() {
        assert!(auto_install_disabled(InstallIdentity::Dev, Some("0")));
        assert!(auto_install_disabled(InstallIdentity::Dev, Some(" 0 ")));
        assert!(!auto_install_disabled(InstallIdentity::Dev, Some("1")));
        assert!(!auto_install_disabled(InstallIdentity::Dev, None));
        assert!(!auto_install_disabled(InstallIdentity::DevTest, Some("0")));
    }

    #[test]
    fn install_dev_receipt_uses_the_shared_camel_case_fields() {
        let receipt = InstallDevReceipt::start(
            "delta-dev",
            Path::new("dist/delta-dev-setup-0.1.1-dev.1-x64.exe"),
            "0.1.1-dev.1",
        );
        let value = serde_json::to_value(&receipt).unwrap();
        for field in [
            "identity",
            "installed",
            "skipped",
            "reason",
            "error",
            "firstInstall",
            "oldVersion",
            "newVersion",
            "installLocation",
            "wasRunning",
            "windowOpen",
            "deferred",
            "deferMs",
            "quitMs",
            "relaunched",
            "background",
            "inJob",
            "elapsedMs",
            "at",
            "installer",
            "verifiedRegistryVersion",
            "verifiedExeVersion",
            "launchMethod",
            "guidance",
        ] {
            assert!(value.get(field).is_some(), "receipt lacks {field}");
        }
        assert_eq!(value["installer"], "delta-dev-setup-0.1.1-dev.1-x64.exe");
        assert_eq!(value["newVersion"], "0.1.1-dev.1");
    }

    #[test]
    fn install_dev_always_starts_the_app_in_the_background_once() {
        // Not running before: just `--background` (D-36).
        assert_eq!(background_launch_args(Vec::new()), vec!["--background"]);
        // Running before: its previous arguments are kept, `--background` added once.
        let previous = vec!["--data-dir".to_string(), "profile-data".to_string()];
        assert_eq!(
            background_launch_args(previous),
            vec!["--data-dir", "profile-data", "--background"]
        );
        assert_eq!(
            background_launch_args(vec!["--background".into()]),
            vec!["--background"]
        );
        // Real `delta-dev` uses the desktop shell when inside a job; test identities stay on
        // `spawn_detached_app` to keep the gate's isolated environment variables.
        assert!(should_use_desktop_shell(InstallIdentity::Dev, true));
        assert!(!should_use_desktop_shell(InstallIdentity::Dev, false));
        assert!(!should_use_desktop_shell(InstallIdentity::DevTest, true));
        assert!(!should_use_desktop_shell(InstallIdentity::DevTest, false));
    }

    #[test]
    fn development_identities_carry_the_dev_icon_everywhere() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for identity in InstallIdentity::ALL {
            let icon = identity_icon(identity);
            assert_eq!(
                icon.ends_with("icon-dev.ico"),
                identity.is_dev_like(),
                "{}",
                identity.name()
            );
            // The build scripts choose by name: every dev-like identity, and only those,
            // starts with `delta-dev`.
            assert_eq!(
                identity.name().starts_with("delta-dev"),
                identity.is_dev_like()
            );
            let raw = fs::read(root.join(icon)).unwrap();
            assert_eq!(&raw[..4], &[0, 0, 1, 0], "{icon} is not an .ico");
        }
        for script in ["crates/delta-desktop/build.rs", "crates/delta-cli/build.rs"] {
            let text = fs::read_to_string(root.join(script)).unwrap();
            assert!(
                text.contains("identity.starts_with(\"delta-dev\")"),
                "{script}"
            );
            assert!(text.contains("assets/icon-dev.ico\""), "{script}");
            assert!(text.contains(".set_icon(icon)"), "{script}");
        }
    }

    #[test]
    fn m3_a05_portable_migration_keeps_modified_and_unknown_files() {
        let root = std::env::var_os("DELTA_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("delta_m3_migrate_{}", uuid::Uuid::new_v4()));
        let directory = root.join("delta-test");
        fs::create_dir_all(directory.join("third-party-licenses")).unwrap();
        let mut files = BTreeMap::new();
        for name in [
            "delta-test.exe",
            "README.txt",
            "third-party-licenses/a.txt",
            "package-manifest.json",
        ] {
            fs::write(directory.join(name), name.as_bytes()).unwrap();
            files.insert(
                name.to_string(),
                sha256_file(&directory.join(name)).unwrap(),
            );
        }
        let manifest = InstallManifest {
            schema_version: 1,
            product: "delta-test".into(),
            channel: "dev".into(),
            version: "0.1.0".into(),
            install_dir: directory.clone(),
            files,
        };
        fs::write(
            directory.join("install-manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        fs::write(directory.join("README.txt"), b"edited by the user").unwrap();
        fs::write(directory.join("user-notes.txt"), b"unknown").unwrap();
        assert!(migrate_portable(&directory, InstallIdentity::DevTest).is_err());
        #[cfg(windows)]
        {
            // A running M2 app keeps its executable open without write sharing: the
            // migration must refuse before removing or renaming anything.
            use std::os::windows::fs::OpenOptionsExt;
            let held = fs::OpenOptions::new()
                .read(true)
                .share_mode(0x1) // FILE_SHARE_READ only
                .open(directory.join("delta-test.exe"))
                .unwrap();
            let error = migrate_portable(&directory, InstallIdentity::Test).unwrap_err();
            assert_eq!(error.code, "MIGRATE_IN_USE");
            drop(held);
            for name in [
                "README.txt",
                "install-manifest.json",
                "third-party-licenses/a.txt",
            ] {
                assert!(directory.join(name).is_file(), "{name} changed");
            }
            assert!(!directory.join("README.txt.m2-modified").exists());
        }
        let report = migrate_portable(&directory, InstallIdentity::Test).unwrap();
        assert_eq!(
            report.kept_modified,
            vec!["README.txt -> README.txt.m2-modified".to_string()]
        );
        assert_eq!(report.kept_unknown, vec!["user-notes.txt".to_string()]);
        assert!(!directory.join("third-party-licenses").exists());
        assert!(report.removed.contains(&"delta-test.exe".to_string()));
        assert!(report
            .removed
            .contains(&"install-manifest.json".to_string()));
        assert!(!directory.join("delta-test.exe").exists());
        assert!(!directory.join("package-manifest.json").exists());
        assert!(!directory.join("README.txt").exists());
        assert_eq!(
            fs::read(directory.join("README.txt.m2-modified")).unwrap(),
            b"edited by the user"
        );
        assert!(directory.join("user-notes.txt").is_file());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn package_build_keeps_only_the_newest_installer_of_the_identity() {
        let directory = std::env::var_os("DELTA_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("delta_m3_prune_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let names = [
            "delta-dev-setup-0.1.1-dev.20261109010101-x64.exe",
            "delta-dev-setup-0.1.1-dev.20261109020202-x64.exe",
            "delta-dev-test-setup-0.1.1-dev.20261109010101-x64.exe",
            "delta-setup-0.1.1-x64.exe",
            "notes.txt",
        ];
        for name in names {
            fs::write(directory.join(name), b"x").unwrap();
            if name.ends_with(".exe") {
                fs::write(InstallerArtifact::sidecar(&directory.join(name)), b"{}").unwrap();
            }
        }
        let keep = directory.join(names[1]);
        let removed = prune_older_packages(&directory, InstallIdentity::Dev, &keep).unwrap();
        assert_eq!(removed, vec![names[0].to_string()]);
        assert!(!directory.join(names[0]).exists());
        assert!(!InstallerArtifact::sidecar(&directory.join(names[0])).exists());
        for name in &names[1..] {
            assert!(directory.join(name).is_file(), "{name} must be kept");
        }
        fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn key_isolation_and_manifest_path_guards_prevent_bypasses() {
        let root = std::env::var_os("DELTA_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("delta_m3_guards_{}", uuid::Uuid::new_v4()));
        let key_dir = root.join("release-keys");
        fs::create_dir_all(&key_dir).unwrap();
        let direct = key_dir.join("update-signing.key");
        let dotted = key_dir.join(".").join("update-signing.key");
        assert!(same_path_normalized(&direct, &dotted));
        if cfg!(windows) {
            let flipped = PathBuf::from(
                direct
                    .display()
                    .to_string()
                    .replace('\\', "/")
                    .to_ascii_uppercase(),
            );
            assert!(same_path_normalized(&direct, &flipped));
        }

        // `inside_repository` must detect a git repo even when `repository` is a subdirectory
        // or a completely unrelated directory.
        let fake_repo = root.join("fake-repo");
        let sub_dir = fake_repo.join("crates/delta-cli");
        fs::create_dir_all(fake_repo.join(".git")).unwrap();
        fs::create_dir_all(&sub_dir).unwrap();
        assert!(inside_repository(&fake_repo.join("leak.key"), &sub_dir));
        assert!(inside_repository(
            &fake_repo.join("nested/leak.key"),
            &key_dir
        ));
        assert!(!inside_repository(&key_dir.join("ok.key"), &sub_dir));

        // `plain_file_name` rejects traversal and path separators.
        assert!(plain_file_name("delta-test-setup-0.1.1-x64.exe").is_ok());
        for bad in ["", "../escape.exe", "sub/file.exe", "sub\\file.exe", "a:b"] {
            assert!(plain_file_name(bad).is_err(), "accepted '{bad}'");
        }

        // Modified `package-manifest.json` must be removed cleanly, not misclassified as unknown.
        let mig = root.join("mig/delta-test");
        fs::create_dir_all(&mig).unwrap();
        fs::write(mig.join("package-manifest.json"), b"v1").unwrap();
        let pkg_hash = sha256_file(&mig.join("package-manifest.json")).unwrap();
        let manifest = InstallManifest {
            schema_version: 1,
            product: "delta-test".into(),
            channel: "dev".into(),
            version: "0.1.0".into(),
            install_dir: mig.clone(),
            files: BTreeMap::from([("package-manifest.json".into(), pkg_hash)]),
        };
        fs::write(
            mig.join("install-manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        // Modify package-manifest.json after recording its hash.
        fs::write(mig.join("package-manifest.json"), b"v1-edited").unwrap();
        let report = migrate_portable(&mig, InstallIdentity::Test).unwrap();
        assert!(report.kept_unknown.is_empty(), "{report:?}");
        assert!(report.kept_modified.is_empty(), "{report:?}");
        assert!(!mig.join("package-manifest.json").exists());
        assert!(!mig.join("package-manifest.json.m2-modified").exists());

        // `file_version_info` safely handles a non-PE file without panicking or UB.
        let non_pe = root.join("not-a-pe.bin");
        fs::write(&non_pe, b"hello").unwrap();
        assert!(file_version_info(&non_pe).is_none());

        // `load_signing_key` reads explicit key files in both hex and PKCS#8 Ed25519 PEM formats.
        let generated = update::generate_signing_key().unwrap();
        let hex_path = key_dir.join("explicit-hex.key");
        fs::write(&hex_path, hex::encode(generated.to_bytes())).unwrap();
        assert_eq!(
            load_signing_key(Some(&hex_path), false).unwrap().to_bytes(),
            generated.to_bytes()
        );
        // Release verify rejects missing files cleanly.
        let missing_dir = root.join("empty-release");
        fs::create_dir_all(&missing_dir).unwrap();
        assert!(release_verify(&missing_dir, "0.1.2").is_err());

        fs::remove_dir_all(&root).unwrap();
    }
}

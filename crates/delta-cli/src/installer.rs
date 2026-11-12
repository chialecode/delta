//! Inno Setup installers, local dev updates, signed release artifacts and M2 migration
//! (`S-25`..`S-34`). Installers are built only from a clean, fingerprinted workspace; the
//! signing key never reaches logs or artifacts.

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    io::Write,
    path::{Path, PathBuf},
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
    check_runtime_imports, collect_files, copy_tree, failure, pe_imports, reject_link,
    relative_file, InstallManifest,
};

/// `fs::canonicalize` without the `\\?\` prefix, so paths read normally in logs and Inno.
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
    pub inno_setup: String,
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

/// Locate the pinned Inno Setup 7.1.x compiler (`S-25`). The template itself refuses other
/// versions through ISPP `Ver`, so a mismatching compiler fails the build.
pub(crate) fn find_iscc() -> Result<PathBuf, ErrorEnvelope> {
    let mut candidates = Vec::new();
    if let Some(explicit) = std::env::var_os("DELTA_ISCC") {
        candidates.push(PathBuf::from(explicit));
    }
    for (variable, suffix) in [
        ("LOCALAPPDATA", "Programs/Inno Setup 7/ISCC.exe"),
        ("ProgramFiles(x86)", "Inno Setup 7/ISCC.exe"),
        ("ProgramFiles", "Inno Setup 7/ISCC.exe"),
    ] {
        if let Some(base) = std::env::var_os(variable) {
            candidates.push(PathBuf::from(base).join(suffix));
        }
    }
    candidates.into_iter().find(|path| path.is_file()).ok_or_else(|| {
        failure(
            "INNO_SETUP_MISSING",
            "Inno Setup 7.1.x (ISCC.exe) not found; install it with `winget install --id JRSoftware.InnoSetup.7 --version 7.1.1 --scope user`. No ZIP fallback is produced.",
        )
    })
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

/// Shared, once-per-invocation preparation: workspace checks, license inventory, compiler.
pub(crate) struct PackagingContext {
    root: PathBuf,
    output: PathBuf,
    licenses: PathBuf,
    iscc: PathBuf,
    tags: Vec<String>,
    pub fingerprint: String,
}

/// The collected license tree is copied into every staged installer; it is scratch data and
/// must not pile up in the output directory across builds.
impl Drop for PackagingContext {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.licenses);
    }
}

impl PackagingContext {
    pub fn prepare(root: &Path, output: &Path) -> Result<Self, ErrorEnvelope> {
        let root = plain_canonical(root).map_err(|e| failure("PACKAGE_ROOT", e))?;
        if !root.join("crates/delta-cli/installer/delta.iss").is_file() {
            return Err(failure(
                "PACKAGE_ROOT",
                "build requires the DELTA workspace",
            ));
        }
        let iscc = find_iscc()?;
        let fingerprint = crate::verify::source_fingerprint(&root)?;
        fs::create_dir_all(output).map_err(|e| failure("PACKAGE_OUTPUT", e))?;
        let output = plain_canonical(output).map_err(|e| failure("PACKAGE_OUTPUT", e))?;
        let metadata_path = output.join("metadata.json");
        let metadata = run_quiet_command(
            OsStr::new("cargo"),
            ["metadata", "--locked", "--format-version", "1"],
            Some(&root),
            &BTreeMap::new(),
        )
        .map_err(|e| failure("PACKAGE_METADATA", e))?;
        if metadata.status_code != Some(0) || metadata.timed_out {
            return Err(failure("PACKAGE_METADATA", "locked metadata failed"));
        }
        fs::write(&metadata_path, &metadata.stdout).map_err(|e| failure("PACKAGE_METADATA", e))?;
        let licenses = output.join(format!("licenses-{}", uuid::Uuid::new_v4()));
        let collected = run_quiet_command(
            OsStr::new("python"),
            [
                root.join("scripts/collect_third_party_licenses.py")
                    .display()
                    .to_string(),
                "--metadata".into(),
                metadata_path.display().to_string(),
                "--output".into(),
                licenses.display().to_string(),
            ],
            Some(&root),
            &BTreeMap::from([("PYTHONUTF8".into(), "1".into())]),
        )
        .map_err(|e| failure("PACKAGE_LICENSES", e))?;
        if collected.status_code != Some(0) || collected.timed_out {
            return Err(failure("PACKAGE_LICENSES", "license collection failed"));
        }
        let tags = git_tags(&root)?;
        Ok(Self {
            root,
            output,
            licenses,
            iscc,
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

    /// Release build with static CRT, staged files and an Inno Setup `setup.exe` in `target_dir`.
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
            ("RUSTFLAGS".into(), "-C target-feature=+crt-static".into()),
            (
                "CARGO_ENCODED_RUSTFLAGS".into(),
                "-C\u{1f}target-feature=+crt-static".into(),
            ),
            (
                "CARGO_BUILD_JOBS".into(),
                std::env::var("CARGO_BUILD_JOBS").unwrap_or_else(|_| "4".into()),
            ),
        ]);
        let started = Instant::now();
        let build = run_quiet_command(
            OsStr::new("cargo"),
            [
                "build".to_string(),
                "--release".into(),
                "--locked".into(),
                "--target".into(),
                "x86_64-pc-windows-msvc".into(),
                "-p".into(),
                "delta-desktop".into(),
                "-p".into(),
                "delta-cli".into(),
                "--target-dir".into(),
                target.display().to_string(),
            ],
            Some(&self.root),
            &environment,
        )
        .map_err(|e| failure("PACKAGE_BUILD", e))?;
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
        let stage = self.output.join(format!(
            "stage-{}-{version}-{}",
            identity.name(),
            uuid::Uuid::new_v4()
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
        copy_tree(&self.licenses, &stage.join("third-party-licenses"))?;
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
        let base = setup_name.trim_end_matches(".exe").to_string();
        let defines = [
            ("Identity", identity.name().to_string()),
            ("DisplayName", identity.display_name().to_string()),
            (
                "AppGuid",
                identity.app_id().trim_matches(['{', '}']).to_string(),
            ),
            ("Version", version_text.clone()),
            ("NumericVersion", numeric_version(version)),
            ("DesktopExe", identity.desktop_exe().to_string()),
            ("CliExe", identity.cli_exe().to_string()),
            ("MutexName", identity.mutex_name()),
            // Dev-like identities put the desktop shortcut back on every silent over-install,
            // a visible sign that a new local build was installed.
            (
                "DesktopOnUpdate",
                if identity.is_dev_like() { "1" } else { "0" }.to_string(),
            ),
            ("Aumid", identity.app_user_model_id().to_string()),
            ("SourceDir", stage.display().to_string()),
            ("OutputDir", target_dir.display().to_string()),
            ("OutputBase", base),
            // The setup and the uninstall entry show the same icon as the installed binaries.
            (
                "IconFile",
                self.root
                    .join(identity_icon(identity))
                    .display()
                    .to_string(),
            ),
        ];
        let mut args: Vec<String> = defines
            .iter()
            .map(|(name, value)| format!("/D{name}={value}"))
            .collect();
        args.push("/Q".into());
        args.push(
            self.root
                .join("crates/delta-cli/installer/delta.iss")
                .display()
                .to_string(),
        );
        let compiled = run_quiet_command_with_timeout(
            self.iscc.as_os_str(),
            &args,
            Some(&self.root),
            &BTreeMap::new(),
            Duration::from_secs(300),
        )
        .map_err(|e| failure("INNO_COMPILE", e))?;
        let log_name = format!("iscc-{}-{version_text}-log.json", identity.name());
        fs::write(
            self.output.join(&log_name),
            serde_json::to_vec_pretty(&compiled).map_err(|e| failure("INNO_COMPILE", e))?,
        )
        .map_err(|e| failure("INNO_COMPILE", e))?;
        let setup_path = target_dir.join(&setup_name);
        if compiled.status_code != Some(0) || compiled.timed_out || !setup_path.is_file() {
            return Err(failure(
                "INNO_COMPILE",
                format!("Inno Setup compilation failed; see {log_name}"),
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
            inno_setup: "7.1.x (template-enforced)".into(),
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
    if !path.is_file() {
        return Err(failure(
            "MISSING_PRODUCT_BINARY",
            format!("missing {}", path.display()),
        ));
    }
    check_runtime_imports(&pe_imports(path)?)?;
    let result = run_quiet_command(path.as_os_str(), ["--build-info"], None, &BTreeMap::new())
        .map_err(|e| failure("PACKAGE_BINARY", e))?;
    let info: Value = serde_json::from_str(&result.stdout)
        .map_err(|_| failure("PACKAGE_BINARY", "binary did not return build information"))?;
    if result.status_code != Some(0)
        || result.timed_out
        || info["product"] != product
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
// Installed state (HKCU uninstall key written by Inno Setup)
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
        let mut buffer = vec![0u16; 1024];
        let mut size = (buffer.len() * 2) as u32;
        let status = RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut size,
        );
        if status != 0 {
            return None;
        }
        let length = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
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
        let mut data = vec![0u8; size as usize];
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
            || (length as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>()
        {
            return None;
        }
        let fixed = &*(pointer as *const VS_FIXEDFILEINFO);
        let numbers = [
            (fixed.dwFileVersionMS >> 16) as u16,
            (fixed.dwFileVersionMS & 0xffff) as u16,
            (fixed.dwFileVersionLS >> 16) as u16,
            (fixed.dwFileVersionLS & 0xffff) as u16,
        ];
        if VerQueryValueW(
            data.as_ptr().cast(),
            wide("\\VarFileInfo\\Translation").as_ptr(),
            &mut pointer,
            &mut length,
        ) == 0
            || length < 4
        {
            return None;
        }
        let translation = std::slice::from_raw_parts(pointer as *const u16, 2);
        let query = format!(
            "\\StringFileInfo\\{:04x}{:04x}\\ProductVersion",
            translation[0], translation[1]
        );
        if VerQueryValueW(
            data.as_ptr().cast(),
            wide(&query).as_ptr(),
            &mut pointer,
            &mut length,
        ) == 0
            || length == 0
        {
            return None;
        }
        let text = std::slice::from_raw_parts(pointer as *const u16, length as usize);
        let end = text.iter().position(|c| *c == 0).unwrap_or(text.len());
        Some((String::from_utf16_lossy(&text[..end]), numbers))
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
pub(crate) struct SetupRun {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub elapsed_ms: u64,
    pub log: PathBuf,
}

/// Run an installer silently (`/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP-`). The quiet
/// runner hides consoles and aborts if any window becomes visible (`D-25`).
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

/// Run the identity's Inno uninstaller silently and wait until its key and files are gone.
pub(crate) fn run_uninstall_silently(
    identity: InstallIdentity,
    log: &Path,
) -> Result<SetupRun, ErrorEnvelope> {
    let started = Instant::now();
    let directory =
        install_dir(identity).ok_or_else(|| failure("UNINSTALL", "LOCALAPPDATA is not set"))?;
    let uninstaller = directory.join("unins000.exe");
    if !uninstaller.is_file() {
        return Err(failure("UNINSTALL", "uninstaller is missing"));
    }
    // Inno's uninstaller finishes in a second phase (a temporary copy) after the first phase
    // exits; inside a kill-on-close job that phase would be cut off before it removes
    // unins000.exe. Start it outside the job and wait for its own completion marker.
    delta_app::spawn_detached_app(
        uninstaller.as_os_str(),
        [
            "/VERYSILENT".to_string(),
            "/SUPPRESSMSGBOXES".into(),
            "/NORESTART".into(),
            format!("/LOG={}", log.display()),
        ],
        &BTreeMap::new(),
        false,
    )
    .map_err(|e| failure("UNINSTALL", e))?;
    let finished = |log: &Path| {
        installed_version(identity).is_none()
            && !uninstaller.exists()
            && fs::read(log)
                .map(|bytes| String::from_utf8_lossy(&bytes).contains("_unins-done.tmp"))
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct InstallDevReceipt {
    pub identity: String,
    pub previous_version: Option<String>,
    pub installed_version: String,
    pub was_running: bool,
    pub restarted: bool,
    pub restart_process_id: Option<u32>,
    pub setup: SetupRun,
}

/// `install-dev` (`S-30`): only dev-channel packages; never kills a running app.
pub(crate) fn install_dev(
    package: &Path,
    expected: InstallIdentity,
    log_dir: &Path,
) -> Result<InstallDevReceipt, ErrorEnvelope> {
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
    let previous = installed_version(identity);
    if let Some(previous) = &previous {
        if !delta_app::is_upgrade(previous, &artifact.version) {
            return Err(failure(
                "INSTALL_DEV_DOWNGRADE",
                format!(
                    "installed {previous} is not older than package {}; nothing changed",
                    artifact.version
                ),
            ));
        }
    }
    let was_running = delta_app::identity_is_running(identity);
    let mut relaunch_args = Vec::new();
    if was_running {
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
        relaunch_args = reply.relaunch_args;
    }
    fs::create_dir_all(log_dir).map_err(|e| failure("INSTALL_DEV_LOG", e))?;
    let log = log_dir.join(format!(
        "install-dev-{}-{}.log",
        identity.name(),
        artifact.version
    ));
    let setup = run_setup_silently(package, &log)?;
    let installed = installed_version(identity).unwrap_or_default();
    if setup.exit_code != Some(0) || setup.timed_out || installed != artifact.version {
        return Err(failure(
            "INSTALL_DEV_FAILED",
            format!(
                "setup exit {:?}; installed version {installed:?}; see {}",
                setup.exit_code,
                log.display()
            ),
        ));
    }
    let directory = install_dir(identity).unwrap_or_default();
    verify_identity_binary(
        &directory.join(identity.desktop_exe()),
        "delta-desktop",
        identity,
        &artifact.version,
    )?;
    let mut restart_process_id = None;
    if was_running {
        let launch = delta_app::spawn_detached_app(
            directory.join(identity.desktop_exe()).as_os_str(),
            &relaunch_args,
            &BTreeMap::new(),
            false,
        )
        .map_err(|e| failure("INSTALL_DEV_RESTART", e))?;
        restart_process_id = Some(launch.process_id);
    }
    Ok(InstallDevReceipt {
        identity: identity.name().into(),
        previous_version: previous,
        installed_version: installed,
        was_running,
        restarted: restart_process_id.is_some(),
        restart_process_id,
        setup,
    })
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

/// Whether `path` would land inside `repository`, judged by its nearest existing ancestor
/// (the key directory may not exist yet).
fn inside_repository(path: &Path, repository: &Path) -> bool {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let Ok(repository) = fs::canonicalize(repository) else {
        return false;
    };
    absolute
        .ancestors()
        .skip(1)
        .find_map(|ancestor| fs::canonicalize(ancestor).ok())
        .is_some_and(|existing| existing.starts_with(&repository))
}

/// Load the signing key. The release identity also accepts `DELTA_RELEASE_SIGNING_KEY` (CI);
/// otherwise the key file is read. The key never reaches logs or artifacts.
pub(crate) fn load_signing_key(
    key_file: &Path,
    allow_environment: bool,
) -> Result<ed25519_dalek::SigningKey, ErrorEnvelope> {
    let hex_key = match std::env::var("DELTA_RELEASE_SIGNING_KEY") {
        Ok(value) if allow_environment && !value.trim().is_empty() => value,
        _ => fs::read_to_string(key_file).map_err(|_| {
            failure(
                "RELEASE_KEY_MISSING",
                format!(
                    "no signing key at {}; nothing was built or published",
                    key_file.display()
                ),
            )
        })?,
    };
    update::parse_signing_key(hex_key.trim()).map_err(|_| {
        failure(
            "RELEASE_KEY_INVALID",
            "the signing key file is not a valid key",
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
    key_file: &Path,
) -> Result<ReleaseArtifacts, ErrorEnvelope> {
    let channel = identity.update_channel().ok_or_else(|| {
        failure(
            "RELEASE_IDENTITY",
            "release-build produces release or test-release identities only",
        )
    })?;
    let is_release = identity == InstallIdentity::Release;
    // Test rehearsals use their own throw-away key; the release key never signs them.
    if !is_release && default_release_key_file().is_ok_and(|default| default == key_file) {
        return Err(failure(
            "RELEASE_KEY_FILE",
            "test releases must use a separate --key-file, never the release signing key",
        ));
    }
    let key = load_signing_key(key_file, is_release)?;
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
    let staging = context.output().join(format!(
        ".staging-{}-{}",
        identity.name(),
        uuid::Uuid::new_v4()
    ));
    let final_directory = context
        .output()
        .join(format!("{}-{}", identity.name(), version));
    if final_directory.exists() {
        return Err(failure(
            "RELEASE_EXISTS",
            format!("{} already exists", final_directory.display()),
        ));
    }
    let result = (|| -> Result<ReleaseArtifacts, ErrorEnvelope> {
        let artifact = context.build(identity, version, &staging)?;
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
    let installer =
        fs::read(directory.join(&manifest.file)).map_err(|e| failure("UPDATE_TEST_SOURCE", e))?;
    let signature = fs::read_to_string(directory.join(format!("{}.sig", manifest.file)))
        .map_err(|e| failure("UPDATE_TEST_SOURCE", e))?;
    let server = update::UpdateLoopbackServer::start().map_err(|e| failure(e.code(), e))?;
    server.publish(&manifest, installer, Some(signature));
    let public_key = fs::read_to_string(directory.join("public-key.txt")).unwrap_or_default();
    println!(
        "{}",
        json!({
            "status": "serving",
            "version": manifest.version,
            "seconds": duration.as_secs(),
            "DELTA_UPDATE_SOURCE": server.latest_url(),
            "DELTA_UPDATE_PUBLIC_KEY": public_key.trim(),
        })
    );
    std::thread::sleep(duration);
    Ok(())
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
        let path = directory.join(relative_file(name)?);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        let matches = metadata.is_file()
            && reject_link(&path).is_ok()
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
        if path.is_file() {
            fs::remove_file(&path).map_err(|e| failure("MIGRATE_REMOVE", e))?;
            if !report.removed.iter().any(|n| n == manifest_file) {
                report.removed.push(manifest_file.into());
            }
        }
        report
            .kept_modified
            .retain(|n| !n.starts_with(&format!("{manifest_file} ->")));
    }
    let mut remaining = BTreeMap::new();
    collect_files(&directory, &directory, &mut remaining)?;
    report.kept_unknown = remaining
        .keys()
        .filter(|name| {
            !report
                .kept_modified
                .iter()
                .any(|kept| kept.ends_with(&format!("-> {name}")))
        })
        .cloned()
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
}

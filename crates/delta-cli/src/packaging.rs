//! Build-time packaging and an installer that only removes verified, manifest-owned files.

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

use delta_app::{run_quiet_command, ErrorEnvelope};
use delta_data::sha256_file;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::verify_m2::PerUserPackageReceipt;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PackageArtifact {
    pub channel: String,
    pub directory: PathBuf,
    pub zip_path: PathBuf,
    pub zip_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PackageManifest {
    schema_version: u32,
    product: String,
    version: String,
    channel: String,
    source_fingerprint: String,
    files: BTreeMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct InstallManifest {
    schema_version: u32,
    product: String,
    channel: String,
    version: String,
    install_dir: PathBuf,
    files: BTreeMap<String, String>,
}

fn failure(code: &str, error: impl std::fmt::Display) -> ErrorEnvelope {
    ErrorEnvelope::new(code, error.to_string(), false)
}

/// Read PE import names without running the binary or relying on developer tools.
fn pe_imports(path: &Path) -> Result<Vec<String>, ErrorEnvelope> {
    const MAX_BINARY_BYTES: u64 = 256 * 1024 * 1024;
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|e| failure("PACKAGE_PE", e))?
        .take(MAX_BINARY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| failure("PACKAGE_PE", e))?;
    if bytes.len() as u64 > MAX_BINARY_BYTES {
        return Err(failure(
            "PACKAGE_PE",
            "Windows binary exceeds the inspection limit",
        ));
    }
    parse_pe_imports(&bytes)
}

fn parse_pe_imports(bytes: &[u8]) -> Result<Vec<String>, ErrorEnvelope> {
    let invalid = || failure("PACKAGE_PE", "invalid or truncated Windows PE import table");
    let word = |offset: usize| -> Result<u16, ErrorEnvelope> {
        let raw: [u8; 2] = bytes
            .get(offset..offset.checked_add(2).ok_or_else(invalid)?)
            .ok_or_else(invalid)?
            .try_into()
            .map_err(|_| invalid())?;
        Ok(u16::from_le_bytes(raw))
    };
    let dword = |offset: usize| -> Result<u32, ErrorEnvelope> {
        let raw: [u8; 4] = bytes
            .get(offset..offset.checked_add(4).ok_or_else(invalid)?)
            .ok_or_else(invalid)?
            .try_into()
            .map_err(|_| invalid())?;
        Ok(u32::from_le_bytes(raw))
    };
    if bytes.get(..2) != Some(b"MZ") {
        return Err(invalid());
    }
    let pe = dword(0x3c)? as usize;
    let signature_end = pe.checked_add(4).ok_or_else(invalid)?;
    if bytes.get(pe..signature_end) != Some(b"PE\0\0") {
        return Err(invalid());
    }
    let section_count = word(signature_end + 2)? as usize;
    let optional_size = word(signature_end + 16)? as usize;
    let optional = signature_end + 20;
    let directories = optional
        + match word(optional)? {
            0x20b => 112,
            0x10b => 96,
            _ => return Err(invalid()),
        };
    if section_count == 0 || section_count > 96 || optional_size < directories - optional + 16 {
        return Err(invalid());
    }
    let directory_count = dword(directories - 4)? as usize;
    if !(2..=16).contains(&directory_count)
        || directories
            .checked_add(directory_count * 8)
            .is_none_or(|end| end > optional + optional_size)
    {
        return Err(invalid());
    }
    let section_table = optional.checked_add(optional_size).ok_or_else(invalid)?;
    if section_table
        .checked_add(section_count * 40)
        .is_none_or(|end| end > bytes.len())
    {
        return Err(invalid());
    }
    let mut sections = Vec::new();
    for index in 0..section_count {
        let header = section_table.checked_add(index * 40).ok_or_else(invalid)?;
        sections.push((
            dword(header + 12)?,
            dword(header + 16)?,
            dword(header + 20)?,
        ));
    }
    let offset = |rva: u32| -> Result<(usize, usize), ErrorEnvelope> {
        for &(virtual_address, raw_size, raw_address) in &sections {
            if let Some(relative) = rva
                .checked_sub(virtual_address)
                .filter(|relative| *relative < raw_size)
            {
                let start = raw_address.checked_add(relative).ok_or_else(invalid)? as usize;
                let end = raw_address.checked_add(raw_size).ok_or_else(invalid)? as usize;
                if end > bytes.len() {
                    return Err(invalid());
                }
                return Ok((start, end));
            }
        }
        Err(invalid())
    };
    let mut names = Vec::new();
    // Normal imports and delay-load imports must obey the same runtime policy.
    for (directory_index, descriptor_size, name_field) in [(1, 20, 12), (13, 32, 4)] {
        if directory_index >= directory_count {
            continue;
        }
        let import_rva = dword(directories + directory_index * 8)?;
        let import_size = dword(directories + directory_index * 8 + 4)? as usize;
        if import_rva == 0 && import_size == 0 {
            continue;
        }
        if import_rva == 0 || !(descriptor_size..=descriptor_size * 4096).contains(&import_size) {
            return Err(invalid());
        }
        let (directory, section_end) = offset(import_rva)?;
        if directory
            .checked_add(import_size)
            .is_none_or(|end| end > section_end)
        {
            return Err(invalid());
        }
        let mut terminated = false;
        for index in 0..import_size / descriptor_size {
            let descriptor = directory + index * descriptor_size;
            if bytes[descriptor..descriptor + descriptor_size]
                .iter()
                .all(|byte| *byte == 0)
            {
                terminated = true;
                break;
            }
            // Modern PE delay descriptors use RVAs; reject legacy VA descriptors
            // rather than overlooking a dependency we cannot safely inspect.
            if directory_index == 13 && dword(descriptor)? != 1 {
                return Err(invalid());
            }
            let (start, end) = offset(dword(descriptor + name_field)?)?;
            let name_bytes = &bytes[start..end.min(start.saturating_add(512))];
            let null = name_bytes
                .iter()
                .position(|byte| *byte == 0)
                .ok_or_else(invalid)?;
            let name = std::str::from_utf8(&name_bytes[..null]).map_err(|_| invalid())?;
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
            {
                return Err(invalid());
            }
            names.push(name.to_ascii_lowercase());
        }
        if !terminated {
            return Err(invalid());
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

fn check_runtime_imports(imports: &[String]) -> Result<(), ErrorEnvelope> {
    if imports.iter().any(|name| {
        ["vcruntime", "msvcp", "msvcr", "concrt"]
            .iter()
            .any(|prefix| name.to_ascii_lowercase().starts_with(prefix))
    }) {
        return Err(failure(
            "PACKAGE_RUNTIME_DEPENDENCY",
            "package requires an external MSVC runtime; rebuild with static CRT",
        ));
    }
    Ok(())
}

fn product(channel: &str) -> Result<&'static str, ErrorEnvelope> {
    match channel {
        "release" => Ok("delta"),
        "dev" => Ok("delta-dev"),
        _ => Err(failure(
            "INVALID_CHANNEL",
            "package channel must be release or dev",
        )),
    }
}

fn reject_link(path: &Path) -> Result<(), ErrorEnvelope> {
    let metadata = fs::symlink_metadata(path).map_err(|e| failure("PACKAGE_PATH", e))?;
    #[cfg(windows)]
    let linked = {
        use std::os::windows::fs::MetadataExt as _;
        metadata.file_attributes() & 0x400 != 0
    };
    #[cfg(not(windows))]
    let linked = metadata.file_type().is_symlink();
    if linked {
        return Err(failure(
            "PACKAGE_PATH",
            "installation contains a link or junction",
        ));
    }
    Ok(())
}

fn relative_file(name: &str) -> Result<PathBuf, ErrorEnvelope> {
    let path = PathBuf::from(name);
    if name.is_empty()
        || name.contains([':', '\\'])
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(failure(
            "PACKAGE_MANIFEST",
            "manifest file path is not a normal relative path",
        ));
    }
    Ok(path)
}

fn checked_file(root: &Path, name: &str, expected: &str) -> Result<PathBuf, ErrorEnvelope> {
    let path = root.join(relative_file(name)?);
    let mut ancestor = path.as_path();
    while ancestor != root {
        reject_link(ancestor)?;
        ancestor = ancestor
            .parent()
            .ok_or_else(|| failure("PACKAGE_PATH", "missing parent"))?;
    }
    let resolved = fs::canonicalize(&path).map_err(|e| failure("PACKAGE_PATH", e))?;
    if !resolved.starts_with(root)
        || !resolved.is_file()
        || sha256_file(&resolved).map_err(|e| failure("PACKAGE_HASH", e))? != expected
    {
        return Err(failure(
            "PACKAGE_HASH",
            "package file identity or hash differs",
        ));
    }
    Ok(resolved)
}

fn collect_files(
    root: &Path,
    directory: &Path,
    files: &mut BTreeMap<String, String>,
) -> Result<(), ErrorEnvelope> {
    for entry in fs::read_dir(directory).map_err(|e| failure("PACKAGE_FILES", e))? {
        let path = entry.map_err(|e| failure("PACKAGE_FILES", e))?.path();
        reject_link(&path)?;
        if path.is_dir() {
            collect_files(root, &path, files)?;
        } else {
            let name = path
                .strip_prefix(root)
                .map_err(|e| failure("PACKAGE_FILES", e))?
                .to_string_lossy()
                .replace('\\', "/");
            files.insert(
                name,
                sha256_file(&path).map_err(|e| failure("PACKAGE_HASH", e))?,
            );
        }
    }
    Ok(())
}

fn copy_tree(source: &Path, target: &Path) -> Result<(), ErrorEnvelope> {
    let mut files = BTreeMap::new();
    collect_files(source, source, &mut files)?;
    for name in files.keys() {
        let destination = target.join(relative_file(name)?);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|e| failure("PACKAGE_COPY", e))?;
        }
        fs::copy(source.join(name), destination).map_err(|e| failure("PACKAGE_COPY", e))?;
    }
    Ok(())
}

fn cleanup_staging(path: &Path, parent: &Path) -> Result<(), ErrorEnvelope> {
    let resolved = fs::canonicalize(path).map_err(|e| failure("INSTALL_CLEANUP", e))?;
    if resolved != path || resolved.parent() != Some(parent) {
        return Err(failure(
            "INSTALL_CLEANUP",
            "staging identity changed; directory preserved",
        ));
    }
    reject_link(path)?;
    collect_files(path, path, &mut BTreeMap::new())?;
    fs::remove_dir_all(path).map_err(|e| failure("INSTALL_CLEANUP", e))
}

pub(crate) fn verify_binary(
    path: &Path,
    channel: &str,
    expected_product: &str,
) -> Result<(), ErrorEnvelope> {
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
        || info["product"] != expected_product
        || info["channel"] != channel
        || info["version"] != env!("CARGO_PKG_VERSION")
    {
        return Err(failure(
            "PACKAGE_BINARY",
            "binary product, version or compiled channel does not match package",
        ));
    }
    Ok(())
}

/// The only build-time command. Installation from a completed package requires no toolchain.
pub(crate) fn build_packages(
    root: &Path,
    output: &Path,
) -> Result<Vec<PackageArtifact>, ErrorEnvelope> {
    let root = fs::canonicalize(root).map_err(|e| failure("PACKAGE_ROOT", e))?;
    if !root.join("crates/delta-desktop/Cargo.toml").is_file() {
        return Err(failure(
            "PACKAGE_ROOT",
            "build requires the DELTA workspace",
        ));
    }
    let source_fingerprint = crate::verify::source_fingerprint(&root)?;
    fs::create_dir_all(output).map_err(|e| failure("PACKAGE_OUTPUT", e))?;
    let output = fs::canonicalize(output).map_err(|e| failure("PACKAGE_OUTPUT", e))?;
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
    let license_result = run_quiet_command(
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
    fs::write(
        output.join("license-collection-log.json"),
        serde_json::to_vec_pretty(&license_result).map_err(|e| failure("PACKAGE_LICENSES", e))?,
    )
    .map_err(|e| failure("PACKAGE_LICENSES", e))?;
    if license_result.status_code != Some(0) || license_result.timed_out {
        return Err(failure(
            "PACKAGE_LICENSES",
            "license collection failed; inspect license-collection-log.json",
        ));
    }
    let mut artifacts = Vec::new();
    for channel in ["release", "dev"] {
        let app = product(channel)?;
        let target = root.join(".local/target-packages");
        let environment = BTreeMap::from([
            ("DELTA_CHANNEL".into(), channel.into()),
            ("RUSTFLAGS".into(), "-C target-feature=+crt-static".into()),
            // Cargo gives this inherited variable precedence over RUSTFLAGS.
            (
                "CARGO_ENCODED_RUSTFLAGS".into(),
                "-C\u{1f}target-feature=+crt-static".into(),
            ),
            (
                "CARGO_BUILD_JOBS".into(),
                std::env::var("CARGO_BUILD_JOBS").unwrap_or_else(|_| "4".into()),
            ),
        ]);
        let build = run_quiet_command(
            OsStr::new("cargo"),
            [
                "build".into(),
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
            Some(&root),
            &environment,
        )
        .map_err(|e| failure("PACKAGE_BUILD", e))?;
        fs::write(
            output.join(format!("build-{channel}-log.json")),
            serde_json::to_vec_pretty(&build).map_err(|e| failure("PACKAGE_BUILD", e))?,
        )
        .map_err(|e| failure("PACKAGE_BUILD", e))?;
        if build.status_code != Some(0) || build.timed_out {
            return Err(failure(
                "PACKAGE_BUILD",
                format!("{channel} release build failed"),
            ));
        }
        let desktop = target.join("x86_64-pc-windows-msvc/release/delta.exe");
        let cli = target.join("x86_64-pc-windows-msvc/release/delta-cli.exe");
        verify_binary(&desktop, channel, "delta-desktop")?;
        verify_binary(&cli, channel, "delta-cli")?;
        // A new versioned package directory never overwrites a previous artifact.
        let directory = output.join(format!(
            "{app}-{}-{}",
            env!("CARGO_PKG_VERSION"),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&directory).map_err(|e| failure("PACKAGE_STAGE", e))?;
        fs::copy(&desktop, directory.join(format!("{app}.exe")))
            .map_err(|e| failure("PACKAGE_COPY", e))?;
        fs::copy(
            &cli,
            directory.join(if channel == "dev" {
                "delta-dev-cli.exe"
            } else {
                "delta-cli.exe"
            }),
        )
        .map_err(|e| failure("PACKAGE_COPY", e))?;
        fs::write(
            directory.join("binary-imports.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "target":"x86_64-pc-windows-msvc", "crt_static":true,
                "desktop":pe_imports(&desktop)?, "cli":pe_imports(&cli)?,
            }))
            .map_err(|e| failure("PACKAGE_PE", e))?,
        )
        .map_err(|e| failure("PACKAGE_PE", e))?;
        for name in ["LICENSE", "THIRD-PARTY-NOTICES.txt"] {
            fs::copy(root.join(name), directory.join(name))
                .map_err(|e| failure("PACKAGE_COPY", e))?;
        }
        copy_tree(&licenses, &directory.join("third-party-licenses"))?;
        fs::write(directory.join("README.txt"), format!(
            "DELTA {} ({channel})\r\n\r\nWindows x64; no Rust/MSVC/Python development tools or separately installed VC runtime required. The CRT is statically linked; binary-imports.json lists Windows DLL dependencies.\r\nRun {app}.exe to open the desktop.\r\nInstall for the current user from this extracted directory:\r\n{installer} package-install --channel {channel} --package-dir .\r\nClose the desktop, then uninstall using this extracted CLI outside Programs/{app}:\r\n{installer} package-uninstall --channel {channel}\r\nUninstall removes verified application files and preserves library data and unknown files. Running the installed CLI to uninstall refuses before removing files.\r\nThis package is unsigned. Clean-machine, IME, keyboard and scaling checks remain manual.\r\n", env!("CARGO_PKG_VERSION"),
            installer = if channel == "dev" { "delta-dev-cli.exe" } else { "delta-cli.exe" })) .map_err(|e| failure("PACKAGE_README", e))?;
        let mut files = BTreeMap::new();
        collect_files(&directory, &directory, &mut files)?;
        let manifest = PackageManifest {
            schema_version: 1,
            product: app.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            channel: channel.into(),
            source_fingerprint: source_fingerprint.clone(),
            files,
        };
        fs::write(
            directory.join("package-manifest.json"),
            serde_json::to_vec_pretty(&manifest).map_err(|e| failure("PACKAGE_MANIFEST", e))?,
        )
        .map_err(|e| failure("PACKAGE_MANIFEST", e))?;
        let zip_path = output.join(format!(
            "{app}-{}-windows-x86_64-{}.zip",
            env!("CARGO_PKG_VERSION"),
            uuid::Uuid::new_v4()
        ));
        let mut zip = zip::ZipWriter::new(
            fs::File::create(&zip_path).map_err(|e| failure("PACKAGE_ZIP", e))?,
        );
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        let mut archive_files = BTreeMap::new();
        collect_files(&directory, &directory, &mut archive_files)?;
        for name in archive_files.keys() {
            zip.start_file(format!("{app}/{name}"), options)
                .map_err(|e| failure("PACKAGE_ZIP", e))?;
            zip.write_all(&fs::read(directory.join(name)).map_err(|e| failure("PACKAGE_ZIP", e))?)
                .map_err(|e| failure("PACKAGE_ZIP", e))?;
        }
        zip.finish().map_err(|e| failure("PACKAGE_ZIP", e))?;
        let zip_sha256 = sha256_file(&zip_path).map_err(|e| failure("PACKAGE_ZIP", e))?;
        artifacts.push(PackageArtifact {
            channel: channel.into(),
            directory,
            zip_path,
            zip_sha256,
        });
    }
    if crate::verify::source_fingerprint(&root)? != source_fingerprint {
        return Err(failure(
            "PACKAGE_SOURCE_CHANGED",
            "source changed during packaging; artifacts are not accepted",
        ));
    }
    fs::write(
        output.join("package-artifacts.json"),
        serde_json::to_vec_pretty(&artifacts).map_err(|e| failure("PACKAGE_ARTIFACTS", e))?,
    )
    .map_err(|e| failure("PACKAGE_ARTIFACTS", e))?;
    Ok(artifacts)
}

pub(crate) fn install(
    package_dir: &Path,
    local_app_data: &Path,
    channel: &str,
    artifact_output: &Path,
) -> Result<PerUserPackageReceipt, ErrorEnvelope> {
    let app = product(channel)?; // Validate before any filesystem mutation.
    let directory = fs::canonicalize(package_dir).map_err(|e| failure("MISSING_PACKAGE", e))?;
    reject_link(package_dir)?;
    reject_link(&directory.join("package-manifest.json"))?;
    let manifest: PackageManifest = serde_json::from_slice(
        &fs::read(directory.join("package-manifest.json"))
            .map_err(|e| failure("PACKAGE_MANIFEST", e))?,
    )
    .map_err(|e| failure("PACKAGE_MANIFEST", e))?;
    if manifest.schema_version != 1
        || manifest.channel != channel
        || manifest.product != app
        || manifest.version != env!("CARGO_PKG_VERSION")
    {
        return Err(failure(
            "PACKAGE_MANIFEST",
            "package identity or compiled channel differs",
        ));
    }
    if manifest.source_fingerprint.len() != 64
        || !manifest
            .source_fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(failure(
            "PACKAGE_MANIFEST",
            "package has no valid source fingerprint",
        ));
    }
    let mut actual = BTreeMap::new();
    collect_files(&directory, &directory, &mut actual)?;
    actual.remove("package-manifest.json");
    if actual != manifest.files
        || manifest.files.contains_key("install-manifest.json")
        || !manifest.files.contains_key(&format!("{app}.exe"))
    {
        return Err(failure(
            "PACKAGE_MANIFEST",
            "package files differ from the complete manifest",
        ));
    }
    for (name, hash) in &manifest.files {
        checked_file(&directory, name, hash)?;
    }
    let cli_name = if channel == "dev" {
        "delta-dev-cli.exe"
    } else {
        "delta-cli.exe"
    };
    verify_binary(
        &directory.join(format!("{app}.exe")),
        channel,
        "delta-desktop",
    )?;
    verify_binary(&directory.join(cli_name), channel, "delta-cli")?;
    let artifact_list = artifact_output.join("package-artifacts.json");
    let artifact: Option<PackageArtifact> = if artifact_list.is_file() {
        serde_json::from_slice::<Vec<PackageArtifact>>(
            &fs::read(artifact_list).map_err(|e| failure("PACKAGE_ARTIFACTS", e))?,
        )
        .map_err(|e| failure("PACKAGE_ARTIFACTS", e))?
        .into_iter()
        .find(|item| item.channel == channel && item.directory == directory)
    } else {
        None
    };
    if local_app_data.exists() {
        reject_link(local_app_data)?;
    }
    fs::create_dir_all(local_app_data).map_err(|e| failure("INSTALL_ROOT", e))?;
    let local = fs::canonicalize(local_app_data).map_err(|e| failure("INSTALL_ROOT", e))?;
    let programs = local.join("Programs");
    if programs.exists() {
        reject_link(&programs)?;
    }
    fs::create_dir_all(&programs).map_err(|e| failure("INSTALL_ROOT", e))?;
    if fs::canonicalize(&programs).map_err(|e| failure("INSTALL_ROOT", e))? != programs {
        return Err(failure(
            "INSTALL_ROOT",
            "Programs redirects outside the target root",
        ));
    }
    let install_dir = programs.join(app);
    if install_dir.exists() {
        return Err(failure(
            "INSTALL_EXISTS",
            "installation already exists; uninstall its verified files first",
        ));
    }
    let staging = programs.join(format!(".{app}-install-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&staging).map_err(|e| failure("INSTALL_DIRECTORY", e))?;
    let staged = (|| {
        copy_tree(&directory, &staging)?;
        for (name, hash) in &manifest.files {
            checked_file(&staging, name, hash)?;
        }
        let mut files = manifest.files;
        files.insert(
            "package-manifest.json".into(),
            sha256_file(&staging.join("package-manifest.json"))
                .map_err(|e| failure("INSTALL_HASH", e))?,
        );
        let installed = InstallManifest {
            schema_version: 1,
            product: app.into(),
            channel: channel.into(),
            version: manifest.version,
            install_dir: install_dir.clone(),
            files,
        };
        fs::write(
            staging.join("install-manifest.json"),
            serde_json::to_vec_pretty(&installed).map_err(|e| failure("INSTALL_MANIFEST", e))?,
        )
        .map_err(|e| failure("INSTALL_MANIFEST", e))?;
        fs::rename(&staging, &install_dir).map_err(|e| failure("INSTALL_COMMIT", e))?;
        Ok(installed)
    })();
    let installed = match staged {
        Ok(installed) => installed,
        Err(error) => {
            cleanup_staging(&staging, &programs)?;
            return Err(error);
        }
    };
    let mut files_installed: Vec<_> = installed.files.keys().cloned().collect();
    files_installed.push("install-manifest.json".into());
    Ok(PerUserPackageReceipt {
        channel: channel.into(),
        install_dir: install_dir.display().to_string(),
        zip_path: artifact
            .as_ref()
            .map(|a| a.zip_path.display().to_string())
            .unwrap_or_default(),
        zip_sha256: artifact.map(|a| a.zip_sha256).unwrap_or_default(),
        files_installed,
        data_preserved_on_uninstall: true,
    })
}

pub(crate) fn uninstall(local_app_data: &Path, channel: &str) -> Result<(), ErrorEnvelope> {
    let app = product(channel)?;
    if !local_app_data.exists() {
        return Ok(());
    }
    let local = fs::canonicalize(local_app_data).map_err(|e| failure("UNINSTALL_ROOT", e))?;
    let programs = local.join("Programs");
    let directory = programs.join(app);
    if !directory.exists() {
        return Ok(());
    }
    reject_link(&programs)?;
    reject_link(&directory)?;
    let resolved = fs::canonicalize(&directory).map_err(|e| failure("UNINSTALL_ROOT", e))?;
    if resolved != directory || resolved.parent() != Some(programs.as_path()) {
        return Err(failure(
            "UNINSTALL_ROOT",
            "installation redirects outside the target root",
        ));
    }
    let manifest_path = directory.join("install-manifest.json");
    reject_link(&manifest_path)?;
    let manifest: InstallManifest = serde_json::from_slice(
        &fs::read(&manifest_path).map_err(|e| failure("UNINSTALL_MANIFEST", e))?,
    )
    .map_err(|e| failure("UNINSTALL_MANIFEST", e))?;
    if manifest.schema_version != 1
        || manifest.channel != channel
        || manifest.product != app
        || manifest.install_dir != directory
    {
        return Err(failure(
            "UNINSTALL_MANIFEST",
            "installation identity differs; no files removed",
        ));
    }
    let package_manifest_path = checked_file(
        &directory,
        "package-manifest.json",
        manifest.files.get("package-manifest.json").ok_or_else(|| {
            failure(
                "UNINSTALL_MANIFEST",
                "installed package manifest is missing",
            )
        })?,
    )?;
    let packaged: PackageManifest = serde_json::from_slice(
        &fs::read(package_manifest_path).map_err(|e| failure("UNINSTALL_MANIFEST", e))?,
    )
    .map_err(|e| failure("UNINSTALL_MANIFEST", e))?;
    let mut expected = packaged.files;
    expected.insert(
        "package-manifest.json".into(),
        manifest.files["package-manifest.json"].clone(),
    );
    if expected != manifest.files
        || packaged.product != app
        || packaged.channel != channel
        || packaged.version != manifest.version
    {
        return Err(failure(
            "UNINSTALL_MANIFEST",
            "installed ownership differs from package; no files removed",
        ));
    }
    // Validate the entire plan before removing even the first file.
    let mut files = Vec::new();
    let mut parents = std::collections::BTreeSet::new();
    for (name, hash) in &manifest.files {
        let path = checked_file(&directory, name, hash)?;
        let mut parent = path.parent();
        while let Some(p) = parent {
            if p == directory {
                break;
            }
            parents.insert(p.to_path_buf());
            parent = p.parent();
        }
        files.push(path);
    }
    // Windows cannot remove a running executable. Reject before any mutation and
    // let the user run the installer CLI from the extracted portable package.
    if std::env::current_exe()
        .ok()
        .and_then(|path| fs::canonicalize(path).ok())
        .is_some_and(|path| path.starts_with(&directory))
    {
        return Err(failure("UNINSTALL_BUSY", "run package-uninstall from the extracted package CLI (outside the installation directory); no files removed"));
    }
    #[cfg(windows)]
    let _write_leases = files
        .iter()
        .map(|path| {
            fs::OpenOptions::new().write(true).open(path).map_err(|_| {
                failure(
                    "UNINSTALL_BUSY",
                    "close the application and retry; no files removed",
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    for file in files {
        fs::remove_file(file).map_err(|e| failure("UNINSTALL_FILE", e))?;
    }
    #[cfg(windows)]
    drop(_write_leases);
    for parent in parents.into_iter().rev() {
        if fs::read_dir(&parent)
            .map_err(|e| failure("UNINSTALL_DIRECTORY", e))?
            .next()
            .is_none()
        {
            fs::remove_dir(parent).map_err(|e| failure("UNINSTALL_DIRECTORY", e))?;
        }
    }
    fs::remove_file(manifest_path).map_err(|e| failure("UNINSTALL_MANIFEST", e))?;
    if fs::read_dir(&directory)
        .map_err(|e| failure("UNINSTALL_DIRECTORY", e))?
        .next()
        .is_none()
    {
        fs::remove_dir(&directory).map_err(|e| failure("UNINSTALL_DIRECTORY", e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pe_fixture() -> Vec<u8> {
        let mut bytes = vec![0; 1024];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[0x3c..0x40].copy_from_slice(&0x80_u32.to_le_bytes());
        bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
        bytes[0x84..0x86].copy_from_slice(&0x8664_u16.to_le_bytes());
        bytes[0x86..0x88].copy_from_slice(&1_u16.to_le_bytes());
        bytes[0x94..0x96].copy_from_slice(&0xf0_u16.to_le_bytes());
        bytes[0x98..0x9a].copy_from_slice(&0x20b_u16.to_le_bytes());
        for (position, value) in [
            (0x104, 16_u32), // NumberOfRvaAndSizes.
            (0x110, 0x1000),
            (0x114, 40), // Normal import descriptors.
            (0x170, 0x1040),
            (0x174, 64), // Delay-load descriptors.
            (0x194, 0x1000),
            (0x198, 0x200),
            (0x19c, 0x200),  // Section mapping.
            (0x20c, 0x1080), // Normal DLL name.
            (0x240, 1),
            (0x244, 0x10a0), // RVA-based delay DLL name.
        ] {
            bytes[position..position + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes[0x280..0x28d].copy_from_slice(b"KERNEL32.dll\0");
        bytes[0x2a0..0x2b1].copy_from_slice(b"VCRUNTIME140.dll\0");
        bytes
    }

    #[test]
    fn windows_pe_runtime_imports_include_delay_load_and_reject_truncation() {
        let bytes = pe_fixture();
        let imports = parse_pe_imports(&bytes).unwrap();
        assert_eq!(imports, ["kernel32.dll", "vcruntime140.dll"]);
        assert_eq!(
            check_runtime_imports(&imports).unwrap_err().code,
            "PACKAGE_RUNTIME_DEPENDENCY"
        );
        for length in 0..bytes.len() {
            assert!(
                parse_pe_imports(&bytes[..length]).is_err(),
                "accepted truncated PE at {length}"
            );
        }
        let mut invalid_rva = bytes.clone();
        invalid_rva[0x20c..0x210].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse_pe_imports(&invalid_rva).is_err());
        let mut legacy_delay = bytes;
        legacy_delay[0x240..0x244].fill(0);
        assert!(parse_pe_imports(&legacy_delay).is_err());
        for runtime in [
            "VCRUNTIME140_1.dll",
            "MSVCP140.dll",
            "msvcr120.dll",
            "concrt140.dll",
        ] {
            assert!(check_runtime_imports(&[runtime.into()]).is_err());
        }
        check_runtime_imports(&[
            "kernel32.dll".into(),
            "ucrtbase.dll".into(),
            "d3d11.dll".into(),
        ])
        .unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_pe_inspection_reads_the_actual_executable() {
        let imports = pe_imports(&std::env::current_exe().unwrap()).unwrap();
        assert!(imports.iter().any(|name| name == "kernel32.dll"));
        assert!(imports.iter().all(|name| name.ends_with(".dll")));
    }

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::var_os("DELTA_TEST_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir)
                .join(format!("delta-package-regression-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        fn installation(&self) -> PathBuf {
            let directory = self.0.join("Programs/delta-dev");
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join("owned.txt"), b"owned content").unwrap();
            let files = BTreeMap::from([(
                "owned.txt".into(),
                sha256_file(&directory.join("owned.txt")).unwrap(),
            )]);
            let packaged = PackageManifest {
                schema_version: 1,
                product: "delta-dev".into(),
                channel: "dev".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                source_fingerprint: "a".repeat(64),
                files: files.clone(),
            };
            fs::write(
                directory.join("package-manifest.json"),
                serde_json::to_vec(&packaged).unwrap(),
            )
            .unwrap();
            let mut files = files;
            files.insert(
                "package-manifest.json".into(),
                sha256_file(&directory.join("package-manifest.json")).unwrap(),
            );
            let installed = InstallManifest {
                schema_version: 1,
                product: "delta-dev".into(),
                channel: "dev".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                install_dir: fs::canonicalize(&directory).unwrap(),
                files,
            };
            fs::write(
                directory.join("install-manifest.json"),
                serde_json::to_vec(&installed).unwrap(),
            )
            .unwrap();
            directory
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn uninstall_validates_all_hashes_before_mutation_and_retains_unknown_files() {
        let fixture = Fixture::new();
        let directory = fixture.installation();
        fs::write(directory.join("owned.txt"), b"changed").unwrap();
        assert!(uninstall(&fixture.0, "dev").is_err());
        assert!(directory.join("package-manifest.json").is_file());
        assert!(directory.join("install-manifest.json").is_file());
        fs::write(directory.join("owned.txt"), b"owned content").unwrap();
        fs::write(directory.join("user-file.txt"), b"unowned").unwrap();
        fs::create_dir_all(fixture.0.join("delta-dev")).unwrap();
        fs::write(fixture.0.join("delta-dev/data-marker"), b"data").unwrap();
        uninstall(&fixture.0, "dev").unwrap();
        assert_eq!(
            fs::read(directory.join("user-file.txt")).unwrap(),
            b"unowned"
        );
        assert!(!directory.join("owned.txt").exists());
        assert!(fixture.0.join("delta-dev/data-marker").is_file());
    }

    #[test]
    fn installer_rejects_wrong_identity_before_creating_target() {
        let fixture = Fixture::new();
        let package = fixture.0.join("package");
        fs::create_dir(&package).unwrap();
        let manifest = PackageManifest {
            schema_version: 1,
            product: "delta".into(),
            channel: "release".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            source_fingerprint: "a".repeat(64),
            files: BTreeMap::new(),
        };
        fs::write(
            package.join("package-manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let target = fixture.0.join("new-local-app-data");
        assert!(install(&package, &target, "dev", &fixture.0).is_err());
        assert!(!target.exists());
        assert!(install(&package, &target, "invalid", &fixture.0).is_err());
        assert!(!target.exists());
    }
}

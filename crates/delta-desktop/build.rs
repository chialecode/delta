//! Windows resources for the desktop binary (`S-28`): "D" icon and VERSIONINFO from the
//! injected identity and version, plus the 8 MiB main-thread stack (`M2-K06`). Development
//! identities carry `icon-dev.ico` (the release icon with a "DEV" badge) so they are told apart
//! from the release at a glance.

fn main() {
    #[cfg(all(target_os = "windows", target_env = "msvc"))]
    println!("cargo:rustc-link-arg-bin=delta=/STACK:8388608");
    for name in ["DELTA_VERSION", "DELTA_IDENTITY", "DELTA_CHANNEL"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=assets/icon-dev.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let identity = std::env::var("DELTA_IDENTITY").unwrap_or_else(|_| {
        match std::env::var("DELTA_CHANNEL").as_deref() {
            Ok("release") => "delta".into(),
            Ok("dev") => "delta-dev".into(),
            _ => "delta-dev".into(),
        }
    });
    let display = match identity.as_str() {
        "delta" => "DELTA",
        "delta-dev" => "DELTA Dev",
        "delta-test" => "DELTA Test",
        "delta-dev-test" => "DELTA Dev Test",
        other => panic!("unknown DELTA_IDENTITY '{other}'"),
    };
    let icon = if identity.starts_with("delta-dev") {
        "assets/icon-dev.ico"
    } else {
        "assets/icon.ico"
    };
    let version = std::env::var("DELTA_VERSION")
        .unwrap_or_else(|_| std::env::var("CARGO_PKG_VERSION").unwrap());
    let core: Vec<u64> = version
        .split('-')
        .next()
        .unwrap()
        .split('.')
        .map(|part| part.parse().expect("numeric version"))
        .collect();
    let packed = (core[0] << 48) | (core[1] << 32) | (core[2] << 16);
    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon(icon)
        .set("ProductName", display)
        .set("FileDescription", &format!("{display} desktop workbench"))
        .set("CompanyName", "DELTA Contributors")
        .set(
            "LegalCopyright",
            "Copyright (C) DELTA Contributors. Apache-2.0.",
        )
        .set("OriginalFilename", &format!("{identity}.exe"))
        .set("InternalName", &identity)
        .set("ProductVersion", &version)
        .set("FileVersion", &version)
        .set_version_info(winresource::VersionInfo::FILEVERSION, packed)
        .set_version_info(winresource::VersionInfo::PRODUCTVERSION, packed);
    if let Err(error) = resource.compile() {
        panic!("Windows resource compilation failed: {error}");
    }
}

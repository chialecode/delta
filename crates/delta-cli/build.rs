//! Windows resources for the CLI binary (`S-28`): "D" icon (`icon-dev.ico` for development
//! identities, as the desktop binary) and VERSIONINFO.

fn main() {
    for name in ["DELTA_VERSION", "DELTA_IDENTITY", "DELTA_CHANNEL"] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    println!("cargo:rerun-if-changed=../delta-desktop/assets/icon.ico");
    println!("cargo:rerun-if-changed=../delta-desktop/assets/icon-dev.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let identity = std::env::var("DELTA_IDENTITY").unwrap_or_else(|_| {
        match std::env::var("DELTA_CHANNEL").as_deref() {
            Ok("release") => "delta".into(),
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
        "../delta-desktop/assets/icon-dev.ico"
    } else {
        "../delta-desktop/assets/icon.ico"
    };
    let version = std::env::var("DELTA_VERSION")
        .unwrap_or_else(|_| std::env::var("CARGO_PKG_VERSION").unwrap());
    let core_text = version.split(['-', '+']).next().unwrap_or("");
    let core: Vec<u16> = core_text
        .split('.')
        .map(|part| part.parse::<u16>().expect("numeric u16 version component"))
        .collect();
    assert_eq!(
        core.len(),
        3,
        "expected major.minor.patch version, got '{version}'"
    );
    let packed =
        (u64::from(core[0]) << 48) | (u64::from(core[1]) << 32) | (u64::from(core[2]) << 16);
    let mut resource = winresource::WindowsResource::new();
    resource
        .set_icon(icon)
        .set("ProductName", display)
        .set("FileDescription", &format!("{display} command line"))
        .set("CompanyName", "DELTA Contributors")
        .set(
            "LegalCopyright",
            "Copyright (C) DELTA Contributors. Apache-2.0.",
        )
        .set("OriginalFilename", &format!("{identity}-cli.exe"))
        .set("InternalName", &format!("{identity}-cli"))
        .set("ProductVersion", &version)
        .set("FileVersion", &version)
        .set_version_info(winresource::VersionInfo::FILEVERSION, packed)
        .set_version_info(winresource::VersionInfo::PRODUCTVERSION, packed);
    if let Err(error) = resource.compile() {
        panic!("Windows resource compilation failed: {error}");
    }
}

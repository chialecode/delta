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
    let icon = if identity.starts_with("delta-dev") {
        "../delta-desktop/assets/icon-dev.ico"
    } else {
        "../delta-desktop/assets/icon.ico"
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
        .set("ProductName", "DELTA")
        .set("FileDescription", &format!("{identity} command line"))
        .set("CompanyName", "DELTA Contributors")
        .set(
            "LegalCopyright",
            "Copyright (C) DELTA Contributors. Apache-2.0.",
        )
        .set("OriginalFilename", &format!("{identity}-cli.exe"))
        .set("ProductVersion", &version)
        .set("FileVersion", &version)
        .set_version_info(winresource::VersionInfo::FILEVERSION, packed)
        .set_version_info(winresource::VersionInfo::PRODUCTVERSION, packed);
    if let Err(error) = resource.compile() {
        panic!("Windows resource compilation failed: {error}");
    }
}

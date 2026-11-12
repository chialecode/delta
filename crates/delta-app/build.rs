//! Reject invalid injected build identity and version (`S-26`, `S-27`) before compiling.

fn valid_version(raw: &str) -> bool {
    let (core, pre) = match raw.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (raw, None),
    };
    let parts: Vec<_> = core.split('.').collect();
    let numeric = |part: &str| {
        !part.is_empty()
            && part.bytes().all(|b| b.is_ascii_digit())
            && (part == "0" || !part.starts_with('0'))
    };
    parts.len() == 3
        && parts.iter().all(|part| numeric(part))
        && pre.is_none_or(|pre| {
            let stamp = pre.strip_prefix("dev.").unwrap_or_default();
            stamp.len() == 14 && stamp.bytes().all(|b| b.is_ascii_digit())
        })
}

fn main() {
    for name in [
        "DELTA_VERSION",
        "DELTA_IDENTITY",
        "DELTA_CHANNEL",
        "DELTA_SOURCE_FINGERPRINT",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    if let Ok(version) = std::env::var("DELTA_VERSION") {
        if !valid_version(&version) {
            panic!("DELTA_VERSION '{version}' must be X.Y.Z or X.Y.Z-dev.<yyyymmddHHMMSS>");
        }
    }
    let channel = std::env::var("DELTA_CHANNEL").ok();
    if let Ok(identity) = std::env::var("DELTA_IDENTITY") {
        let expected = match identity.as_str() {
            "delta" => "release",
            "delta-dev" | "delta-test" | "delta-dev-test" => "dev",
            other => panic!("unknown DELTA_IDENTITY '{other}'"),
        };
        if channel.as_deref() != Some(expected) {
            panic!("DELTA_IDENTITY '{identity}' requires DELTA_CHANNEL={expected}");
        }
    }
    if let Ok(fingerprint) = std::env::var("DELTA_SOURCE_FINGERPRINT") {
        if fingerprint.len() != 64 || !fingerprint.bytes().all(|b| b.is_ascii_hexdigit()) {
            panic!("DELTA_SOURCE_FINGERPRINT must be a 64-character hex digest");
        }
    }
}

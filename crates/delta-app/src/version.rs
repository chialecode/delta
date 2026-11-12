//! Version derivation from Git tags (`S-27`).
//!
//! Release versions are `0.1.N` and come from tags `v0.1.N` (the first release is `0.1.1`).
//! Development and test identities use `<next release>-dev.<UTC yyyymmddHHMMSS>`. Versions
//! are injected at build time through `DELTA_VERSION`; `Cargo.toml` is never rewritten.

use crate::identity::InstallIdentity;
use chrono::{DateTime, Utc};
use semver::{Prerelease, Version};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum VersionError {
    #[error("invalid release tag '{0}': release tags must be v0.1.N")]
    InvalidTag(String),
    #[error("invalid version '{0}': {1}")]
    InvalidVersion(String, &'static str),
    #[error("version {requested} is not higher than the latest release {latest}")]
    Downgrade { requested: String, latest: String },
}

impl VersionError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidTag(_) => "VERSION_INVALID_TAG",
            Self::InvalidVersion(..) => "VERSION_INVALID",
            Self::Downgrade { .. } => "VERSION_DOWNGRADE",
        }
    }
}

fn is_release_shape(version: &Version) -> bool {
    version.major == 0 && version.minor == 1 && version.pre.is_empty() && version.build.is_empty()
}

/// Parse `v*` tags. Any tag that starts with `v` followed by a digit must be a valid `v0.1.N`;
/// other tags (for example stage markers) are ignored.
pub fn parse_release_tags(tags: &[String]) -> Result<Vec<Version>, VersionError> {
    let mut versions = Vec::new();
    for tag in tags {
        let tag = tag.trim();
        let Some(raw) = tag.strip_prefix('v') else {
            continue;
        };
        if !raw.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let version = Version::parse(raw).map_err(|_| VersionError::InvalidTag(tag.into()))?;
        if !is_release_shape(&version) {
            return Err(VersionError::InvalidTag(tag.into()));
        }
        versions.push(version);
    }
    versions.sort();
    Ok(versions)
}

/// Next release after the latest tag; `0.1.1` when there is none or only `v0.1.0`.
pub fn next_release_version(tags: &[String]) -> Result<Version, VersionError> {
    let latest = parse_release_tags(tags)?.pop();
    Ok(match latest {
        Some(latest) => Version::new(0, 1, (latest.patch + 1).max(1)),
        None => Version::new(0, 1, 1),
    })
}

pub fn dev_version(tags: &[String], built_at: DateTime<Utc>) -> Result<Version, VersionError> {
    let mut version = next_release_version(tags)?;
    version.pre = Prerelease::new(&format!("dev.{}", built_at.format("%Y%m%d%H%M%S")))
        .map_err(|_| VersionError::InvalidVersion(built_at.to_rfc3339(), "bad build time"))?;
    Ok(version)
}

/// Validate a requested package version against the identity rules and existing tags.
/// Release: `0.1.N`, N >= 1, not lower than the latest tag (equal only rebuilds that tag).
/// Dev and test identities: exactly `<next release>-dev.<14 digit UTC timestamp>`.
pub fn validate_package_version(
    identity: InstallIdentity,
    requested: &str,
    tags: &[String],
) -> Result<Version, VersionError> {
    let version = Version::parse(requested.trim())
        .map_err(|_| VersionError::InvalidVersion(requested.into(), "not semantic versioning"))?;
    if !version.build.is_empty() {
        return Err(VersionError::InvalidVersion(
            requested.into(),
            "build metadata is not allowed",
        ));
    }
    let released = parse_release_tags(tags)?;
    let latest = released
        .last()
        .cloned()
        .unwrap_or_else(|| Version::new(0, 1, 0));
    match identity {
        InstallIdentity::Release => {
            if !is_release_shape(&version) || version.patch == 0 {
                return Err(VersionError::InvalidVersion(
                    requested.into(),
                    "release versions must be 0.1.N with N >= 1",
                ));
            }
            if version < latest {
                return Err(VersionError::Downgrade {
                    requested: version.to_string(),
                    latest: latest.to_string(),
                });
            }
        }
        InstallIdentity::Dev | InstallIdentity::Test | InstallIdentity::DevTest => {
            let pre = version.pre.as_str();
            let stamp = pre.strip_prefix("dev.").unwrap_or_default();
            if stamp.len() != 14 || !stamp.bytes().all(|b| b.is_ascii_digit()) {
                return Err(VersionError::InvalidVersion(
                    requested.into(),
                    "development versions must end with -dev.<yyyymmddHHMMSS>",
                ));
            }
            let base = Version::new(version.major, version.minor, version.patch);
            let next = next_release_version(tags)?;
            if base < next {
                return Err(VersionError::Downgrade {
                    requested: version.to_string(),
                    latest: latest.to_string(),
                });
            }
            if base != next {
                return Err(VersionError::InvalidVersion(
                    requested.into(),
                    "development versions must precede the next release",
                ));
            }
        }
    }
    Ok(version)
}

/// Reject same-identity downgrades and reinstalls of an older build.
pub fn is_upgrade(installed: &str, candidate: &str) -> bool {
    match (Version::parse(installed), Version::parse(candidate)) {
        (Ok(installed), Ok(candidate)) => candidate > installed,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn tags(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn m3_a01_version_derivation_for_release_dev_and_test_identities() {
        let fixture = tags(&["v0.1.0", "v0.1.1", "m2-merged"]);
        let at = Utc.with_ymd_and_hms(2026, 11, 9, 8, 30, 5).unwrap();
        assert_eq!(next_release_version(&fixture).unwrap().to_string(), "0.1.2");
        assert_eq!(next_release_version(&[]).unwrap().to_string(), "0.1.1");
        assert_eq!(
            next_release_version(&tags(&["v0.1.0"]))
                .unwrap()
                .to_string(),
            "0.1.1"
        );
        let dev = dev_version(&fixture, at).unwrap().to_string();
        assert_eq!(dev, "0.1.2-dev.20261109083005");
        for identity in [
            InstallIdentity::Dev,
            InstallIdentity::Test,
            InstallIdentity::DevTest,
        ] {
            assert_eq!(
                validate_package_version(identity, &dev, &fixture)
                    .unwrap()
                    .to_string(),
                dev
            );
            assert!(validate_package_version(identity, "0.1.2", &fixture).is_err());
            assert_eq!(
                validate_package_version(identity, "0.1.1-dev.20261109083005", &fixture)
                    .unwrap_err()
                    .code(),
                "VERSION_DOWNGRADE"
            );
        }
        assert_eq!(
            validate_package_version(InstallIdentity::Release, "0.1.2", &fixture)
                .unwrap()
                .to_string(),
            "0.1.2"
        );
        // Rebuilding the latest tag is allowed; lower versions are downgrades.
        assert!(validate_package_version(InstallIdentity::Release, "0.1.1", &fixture).is_ok());
        assert_eq!(
            validate_package_version(InstallIdentity::Release, "0.1.0", &fixture)
                .unwrap_err()
                .code(),
            "VERSION_INVALID"
        );
        assert_eq!(
            validate_package_version(InstallIdentity::Release, "0.1.1", &tags(&["v0.1.3"]))
                .unwrap_err()
                .code(),
            "VERSION_DOWNGRADE"
        );
        for invalid in ["1.0.0", "0.2.1", "0.1.2-rc.1", "0.1.2+meta", "abc"] {
            assert!(
                validate_package_version(InstallIdentity::Release, invalid, &fixture).is_err(),
                "{invalid}"
            );
        }
        for invalid_tag in ["v0.1", "v0.1.x", "v1.0.0", "v0.1.2-rc.1"] {
            assert_eq!(
                next_release_version(&tags(&[invalid_tag]))
                    .unwrap_err()
                    .code(),
                "VERSION_INVALID_TAG",
                "{invalid_tag}"
            );
        }
        assert!(is_upgrade(
            "0.1.2-dev.20261109083005",
            "0.1.2-dev.20261109083105"
        ));
        assert!(is_upgrade("0.1.2-dev.20261109083005", "0.1.2"));
        assert!(!is_upgrade("0.1.2", "0.1.2"));
        assert!(!is_upgrade("0.1.2", "0.1.1"));
        assert!(!is_upgrade("0.1.2", "garbage"));
    }
}

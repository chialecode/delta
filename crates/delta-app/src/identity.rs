//! Installed application identities, versions and build information (`S-26`, `S-27`).
//!
//! A compiled binary carries exactly one identity. `delta` and `delta-dev` are the user's
//! release and development channels; `delta-test` and `delta-dev-test` exist only for gates
//! so installers can be exercised for real without touching the developer's installations.
//! Source builds (`cargo run` / `cargo test`) have no install identity.

use crate::paths::BuildChannel;
use semver::Version;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstallIdentity {
    /// `delta`: the official release channel.
    Release,
    /// `delta-dev`: the developer's local development channel.
    Dev,
    /// `delta-test`: release-like identity used only by gates (loopback update source allowed).
    Test,
    /// `delta-dev-test`: dev-like identity used only by gates (stands in for `delta-dev`).
    DevTest,
}

impl InstallIdentity {
    pub const ALL: [Self; 4] = [Self::Release, Self::Dev, Self::Test, Self::DevTest];

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "delta" => Some(Self::Release),
            "delta-dev" => Some(Self::Dev),
            "delta-test" => Some(Self::Test),
            "delta-dev-test" => Some(Self::DevTest),
            _ => None,
        }
    }

    /// The identity compiled into this binary. Explicit `DELTA_IDENTITY` wins; otherwise
    /// `DELTA_CHANNEL=release|dev` maps to `delta|delta-dev`; source builds have none.
    pub fn current() -> Option<Self> {
        match option_env!("DELTA_IDENTITY") {
            Some(raw) => Self::parse(raw),
            None => match BuildChannel::current() {
                BuildChannel::Release => Some(Self::Release),
                BuildChannel::Dev => Some(Self::Dev),
                BuildChannel::Source => None,
            },
        }
    }

    /// Lowercase identity name; also the install directory leaf and default data root leaf.
    pub fn name(self) -> &'static str {
        match self {
            Self::Release => "delta",
            Self::Dev => "delta-dev",
            Self::Test => "delta-test",
            Self::DevTest => "delta-dev-test",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Release => "DELTA",
            Self::Dev => "DELTA Dev",
            Self::Test => "DELTA Test",
            Self::DevTest => "DELTA Dev Test",
        }
    }

    /// Data-isolation channel. Test identities compile as `dev` so the release guard applies.
    pub fn channel(self) -> BuildChannel {
        match self {
            Self::Release => BuildChannel::Release,
            Self::Dev | Self::Test | Self::DevTest => BuildChannel::Dev,
        }
    }

    pub fn is_test(self) -> bool {
        matches!(self, Self::Test | Self::DevTest)
    }

    /// Dev-like identities are updated locally by `install-dev`, never online.
    pub fn is_dev_like(self) -> bool {
        matches!(self, Self::Dev | Self::DevTest)
    }

    /// Inno Setup `AppId` (without the doubled leading brace used inside `.iss`).
    pub fn app_id(self) -> &'static str {
        match self {
            Self::Release => "{3FDAF65D-1E1C-4BF6-B4A2-31CFD914D9BD}",
            Self::Dev => "{5D1A489B-D102-4087-B17B-3FFD99D3D824}",
            Self::Test => "{827E62F1-5E62-4C0B-8B68-6DE09193CE70}",
            Self::DevTest => "{339B764E-FCA4-4E02-86EE-D52B4427542C}",
        }
    }

    /// HKCU uninstall key written by Inno Setup for this identity.
    pub fn uninstall_key(self) -> String {
        format!(
            "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\{}_is1",
            self.app_id()
        )
    }

    pub fn desktop_exe(self) -> &'static str {
        match self {
            Self::Release => "delta.exe",
            Self::Dev => "delta-dev.exe",
            Self::Test => "delta-test.exe",
            Self::DevTest => "delta-dev-test.exe",
        }
    }

    pub fn cli_exe(self) -> &'static str {
        match self {
            Self::Release => "delta-cli.exe",
            Self::Dev => "delta-dev-cli.exe",
            Self::Test => "delta-test-cli.exe",
            Self::DevTest => "delta-dev-test-cli.exe",
        }
    }

    /// Named mutex held by the running desktop app; also the installer `AppMutex`.
    pub fn mutex_name(self) -> String {
        format!("{}-instance", self.name())
    }

    /// Per-identity control pipe (`S-29`).
    pub fn pipe_name(self) -> String {
        format!(r"\\.\pipe\{}-control", self.name())
    }

    pub fn app_user_model_id(self) -> &'static str {
        match self {
            Self::Release => "delta.app",
            Self::Dev => "delta.app.dev",
            Self::Test => "delta.app.test",
            Self::DevTest => "delta.app.dev-test",
        }
    }

    /// Online update channel string bound into signatures (`S-31`). Dev-like identities have none.
    pub fn update_channel(self) -> Option<&'static str> {
        match self {
            Self::Release => Some("release"),
            Self::Test => Some("test"),
            Self::Dev | Self::DevTest => None,
        }
    }

    /// Release asset file name for an installer of this identity.
    pub fn setup_file_name(self, version: &str) -> String {
        format!("{}-setup-{version}-x64.exe", self.name())
    }
}

/// Version compiled into this binary (`DELTA_VERSION`), falling back to the crate version for
/// source builds. Validated by `build.rs`, so parsing cannot fail at runtime.
pub fn app_version() -> &'static str {
    option_env!("DELTA_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}

pub fn app_semver() -> Version {
    Version::parse(app_version()).unwrap_or_else(|_| Version::new(0, 0, 0))
}

/// Source fingerprint injected by the package builder; `source` for unpackaged builds.
pub fn build_source_fingerprint() -> &'static str {
    option_env!("DELTA_SOURCE_FINGERPRINT").unwrap_or("source")
}

/// Shared `--build-info` payload for the desktop and CLI binaries.
pub fn build_info(product: &str) -> serde_json::Value {
    serde_json::json!({
        "product": product,
        "version": app_version(),
        "channel": BuildChannel::current().as_str(),
        "identity": InstallIdentity::current().map(InstallIdentity::name),
        "source_fingerprint": build_source_fingerprint(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn identities_are_fully_isolated_and_test_identities_never_compile_as_release() {
        let mut seen = BTreeSet::new();
        for identity in InstallIdentity::ALL {
            for value in [
                identity.name().to_string(),
                identity.app_id().to_string(),
                identity.desktop_exe().to_string(),
                identity.cli_exe().to_string(),
                identity.mutex_name(),
                identity.pipe_name(),
                identity.app_user_model_id().to_string(),
            ] {
                assert!(
                    seen.insert(value.clone()),
                    "duplicate identity value {value}"
                );
            }
            assert_eq!(InstallIdentity::parse(identity.name()), Some(identity));
            if identity.is_test() {
                assert_eq!(identity.channel(), BuildChannel::Dev);
            }
        }
        assert_eq!(InstallIdentity::Release.channel(), BuildChannel::Release);
        assert_eq!(InstallIdentity::Dev.update_channel(), None);
        assert_eq!(InstallIdentity::DevTest.update_channel(), None);
        assert_eq!(InstallIdentity::Test.update_channel(), Some("test"));
        assert_eq!(InstallIdentity::parse("DELTA"), None);
        assert!(Version::parse(app_version()).is_ok());
    }
}

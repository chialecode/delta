//! Release online update (`S-31`): check, resumable download, Ed25519 verification and
//! installer hand-off. Only the release identity checks GitHub Releases; the gate-only
//! `delta-test` identity may use a `127.0.0.1` HTTP source. Development identities never update
//! online (`install-dev` updates them locally).

use crate::identity::{app_semver, InstallIdentity};
use crate::process::{spawn_detached_app, DetachedLaunch};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;

pub const SIGNATURE_DOMAIN: &str = "delta-update-v1";
pub const MAX_UPDATE_BYTES: u64 = 512 * 1024 * 1024;
pub const GITHUB_LATEST_RELEASE: &str =
    "https://api.github.com/repos/chialecode/delta/releases/latest";
pub const ALLOWED_HOSTS: [&str; 4] = [
    "github.com",
    "api.github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];
pub const MANIFEST_FILE: &str = "latest.json";
pub const CHECK_INTERVAL_HOURS: i64 = 24;
const MANIFEST_LIMIT: usize = 64 * 1024;
const SIGNATURE_LIMIT: usize = 1024;

/// Embedded release verification key. Empty until the release key is generated (`ACT-10`);
/// an empty key fails closed so the release build never installs an unverifiable update.
pub const RELEASE_KEY_JSON: &str = include_str!("release-signing-key.json");

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum UpdateError {
    #[error("online update is not available for this build: {0}")]
    NotConfigured(String),
    #[error("update URL rejected: {0}")]
    Transport(String),
    #[error("update redirect rejected: {0}")]
    Redirect(String),
    #[error("update signature is missing")]
    SignatureMissing,
    #[error("update signature does not match the published installer")]
    SignatureInvalid,
    #[error("update is for channel '{found}', this build is '{expected}'")]
    ChannelMismatch { expected: String, found: String },
    #[error("update {offered} is not newer than the installed {current}")]
    NotNewer { current: String, offered: String },
    #[error("update size {0} exceeds the limit")]
    TooLarge(u64),
    #[error("downloaded installer hash does not match the signed manifest")]
    HashMismatch,
    #[error("downloaded installer size does not match the signed manifest")]
    SizeMismatch,
    #[error("update manifest is invalid: {0}")]
    Manifest(String),
    #[error("network error while updating: {0}")]
    Network(String),
    #[error("update cannot start while work is running: {0}")]
    Busy(String),
    #[error("pre-update backup failed; nothing was installed: {0}")]
    BackupFailed(String),
    #[error("update file error: {0}")]
    Io(String),
}

impl UpdateError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotConfigured(_) => "UPDATE_NOT_CONFIGURED",
            Self::Transport(_) => "UPDATE_TRANSPORT",
            Self::Redirect(_) => "UPDATE_REDIRECT",
            Self::SignatureMissing => "UPDATE_SIGNATURE_MISSING",
            Self::SignatureInvalid => "UPDATE_SIGNATURE_INVALID",
            Self::ChannelMismatch { .. } => "UPDATE_CHANNEL_MISMATCH",
            Self::NotNewer { .. } => "UPDATE_NOT_NEWER",
            Self::TooLarge(_) => "UPDATE_TOO_LARGE",
            Self::HashMismatch => "UPDATE_HASH_MISMATCH",
            Self::SizeMismatch => "UPDATE_SIZE_MISMATCH",
            Self::Manifest(_) => "UPDATE_MANIFEST",
            Self::Network(_) => "UPDATE_NETWORK",
            Self::Busy(_) => "UPDATE_BUSY",
            Self::BackupFailed(_) => "UPDATE_BACKUP_FAILED",
            Self::Io(_) => "UPDATE_IO",
        }
    }
}

fn io(error: impl std::fmt::Display) -> UpdateError {
    UpdateError::Io(error.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateManifest {
    pub schema_version: u32,
    pub channel: String,
    pub version: String,
    pub file: String,
    pub size: u64,
    pub sha256: String,
    pub published_at: String,
}

/// Exact byte string covered by the detached signature.
pub fn canonical_message(channel: &str, version: &str, file: &str, sha256: &str) -> String {
    format!("{SIGNATURE_DOMAIN}\n{channel}\n{version}\n{file}\n{sha256}")
}

pub fn sign_manifest(key: &SigningKey, manifest: &UpdateManifest) -> String {
    let message = canonical_message(
        &manifest.channel,
        &manifest.version,
        &manifest.file,
        &manifest.sha256,
    );
    hex::encode(key.sign(message.as_bytes()).to_bytes())
}

pub fn parse_public_key(hex_key: &str) -> Result<VerifyingKey, UpdateError> {
    let bytes: [u8; 32] = hex::decode(hex_key.trim())
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| UpdateError::NotConfigured("public key must be 32 bytes of hex".into()))?;
    VerifyingKey::from_bytes(&bytes)
        .map_err(|_| UpdateError::NotConfigured("public key is not a valid Ed25519 point".into()))
}

const PKCS8_ED25519_V1_PREFIX: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];
const PKCS8_ED25519_V2_PREFIX: [u8; 16] = [
    0x30, 0x51, 0x02, 0x01, 0x01, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];
const PKCS8_ED25519_V2_PUB_PREFIX: [u8; 3] = [0x81, 0x21, 0x00];
const PEM_PRIVATE_BEGIN: &str = concat!("-----BEGIN ", "PRIVATE KEY-----");
const PEM_PRIVATE_END: &str = concat!("-----END ", "PRIVATE KEY-----");

fn decode_base64_strict(input: &str) -> Result<Vec<u8>, UpdateError> {
    let invalid = || UpdateError::NotConfigured("invalid base64 in signing key PEM".into());
    let chars: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if chars.is_empty() || !chars.len().is_multiple_of(4) {
        return Err(invalid());
    }
    let val = |b: u8| -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity((chars.len() / 4) * 3);
    for (idx, chunk) in chars.as_chunks::<4>().0.iter().enumerate() {
        let is_last = (idx + 1) * 4 == chars.len();
        let a = val(chunk[0]).ok_or_else(invalid)?;
        let b = val(chunk[1]).ok_or_else(invalid)?;
        if chunk[2] == b'=' {
            if !is_last || chunk[3] != b'=' || (b & 0x0f) != 0 {
                return Err(invalid());
            }
            out.push((a << 2) | (b >> 4));
        } else if chunk[3] == b'=' {
            if !is_last {
                return Err(invalid());
            }
            let c = val(chunk[2]).ok_or_else(invalid)?;
            if (c & 0x03) != 0 {
                return Err(invalid());
            }
            out.push((a << 2) | (b >> 4));
            out.push((b << 4) | (c >> 2));
        } else {
            let c = val(chunk[2]).ok_or_else(invalid)?;
            let d = val(chunk[3]).ok_or_else(invalid)?;
            out.push((a << 2) | (b >> 4));
            out.push((b << 4) | (c >> 2));
            out.push((c << 6) | d);
        }
    }
    Ok(out)
}

fn parse_signing_key_pem(pem: &str) -> Result<SigningKey, UpdateError> {
    let invalid = || {
        UpdateError::NotConfigured(
            "signing key PEM must be an unencrypted PKCS#8 Ed25519 private key".into(),
        )
    };
    let trimmed = pem.trim();
    let rest = trimmed
        .strip_prefix(PEM_PRIVATE_BEGIN)
        .ok_or_else(invalid)?;
    let body = rest.strip_suffix(PEM_PRIVATE_END).ok_or_else(invalid)?;
    if body.contains("-----") {
        return Err(invalid());
    }
    let der = decode_base64_strict(body)?;
    if der.len() == 48 && der.starts_with(&PKCS8_ED25519_V1_PREFIX) {
        let seed: [u8; 32] = der[16..48].try_into().map_err(|_| invalid())?;
        return Ok(SigningKey::from_bytes(&seed));
    }
    if der.len() == 83
        && der.starts_with(&PKCS8_ED25519_V2_PREFIX)
        && der[48..51] == PKCS8_ED25519_V2_PUB_PREFIX
    {
        let seed: [u8; 32] = der[16..48].try_into().map_err(|_| invalid())?;
        let key = SigningKey::from_bytes(&seed);
        if key.verifying_key().to_bytes() != der[51..83] {
            return Err(invalid());
        }
        return Ok(key);
    }
    Err(invalid())
}

/// Parse an Ed25519 signing key from either 32-byte hex (local key file) or PKCS#8 Ed25519 PEM
/// (`UPDATE_SIGNING_KEY_PEM` in CI).
pub fn parse_signing_key(raw: &str) -> Result<SigningKey, UpdateError> {
    let trimmed = raw.trim();
    if trimmed.starts_with(PEM_PRIVATE_BEGIN) {
        return parse_signing_key_pem(trimmed);
    }
    let bytes: [u8; 32] = hex::decode(trimmed)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| {
            UpdateError::NotConfigured(
                "signing key must be 32 bytes of hex or PKCS#8 Ed25519 PEM".into(),
            )
        })?;
    Ok(SigningKey::from_bytes(&bytes))
}

/// Generate a fresh signing key from the operating system CSPRNG.
pub fn generate_signing_key() -> Result<SigningKey, UpdateError> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| UpdateError::Io(e.to_string()))?;
    Ok(SigningKey::from_bytes(&seed))
}

pub fn verify_manifest_signature(
    key: &VerifyingKey,
    manifest: &UpdateManifest,
    signature_hex: &str,
) -> Result<(), UpdateError> {
    if signature_hex.trim().is_empty() {
        return Err(UpdateError::SignatureMissing);
    }
    let bytes: [u8; 64] = hex::decode(signature_hex.trim())
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or(UpdateError::SignatureInvalid)?;
    let message = canonical_message(
        &manifest.channel,
        &manifest.version,
        &manifest.file,
        &manifest.sha256,
    );
    key.verify(message.as_bytes(), &Signature::from_bytes(&bytes))
        .map_err(|_| UpdateError::SignatureInvalid)
}

/// The embedded release key, or `None` while it has not been generated.
pub fn embedded_release_public_key() -> Option<VerifyingKey> {
    let value: serde_json::Value = serde_json::from_str(RELEASE_KEY_JSON).ok()?;
    let key = value["public_key_hex"].as_str()?;
    if key.is_empty() {
        return None;
    }
    parse_public_key(key).ok()
}

#[derive(Debug, Clone)]
pub struct UpdatePolicy {
    pub identity: InstallIdentity,
    pub channel: String,
    pub source_url: String,
    pub allow_loopback_http: bool,
    pub public_key: Option<VerifyingKey>,
    pub max_bytes: u64,
}

impl UpdatePolicy {
    /// The release identity always uses GitHub Releases and the embedded key; environment
    /// overrides are honoured only by the gate-only `delta-test` identity.
    pub fn for_identity(identity: InstallIdentity) -> Result<Self, UpdateError> {
        match identity {
            InstallIdentity::Release => Ok(Self::release(embedded_release_public_key())),
            InstallIdentity::Test => {
                let source = std::env::var("DELTA_UPDATE_SOURCE").map_err(|_| {
                    UpdateError::NotConfigured("DELTA_UPDATE_SOURCE is not set".into())
                })?;
                let key = std::env::var("DELTA_UPDATE_PUBLIC_KEY").map_err(|_| {
                    UpdateError::NotConfigured("DELTA_UPDATE_PUBLIC_KEY is not set".into())
                })?;
                Ok(Self::test(source, Some(parse_public_key(&key)?)))
            }
            InstallIdentity::Dev | InstallIdentity::DevTest => Err(UpdateError::NotConfigured(
                "development identities are updated locally with install-dev".into(),
            )),
        }
    }

    pub fn release(public_key: Option<VerifyingKey>) -> Self {
        Self {
            identity: InstallIdentity::Release,
            channel: "release".into(),
            source_url: GITHUB_LATEST_RELEASE.into(),
            allow_loopback_http: false,
            public_key,
            max_bytes: MAX_UPDATE_BYTES,
        }
    }

    pub fn test(source_url: String, public_key: Option<VerifyingKey>) -> Self {
        Self {
            identity: InstallIdentity::Test,
            channel: "test".into(),
            source_url,
            allow_loopback_http: true,
            public_key,
            max_bytes: MAX_UPDATE_BYTES,
        }
    }

    pub fn check_url(&self, raw: &str) -> Result<(), UpdateError> {
        url_allowed(raw, self.allow_loopback_http)
    }

    fn key(&self) -> Result<&VerifyingKey, UpdateError> {
        self.public_key.as_ref().ok_or_else(|| {
            UpdateError::NotConfigured("the update verification key has not been generated".into())
        })
    }

    /// Validate manifest metadata, policy bindings, and detached Ed25519 signature without
    /// comparing against an installed version. Shared by `evaluate`, `download`, `verify_installer`,
    /// and `launch_installer` so direct verifier/launcher callers cannot bypass policy checks.
    pub(crate) fn validate_manifest_and_signature(
        &self,
        manifest: &UpdateManifest,
        signature_hex: Option<&str>,
    ) -> Result<Version, UpdateError> {
        if manifest.schema_version != 1 {
            return Err(UpdateError::Manifest("unsupported schema version".into()));
        }
        if manifest.channel != self.channel {
            return Err(UpdateError::ChannelMismatch {
                expected: self.channel.clone(),
                found: manifest.channel.clone(),
            });
        }
        let offered = Version::parse(&manifest.version)
            .map_err(|_| UpdateError::Manifest("version is not semantic versioning".into()))?;
        if manifest.file != self.identity.setup_file_name(&manifest.version) {
            return Err(UpdateError::Manifest(
                "installer file name does not match identity and version".into(),
            ));
        }
        if manifest.sha256.len() != 64
            || !manifest
                .sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(UpdateError::Manifest("sha256 must be lowercase hex".into()));
        }
        if manifest.size == 0 || manifest.size > self.max_bytes {
            return Err(UpdateError::TooLarge(manifest.size));
        }
        let signature = signature_hex.ok_or(UpdateError::SignatureMissing)?;
        verify_manifest_signature(self.key()?, manifest, signature)?;
        Ok(offered)
    }

    /// Validate an offer completely before any installer byte is downloaded.
    pub fn evaluate(
        &self,
        current: &Version,
        manifest: &UpdateManifest,
        signature_hex: Option<&str>,
    ) -> Result<Version, UpdateError> {
        let offered = self.validate_manifest_and_signature(manifest, signature_hex)?;
        if offered <= *current {
            return Err(UpdateError::NotNewer {
                current: current.to_string(),
                offered: offered.to_string(),
            });
        }
        Ok(offered)
    }
}

/// HTTPS to an allowlisted GitHub host, or (test policy only) plain HTTP to `127.0.0.1`.
pub fn url_allowed(raw: &str, allow_loopback_http: bool) -> Result<(), UpdateError> {
    let url = reqwest::Url::parse(raw).map_err(|_| UpdateError::Transport(raw.into()))?;
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if !url.username().is_empty() || url.password().is_some() {
        return Err(UpdateError::Transport("credentials in URL".into()));
    }
    match url.scheme() {
        "https" if ALLOWED_HOSTS.contains(&host.as_str()) && url.port().is_none() => Ok(()),
        "http" if allow_loopback_http && host == "127.0.0.1" => Ok(()),
        "https" => Err(UpdateError::Transport(format!(
            "host {host} is not allowed"
        ))),
        scheme => Err(UpdateError::Transport(format!(
            "{scheme} is not allowed; updates require HTTPS"
        ))),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateOffer {
    pub manifest: UpdateManifest,
    pub signature_hex: String,
    pub installer_url: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdatePreferences {
    pub auto_check: bool,
    pub last_check_at: Option<String>,
}

impl UpdatePreferences {
    fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("settings").join("update.json")
    }

    /// Defaults to automatic checks on (`Q-05`).
    pub fn load(data_dir: &Path) -> Self {
        fs::read(Self::path(data_dir))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(Self {
                auto_check: true,
                last_check_at: None,
            })
    }

    pub fn save(&self, data_dir: &Path) -> Result<(), UpdateError> {
        let path = Self::path(data_dir);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(io)?;
        }
        let temporary = path.with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4().simple()));
        let bytes = serde_json::to_vec_pretty(self).map_err(io)?;
        fs::write(&temporary, bytes).map_err(io)?;
        if let Err(err) = fs::rename(&temporary, &path) {
            let _ = fs::remove_file(&temporary);
            return Err(io(err));
        }
        Ok(())
    }

    pub fn check_due(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        self.auto_check
            && self
                .last_check_at
                .as_deref()
                .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
                .is_none_or(|at| {
                    now.signed_duration_since(at) >= chrono::Duration::hours(CHECK_INTERVAL_HOURS)
                })
    }
}

pub struct UpdateClient {
    policy: UpdatePolicy,
    http: reqwest::Client,
    runtime: tokio::runtime::Runtime,
}

impl UpdateClient {
    pub fn new(policy: UpdatePolicy) -> Result<Self, UpdateError> {
        policy.check_url(&policy.source_url)?;
        let allow_loopback = policy.allow_loopback_http;
        let redirect = reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 5 {
                attempt.error("too many redirects")
            } else if url_allowed(attempt.url().as_str(), allow_loopback).is_ok() {
                attempt.follow()
            } else {
                let host = attempt.url().host_str().unwrap_or("?").to_string();
                attempt.error(format!("redirect to {host} is not allowed"))
            }
        });
        let http = reqwest::Client::builder()
            .redirect(redirect)
            .user_agent(concat!("delta-updater/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| UpdateError::Network(e.to_string()))?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(io)?;
        Ok(Self {
            policy,
            http,
            runtime,
        })
    }

    pub fn policy(&self) -> &UpdatePolicy {
        &self.policy
    }

    fn map_error(error: reqwest::Error) -> UpdateError {
        if error.is_redirect() {
            UpdateError::Redirect(error.to_string())
        } else {
            UpdateError::Network(error.without_url().to_string())
        }
    }

    fn run_async<F, T>(&self, future: F) -> T
    where
        F: std::future::Future<Output = T> + Send,
        T: Send,
    {
        if tokio::runtime::Handle::try_current().is_ok() {
            std::thread::scope(|s| {
                s.spawn(|| self.runtime.block_on(future))
                    .join()
                    .expect("update async helper thread panicked")
            })
        } else {
            self.runtime.block_on(future)
        }
    }

    fn get_limited(&self, url: &str, limit: usize) -> Result<Vec<u8>, UpdateError> {
        self.policy.check_url(url)?;
        self.run_async(async {
            let mut response = self
                .http
                .get(url)
                .header("Accept", "application/vnd.github+json, application/json")
                .send()
                .await
                .map_err(Self::map_error)?;
            if !response.status().is_success() {
                return Err(UpdateError::Network(format!("HTTP {}", response.status())));
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(Self::map_error)? {
                body.extend_from_slice(&chunk);
                if body.len() > limit {
                    return Err(UpdateError::Manifest("response exceeds size limit".into()));
                }
            }
            Ok(body)
        })
    }

    /// Ask the source for the latest non-prerelease. `Ok(None)` means already up to date.
    pub fn check(&self, current: &Version) -> Result<Option<UpdateOffer>, UpdateError> {
        let release: serde_json::Value =
            serde_json::from_slice(&self.get_limited(&self.policy.source_url, 1024 * 1024)?)
                .map_err(|e| UpdateError::Manifest(e.to_string()))?;
        if release["draft"] == true || release["prerelease"] == true {
            return Ok(None);
        }
        let assets: BTreeMap<String, String> = release["assets"]
            .as_array()
            .ok_or_else(|| UpdateError::Manifest("release has no assets".into()))?
            .iter()
            .filter_map(|asset| {
                Some((
                    asset["name"].as_str()?.to_string(),
                    asset["browser_download_url"].as_str()?.to_string(),
                ))
            })
            .collect();
        let manifest_url = assets
            .get(MANIFEST_FILE)
            .ok_or_else(|| UpdateError::Manifest("latest.json asset is missing".into()))?;
        let manifest: UpdateManifest =
            serde_json::from_slice(&self.get_limited(manifest_url, MANIFEST_LIMIT)?)
                .map_err(|e| UpdateError::Manifest(e.to_string()))?;
        let signature = match assets.get(&format!("{}.sig", manifest.file)) {
            Some(url) => Some(
                String::from_utf8(self.get_limited(url, SIGNATURE_LIMIT)?)
                    .map_err(|_| UpdateError::SignatureInvalid)?,
            ),
            None => None,
        };
        let installer_url = assets
            .get(&manifest.file)
            .ok_or_else(|| UpdateError::Manifest("installer asset is missing".into()))?
            .clone();
        self.policy.check_url(&installer_url)?;
        match self
            .policy
            .evaluate(current, &manifest, signature.as_deref())
        {
            Ok(_) => Ok(Some(UpdateOffer {
                manifest,
                signature_hex: signature.unwrap_or_default().trim().to_string(),
                installer_url,
            })),
            Err(UpdateError::NotNewer { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Download with HTTP Range resume into `directory`. A partial file survives network
    /// errors; a server that ignores Range, returns HTTP 416, or returns an unexpected
    /// `Content-Range` restarts the whole download. The finished file is verified (policy,
    /// size, SHA-256, signature) before it gets its final name.
    pub fn download(
        &self,
        offer: &UpdateOffer,
        directory: &Path,
        cancel: &AtomicBool,
    ) -> Result<PathBuf, UpdateError> {
        self.policy.check_url(&offer.installer_url)?;
        self.policy
            .validate_manifest_and_signature(&offer.manifest, Some(&offer.signature_hex))?;
        if cancel.load(Ordering::SeqCst) {
            return Err(UpdateError::Network("download cancelled".into()));
        }
        fs::create_dir_all(directory).map_err(io)?;
        let target = directory.join(&offer.manifest.file);
        if target.is_file() && verify_installer(&self.policy, offer, &target).is_ok() {
            return Ok(target);
        }
        let partial = directory.join(format!("{}.partial", offer.manifest.file));
        let mut existing = fs::metadata(&partial).map(|m| m.len()).unwrap_or(0);
        if existing >= offer.manifest.size {
            fs::remove_file(&partial).map_err(io)?;
            existing = 0;
        }
        self.run_async(async {
            for attempt in 0..2u8 {
                if cancel.load(Ordering::SeqCst) {
                    return Err(UpdateError::Network("download cancelled".into()));
                }
                let mut request = self.http.get(&offer.installer_url);
                if existing > 0 {
                    request = request.header("Range", format!("bytes={existing}-"));
                }
                let mut response = await_with_cancel(request.send(), cancel)
                    .await?
                    .map_err(Self::map_error)?;
                let status = response.status();
                if existing > 0 && status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
                    let _ = fs::remove_file(&partial);
                    existing = 0;
                    if attempt == 0 {
                        continue;
                    }
                    return Err(UpdateError::Network(format!("HTTP {status}")));
                }
                let mut file = if existing > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT {
                    let range_header = response
                        .headers()
                        .get(reqwest::header::CONTENT_RANGE)
                        .and_then(|v| v.to_str().ok());
                    if !content_range_matches(range_header, existing, offer.manifest.size) {
                        let _ = fs::remove_file(&partial);
                        existing = 0;
                        if attempt == 0 {
                            continue;
                        }
                        return Err(UpdateError::Network(
                            "server returned mismatched Content-Range on resume".into(),
                        ));
                    }
                    fs::OpenOptions::new()
                        .append(true)
                        .open(&partial)
                        .map_err(io)?
                } else if status.is_success() {
                    existing = 0;
                    fs::File::create(&partial).map_err(io)?
                } else {
                    return Err(UpdateError::Network(format!("HTTP {status}")));
                };
                let mut written = existing;
                while let Some(chunk) = await_with_cancel(response.chunk(), cancel)
                    .await?
                    .map_err(Self::map_error)?
                {
                    if cancel.load(Ordering::SeqCst) {
                        return Err(UpdateError::Network("download cancelled".into()));
                    }
                    written += chunk.len() as u64;
                    if written > offer.manifest.size || written > self.policy.max_bytes {
                        drop(file);
                        let _ = fs::remove_file(&partial);
                        return Err(UpdateError::SizeMismatch);
                    }
                    file.write_all(&chunk).map_err(io)?;
                }
                file.sync_all().map_err(io)?;
                if written < offer.manifest.size {
                    return Err(UpdateError::Network(
                        "download interrupted; it will resume".into(),
                    ));
                }
                return Ok(());
            }
            Err(UpdateError::Network(
                "download retry budget exhausted".into(),
            ))
        })?;
        if let Err(error) = verify_installer(&self.policy, offer, &partial) {
            let _ = fs::remove_file(&partial);
            return Err(error);
        }
        fs::rename(&partial, &target).map_err(io)?;
        Ok(target)
    }
}

async fn await_with_cancel<F, T>(future: F, cancel: &AtomicBool) -> Result<T, UpdateError>
where
    F: std::future::Future<Output = T>,
{
    if cancel.load(Ordering::SeqCst) {
        return Err(UpdateError::Network("download cancelled".into()));
    }
    tokio::pin!(future);
    loop {
        tokio::select! {
            biased;
            res = &mut future => return Ok(res),
            _ = tokio::time::sleep(Duration::from_millis(20)) => {
                if cancel.load(Ordering::SeqCst) {
                    return Err(UpdateError::Network("download cancelled".into()));
                }
            }
        }
    }
}

fn content_range_matches(header: Option<&str>, expected_start: u64, expected_total: u64) -> bool {
    let Some(raw) = header.map(str::trim) else {
        return false;
    };
    let Some(rest) = raw
        .strip_prefix("bytes ")
        .or_else(|| raw.strip_prefix("BYTES "))
    else {
        return false;
    };
    let Some((range_part, total_part)) = rest.split_once('/') else {
        return false;
    };
    let Some((start_str, end_str)) = range_part.split_once('-') else {
        return false;
    };
    let (Ok(start), Ok(end)) = (
        start_str.trim().parse::<u64>(),
        end_str.trim().parse::<u64>(),
    ) else {
        return false;
    };
    if start != expected_start || end < start {
        return false;
    }
    let total_part = total_part.trim();
    if total_part == "*" {
        return true;
    }
    total_part
        .parse::<u64>()
        .is_ok_and(|total| total == expected_total && end < total)
}

pub fn sha256_of(path: &Path) -> Result<String, UpdateError> {
    let mut file = fs::File::open(path).map_err(io)?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer).map_err(io)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(hex::encode(hash.finalize()))
}

/// Re-verify a downloaded installer immediately before it is executed.
/// Enforces full policy/channel/filename/size-cap/signature validation as well as file integrity.
pub fn verify_installer(
    policy: &UpdatePolicy,
    offer: &UpdateOffer,
    path: &Path,
) -> Result<(), UpdateError> {
    policy.validate_manifest_and_signature(&offer.manifest, Some(&offer.signature_hex))?;
    let size = fs::metadata(path).map_err(io)?.len();
    if size != offer.manifest.size {
        return Err(UpdateError::SizeMismatch);
    }
    if sha256_of(path)? != offer.manifest.sha256 {
        return Err(UpdateError::HashMismatch);
    }
    Ok(())
}

/// Encode the app's relaunch arguments for the installer's restart entry.
pub fn encode_relaunch_args(args: &[String]) -> String {
    hex::encode(serde_json::to_vec(args).unwrap_or_default())
}

pub fn decode_relaunch_args(encoded: &str) -> Option<Vec<String>> {
    serde_json::from_slice(&hex::decode(encoded).ok()?).ok()
}

/// Silent installer arguments shared by the updater, `install-dev` and the gates: `/S`, the
/// `/LOG=` file the installer appends to, and after an update `/RELAUNCH` with the app's
/// previous arguments (`/DELTAARGS=`, handed back through `--restore-args`).
/// The `/LOG=` option of DELTA's installer and uninstaller. NSIS's GetOptions ends an option
/// value at the next "/", so the path gets backslashes only.
pub fn installer_log_arg(log: &Path) -> String {
    format!("/LOG={}", log.display().to_string().replace('/', "\\"))
}

pub fn silent_installer_args(log: &Path, restart_args: Option<&[String]>) -> Vec<String> {
    let mut args = vec!["/S".to_string(), installer_log_arg(log)];
    if let Some(restart) = restart_args {
        args.push("/RELAUNCH".into());
        args.push(format!("/DELTAARGS={}", encode_relaunch_args(restart)));
    }
    args
}

/// Start the verified installer detached. The caller must have finished the consistent
/// backup and must exit afterwards; the installer (given `/RELAUNCH`) waits for the identity mutex.
pub fn launch_installer(
    policy: &UpdatePolicy,
    offer: &UpdateOffer,
    installer: &Path,
    log: &Path,
    restart_args: &[String],
) -> Result<DetachedLaunch, UpdateError> {
    let offered =
        policy.validate_manifest_and_signature(&offer.manifest, Some(&offer.signature_hex))?;
    let current = current_version();
    if offered <= current {
        return Err(UpdateError::NotNewer {
            current: current.to_string(),
            offered: offered.to_string(),
        });
    }
    verify_installer(policy, offer, installer)?;
    spawn_detached_app(
        installer.as_os_str(),
        silent_installer_args(log, Some(restart_args)),
        &BTreeMap::new(),
        false,
    )
    .map_err(UpdateError::Io)
}

pub fn current_version() -> Version {
    app_semver()
}

/// Minimal HTTP/1.1 release source on `127.0.0.1` for tests and the M3 rehearsal. It serves
/// the GitHub `releases/latest` JSON shape, honours `Range`, can cut the first installer
/// response short and can redirect to a host outside the allowlist.
pub struct UpdateLoopbackServer {
    pub base_url: String,
    files: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    pub ranges_seen: Arc<Mutex<Vec<String>>>,
    interrupt_after: Arc<AtomicUsize>,
    redirect_installer_to: Arc<Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl UpdateLoopbackServer {
    pub fn start() -> Result<Self, UpdateError> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(io)?;
        listener.set_nonblocking(true).map_err(io)?;
        let base_url = format!("http://{}", listener.local_addr().map_err(io)?);
        let files = Arc::new(Mutex::new(BTreeMap::new()));
        let ranges_seen = Arc::new(Mutex::new(Vec::new()));
        let interrupt_after = Arc::new(AtomicUsize::new(0));
        let redirect_installer_to = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (files, ranges, interrupt, redirect, stop) = (
                Arc::clone(&files),
                Arc::clone(&ranges_seen),
                Arc::clone(&interrupt_after),
                Arc::clone(&redirect_installer_to),
                Arc::clone(&stop),
            );
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = serve_loopback(stream, &files, &ranges, &interrupt, &redirect);
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(5)),
                    }
                }
            })
        };
        Ok(Self {
            base_url,
            files,
            ranges_seen,
            interrupt_after,
            redirect_installer_to,
            stop,
            thread: Some(thread),
        })
    }

    pub fn latest_url(&self) -> String {
        format!("{}/repos/chialecode/delta/releases/latest", self.base_url)
    }

    pub fn put(&self, name: &str, bytes: Vec<u8>) {
        self.files.lock().unwrap().insert(name.into(), bytes);
    }

    pub fn remove(&self, name: &str) {
        self.files.lock().unwrap().remove(name);
    }

    /// Cut the next installer response after `bytes` bytes (once).
    pub fn interrupt_next_download_after(&self, bytes: usize) {
        self.interrupt_after.store(bytes, Ordering::SeqCst);
    }

    pub fn redirect_installer(&self, target: Option<String>) {
        *self.redirect_installer_to.lock().unwrap() = target;
    }

    /// Publish `latest.json`, the installer and (optionally) its signature.
    pub fn publish(
        &self,
        manifest: &UpdateManifest,
        installer: Vec<u8>,
        signature: Option<String>,
    ) {
        let mut files = self.files.lock().unwrap();
        files.retain(|name, _| name == "__keep__");
        files.insert(
            MANIFEST_FILE.into(),
            serde_json::to_vec(manifest).unwrap_or_default(),
        );
        files.insert(manifest.file.clone(), installer);
        if let Some(signature) = signature {
            files.insert(format!("{}.sig", manifest.file), signature.into_bytes());
        }
    }
}

impl Drop for UpdateLoopbackServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_loopback(
    mut stream: std::net::TcpStream,
    files: &Mutex<BTreeMap<String, Vec<u8>>>,
    ranges: &Mutex<Vec<String>>,
    interrupt: &AtomicUsize,
    redirect: &Mutex<Option<String>>,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut request = Vec::new();
    let mut buffer = [0u8; 4096];
    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
        let read = stream.read(&mut buffer)?;
        if read == 0 || request.len() > 64 * 1024 {
            return Ok(());
        }
        request.extend_from_slice(&buffer[..read]);
    }
    let text = String::from_utf8_lossy(&request).to_string();
    let path = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string();
    let host = text
        .lines()
        .find_map(|line| {
            line.strip_prefix("Host: ")
                .or_else(|| line.strip_prefix("host: "))
        })
        .unwrap_or("127.0.0.1")
        .trim()
        .to_string();
    let range = text.lines().find_map(|line| {
        let lower = line.to_ascii_lowercase();
        lower
            .strip_prefix("range: bytes=")
            .map(|rest| rest.trim().to_string())
    });
    let respond = |stream: &mut std::net::TcpStream, status: &str, headers: &str, body: &[u8]| {
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes())?;
        stream.write_all(body)
    };
    if path == "/repos/chialecode/delta/releases/latest" {
        let names: Vec<String> = files.lock().unwrap().keys().cloned().collect();
        let assets: Vec<serde_json::Value> = names
            .iter()
            .map(|name| {
                serde_json::json!({"name": name, "browser_download_url": format!("http://{host}/download/{name}")})
            })
            .collect();
        let body = serde_json::to_vec(&serde_json::json!({"draft":false,"prerelease":false,"tag_name":"loopback","assets":assets}))
            .unwrap_or_default();
        return respond(
            &mut stream,
            "200 OK",
            "Content-Type: application/json\r\n",
            &body,
        );
    }
    let Some(name) = path.strip_prefix("/download/") else {
        return respond(&mut stream, "404 Not Found", "", b"");
    };
    let Some(bytes) = files.lock().unwrap().get(name).cloned() else {
        return respond(&mut stream, "404 Not Found", "", b"");
    };
    let installer = name.ends_with(".exe");
    if installer {
        if let Some(target) = redirect.lock().unwrap().clone() {
            return respond(
                &mut stream,
                "302 Found",
                &format!("Location: {target}\r\n"),
                b"",
            );
        }
        ranges
            .lock()
            .unwrap()
            .push(range.clone().unwrap_or_default());
    }
    let start = range
        .as_deref()
        .and_then(|r| r.strip_suffix('-'))
        .and_then(|r| r.parse::<usize>().ok())
        .filter(|start| *start < bytes.len());
    let (status, extra, body) = match start {
        Some(start) => (
            "206 Partial Content",
            format!(
                "Content-Range: bytes {start}-{}/{}\r\n",
                bytes.len() - 1,
                bytes.len()
            ),
            &bytes[start..],
        ),
        None => ("200 OK", String::new(), &bytes[..]),
    };
    let cut = if installer {
        interrupt.swap(0, Ordering::SeqCst)
    } else {
        0
    };
    if cut > 0 && cut < body.len() {
        // Announce the full length but close early: the client sees an interrupted body.
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes())?;
        stream.write_all(&body[..cut])?;
        stream.flush()?;
        return stream.shutdown(std::net::Shutdown::Both);
    }
    respond(&mut stream, status, &extra, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D-35 / M5-W08: the published v0.1.1 `latest.json` and detached signature (copied verbatim
    /// from the GitHub release) must still verify with the embedded release key after the
    /// `ed25519-dalek` 3 / `sha2` 0.11 / `getrandom` 0.4 upgrade; any field change must fail.
    #[test]
    fn published_v0_1_1_manifest_signature_still_verifies() {
        let manifest: UpdateManifest = serde_json::from_str(include_str!(
            "../../../tests/fixtures/release/v0.1.1/latest.json"
        ))
        .unwrap();
        let signature =
            include_str!("../../../tests/fixtures/release/v0.1.1/delta-setup-0.1.1-x64.exe.sig");
        let key = embedded_release_public_key().expect("embedded release key");
        assert_eq!(manifest.version, "0.1.1");
        verify_manifest_signature(&key, &manifest, signature).unwrap();
        let mut tampered = manifest.clone();
        tampered.sha256 = tampered.sha256.replace('5', "6");
        assert!(verify_manifest_signature(&key, &tampered, signature).is_err());
        let mut other = manifest;
        other.version = "0.1.2".into();
        assert!(verify_manifest_signature(&key, &other, signature).is_err());
    }

    fn fixture(
        key: &SigningKey,
        channel: &str,
        version: &str,
        body: &[u8],
    ) -> (UpdateManifest, String) {
        let identity = if channel == "release" {
            InstallIdentity::Release
        } else {
            InstallIdentity::Test
        };
        let manifest = UpdateManifest {
            schema_version: 1,
            channel: channel.into(),
            version: version.into(),
            file: identity.setup_file_name(version),
            size: body.len() as u64,
            sha256: hex::encode(Sha256::digest(body)),
            published_at: "2026-11-09T00:00:00Z".into(),
        };
        let signature = sign_manifest(key, &manifest);
        (manifest, signature)
    }

    #[test]
    fn m3_a07_signature_downgrade_and_transport_policy() {
        let key = generate_signing_key().unwrap();
        let other = generate_signing_key().unwrap();
        let current = Version::parse("0.1.1").unwrap();
        let release = UpdatePolicy::release(Some(key.verifying_key()));
        let body = b"installer bytes".to_vec();
        let (valid, signature) = fixture(&key, "release", "0.1.2", &body);
        assert_eq!(
            release
                .evaluate(&current, &valid, Some(&signature))
                .unwrap(),
            Version::parse("0.1.2").unwrap()
        );
        let reject = |manifest: &UpdateManifest, signature: Option<&str>| {
            release
                .evaluate(&current, manifest, signature)
                .unwrap_err()
                .code()
        };
        // Tampered manifest, missing signature, wrong key, cross-version and cross-channel.
        let mut tampered = valid.clone();
        tampered.sha256 = hex::encode(Sha256::digest(b"other"));
        assert_eq!(
            reject(&tampered, Some(&signature)),
            "UPDATE_SIGNATURE_INVALID"
        );
        assert_eq!(reject(&valid, None), "UPDATE_SIGNATURE_MISSING");
        assert_eq!(reject(&valid, Some("")), "UPDATE_SIGNATURE_MISSING");
        assert_eq!(
            reject(&valid, Some(&sign_manifest(&other, &valid))),
            "UPDATE_SIGNATURE_INVALID"
        );
        let (newer, newer_signature) = fixture(&key, "release", "0.1.3", &body);
        let mut cross_version = newer.clone();
        cross_version.version = "0.1.2".into();
        cross_version.file = valid.file.clone();
        assert_eq!(
            reject(&cross_version, Some(&newer_signature)),
            "UPDATE_SIGNATURE_INVALID"
        );
        let (test_channel, test_signature) = fixture(&key, "test", "0.1.2", &body);
        assert_eq!(
            reject(&test_channel, Some(&test_signature)),
            "UPDATE_CHANNEL_MISMATCH"
        );
        // Equal and lower versions.
        let (same, same_signature) = fixture(&key, "release", "0.1.1", &body);
        assert_eq!(reject(&same, Some(&same_signature)), "UPDATE_NOT_NEWER");
        let (lower, lower_signature) = fixture(&key, "release", "0.1.0", &body);
        assert_eq!(reject(&lower, Some(&lower_signature)), "UPDATE_NOT_NEWER");
        // Size limit and name binding.
        let mut huge = valid.clone();
        huge.size = MAX_UPDATE_BYTES + 1;
        let huge_signature = sign_manifest(&key, &huge);
        assert_eq!(reject(&huge, Some(&huge_signature)), "UPDATE_TOO_LARGE");
        let mut renamed = valid.clone();
        renamed.file = "evil.exe".into();
        let renamed_signature = sign_manifest(&key, &renamed);
        assert_eq!(
            reject(&renamed, Some(&renamed_signature)),
            "UPDATE_MANIFEST"
        );
        // No embedded key fails closed.
        assert_eq!(
            UpdatePolicy::release(None)
                .evaluate(&current, &valid, Some(&signature))
                .unwrap_err()
                .code(),
            "UPDATE_NOT_CONFIGURED"
        );
        // Transport: HTTPS to allowlisted hosts only; loopback HTTP only for the test policy.
        for ok in [
            "https://api.github.com/repos/chialecode/delta/releases/latest",
            "https://github.com/chialecode/delta/releases/download/v0.1.2/x.exe",
            "https://objects.githubusercontent.com/a",
            "https://release-assets.githubusercontent.com/a",
        ] {
            assert!(url_allowed(ok, false).is_ok(), "{ok}");
        }
        for bad in [
            "http://api.github.com/repos/chialecode/delta/releases/latest",
            "https://evil.example.com/x.exe",
            "https://github.com.evil.example/x.exe",
            concat!("https://user", ":pass@github.com/x.exe"),
            "https://github.com:8443/x.exe",
            "http://127.0.0.1:8080/x.exe",
            "file:///c/x.exe",
        ] {
            assert_eq!(
                url_allowed(bad, false).unwrap_err().code(),
                "UPDATE_TRANSPORT",
                "{bad}"
            );
        }
        assert!(url_allowed("http://127.0.0.1:8080/x.exe", true).is_ok());
        assert!(url_allowed("http://localhost:8080/x.exe", true).is_err());
        assert!(url_allowed("http://10.0.0.1/x.exe", true).is_err());
        assert!(UpdatePolicy::release(None)
            .check_url("http://127.0.0.1:1/")
            .is_err());
        assert!(UpdatePolicy::for_identity(InstallIdentity::Dev).is_err());
        assert!(UpdatePolicy::for_identity(InstallIdentity::DevTest).is_err());
        let release_policy = UpdatePolicy::for_identity(InstallIdentity::Release).unwrap();
        assert!(!release_policy.allow_loopback_http);
        assert_eq!(release_policy.source_url, GITHUB_LATEST_RELEASE);
    }

    #[test]
    fn m3_a08_loopback_check_resume_and_redirect_rejection() {
        let root = std::env::var_os("DELTA_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("delta_m3_update_{}", uuid::Uuid::new_v4()));
        let key = generate_signing_key().unwrap();
        let server = UpdateLoopbackServer::start().unwrap();
        let policy = UpdatePolicy::test(server.latest_url(), Some(key.verifying_key()));
        let client = UpdateClient::new(policy.clone()).unwrap();
        let body: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let current = Version::parse("0.1.2-dev.20261109000000").unwrap();
        let version = "0.1.2-dev.20261109000100";
        let (manifest, signature) = fixture(&key, "test", version, &body);

        // Missing signature asset is rejected before download.
        server.publish(&manifest, body.clone(), None);
        assert_eq!(
            client.check(&current).unwrap_err().code(),
            "UPDATE_SIGNATURE_MISSING"
        );
        server.publish(&manifest, body.clone(), Some(signature.clone()));
        let offer = client
            .check(&current)
            .unwrap()
            .expect("newer version offered");
        assert!(client
            .check(&Version::parse(version).unwrap())
            .unwrap()
            .is_none());

        // First download is cut short, the second resumes with Range and verifies.
        server.interrupt_next_download_after(100_000);
        let cancel = AtomicBool::new(false);
        assert_eq!(
            client.download(&offer, &root, &cancel).unwrap_err().code(),
            "UPDATE_NETWORK"
        );
        let partial = root.join(format!("{}.partial", manifest.file));
        assert_eq!(fs::metadata(&partial).unwrap().len(), 100_000);
        let installer = client.download(&offer, &root, &cancel).unwrap();
        assert_eq!(fs::read(&installer).unwrap(), body);
        assert_eq!(
            *server.ranges_seen.lock().unwrap(),
            vec![String::new(), "100000-".to_string()]
        );
        verify_installer(&policy, &offer, &installer).unwrap();
        fs::write(&installer, b"tampered").unwrap();
        assert_eq!(
            verify_installer(&policy, &offer, &installer)
                .unwrap_err()
                .code(),
            "UPDATE_SIZE_MISMATCH"
        );
        fs::remove_file(&installer).unwrap();

        // Corrupted bytes with the right length: hash mismatch, partial discarded.
        let mut corrupted = body.clone();
        corrupted[10] ^= 0xff;
        server.put(&manifest.file, corrupted);
        assert_eq!(
            client.download(&offer, &root, &cancel).unwrap_err().code(),
            "UPDATE_HASH_MISMATCH"
        );
        assert!(!partial.exists());

        // A redirect outside the allowlist is refused by the HTTP client itself.
        server.put(&manifest.file, body);
        server.redirect_installer(Some("https://evil.example.com/x.exe".into()));
        assert_eq!(
            client.download(&offer, &root, &cancel).unwrap_err().code(),
            "UPDATE_REDIRECT"
        );
        drop(server);
        fs::remove_dir_all(&root).unwrap();

        let args = vec!["--data-dir".to_string(), "data dir/\"z\"".to_string()];
        assert_eq!(
            decode_relaunch_args(&encode_relaunch_args(&args)).unwrap(),
            args
        );
        // The NSIS installer's options: silent, its log, and after an update the relaunch with
        // the previous arguments (hex, so they reach it as one token without quoting).
        let log = Path::new("u.log");
        assert_eq!(silent_installer_args(log, None), vec!["/S", "/LOG=u.log"]);
        let restart = silent_installer_args(log, Some(&args));
        assert_eq!(&restart[..3], &["/S", "/LOG=u.log", "/RELAUNCH"]);
        let hex = restart[3].strip_prefix("/DELTAARGS=").unwrap();
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(decode_relaunch_args(hex).unwrap(), args);
        // A path joined with "/" (a gate's `.local/reports/...`) must not be cut at the "/".
        assert_eq!(
            silent_installer_args(Path::new(r"repo\.local/reports/m3/a.log"), None)[1],
            r"/LOG=repo\.local\reports\m3\a.log"
        );
    }

    #[test]
    fn update_preferences_default_on_and_respect_interval() {
        let now = chrono::Utc::now();
        let mut preferences = UpdatePreferences {
            auto_check: true,
            last_check_at: None,
        };
        assert!(preferences.check_due(now));
        preferences.last_check_at = Some((now - chrono::Duration::hours(2)).to_rfc3339());
        assert!(!preferences.check_due(now));
        preferences.last_check_at = Some((now - chrono::Duration::hours(25)).to_rfc3339());
        assert!(preferences.check_due(now));
        preferences.auto_check = false;
        assert!(!preferences.check_due(now));
    }

    #[test]
    fn verify_installer_enforces_policy_and_launch_rejects_downgrade() {
        let root = std::env::var_os("DELTA_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("delta_m3_policy_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let key = generate_signing_key().unwrap();
        let release = UpdatePolicy::release(Some(key.verifying_key()));
        let body = b"signed installer bytes".to_vec();
        let file_path = root.join("installer.exe");
        fs::write(&file_path, &body).unwrap();

        // Cross-channel signed manifest must be rejected by verify_installer directly.
        let (test_manifest, test_sig) = fixture(&key, "test", "9.9.9", &body);
        let cross_channel_offer = UpdateOffer {
            manifest: test_manifest,
            signature_hex: test_sig,
            installer_url: "https://github.com/chialecode/delta/releases/download/v9.9.9/x.exe"
                .into(),
        };
        assert_eq!(
            verify_installer(&release, &cross_channel_offer, &file_path)
                .unwrap_err()
                .code(),
            "UPDATE_CHANNEL_MISMATCH"
        );

        // Signed manifest with unexpected/traversal filename must be rejected by verify_installer.
        let mut bad_name_manifest = fixture(&key, "release", "9.9.9", &body).0;
        bad_name_manifest.file = "../evil.exe".into();
        let bad_name_offer = UpdateOffer {
            signature_hex: sign_manifest(&key, &bad_name_manifest),
            manifest: bad_name_manifest,
            installer_url: "https://github.com/chialecode/delta/releases/download/v9.9.9/x.exe"
                .into(),
        };
        assert_eq!(
            verify_installer(&release, &bad_name_offer, &file_path)
                .unwrap_err()
                .code(),
            "UPDATE_MANIFEST"
        );

        // Downgrade offer cannot be launched even if signed and hash-matching.
        let (old_manifest, old_sig) = fixture(&key, "release", "0.0.1", &body);
        let old_offer = UpdateOffer {
            manifest: old_manifest,
            signature_hex: old_sig,
            installer_url: "https://github.com/chialecode/delta/releases/download/v0.0.1/x.exe"
                .into(),
        };
        assert_eq!(
            launch_installer(&release, &old_offer, &file_path, &root.join("u.log"), &[])
                .unwrap_err()
                .code(),
            "UPDATE_NOT_NEWER"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn download_recovers_from_http_416_and_mismatched_content_range() {
        let root = std::env::var_os("DELTA_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("delta_m3_range_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let key = generate_signing_key().unwrap();
        let body = b"full-installer-payload-0123456789abcdef".to_vec();
        let (manifest, sig) = fixture(&key, "test", "9.9.1", &body);

        // Minimal server: first request with Range returns 416; retry without Range returns 200.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let body_server = body.clone();
        let server_handle = std::thread::spawn(move || {
            for mode in ["416", "200", "bad206", "200"] {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut req = [0u8; 4096];
                let _ = stream.read(&mut req);
                let (head, payload): (String, &[u8]) = match mode {
                    "416" => (
                        "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .into(),
                        b"",
                    ),
                    "bad206" => (
                        format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body_server.len() - 1,
                            body_server.len(),
                            body_server.len()
                        ),
                        &body_server,
                    ),
                    _ => (
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body_server.len()
                        ),
                        &body_server,
                    ),
                };
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(payload);
            }
        });

        let policy = UpdatePolicy::test(
            format!("http://{addr}/repos/chialecode/delta/releases/latest"),
            Some(key.verifying_key()),
        );
        let client = UpdateClient::new(policy).unwrap();
        let offer = UpdateOffer {
            manifest: manifest.clone(),
            signature_hex: sig,
            installer_url: format!("http://{addr}/download/{}", manifest.file),
        };
        let cancel = AtomicBool::new(false);

        // Case 1: stale partial triggers HTTP 416 -> restarts from 0 and succeeds.
        let partial = root.join(format!("{}.partial", manifest.file));
        fs::write(&partial, &body[..10]).unwrap();
        let path1 = client.download(&offer, &root, &cancel).unwrap();
        assert_eq!(fs::read(&path1).unwrap(), body);
        fs::remove_file(&path1).unwrap();

        // Case 2: server answers 206 with wrong Content-Range start -> discards partial and restarts.
        fs::write(&partial, &body[..10]).unwrap();
        let path2 = client.download(&offer, &root, &cancel).unwrap();
        assert_eq!(fs::read(&path2).unwrap(), body);

        let _ = server_handle.join();
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn download_cancels_promptly_when_server_stalls() {
        let root = std::env::var_os("DELTA_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("delta_m3_cancel_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let key = generate_signing_key().unwrap();
        let body = b"stalled-installer-body".to_vec();
        let (manifest, sig) = fixture(&key, "test", "9.9.2", &body);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let release_server = Arc::new(AtomicBool::new(false));
        let release_flag = Arc::clone(&release_server);
        let server_handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                // Hold the socket open without sending headers until test releases us.
                while !release_flag.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        });

        let policy = UpdatePolicy::test(
            format!("http://{addr}/repos/chialecode/delta/releases/latest"),
            Some(key.verifying_key()),
        );
        let client = UpdateClient::new(policy).unwrap();
        let offer = UpdateOffer {
            manifest: manifest.clone(),
            signature_hex: sig,
            installer_url: format!("http://{addr}/download/{}", manifest.file),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_trigger = Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            cancel_trigger.store(true, Ordering::SeqCst);
        });

        let started = std::time::Instant::now();
        let err = client.download(&offer, &root, &cancel).unwrap_err();
        assert_eq!(err.code(), "UPDATE_NETWORK");
        assert!(err.to_string().contains("cancelled"));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "cancel should abort promptly rather than waiting for 60s read_timeout"
        );

        release_server.store(true, Ordering::SeqCst);
        let _ = server_handle.join();
        fs::remove_dir_all(&root).unwrap();
    }

    fn encode_base64_for_test(bytes: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
            let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
            out.push(if chunk.len() > 1 {
                TABLE[((n >> 6) & 0x3f) as usize] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                TABLE[(n & 0x3f) as usize] as char
            } else {
                '='
            });
        }
        out
    }

    #[test]
    fn parse_signing_key_accepts_hex_and_pkcs8_ed25519_pem() {
        let key = generate_signing_key().unwrap();
        let hex_key = hex::encode(key.to_bytes());
        assert_eq!(
            parse_signing_key(&hex_key).unwrap().to_bytes(),
            key.to_bytes()
        );

        // RFC 8410 PKCS#8 v1 Ed25519 PEM (48-byte DER).
        let mut der_v1 = Vec::with_capacity(48);
        der_v1.extend_from_slice(&PKCS8_ED25519_V1_PREFIX);
        der_v1.extend_from_slice(&key.to_bytes());
        let pem_v1 = format!(
            "{PEM_PRIVATE_BEGIN}\n{}\n{PEM_PRIVATE_END}\n",
            encode_base64_for_test(&der_v1)
        );
        let parsed_v1 = parse_signing_key(&pem_v1).unwrap();
        assert_eq!(parsed_v1.to_bytes(), key.to_bytes());
        assert_eq!(parsed_v1.verifying_key(), key.verifying_key());

        // RFC 8410 PKCS#8 v2 Ed25519 PEM with matching public key (83-byte DER).
        let mut der_v2 = Vec::with_capacity(83);
        der_v2.extend_from_slice(&PKCS8_ED25519_V2_PREFIX);
        der_v2.extend_from_slice(&key.to_bytes());
        der_v2.extend_from_slice(&PKCS8_ED25519_V2_PUB_PREFIX);
        der_v2.extend_from_slice(&key.verifying_key().to_bytes());
        let pem_v2 = format!(
            "{PEM_PRIVATE_BEGIN}\r\n{}\r\n{PEM_PRIVATE_END}",
            encode_base64_for_test(&der_v2)
        );
        assert_eq!(
            parse_signing_key(&pem_v2).unwrap().to_bytes(),
            key.to_bytes()
        );

        // Independent RFC 8410 Section 10.3 Ed25519 test vectors (not built from production
        // prefix constants): verify both PKCS#8 v1 (48-byte DER) and v2 OneAsymmetricKey
        // (83-byte DER, SEQUENCE length 0x51 = 81).
        let rfc_seed = "d4ee72dbf913584ad5b6d8f1f769f8ad3afe7c28cbf1d4fbe097a88f44755842";
        let rfc_pub = "19bf44096984cdfe8541bac167dc3b96c85086aa30b6b6cb0c5c38ad703166e1";
        let rfc_v1_pem = format!(
            "{PEM_PRIVATE_BEGIN}\nMC4CAQAwBQYDK2VwBCIEINTuctv5E1hK1bbY8fdp+K06/nwoy/HU++CXqI9EdVhC\n{PEM_PRIVATE_END}\n"
        );
        let rfc_v1 = parse_signing_key(&rfc_v1_pem).unwrap();
        assert_eq!(hex::encode(rfc_v1.to_bytes()), rfc_seed);
        assert_eq!(hex::encode(rfc_v1.verifying_key().to_bytes()), rfc_pub);

        let rfc_v2_pem = format!(
            "{PEM_PRIVATE_BEGIN}\nMFECAQEwBQYDK2VwBCIEINTuctv5E1hK1bbY8fdp+K06/nwoy/HU++CXqI9EdVhCgSEAGb9ECWmEzf6FQbrBZ9w7lshQhqowtrbLDFw4rXAxZuE=\n{PEM_PRIVATE_END}\n"
        );
        let rfc_v2 = parse_signing_key(&rfc_v2_pem).unwrap();
        assert_eq!(hex::encode(rfc_v2.to_bytes()), rfc_seed);
        assert_eq!(hex::encode(rfc_v2.verifying_key().to_bytes()), rfc_pub);

        // Wrong SEQUENCE length (0x53 instead of 0x51) in v2 DER must be rejected.
        let bad_len_v2_pem = format!(
            "{PEM_PRIVATE_BEGIN}\nMFMCAQEwBQYDK2VwBCIEINTuctv5E1hK1bbY8fdp+K06/nwoy/HU++CXqI9EdVhCgSEAGb9ECWmEzf6FQbrBZ9w7lshQhqowtrbLDFw4rXAxZuE=\n{PEM_PRIVATE_END}\n"
        );
        assert!(parse_signing_key(&bad_len_v2_pem).is_err());

        // Mismatched public key in v2 DER must be rejected.
        let mut bad_v2 = der_v2.clone();
        bad_v2[82] ^= 0x01;
        let bad_pem_v2 = format!(
            "{PEM_PRIVATE_BEGIN}\n{}\n{PEM_PRIVATE_END}",
            encode_base64_for_test(&bad_v2)
        );
        assert!(parse_signing_key(&bad_pem_v2).is_err());

        // Wrong OID or truncated DER or trailing garbage must be rejected.
        let mut bad_oid = der_v1.clone();
        bad_oid[11] = 0x71;
        for bad in [
            "".to_string(),
            "not-a-key".to_string(),
            format!(
                "{PEM_PRIVATE_BEGIN}\n{}\n{PEM_PRIVATE_END}",
                encode_base64_for_test(&bad_oid)
            ),
            format!(
                "{PEM_PRIVATE_BEGIN}\n{}\n{PEM_PRIVATE_END}extra",
                encode_base64_for_test(&der_v1)
            ),
            format!(
                "{PEM_PRIVATE_BEGIN}\n{}\n{PEM_PRIVATE_END}",
                encode_base64_for_test(&der_v1[..47])
            ),
        ] {
            assert!(parse_signing_key(&bad).is_err(), "accepted {bad:?}");
        }
    }
}

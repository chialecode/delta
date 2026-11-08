//! File-based credential store (`D-32`).
//!
//! Secrets live in `<data_dir>/credentials/<namespace>/`, one file per provider/account,
//! encrypted with Windows DPAPI for the current user (no Windows Credential Manager). The
//! file name is the hex of the provider and account, so nothing readable is exposed; the
//! namespace, provider and account are bound into the DPAPI entropy, so a file copied to
//! another slot does not decrypt. Writes are atomic (temporary file + rename). Without DPAPI
//! the store refuses to save or read instead of falling back to plaintext.
//!
//! Consistent backups and exports never include this directory (they copy the SQLite
//! snapshot and attachments only); deleting the library deletes it with the data directory.

use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

const FILE_MAGIC: &[u8] = b"DELTA-CRED-1\n";

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("credential store error for '{target}': {reason}")]
    Store { target: String, reason: String },
    #[error("credential not found for '{0}'")]
    NotFound(String),
}

#[derive(Debug, Clone)]
pub struct CredentialStore {
    directory: PathBuf,
    namespace: String,
}

impl CredentialStore {
    /// Store under `<data_dir>/credentials/<namespace>`. `namespace` is the identity's
    /// credential prefix (`delta`, `delta-dev`, `delta-test`, or a `delta-test-*` run id).
    pub fn new(data_dir: &Path, namespace: impl Into<String>) -> Self {
        let namespace = namespace.into();
        Self {
            directory: data_dir.join("credentials").join(&namespace),
            namespace,
        }
    }

    pub fn for_paths(paths: &crate::paths::ResolvedAppPaths) -> Self {
        Self::new(&paths.data_dir, paths.credential_prefix.clone())
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn service_target(&self, provider: &str) -> String {
        format!(
            "{}:{}",
            self.namespace,
            provider.trim().to_ascii_lowercase()
        )
    }

    /// Opaque reference stored in settings instead of the secret.
    pub fn opaque_reference(&self, provider: &str) -> String {
        format!("credential://{}", self.service_target(provider))
    }

    fn slot(&self, provider: &str, account_user: &str) -> (String, PathBuf, Vec<u8>) {
        let provider = provider.trim().to_ascii_lowercase();
        let target = self.service_target(&provider);
        let file = self.directory.join(format!(
            "{}.{}.bin",
            hex::encode(provider.as_bytes()),
            hex::encode(account_user.as_bytes())
        ));
        let entropy = format!("{}\n{provider}\n{account_user}", self.namespace).into_bytes();
        (target, file, entropy)
    }

    pub fn set_secret(
        &self,
        provider: &str,
        account_user: &str,
        secret: &str,
    ) -> Result<String, CredentialError> {
        let (target, file, entropy) = self.slot(provider, account_user);
        let error = |reason: String| CredentialError::Store {
            target: target.clone(),
            reason,
        };
        let sealed = dpapi::protect(secret.as_bytes(), &entropy).map_err(error)?;
        fs::create_dir_all(&self.directory).map_err(|e| error(e.to_string()))?;
        let temporary = file.with_extension(format!("tmp-{}", uuid::Uuid::new_v4().simple()));
        let mut bytes = FILE_MAGIC.to_vec();
        bytes.extend_from_slice(&sealed);
        let written = fs::write(&temporary, &bytes).and_then(|()| fs::rename(&temporary, &file));
        if let Err(e) = written {
            let _ = fs::remove_file(&temporary);
            return Err(error(e.to_string()));
        }
        Ok(self.opaque_reference(provider))
    }

    pub fn get_secret(
        &self,
        provider: &str,
        account_user: &str,
    ) -> Result<String, CredentialError> {
        let (target, file, entropy) = self.slot(provider, account_user);
        let bytes = match fs::read(&file) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(CredentialError::NotFound(target))
            }
            Err(e) => {
                return Err(CredentialError::Store {
                    target,
                    reason: e.to_string(),
                })
            }
        };
        let sealed = bytes
            .strip_prefix(FILE_MAGIC)
            .ok_or_else(|| CredentialError::Store {
                target: target.clone(),
                reason: "unrecognised credential file".into(),
            })?;
        let plain =
            dpapi::unprotect(sealed, &entropy).map_err(|reason| CredentialError::Store {
                target: target.clone(),
                reason,
            })?;
        String::from_utf8(plain).map_err(|_| CredentialError::Store {
            target,
            reason: "credential is not UTF-8".into(),
        })
    }

    pub fn delete_secret(&self, provider: &str, account_user: &str) -> Result<(), CredentialError> {
        let (target, file, _) = self.slot(provider, account_user);
        match fs::remove_file(&file) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(CredentialError::Store {
                target,
                reason: e.to_string(),
            }),
        }
    }
}

#[cfg(windows)]
mod dpapi {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    fn blob(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: bytes.len() as u32,
            pbData: bytes.as_ptr() as *mut u8,
        }
    }

    fn take(output: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        // SAFETY: DPAPI allocated `cbData` bytes at `pbData` with LocalAlloc.
        let bytes =
            unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
        unsafe { LocalFree(output.pbData.cast()) };
        bytes
    }

    pub fn protect(plain: &[u8], entropy: &[u8]) -> Result<Vec<u8>, String> {
        let input = blob(plain);
        let extra = blob(entropy);
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        let ok = unsafe {
            CryptProtectData(
                &input,
                std::ptr::null(),
                &extra,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if ok == 0 {
            return Err(format!(
                "DPAPI encryption failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(take(output))
    }

    pub fn unprotect(sealed: &[u8], entropy: &[u8]) -> Result<Vec<u8>, String> {
        let input = blob(sealed);
        let extra = blob(entropy);
        let mut output = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        let ok = unsafe {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                &extra,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if ok == 0 {
            return Err(
                "DPAPI decryption failed (another user, another machine or a changed file)".into(),
            );
        }
        Ok(take(output))
    }
}

#[cfg(not(windows))]
mod dpapi {
    pub fn protect(_: &[u8], _: &[u8]) -> Result<Vec<u8>, String> {
        Err("credential encryption is unavailable on this platform; nothing was saved".into())
    }

    pub fn unprotect(_: &[u8], _: &[u8]) -> Result<Vec<u8>, String> {
        Err("credential encryption is unavailable on this platform".into())
    }
}

/// Redact any known secret values from diagnostic or log strings (S-04 / AC-17).
pub fn redact_secrets(input: &str, secrets: &[&str]) -> String {
    let mut output = input.to_string();
    for secret in secrets {
        let s = secret.trim();
        if s.len() >= 4 {
            output = output.replace(s, "[REDACTED_SECRET]");
        }
    }
    output
}

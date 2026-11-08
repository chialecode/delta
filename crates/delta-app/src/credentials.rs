use crate::paths::BuildChannel;
use keyring::Entry;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("credential store error for '{target}': {reason}")]
    Keyring { target: String, reason: String },
    #[error("credential not found for '{0}'")]
    NotFound(String),
}

#[derive(Debug, Clone)]
pub struct CredentialStore {
    prefix: String,
}

impl CredentialStore {
    pub fn for_channel(channel: BuildChannel) -> Self {
        Self {
            prefix: channel.credential_prefix().to_string(),
        }
    }

    pub fn with_custom_prefix(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
        }
    }

    pub fn service_target(&self, provider: &str) -> String {
        format!("{}:{}", self.prefix, provider.trim().to_ascii_lowercase())
    }

    pub fn opaque_reference(&self, provider: &str) -> String {
        format!("keyring://{}", self.service_target(provider))
    }

    pub fn set_secret(
        &self,
        provider: &str,
        account_user: &str,
        secret: &str,
    ) -> Result<String, CredentialError> {
        let target = self.service_target(provider);
        let entry = Entry::new(&target, account_user).map_err(|e| CredentialError::Keyring {
            target: target.clone(),
            reason: e.to_string(),
        })?;
        entry
            .set_password(secret)
            .map_err(|e| CredentialError::Keyring {
                target: target.clone(),
                reason: e.to_string(),
            })?;
        Ok(self.opaque_reference(provider))
    }

    pub fn get_secret(
        &self,
        provider: &str,
        account_user: &str,
    ) -> Result<String, CredentialError> {
        let target = self.service_target(provider);
        let entry = Entry::new(&target, account_user).map_err(|e| CredentialError::Keyring {
            target: target.clone(),
            reason: e.to_string(),
        })?;
        match entry.get_password() {
            Ok(pw) => Ok(pw),
            Err(keyring::Error::NoEntry) => Err(CredentialError::NotFound(target)),
            Err(e) => Err(CredentialError::Keyring {
                target,
                reason: e.to_string(),
            }),
        }
    }

    pub fn delete_secret(&self, provider: &str, account_user: &str) -> Result<(), CredentialError> {
        let target = self.service_target(provider);
        let entry = Entry::new(&target, account_user).map_err(|e| CredentialError::Keyring {
            target: target.clone(),
            reason: e.to_string(),
        })?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(CredentialError::Keyring {
                target,
                reason: e.to_string(),
            }),
        }
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

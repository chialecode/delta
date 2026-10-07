//! Unified capability contracts (integration-contracts.md §2).
//!
//! Money/quantity values cross this boundary as decimal strings; times are
//! UTC RFC3339; `None` is distinct from zero. Scopes are host-issued.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

pub const ACTOR_USER: &str = "user";
pub const ACTOR_AI: &str = "ai";
pub const ENVELOPE_SCHEMA_VERSION: &str = "1";

/// Who is calling: UI queries and AI tools use the same services.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    User,
    Ai,
}

impl ActorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ActorKind::User => ACTOR_USER,
            ActorKind::Ai => ACTOR_AI,
        }
    }
}

/// Cancellation handle shared between host and running operations.
/// Backed by a `CancellationToken` so async tasks can await cancellation.
#[derive(Debug, Clone)]
pub struct CancelToken(pub std::sync::Arc<tokio_util::sync::CancellationToken>);

impl Default for CancelToken {
    fn default() -> Self {
        Self(std::sync::Arc::new(
            tokio_util::sync::CancellationToken::new(),
        ))
    }
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.cancel();
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
    /// Resolves when cancelled.
    pub async fn cancelled(&self) {
        self.0.clone().cancelled().await;
    }
}

/// Per-call context: identity, scope reference, run/generation binding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallContext {
    pub request_id: String,
    pub library_id: String,
    pub actor_kind: ActorKind,
    /// Present for AI tool calls; must match the active run instance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    /// Host-issued scope reference; never model-supplied.
    pub scope_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline: Option<DateTime<Utc>>,
}

/// Successful result envelope: value + scope snapshot + evidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultEnvelope<T> {
    pub schema_version: String,
    pub request_id: String,
    pub result_id: String,
    pub value: T,
    pub scope_snapshot: serde_json::Value,
    pub source_versions: Vec<String>,
    pub quality: String,
    #[serde(default)]
    pub missing_inputs: Vec<String>,
    #[serde(default)]
    pub evidence_refs: Vec<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl<T> ResultEnvelope<T> {
    pub fn new(request_id: &str, value: T) -> Self {
        Self {
            schema_version: ENVELOPE_SCHEMA_VERSION.into(),
            request_id: request_id.into(),
            result_id: uuid::Uuid::new_v4().to_string(),
            value,
            scope_snapshot: serde_json::Value::Null,
            source_versions: Vec::new(),
            quality: "complete".into(),
            missing_inputs: Vec::new(),
            evidence_refs: Vec::new(),
            warnings: Vec::new(),
        }
    }
    pub fn with_quality(mut self, q: &str) -> Self {
        self.quality = q.into();
        self
    }
    pub fn with_evidence(mut self, refs: Vec<String>) -> Self {
        self.evidence_refs = refs;
        self
    }
}

/// Error envelope with safe messages only (no secrets, no raw payloads).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    pub schema_version: String,
    pub request_id: String,
    pub code: String,
    pub retryable: bool,
    pub safe_message: String,
    pub details_allowlist: Vec<String>,
    pub correlation_id: String,
}

/// Application error codes (integration-contracts §2).
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("scope denied")]
    ScopeDenied,
    #[error("revision conflict: {0}")]
    RevisionConflict(String),
    #[error("missing input: {0}")]
    MissingInput(String),
    #[error("unsupported capability: {0}")]
    UnsupportedCapability(String),
    #[error("provider unavailable: {0}")]
    ProviderUnavailable(String),
    #[error("rate limited")]
    RateLimited,
    #[error("cancelled")]
    Cancelled,
    /// The authorization the run was started with was revoked (F-09).
    #[error("authorization revoked: {0}")]
    Revoked(String),
    #[error("interrupted")]
    Interrupted,
    #[error("protocol error: {0}")]
    ProtocolError(String),
    #[error("stale generation: run {0} is no longer active")]
    StaleGeneration(String),
    #[error("storage error: {0}")]
    Storage(String),
}

impl AppError {
    pub fn code(&self) -> &'static str {
        match self {
            AppError::InvalidArgument(_) => "INVALID_ARGUMENT",
            AppError::ScopeDenied => "SCOPE_DENIED",
            AppError::RevisionConflict(_) => "REVISION_CONFLICT",
            AppError::MissingInput(_) => "MISSING_INPUT",
            AppError::UnsupportedCapability(_) => "UNSUPPORTED_CAPABILITY",
            AppError::ProviderUnavailable(_) => "PROVIDER_UNAVAILABLE",
            AppError::RateLimited => "RATE_LIMITED",
            AppError::Cancelled => "CANCELLED",
            AppError::Revoked(_) => "REVOKED",
            AppError::Interrupted => "INTERRUPTED",
            AppError::ProtocolError(_) => "PROTOCOL_ERROR",
            AppError::StaleGeneration(_) => "STALE_GENERATION",
            AppError::Storage(_) => "STORAGE_ERROR",
        }
    }
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            AppError::ProviderUnavailable(_) | AppError::RateLimited
        )
    }
    pub fn safe_message(&self) -> String {
        // Message strings are constructed from codes and ids only; secrets and
        // raw provider payloads never enter AppError variants.
        self.to_string()
    }
    pub fn to_envelope(&self, request_id: &str) -> ErrorEnvelope {
        ErrorEnvelope {
            schema_version: ENVELOPE_SCHEMA_VERSION.into(),
            request_id: request_id.into(),
            code: self.code().into(),
            retryable: self.retryable(),
            safe_message: self.safe_message(),
            details_allowlist: Vec::new(),
            correlation_id: uuid::Uuid::new_v4().to_string(),
        }
    }
}

impl From<delta_core::DomainError> for AppError {
    fn from(e: delta_core::DomainError) -> Self {
        match e {
            delta_core::DomainError::InvalidArgument(m) => AppError::InvalidArgument(m),
            delta_core::DomainError::Oversell { .. } => AppError::InvalidArgument(e.to_string()),
            delta_core::DomainError::NumericOverflow(m) => AppError::InvalidArgument(m),
            delta_core::DomainError::MissingInput(m) => AppError::MissingInput(m),
            delta_core::DomainError::Conflict(a, b) => {
                AppError::RevisionConflict(format!("{a}: {b}"))
            }
            delta_core::DomainError::Cancelled => AppError::Cancelled,
        }
    }
}

impl fmt::Display for CallContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CallContext({}, {})", self.request_id, self.scope_ref)
    }
}

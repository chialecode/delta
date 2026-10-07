//! Domain errors. `safe_message` is what may be shown to users or models;
//! raw provider payloads never enter this type.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DomainError {
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("oversell rejected for {instrument}: have {available}, requested {requested}")]
    Oversell {
        instrument: String,
        available: String,
        requested: String,
    },
    #[error("numeric overflow or out-of-range value rejected: {0}")]
    NumericOverflow(String),
    #[error("missing input: {0}")]
    MissingInput(String),
    #[error("event {0} conflicts with ledger state: {1}")]
    Conflict(String, String),
    #[error("cancelled")]
    Cancelled,
}

impl DomainError {
    pub fn code(&self) -> &'static str {
        match self {
            DomainError::InvalidArgument(_) => "INVALID_ARGUMENT",
            DomainError::Oversell { .. } => "INVALID_ARGUMENT",
            DomainError::NumericOverflow(_) => "INVALID_ARGUMENT",
            DomainError::MissingInput(_) => "MISSING_INPUT",
            DomainError::Conflict(_, _) => "REVISION_CONFLICT",
            DomainError::Cancelled => "CANCELLED",
        }
    }
}

pub type DomainResult<T> = Result<T, DomainError>;

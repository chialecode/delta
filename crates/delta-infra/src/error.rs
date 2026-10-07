//! Infrastructure error type: safe messages only (no payloads, no secrets).

#[derive(Debug, thiserror::Error)]
pub enum InfraError {
    #[error("storage error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("migration failed: {0}")]
    Migration(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("csv error: {0}")]
    Csv(#[from] csv::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error("backup verification failed: {0}")]
    BackupVerification(String),
    #[error("{0}")]
    Rejected(String),
}

pub type InfraResult<T> = Result<T, InfraError>;

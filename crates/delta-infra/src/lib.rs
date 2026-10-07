//! DELTA infrastructure: SQLite storage and the narrow model client.

pub mod error;
pub mod host;
pub mod model;
pub mod sqlite;

pub use error::InfraError;
pub use model::DeltaModelClient;

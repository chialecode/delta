//! Stable identifiers. Display symbols (e.g. `AAPL`) are not primary keys.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! typed_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }
            #[allow(dead_code)]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}:{}", $prefix, self.0)
            }
        }
    };
}

typed_id!(LibraryId, "library");
typed_id!(AccountId, "account");
typed_id!(AssetId, "asset");
typed_id!(InstrumentId, "instrument");
typed_id!(EventId, "event");
typed_id!(TransferGroupId, "transfer-group");

impl LibraryId {
    /// Deterministic id used by tests and demo-library creation.
    pub fn demo() -> Self {
        Self("demo".into())
    }
}

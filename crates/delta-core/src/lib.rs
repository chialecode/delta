//! DELTA domain core: events, double-entry postings, FIFO ledger, valuation.
//!
//! This crate is the financial truth owner. It must not depend on UI,
//! database drivers, network or model-transport code. Money and quantities
//! are `rust_decimal::Decimal`; arithmetic uses checked operations so that
//! out-of-range values are rejected instead of silently overflowing.

pub mod error;
pub mod events;
pub mod ids;
pub mod ledger;
pub mod money;
pub mod pnl;
pub mod postings;
pub mod scope;
pub mod valuation;

pub use error::DomainError;
pub use events::{Corrections, EconomicEvent, EventPayload, Fee, TransferKind};
pub use ids::{AccountId, AssetId, EventId, InstrumentId, LibraryId, TransferGroupId};
pub use money::{Amount, Quantity};
pub use scope::{AnalysisScope, ScopeFault, ScopeSnapshot, ScopeView};

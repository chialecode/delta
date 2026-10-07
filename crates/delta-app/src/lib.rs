//! DELTA application services: contracts, queries, and the AI runtime.

pub mod ai;
pub mod contracts;

pub use contracts::{AppError, CallContext, ErrorEnvelope, ResultEnvelope, ACTOR_AI, ACTOR_USER};

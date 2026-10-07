//! AI orchestration: tool gateway, model client port, agent runtime and
//! session state. The model transport lives in delta-infra; this module owns
//! scope, budgets, history and validation (rust-model-client.md §2).

pub mod gateway;
pub mod grants;
pub mod runtime;
pub mod session;

pub use gateway::{tool_catalog, AnalysisHost, ToolGateway, TOOL_NAMES};
pub use grants::{Grants, Revocation, RunLease};
pub use runtime::{
    AgentRuntime, ModelClient, ModelConnectionConfig, ModelError, ModelEvent, ModelRequest,
    Protocol, ProxyPolicy, RunOutcome, RunRequest, StopReason,
};
pub use session::{
    CheckpointRecord, MessageKind, MessageRecord, MessageStatus, NewCheckpoint, NewMessage, NewRun,
    NewToolCall, RunBudget, RunRecord, RunState, SessionStore, ToolCallRecord,
};

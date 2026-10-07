//! Session persistence contract: sessions, runs, messages, tool calls and
//! checkpoints. SQLite implementation lives in delta-infra; the runtime only
//! depends on this trait (context-state-management.md §1).

use crate::contracts::AppError;
use delta_core::ScopeSnapshot;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Created,
    Preparing,
    Running,
    Validating,
    Succeeded,
    Failed,
    Cancelling,
    Cancelled,
    /// Stopped because its authorization (model connection or accounts) was
    /// revoked while it ran (F-09).
    Revoked,
    Interrupted,
}

impl RunState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            RunState::Succeeded
                | RunState::Failed
                | RunState::Cancelled
                | RunState::Revoked
                | RunState::Interrupted
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunBudget {
    pub max_tool_calls: u64,
    pub max_model_requests: u64,
    /// Estimated context tokens before compaction is attempted.
    pub context_window_tokens: u64,
    pub max_duration_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: String,
    pub session_id: String,
    pub generation: u64,
    pub scope_snapshot: ScopeSnapshot,
    pub tool_schema_hash: String,
    pub model_ref: String,
    pub budget: RunBudget,
    pub state: RunState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint_ref: Option<String>,
}

pub struct NewRun {
    pub session_id: String,
    pub generation: u64,
    pub scope_snapshot: ScopeSnapshot,
    pub tool_schema_hash: String,
    pub model_ref: String,
    pub budget: RunBudget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    System,
    User,
    Assistant,
    ToolCall,
    ToolOutput,
    Summary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageStatus {
    Complete,
    Partial,
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRecord {
    pub id: String,
    pub session_id: String,
    pub run_id: String,
    pub sequence: i64,
    pub kind: MessageKind,
    /// Typed payload: text, tool call reference, or protocol opaque item.
    pub payload: serde_json::Value,
    pub status: MessageStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_ref: Option<String>,
}

pub struct NewMessage {
    pub session_id: String,
    pub run_id: String,
    pub kind: MessageKind,
    pub payload: serde_json::Value,
    pub status: MessageStatus,
    pub connection_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub id: String,
    pub run_id: String,
    pub call_id: String,
    pub args_hash: String,
    pub tool_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_ref: Option<String>,
    pub status: String,
}

pub struct NewToolCall {
    pub run_id: String,
    pub call_id: String,
    pub args_hash: String,
    pub tool_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointRecord {
    pub id: String,
    pub session_id: String,
    /// Source message id range covered by the summary.
    pub source_range: String,
    pub source_hash: String,
    pub summary: String,
    pub model_ref: String,
    pub template_version: String,
    pub context_version: String,
}

pub struct NewCheckpoint {
    pub session_id: String,
    pub source_range: String,
    pub source_hash: String,
    pub summary: String,
    pub model_ref: String,
    pub template_version: String,
    pub context_version: String,
}

/// Session/run/message/tool-call/checkpoint repository.
pub trait SessionStore: Send + Sync {
    fn create_session(&self, library_id: &str) -> Result<String, AppError>;
    fn session_exists(&self, session_id: &str) -> Result<bool, AppError>;
    fn create_run(&self, run: &NewRun) -> Result<RunRecord, AppError>;
    fn get_run(&self, run_id: &str) -> Result<Option<RunRecord>, AppError>;
    fn update_run_state(&self, run_id: &str, state: RunState) -> Result<(), AppError>;
    /// Generation of the newest run in a session (0 when none).
    fn latest_generation(&self, session_id: &str) -> Result<u64, AppError>;
    fn append_message(&self, msg: &NewMessage) -> Result<MessageRecord, AppError>;
    /// Messages after the given checkpoint (session order); checkpoint = None
    /// returns the full history.
    fn messages(
        &self,
        session_id: &str,
        after_message_id: Option<&str>,
    ) -> Result<Vec<MessageRecord>, AppError>;
    fn record_tool_call(&self, call: &NewToolCall) -> Result<ToolCallRecord, AppError>;
    fn get_tool_call(
        &self,
        run_id: &str,
        call_id: &str,
    ) -> Result<Option<ToolCallRecord>, AppError>;
    fn complete_tool_call(
        &self,
        run_id: &str,
        call_id: &str,
        result_ref: &str,
        status: &str,
    ) -> Result<(), AppError>;
    fn tool_calls(&self, run_id: &str) -> Result<Vec<ToolCallRecord>, AppError>;
    /// Insert a checkpoint and switch the session's active entry in one
    /// transaction (compaction atomicity); failure keeps the old entry.
    fn insert_checkpoint(&self, cp: &NewCheckpoint) -> Result<CheckpointRecord, AppError>;
    fn active_checkpoint(&self, session_id: &str) -> Result<Option<CheckpointRecord>, AppError>;
    /// Mark non-terminal runs interrupted after a host restart. Returns the
    /// number of runs updated.
    fn mark_interrupted_runs(&self) -> Result<usize, AppError>;
    /// Every run in a session, oldest generation first.
    fn runs(&self, session_id: &str) -> Result<Vec<RunRecord>, AppError>;
    /// Library that owns the session, used when a scope change opens another one.
    fn session_library_id(&self, session_id: &str) -> Result<String, AppError>;
    /// Child session already opened for this parent and economic scope, if any.
    fn find_continuation(
        &self,
        parent_session_id: &str,
        scope_key: &str,
    ) -> Result<Option<String>, AppError>;
    /// Remember that `child` continues `parent` for `scope_key`.
    fn bind_continuation(
        &self,
        parent_session_id: &str,
        scope_key: &str,
        child_session_id: &str,
    ) -> Result<(), AppError>;
    /// Persist the assistant report and the evidence ids it may cite.
    fn save_report(&self, body: &str, scope: &str, refs: &[String]) -> Result<String, AppError>;
}

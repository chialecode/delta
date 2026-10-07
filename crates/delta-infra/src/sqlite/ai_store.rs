//! SQLite implementation of the AI session store (SessionStore trait).
//! Transcript, tool calls, runs and checkpoints live in the same library
//! database and are covered by the same backups (data-and-security.md).

use crate::error::InfraResult;
use crate::sqlite::store::Library;
use delta_app::ai::session::{
    CheckpointRecord, MessageKind, MessageRecord, MessageStatus, NewCheckpoint, NewMessage, NewRun,
    NewToolCall, RunRecord, RunState, SessionStore, ToolCallRecord,
};
use delta_app::contracts::AppError;
use rusqlite::{params, OptionalExtension};

fn app_err(e: impl Into<crate::error::InfraError>) -> AppError {
    AppError::Storage(e.into().to_string())
}

/// A stored value that cannot be decoded is corruption: surface it as a
/// row error instead of substituting a default (data-and-security.md).
fn corrupt(col: usize, what: impl Into<String>) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(col, rusqlite::types::Type::Text, what.into().into())
}

fn json_col<T: serde::de::DeserializeOwned>(
    r: &rusqlite::Row<'_>,
    col: usize,
) -> rusqlite::Result<T> {
    let raw: String = r.get(col)?;
    serde_json::from_str(&raw).map_err(|e| corrupt(col, format!("invalid JSON: {e}")))
}

fn run_state_from(s: &str) -> Option<RunState> {
    Some(match s {
        "created" => RunState::Created,
        "preparing" => RunState::Preparing,
        "running" => RunState::Running,
        "validating" => RunState::Validating,
        "succeeded" => RunState::Succeeded,
        "failed" => RunState::Failed,
        "cancelling" => RunState::Cancelling,
        "cancelled" => RunState::Cancelled,
        "revoked" => RunState::Revoked,
        "interrupted" => RunState::Interrupted,
        _ => return None,
    })
}

fn state_str(s: RunState) -> &'static str {
    match s {
        RunState::Created => "created",
        RunState::Preparing => "preparing",
        RunState::Running => "running",
        RunState::Validating => "validating",
        RunState::Succeeded => "succeeded",
        RunState::Failed => "failed",
        RunState::Cancelling => "cancelling",
        RunState::Cancelled => "cancelled",
        RunState::Revoked => "revoked",
        RunState::Interrupted => "interrupted",
    }
}

impl Library {
    fn row_to_run(r: &rusqlite::Row<'_>) -> rusqlite::Result<RunRecord> {
        Ok(RunRecord {
            id: r.get(0)?,
            session_id: r.get(1)?,
            generation: r.get::<_, i64>(2)? as u64,
            scope_snapshot: json_col(r, 3)?,
            tool_schema_hash: r.get(4)?,
            model_ref: r.get(5)?,
            budget: json_col(r, 6)?,
            state: {
                let raw: String = r.get(7)?;
                run_state_from(&raw)
                    .ok_or_else(|| corrupt(7, format!("unknown run state {raw:?}")))?
            },
            checkpoint_ref: r.get(8)?,
        })
    }
}

impl SessionStore for Library {
    fn create_session(&self, library_id: &str) -> Result<String, AppError> {
        let id = uuid::Uuid::new_v4().to_string();
        self.with(|c| {
            c.execute(
                "INSERT INTO ai_session (id, library_id, created_at) VALUES (?1, ?2, ?3)",
                params![id, library_id, chrono::Utc::now().to_rfc3339()],
            )
        })
        .map_err(app_err)?;
        Ok(id)
    }

    fn session_exists(&self, session_id: &str) -> Result<bool, AppError> {
        self.with(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM ai_session WHERE id = ?1",
                params![session_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()
        })
        .map_err(app_err)
        .map(|n| n.unwrap_or(0) > 0)
    }

    fn create_run(&self, run: &NewRun) -> Result<RunRecord, AppError> {
        let id = uuid::Uuid::new_v4().to_string();
        let budget =
            serde_json::to_string(&run.budget).map_err(|e| AppError::Storage(e.to_string()))?;
        let scope = serde_json::to_string(&run.scope_snapshot)
            .map_err(|e| AppError::Storage(e.to_string()))?;
        self.with(|c| {
            c.execute(
                "INSERT INTO ai_run (id, session_id, generation, scope_snapshot, tool_schema_hash, model_ref, budget, state, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'created', ?8)",
                params![
                    id,
                    run.session_id,
                    run.generation as i64,
                    scope,
                    run.tool_schema_hash,
                    run.model_ref,
                    budget,
                    chrono::Utc::now().to_rfc3339()
                ],
            )
        })
        .map_err(app_err)?;
        Ok(RunRecord {
            id,
            session_id: run.session_id.clone(),
            generation: run.generation,
            scope_snapshot: run.scope_snapshot.clone(),
            tool_schema_hash: run.tool_schema_hash.clone(),
            model_ref: run.model_ref.clone(),
            budget: run.budget.clone(),
            state: RunState::Created,
            checkpoint_ref: None,
        })
    }

    fn get_run(&self, run_id: &str) -> Result<Option<RunRecord>, AppError> {
        self.with(|c| {
            c.query_row(
                "SELECT id, session_id, generation, scope_snapshot, tool_schema_hash, model_ref, budget, state, checkpoint_ref FROM ai_run WHERE id = ?1",
                params![run_id],
                Self::row_to_run,
            )
            .optional()
        })
        .map_err(app_err)
    }

    fn update_run_state(&self, run_id: &str, state: RunState) -> Result<(), AppError> {
        self.with(|c| {
            c.execute(
                "UPDATE ai_run SET state = ?2 WHERE id = ?1",
                params![run_id, state_str(state)],
            )
        })
        .map_err(app_err)?;
        Ok(())
    }

    fn latest_generation(&self, session_id: &str) -> Result<u64, AppError> {
        self.with(|c| {
            c.query_row(
                "SELECT COALESCE(MAX(generation), 0) FROM ai_run WHERE session_id = ?1",
                params![session_id],
                |r| r.get::<_, i64>(0),
            )
        })
        .map_err(app_err)
        .map(|v| v as u64)
    }

    fn append_message(&self, msg: &NewMessage) -> Result<MessageRecord, AppError> {
        let id = uuid::Uuid::new_v4().to_string();
        let payload =
            serde_json::to_string(&msg.payload).map_err(|e| AppError::Storage(e.to_string()))?;
        let kind = match msg.kind {
            MessageKind::System => "system",
            MessageKind::User => "user",
            MessageKind::Assistant => "assistant",
            MessageKind::ToolCall => "tool_call",
            MessageKind::ToolOutput => "tool_output",
            MessageKind::Summary => "summary",
        };
        let status = match msg.status {
            MessageStatus::Complete => "complete",
            MessageStatus::Partial => "partial",
            MessageStatus::Interrupted => "interrupted",
        };
        let sequence = self
            .with(|c| {
                c.query_row(
                    "SELECT COALESCE(MAX(sequence), -1) + 1 FROM ai_message WHERE session_id = ?1",
                    params![msg.session_id],
                    |r| r.get::<_, i64>(0),
                )
            })
            .map_err(app_err)?;
        self.with(|c| {
            c.execute(
                "INSERT INTO ai_message (id, session_id, run_id, sequence, kind, payload, status, connection_ref, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    id,
                    msg.session_id,
                    msg.run_id,
                    sequence,
                    kind,
                    payload,
                    status,
                    msg.connection_ref,
                    chrono::Utc::now().to_rfc3339()
                ],
            )
        })
        .map_err(app_err)?;
        Ok(MessageRecord {
            id,
            session_id: msg.session_id.clone(),
            run_id: msg.run_id.clone(),
            sequence,
            kind: msg.kind,
            payload: msg.payload.clone(),
            status: msg.status,
            connection_ref: msg.connection_ref.clone(),
        })
    }

    fn messages(
        &self,
        session_id: &str,
        after_message_id: Option<&str>,
    ) -> Result<Vec<MessageRecord>, AppError> {
        // after_message_id is a checkpoint source_range: messages with a
        // sequence strictly greater than that message's sequence.
        let start_seq: i64 = match after_message_id {
            Some(id) => self
                .with(|c| {
                    c.query_row(
                        "SELECT sequence FROM ai_message WHERE id = ?1 AND session_id = ?2",
                        params![id, session_id],
                        |r| r.get::<_, i64>(0),
                    )
                    .optional()
                })
                .map_err(app_err)?
                .unwrap_or(-1),
            None => -1,
        };
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, session_id, run_id, sequence, kind, payload, status, connection_ref
                 FROM ai_message WHERE session_id = ?1 AND sequence > ?2 ORDER BY sequence",
            )?;
            let rows = stmt.query_map(params![session_id, start_seq], |r| {
                let kind: String = r.get(4)?;
                let status: String = r.get(6)?;
                Ok(MessageRecord {
                    id: r.get(0)?,
                    session_id: r.get(1)?,
                    run_id: r.get(2)?,
                    sequence: r.get(3)?,
                    kind: match kind.as_str() {
                        "system" => MessageKind::System,
                        "user" => MessageKind::User,
                        "assistant" => MessageKind::Assistant,
                        "tool_call" => MessageKind::ToolCall,
                        "tool_output" => MessageKind::ToolOutput,
                        "summary" => MessageKind::Summary,
                        other => return Err(corrupt(4, format!("unknown message kind {other:?}"))),
                    },
                    payload: json_col(r, 5)?,
                    status: match status.as_str() {
                        "complete" => MessageStatus::Complete,
                        "partial" => MessageStatus::Partial,
                        "interrupted" => MessageStatus::Interrupted,
                        other => {
                            return Err(corrupt(6, format!("unknown message status {other:?}")))
                        }
                    },
                    connection_ref: r.get(7)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<MessageRecord>>>()
        })
        .map_err(app_err)
    }

    fn record_tool_call(&self, call: &NewToolCall) -> Result<ToolCallRecord, AppError> {
        let id = uuid::Uuid::new_v4().to_string();
        self.with(|c| {
            c.execute(
                "INSERT INTO ai_tool_call (id, run_id, call_id, args_hash, tool_name, status) VALUES (?1, ?2, ?3, ?4, ?5, 'pending')",
                params![id, call.run_id, call.call_id, call.args_hash, call.tool_name],
            )
        })
        .map_err(app_err)?;
        Ok(ToolCallRecord {
            id,
            run_id: call.run_id.clone(),
            call_id: call.call_id.clone(),
            args_hash: call.args_hash.clone(),
            tool_name: call.tool_name.clone(),
            result_ref: None,
            status: "pending".into(),
        })
    }

    fn get_tool_call(
        &self,
        run_id: &str,
        call_id: &str,
    ) -> Result<Option<ToolCallRecord>, AppError> {
        self.with(|c| {
            c.query_row(
                "SELECT id, run_id, call_id, args_hash, tool_name, result_ref, status FROM ai_tool_call WHERE run_id = ?1 AND call_id = ?2",
                params![run_id, call_id],
                |r| {
                    Ok(ToolCallRecord {
                        id: r.get(0)?,
                        run_id: r.get(1)?,
                        call_id: r.get(2)?,
                        args_hash: r.get(3)?,
                        tool_name: r.get(4)?,
                        result_ref: r.get(5)?,
                        status: r.get(6)?,
                    })
                },
            )
            .optional()
        })
        .map_err(app_err)
    }

    fn complete_tool_call(
        &self,
        run_id: &str,
        call_id: &str,
        result_ref: &str,
        status: &str,
    ) -> Result<(), AppError> {
        self.with(|c| {
            c.execute(
                "UPDATE ai_tool_call SET result_ref = ?3, status = ?4 WHERE run_id = ?1 AND call_id = ?2",
                params![run_id, call_id, result_ref, status],
            )
        })
        .map_err(app_err)?;
        Ok(())
    }

    fn tool_calls(&self, run_id: &str) -> Result<Vec<ToolCallRecord>, AppError> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, run_id, call_id, args_hash, tool_name, result_ref, status FROM ai_tool_call WHERE run_id = ?1 ORDER BY rowid",
            )?;
            let rows = stmt.query_map(params![run_id], |r| {
                Ok(ToolCallRecord {
                    id: r.get(0)?,
                    run_id: r.get(1)?,
                    call_id: r.get(2)?,
                    args_hash: r.get(3)?,
                    tool_name: r.get(4)?,
                    result_ref: r.get(5)?,
                    status: r.get(6)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<ToolCallRecord>>>()
        })
        .map_err(app_err)
    }

    fn insert_checkpoint(&self, cp: &NewCheckpoint) -> Result<CheckpointRecord, AppError> {
        if self
            .fail_next_checkpoint
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(AppError::Storage(
                "checkpoint write failed; the previous checkpoint stays active".into(),
            ));
        }
        let id = uuid::Uuid::new_v4().to_string();
        // One transaction: insert + switch active entry. Failure keeps the
        // previous checkpoint active (context-state-management §4).
        self.with(|c| {
            let tx = c.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO ai_checkpoint (id, session_id, source_range, source_hash, summary, model_ref, template_version, context_version, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    id,
                    cp.session_id,
                    cp.source_range,
                    cp.source_hash,
                    cp.summary,
                    cp.model_ref,
                    cp.template_version,
                    cp.context_version,
                    chrono::Utc::now().to_rfc3339()
                ],
            )?;
            tx.execute(
                "UPDATE ai_session SET active_checkpoint_id = ?2 WHERE id = ?1",
                params![cp.session_id, id],
            )?;
            tx.commit()?;
            Ok::<(), rusqlite::Error>(())
        })
        .map_err(app_err)?;
        Ok(CheckpointRecord {
            id,
            session_id: cp.session_id.clone(),
            source_range: cp.source_range.clone(),
            source_hash: cp.source_hash.clone(),
            summary: cp.summary.clone(),
            model_ref: cp.model_ref.clone(),
            template_version: cp.template_version.clone(),
            context_version: cp.context_version.clone(),
        })
    }

    fn active_checkpoint(&self, session_id: &str) -> Result<Option<CheckpointRecord>, AppError> {
        self.with(|c| {
            c.query_row(
                "SELECT cp.id, cp.session_id, cp.source_range, cp.source_hash, cp.summary, cp.model_ref, cp.template_version, cp.context_version
                 FROM ai_checkpoint cp JOIN ai_session s ON s.active_checkpoint_id = cp.id
                 WHERE cp.session_id = ?1",
                params![session_id],
                |r| {
                    Ok(CheckpointRecord {
                        id: r.get(0)?,
                        session_id: r.get(1)?,
                        source_range: r.get(2)?,
                        source_hash: r.get(3)?,
                        summary: r.get(4)?,
                        model_ref: r.get(5)?,
                        template_version: r.get(6)?,
                        context_version: r.get(7)?,
                    })
                },
            )
            .optional()
        })
        .map_err(app_err)
    }

    fn mark_interrupted_runs(&self) -> Result<usize, AppError> {
        self.with(|c| {
            c.execute(
                "UPDATE ai_run SET state = 'interrupted' WHERE state NOT IN ('succeeded','failed','cancelled','revoked','interrupted')",
                [],
            )
        })
        .map_err(app_err)
    }

    fn runs(&self, session_id: &str) -> Result<Vec<RunRecord>, AppError> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, session_id, generation, scope_snapshot, tool_schema_hash, model_ref, budget, state, checkpoint_ref
                 FROM ai_run WHERE session_id = ?1 ORDER BY generation",
            )?;
            let rows = stmt.query_map(params![session_id], Self::row_to_run)?;
            rows.collect::<rusqlite::Result<Vec<RunRecord>>>()
        })
        .map_err(app_err)
    }

    fn session_library_id(&self, session_id: &str) -> Result<String, AppError> {
        self.with(|c| {
            c.query_row(
                "SELECT library_id FROM ai_session WHERE id = ?1",
                params![session_id],
                |r| r.get(0),
            )
        })
        .map_err(|_| AppError::Storage(format!("session {session_id} not found")))
    }

    fn find_continuation(
        &self,
        parent_session_id: &str,
        scope_key: &str,
    ) -> Result<Option<String>, AppError> {
        self.with(|c| {
            c.query_row(
                "SELECT session_id FROM ai_scope_continuation WHERE parent_session_id = ?1 AND scope_key = ?2",
                params![parent_session_id, scope_key],
                |r| r.get(0),
            )
            .optional()
        })
        .map_err(app_err)
    }

    fn bind_continuation(
        &self,
        parent_session_id: &str,
        scope_key: &str,
        child_session_id: &str,
    ) -> Result<(), AppError> {
        self.with(|c| {
            c.execute(
                "INSERT INTO ai_scope_continuation (parent_session_id, scope_key, session_id) VALUES (?1, ?2, ?3)",
                params![parent_session_id, scope_key, child_session_id],
            )
        })
        .map_err(app_err)?;
        Ok(())
    }

    fn save_report(&self, body: &str, scope: &str, refs: &[String]) -> Result<String, AppError> {
        self.save_report_evidence(body, scope, refs)
            .map_err(app_err)
    }
}

/// Persisted transcript integrity helper used by backup tests.
pub fn count_messages(library: &Library, session_id: &str) -> InfraResult<i64> {
    library.with(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM ai_message WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )
        .map_err(Into::into)
    })
}

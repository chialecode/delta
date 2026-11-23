use chrono::Utc;
use rusqlite::{params, OptionalExtension};

use crate::m2_schema::{
    AiCheckpointRecord, AiMessageRecord, AiRunRecord, AiSessionRecord, AiToolCallRecord,
    ModelConnectionRecord, FAIL_NEXT_AI_COMPACTION,
};
use crate::storage::{SqliteStore, StorageError};

fn map_model_connection(r: &rusqlite::Row<'_>) -> rusqlite::Result<ModelConnectionRecord> {
    Ok(ModelConnectionRecord {
        id: r.get(0)?,
        protocol: r.get(1)?,
        base_url: r.get(2)?,
        model_id: r.get(3)?,
        credential_ref: r.get(4)?,
        context_window: r.get(5)?,
        capabilities_json: r.get(6)?,
        timeouts_json: r.get(7)?,
        budget_json: r.get(8)?,
        updated_at: r.get(9)?,
    })
}

impl SqliteStore {
    pub fn upsert_model_connection(
        &mut self,
        conn: &ModelConnectionRecord,
    ) -> Result<(), StorageError> {
        self.conn().execute(
            "INSERT INTO model_connection (
                id, protocol, base_url, model_id, credential_ref,
                context_window, capabilities_json, timeouts_json, budget_json, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
                protocol = excluded.protocol,
                base_url = excluded.base_url,
                model_id = excluded.model_id,
                credential_ref = excluded.credential_ref,
                context_window = excluded.context_window,
                capabilities_json = excluded.capabilities_json,
                timeouts_json = excluded.timeouts_json,
                budget_json = excluded.budget_json,
                updated_at = excluded.updated_at",
            params![
                conn.id,
                conn.protocol,
                conn.base_url,
                conn.model_id,
                conn.credential_ref,
                conn.context_window,
                conn.capabilities_json,
                conn.timeouts_json,
                conn.budget_json,
                conn.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_model_connection(&self, id: &str) -> Result<ModelConnectionRecord, StorageError> {
        self.conn()
            .prepare_cached(
                "SELECT id, protocol, base_url, model_id, credential_ref,
                        context_window, capabilities_json, timeouts_json, budget_json, updated_at
                 FROM model_connection WHERE id = ?1",
            )?
            .query_row(params![id], map_model_connection)
            .map_err(StorageError::from)
    }

    pub fn list_model_connections(&self) -> Result<Vec<ModelConnectionRecord>, StorageError> {
        let mut stmt = self.conn().prepare_cached(
            "SELECT id, protocol, base_url, model_id, credential_ref,
                    context_window, capabilities_json, timeouts_json, budget_json, updated_at
             FROM model_connection ORDER BY updated_at DESC, id ASC",
        )?;
        let rows = stmt.query_map([], map_model_connection)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    pub fn create_ai_session(&mut self, session_id: &str) -> Result<AiSessionRecord, StorageError> {
        let lib_id: String = self
            .conn()
            .query_row("SELECT id FROM library LIMIT 1", [], |r| r.get(0))?;
        let now = Utc::now().to_rfc3339();
        self.conn().execute(
            "INSERT INTO ai_session (id, library_id, schema_version, active_checkpoint_id, status, created_at, updated_at)
             VALUES (?1, ?2, 1, NULL, 'active', ?3, ?3)
             ON CONFLICT(id) DO UPDATE SET updated_at = excluded.updated_at",
            params![session_id, lib_id, now],
        )?;
        self.get_ai_session(session_id)
    }

    pub fn get_ai_session(&self, session_id: &str) -> Result<AiSessionRecord, StorageError> {
        self.conn()
            .prepare_cached(
                "SELECT id, library_id, schema_version, active_checkpoint_id, status, created_at, updated_at
                 FROM ai_session WHERE id = ?1",
            )?
            .query_row(params![session_id], |r| {
                Ok(AiSessionRecord {
                    id: r.get(0)?,
                    library_id: r.get(1)?,
                    schema_version: r.get(2)?,
                    active_checkpoint_id: r.get(3)?,
                    status: r.get(4)?,
                    created_at: r.get(5)?,
                    updated_at: r.get(6)?,
                })
            })
            .map_err(StorageError::from)
    }

    pub fn create_ai_run(&mut self, run: &AiRunRecord) -> Result<(), StorageError> {
        self.conn().execute(
            "INSERT INTO ai_run (
                id, session_id, generation, scope_json, dataset_version, ledger_revision,
                tool_schema_hash, model_connection_id, budget_json, state,
                checkpoint_ref, report_json, usage_json, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                run.id,
                run.session_id,
                run.generation,
                run.scope_json,
                run.dataset_version,
                run.ledger_revision,
                run.tool_schema_hash,
                run.model_connection_id,
                run.budget_json,
                run.state,
                run.checkpoint_ref,
                run.report_json,
                run.usage_json,
                run.created_at,
                run.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn update_ai_run_state(
        &mut self,
        run_id: &str,
        state: &str,
        report_json: Option<&str>,
        usage_json: Option<&str>,
    ) -> Result<(), StorageError> {
        let now = Utc::now().to_rfc3339();
        self.conn().execute(
            "UPDATE ai_run
             SET state = ?2,
                 report_json = COALESCE(?3, report_json),
                 usage_json = COALESCE(?4, usage_json),
                 updated_at = ?5
             WHERE id = ?1",
            params![run_id, state, report_json, usage_json, now],
        )?;
        Ok(())
    }

    pub fn get_ai_run(&self, run_id: &str) -> Result<AiRunRecord, StorageError> {
        self.conn()
            .prepare_cached(
                "SELECT id, session_id, generation, scope_json, dataset_version, ledger_revision,
                        tool_schema_hash, model_connection_id, budget_json, state,
                        checkpoint_ref, report_json, usage_json, created_at, updated_at
                 FROM ai_run WHERE id = ?1",
            )?
            .query_row(params![run_id], |r| {
                Ok(AiRunRecord {
                    id: r.get(0)?,
                    session_id: r.get(1)?,
                    generation: r.get(2)?,
                    scope_json: r.get(3)?,
                    dataset_version: r.get(4)?,
                    ledger_revision: r.get(5)?,
                    tool_schema_hash: r.get(6)?,
                    model_connection_id: r.get(7)?,
                    budget_json: r.get(8)?,
                    state: r.get(9)?,
                    checkpoint_ref: r.get(10)?,
                    report_json: r.get(11)?,
                    usage_json: r.get(12)?,
                    created_at: r.get(13)?,
                    updated_at: r.get(14)?,
                })
            })
            .map_err(StorageError::from)
    }

    pub fn append_ai_message(&mut self, msg: &AiMessageRecord) -> Result<(), StorageError> {
        self.conn().execute(
            "INSERT INTO ai_message (id, session_id, run_id, turn_id, sequence, kind, payload_json, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                msg.id,
                msg.session_id,
                msg.run_id,
                msg.turn_id,
                msg.sequence,
                msg.kind,
                msg.payload_json,
                msg.status,
                msg.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn list_ai_messages(&self, session_id: &str) -> Result<Vec<AiMessageRecord>, StorageError> {
        let mut stmt = self.conn().prepare_cached(
            "SELECT id, session_id, run_id, turn_id, sequence, kind, payload_json, status, created_at
             FROM ai_message WHERE session_id = ?1 ORDER BY sequence ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |r| {
            Ok(AiMessageRecord {
                id: r.get(0)?,
                session_id: r.get(1)?,
                run_id: r.get(2)?,
                turn_id: r.get(3)?,
                sequence: r.get(4)?,
                kind: r.get(5)?,
                payload_json: r.get(6)?,
                status: r.get(7)?,
                created_at: r.get(8)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    /// Record an AI tool call with idempotent `(run_id, call_id)` semantics (`S-21`):
    /// - If `(run_id, call_id)` already exists with the same `args_hash`, returns existing `result_json`.
    /// - If `(run_id, call_id)` exists with a different `args_hash`, returns `Err(StorageError::IntegrityFailed)`.
    pub fn record_ai_tool_call(
        &mut self,
        tc: &AiToolCallRecord,
    ) -> Result<Option<String>, StorageError> {
        let tx = self.conn_mut().savepoint()?;
        let existing: Option<(String, Option<String>)> = tx
            .query_row(
                "SELECT args_hash, result_json FROM ai_tool_call WHERE run_id = ?1 AND call_id = ?2",
                params![tc.run_id, tc.call_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        if let Some((existing_hash, existing_res)) = existing {
            if existing_hash != tc.args_hash {
                return Err(StorageError::IntegrityFailed(
                    "duplicate tool call_id with mismatched arguments".into(),
                ));
            }
            return Ok(existing_res);
        }

        tx.execute(
            "INSERT INTO ai_tool_call (id, run_id, call_id, args_hash, tool_name, args_json, result_json, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                tc.id,
                tc.run_id,
                tc.call_id,
                tc.args_hash,
                tc.tool_name,
                tc.args_json,
                tc.result_json,
                tc.status,
                tc.created_at,
            ],
        )?;
        tx.commit()?;
        Ok(tc.result_json.clone())
    }

    /// Atomically insert an `ai_checkpoint` and switch `ai_session.active_checkpoint_id` (`S-21`, `M2-A-15`).
    /// Fault injection via `FAIL_NEXT_AI_COMPACTION` aborts the savepoint and preserves the previous pointer.
    pub fn commit_ai_checkpoint_atomic(
        &mut self,
        cp: &AiCheckpointRecord,
    ) -> Result<(), StorageError> {
        let tx = self.conn_mut().savepoint()?;
        tx.execute(
            "INSERT INTO ai_checkpoint (
                id, session_id, source_start_seq, source_end_seq,
                source_hash, summary_text, model_ref, template_version, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                cp.id,
                cp.session_id,
                cp.source_start_seq,
                cp.source_end_seq,
                cp.source_hash,
                cp.summary_text,
                cp.model_ref,
                cp.template_version,
                cp.created_at,
            ],
        )?;

        if FAIL_NEXT_AI_COMPACTION.with(|flag| flag.replace(false)) {
            return Err(StorageError::IntegrityFailed(
                "injected AI checkpoint compaction failure before pointer switch".into(),
            ));
        }

        let affected = tx.execute(
            "UPDATE ai_session SET active_checkpoint_id = ?2, updated_at = ?3 WHERE id = ?1",
            params![cp.session_id, cp.id, cp.created_at],
        )?;
        if affected == 0 {
            return Err(StorageError::IntegrityFailed(format!(
                "ai_session '{}' not found for checkpoint commit",
                cp.session_id
            )));
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_ai_checkpoint(
        &self,
        checkpoint_id: &str,
    ) -> Result<AiCheckpointRecord, StorageError> {
        self.conn()
            .prepare_cached(
                "SELECT id, session_id, source_start_seq, source_end_seq,
                        source_hash, summary_text, model_ref, template_version, created_at
                 FROM ai_checkpoint WHERE id = ?1",
            )?
            .query_row(params![checkpoint_id], |r| {
                Ok(AiCheckpointRecord {
                    id: r.get(0)?,
                    session_id: r.get(1)?,
                    source_start_seq: r.get(2)?,
                    source_end_seq: r.get(3)?,
                    source_hash: r.get(4)?,
                    summary_text: r.get(5)?,
                    model_ref: r.get(6)?,
                    template_version: r.get(7)?,
                    created_at: r.get(8)?,
                })
            })
            .map_err(StorageError::from)
    }

    /// Mark any unfinished `running` or `queued` AI runs and `streaming` messages as `interrupted`
    /// on library startup (`S-21`).
    pub fn mark_interrupted_runs_on_startup(&mut self) -> Result<usize, StorageError> {
        let now = Utc::now().to_rfc3339();
        let tx = self.conn_mut().savepoint()?;
        let affected_runs = tx.execute(
            "UPDATE ai_run SET state = 'interrupted', updated_at = ?1
             WHERE state IN ('queued', 'running', 'streaming', 'calling_tool')",
            params![now],
        )?;
        tx.execute(
            "UPDATE ai_message SET status = 'interrupted'
             WHERE status IN ('streaming', 'pending')",
            [],
        )?;
        tx.commit()?;
        Ok(affected_runs)
    }
}

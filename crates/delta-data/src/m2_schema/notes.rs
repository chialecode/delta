use std::collections::BTreeSet;

use chrono::{NaiveDate, Utc};
use delta_core::{parse_decimal_lossless, to_canonical_text, AdjustmentKind};
use rusqlite::{params, Connection, OptionalExtension};
use rust_decimal::Decimal;
use sha2::{Digest, Sha256};

use crate::m2_schema::{
    AttachmentDto, ChartContextDto, NoteDto, NoteLinkDto, StructuredNoteFields, TradePlanDto,
    FAIL_NEXT_NOTE_SAVE,
};
use crate::storage::{write_bytes_atomic, SqliteStore, StorageError};

fn escape_sql_like(raw: &str) -> String {
    format!(
        "%{}%",
        raw.replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    )
}

pub(crate) fn save_note_revision_in(
    tx: &Connection,
    note_id: &str,
    title: &str,
    template_kind: &str,
    allow_model_access: bool,
    body_markdown: &str,
    structured: &StructuredNoteFields,
    tags: &[String],
) -> Result<i64, StorageError> {
    if FAIL_NEXT_NOTE_SAVE.with(|flag| flag.replace(false)) {
        return Err(StorageError::Io(std::io::Error::other(
            "injected note save disk failure",
        )));
    }

    let now = Utc::now().to_rfc3339();
    let structured_json = serde_json::to_string(structured)?;

    let prev_rev: i64 = tx.query_row(
        "SELECT COALESCE(MAX(revision), 0) FROM note_revision WHERE note_id = ?1",
        params![note_id],
        |r| r.get(0),
    )?;
    let next_rev = prev_rev + 1;

    tx.execute(
        "INSERT INTO note (id, title, current_revision, allow_model_access, template_kind, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
         ON CONFLICT(id) DO UPDATE SET
            title = excluded.title,
            current_revision = excluded.current_revision,
            allow_model_access = excluded.allow_model_access,
            template_kind = excluded.template_kind,
            updated_at = excluded.updated_at",
        params![
            note_id,
            title,
            next_rev,
            i32::from(allow_model_access),
            template_kind,
            now,
        ],
    )?;

    let rev_id = format!("{note_id}:R{next_rev}");
    tx.execute(
        "INSERT INTO note_revision (id, note_id, revision, body_markdown, structured_json, saved_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![rev_id, note_id, next_rev, body_markdown, structured_json, now],
    )?;

    tx.execute("DELETE FROM note_tag WHERE note_id = ?1", params![note_id])?;
    // Case-insensitive dedup matches canonical `TAG:<lower>` primary keys cleanly.
    let mut seen_lower = BTreeSet::new();
    let mut dedup_tags = Vec::new();
    for raw in tags {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if seen_lower.insert(trimmed.to_ascii_lowercase()) {
            dedup_tags.push(trimmed.to_string());
        }
    }
    dedup_tags.sort();
    {
        let mut ins_tag =
            tx.prepare_cached("INSERT INTO tag (id, name) VALUES (?1, ?2) ON CONFLICT DO NOTHING")?;
        let mut find_tag =
            tx.prepare_cached("SELECT id FROM tag WHERE id = ?1 OR name = ?2 LIMIT 1")?;
        let mut ins_nt =
            tx.prepare_cached("INSERT OR IGNORE INTO note_tag (note_id, tag_id) VALUES (?1, ?2)")?;
        for tag_name in &dedup_tags {
            let tag_id = format!("TAG:{}", tag_name.to_ascii_lowercase());
            ins_tag.execute(params![tag_id, tag_name])?;
            let actual_tag_id: String =
                find_tag.query_row(params![tag_id, tag_name], |r| r.get(0))?;
            ins_nt.execute(params![note_id, actual_tag_id])?;
        }
    }

    let tags_text = dedup_tags.join(" ");
    tx.execute("DELETE FROM note_fts WHERE note_id = ?1", params![note_id])?;
    tx.execute(
        "INSERT INTO note_fts (note_id, title, body_markdown, tags_text)
         VALUES (?1, ?2, ?3, ?4)",
        params![note_id, title, body_markdown, tags_text],
    )?;
    Ok(next_rev)
}

pub(crate) fn add_note_link_in(
    tx: &Connection,
    note_id: &str,
    target_type: &str,
    target_id: &str,
    target_revision: i64,
    allocated_quantity: Option<Decimal>,
    relation: &str,
) -> Result<String, StorageError> {
    let id = format!("NL:{note_id}:{target_type}:{target_id}");
    tx.execute(
        "INSERT OR REPLACE INTO note_link (id, note_id, target_type, target_id, target_revision, allocated_quantity, relation)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            note_id,
            target_type,
            target_id,
            target_revision,
            allocated_quantity.map(to_canonical_text),
            relation,
        ],
    )?;
    Ok(id)
}

impl SqliteStore {
    pub fn save_chart_context(&mut self, ctx: &ChartContextDto) -> Result<String, StorageError> {
        let now = Utc::now().to_rfc3339();
        self.conn().execute(
            "INSERT INTO chart_context (
                id, instrument_id, venue, timeframe, visible_start_date, visible_end_date,
                anchor_date, adjustment, indicators_json, dataset_version, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(id) DO UPDATE SET
                instrument_id = excluded.instrument_id,
                venue = excluded.venue,
                timeframe = excluded.timeframe,
                visible_start_date = excluded.visible_start_date,
                visible_end_date = excluded.visible_end_date,
                anchor_date = excluded.anchor_date,
                adjustment = excluded.adjustment,
                indicators_json = excluded.indicators_json,
                dataset_version = excluded.dataset_version",
            params![
                ctx.id,
                ctx.instrument_id,
                ctx.venue,
                ctx.timeframe,
                ctx.visible_start_date.to_string(),
                ctx.visible_end_date.to_string(),
                ctx.anchor_date.map(|d| d.to_string()),
                ctx.adjustment.as_str(),
                ctx.indicators_json,
                ctx.dataset_version,
                now,
            ],
        )?;
        Ok(ctx.id.clone())
    }

    pub fn get_chart_context(&self, id: &str) -> Result<ChartContextDto, StorageError> {
        let latest_dsv: i64 = self.conn().query_row(
            "SELECT COALESCE(MAX(version_num), 0) FROM dataset_version",
            [],
            |r| r.get(0),
        )?;
        let row = self.conn().query_row(
            "SELECT instrument_id, venue, timeframe, visible_start_date, visible_end_date,
                    anchor_date, adjustment, indicators_json, dataset_version
             FROM chart_context WHERE id = ?1",
            params![id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                ))
            },
        )?;
        let (
            instrument_id,
            venue,
            timeframe,
            start_s,
            end_s,
            anchor_s,
            adj_s,
            indicators_json,
            dataset_version,
        ) = row;
        let visible_start_date = NaiveDate::parse_from_str(&start_s, "%Y-%m-%d").map_err(|e| {
            StorageError::IntegrityFailed(format!("invalid chart_context start_date: {e}"))
        })?;
        let visible_end_date = NaiveDate::parse_from_str(&end_s, "%Y-%m-%d").map_err(|e| {
            StorageError::IntegrityFailed(format!("invalid chart_context end_date: {e}"))
        })?;
        let anchor_date = match anchor_s {
            Some(s) => Some(NaiveDate::parse_from_str(&s, "%Y-%m-%d").map_err(|e| {
                StorageError::IntegrityFailed(format!("invalid chart_context anchor_date: {e}"))
            })?),
            None => None,
        };
        let adjustment = AdjustmentKind::parse(&adj_s).ok_or_else(|| {
            StorageError::IntegrityFailed(format!("invalid chart_context adjustment '{adj_s}'"))
        })?;
        let has_newer_dataset_version = dataset_version.is_some_and(|v| latest_dsv > v);

        Ok(ChartContextDto {
            id: id.to_string(),
            instrument_id,
            venue,
            timeframe,
            visible_start_date,
            visible_end_date,
            anchor_date,
            adjustment,
            indicators_json,
            dataset_version,
            latest_dataset_version: latest_dsv,
            has_newer_dataset_version,
        })
    }

    pub fn upsert_trade_plan(&mut self, plan: &TradePlanDto) -> Result<(), StorageError> {
        let now = Utc::now().to_rfc3339();
        self.conn().execute(
            "INSERT INTO trade_plan (id, title, instrument_id, thesis, entry_rule, invalidation, risk_budget, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET
                title = excluded.title,
                instrument_id = excluded.instrument_id,
                thesis = excluded.thesis,
                entry_rule = excluded.entry_rule,
                invalidation = excluded.invalidation,
                risk_budget = excluded.risk_budget,
                status = excluded.status",
            params![
                plan.id,
                plan.title,
                plan.instrument_id,
                plan.thesis,
                plan.entry_rule,
                plan.invalidation,
                plan.risk_budget,
                plan.status,
                now,
            ],
        )?;
        Ok(())
    }

    /// Link a fill (`economic_event`) to a `trade_plan` and/or `note` while enforcing that total
    /// `allocated_quantity` across all revisions in the event's `correction_group_id` does not
    /// exceed the fill's quantity (`M2-A-11`).
    pub fn link_plan_or_note_fill(
        &mut self,
        plan_id: Option<&str>,
        note_id: Option<&str>,
        event_id: &str,
        allocated_quantity: Decimal,
        relation: &str,
    ) -> Result<(), StorageError> {
        if allocated_quantity <= Decimal::ZERO {
            return Err(StorageError::IntegrityFailed(
                "allocated quantity must be positive".into(),
            ));
        }

        let tx = self.conn_mut().savepoint()?;
        let fill_row: Option<(String, String, i64, String)> = tx
            .query_row(
                "SELECT id, quantity, revision, correction_group_id FROM economic_event
                 WHERE (id = ?1 OR correction_group_id = ?1) AND active_in_correction_group = 1
                 ORDER BY revision DESC LIMIT 1",
                params![event_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let (canonical_ev_id, fill_qty_str, current_rev, cg_id) = fill_row.ok_or_else(|| {
            StorageError::IntegrityFailed(format!("fill event '{event_id}' not found"))
        })?;
        let fill_qty = parse_decimal_lossless(&fill_qty_str)?.abs();

        // Collect every identifier referencing this fill across revisions (`id`s + `correction_group_id`).
        let mut group_keys = BTreeSet::new();
        group_keys.insert(event_id.to_string());
        group_keys.insert(canonical_ev_id);
        group_keys.insert(cg_id.clone());
        {
            let mut grp_stmt =
                tx.prepare_cached("SELECT id FROM economic_event WHERE correction_group_id = ?1")?;
            for row in grp_stmt.query_map(params![cg_id], |r| r.get::<_, String>(0))? {
                group_keys.insert(row?);
            }
        }

        if let Some(pid) = plan_id {
            let mut stmt = tx.prepare_cached(
                "SELECT plan_id, event_id, allocated_quantity FROM plan_fill_link WHERE event_id = ?1",
            )?;
            let mut existing_sum = Decimal::ZERO;
            for key in &group_keys {
                for r in stmt.query_map(params![key], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })? {
                    let (row_pid, row_ev, qty_str) = r?;
                    if row_pid == pid && row_ev == event_id {
                        continue;
                    }
                    existing_sum += parse_decimal_lossless(&qty_str)?;
                }
            }
            drop(stmt);
            if existing_sum + allocated_quantity > fill_qty {
                return Err(StorageError::IntegrityFailed(
                    "allocated quantity exceeds fill quantity".into(),
                ));
            }
            tx.execute(
                "INSERT INTO plan_fill_link (plan_id, event_id, allocated_quantity)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(plan_id, event_id) DO UPDATE SET allocated_quantity = excluded.allocated_quantity",
                params![pid, event_id, to_canonical_text(allocated_quantity)],
            )?;
        }

        if let Some(nid) = note_id {
            let mut stmt = tx.prepare_cached(
                "SELECT allocated_quantity FROM note_link
                 WHERE target_type = 'event' AND target_id = ?1 AND allocated_quantity IS NOT NULL",
            )?;
            let mut existing_sum = Decimal::ZERO;
            for key in &group_keys {
                for r in stmt.query_map(params![key], |r| r.get::<_, String>(0))? {
                    existing_sum += parse_decimal_lossless(&r?)?;
                }
            }
            drop(stmt);
            if existing_sum + allocated_quantity > fill_qty {
                return Err(StorageError::IntegrityFailed(
                    "allocated quantity exceeds fill quantity".into(),
                ));
            }
            let link_id = format!("NL:{nid}:{event_id}:{}", uuid::Uuid::new_v4());
            tx.execute(
                "INSERT INTO note_link (id, note_id, target_type, target_id, target_revision, allocated_quantity, relation)
                 VALUES (?1, ?2, 'event', ?3, ?4, ?5, ?6)",
                params![
                    link_id,
                    nid,
                    event_id,
                    current_rev,
                    to_canonical_text(allocated_quantity),
                    relation,
                ],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    pub fn add_note_link(
        &mut self,
        note_id: &str,
        target_type: &str,
        target_id: &str,
        target_revision: i64,
        allocated_quantity: Option<Decimal>,
        relation: &str,
    ) -> Result<String, StorageError> {
        add_note_link_in(
            self.conn(),
            note_id,
            target_type,
            target_id,
            target_revision,
            allocated_quantity,
            relation,
        )
    }

    pub fn save_note_revision(
        &mut self,
        note_id: &str,
        title: &str,
        template_kind: &str,
        allow_model_access: bool,
        body_markdown: &str,
        structured: &StructuredNoteFields,
        tags: &[String],
    ) -> Result<i64, StorageError> {
        let tx = self.conn_mut().savepoint()?;
        let next_rev = save_note_revision_in(
            &tx,
            note_id,
            title,
            template_kind,
            allow_model_access,
            body_markdown,
            structured,
            tags,
        )?;
        tx.commit()?;
        Ok(next_rev)
    }

    pub fn get_note(&self, note_id: &str) -> Result<NoteDto, StorageError> {
        let (title, current_revision, allow_int, template_kind): (String, i64, i64, String) = self
            .conn()
            .prepare_cached(
                "SELECT title, current_revision, allow_model_access, template_kind
                 FROM note WHERE id = ?1",
            )?
            .query_row(params![note_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?;

        let (body_markdown, structured_json, saved_at): (String, String, String) = self
            .conn()
            .prepare_cached(
                "SELECT body_markdown, structured_json, saved_at
                 FROM note_revision
                 WHERE note_id = ?1 AND revision = ?2",
            )?
            .query_row(params![note_id, current_revision], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
        let structured: StructuredNoteFields =
            serde_json::from_str(&structured_json).unwrap_or_default();

        let mut tag_stmt = self.conn().prepare_cached(
            "SELECT t.name FROM tag t
             JOIN note_tag nt ON nt.tag_id = t.id
             WHERE nt.note_id = ?1
             ORDER BY t.name ASC",
        )?;
        let tags: Vec<String> = tag_stmt
            .query_map(params![note_id], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?;

        let mut link_stmt = self.conn().prepare_cached(
            "SELECT id, target_type, target_id, target_revision, allocated_quantity, relation
             FROM note_link WHERE note_id = ?1 ORDER BY id ASC",
        )?;
        let raw_links: Vec<(String, String, String, i64, Option<String>, String)> = link_stmt
            .query_map(params![note_id], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut links = Vec::with_capacity(raw_links.len());
        let mut cg_stmt = self.conn().prepare_cached(
            "SELECT correction_group_id FROM economic_event WHERE id = ?1 LIMIT 1",
        )?;
        let mut rev_stmt = self.conn().prepare_cached(
            "SELECT COALESCE(MAX(revision), ?2) FROM economic_event
             WHERE correction_group_id = ?1 OR id = ?1",
        )?;
        for (id, target_type, target_id, target_revision, alloc_opt, relation) in raw_links {
            let current_target_revision = if target_type == "event" {
                let grp_opt: Option<String> = cg_stmt
                    .query_row(params![target_id], |r| r.get(0))
                    .optional()?;
                let lookup_grp = grp_opt.unwrap_or_else(|| target_id.clone());
                rev_stmt
                    .query_row(params![lookup_grp, target_revision], |r| r.get(0))
                    .unwrap_or(target_revision)
            } else {
                target_revision
            };
            let allocated_quantity = match alloc_opt {
                Some(s) => Some(parse_decimal_lossless(&s)?),
                None => None,
            };
            links.push(NoteLinkDto {
                id,
                note_id: note_id.to_string(),
                target_type,
                target_id,
                target_revision,
                current_target_revision,
                is_target_amended: current_target_revision > target_revision,
                allocated_quantity,
                relation,
            });
        }

        let mut att_stmt = self.conn().prepare_cached(
            "SELECT id, sha256_hash, file_name, mime_type, size_bytes, relative_path
             FROM attachment WHERE note_id = ?1 ORDER BY created_at ASC, id ASC",
        )?;
        let raw_atts: Vec<(String, String, String, String, i64, String)> = att_stmt
            .query_map(params![note_id], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut attachments = Vec::with_capacity(raw_atts.len());
        for (id, sha256_hash, file_name, mime_type, size_bytes, relative_path) in raw_atts {
            let exists_on_disk = self.root_dir().join(&relative_path).is_file();
            attachments.push(AttachmentDto {
                id,
                note_id: note_id.to_string(),
                sha256_hash,
                file_name,
                mime_type,
                size_bytes: size_bytes.max(0) as u64,
                relative_path,
                exists_on_disk,
            });
        }

        Ok(NoteDto {
            id: note_id.to_string(),
            title,
            current_revision,
            allow_model_access: allow_int != 0,
            template_kind,
            body_markdown,
            structured,
            tags,
            links,
            attachments,
            saved_at,
        })
    }

    pub fn list_notes(&self, only_model_accessible: bool) -> Result<Vec<NoteDto>, StorageError> {
        let sql = if only_model_accessible {
            "SELECT id FROM note WHERE allow_model_access = 1 ORDER BY updated_at DESC, id ASC"
        } else {
            "SELECT id FROM note ORDER BY updated_at DESC, id ASC"
        };
        let mut stmt = self.conn().prepare_cached(sql)?;
        let ids: Vec<String> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(self.get_note(&id)?);
        }
        Ok(out)
    }

    pub fn add_note_attachment(
        &mut self,
        note_id: &str,
        file_name: &str,
        mime_type: &str,
        bytes: &[u8],
    ) -> Result<AttachmentDto, StorageError> {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let sha256_hash = hex::encode(hasher.finalize());
        let relative_path = format!("attachments/{sha256_hash}");
        let disk_path = self.root_dir().join(&relative_path);
        if !disk_path.exists() {
            write_bytes_atomic(&disk_path, bytes)?;
        }
        let id = format!("ATT:{note_id}:{sha256_hash}");
        let now = Utc::now().to_rfc3339();
        self.conn().execute(
            "INSERT OR REPLACE INTO attachment (id, note_id, sha256_hash, file_name, mime_type, size_bytes, relative_path, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                note_id,
                sha256_hash,
                file_name,
                mime_type,
                bytes.len() as i64,
                relative_path,
                now,
            ],
        )?;
        self.sync_manifest()?;
        Ok(AttachmentDto {
            id,
            note_id: note_id.to_string(),
            sha256_hash,
            file_name: file_name.to_string(),
            mime_type: mime_type.to_string(),
            size_bytes: bytes.len() as u64,
            relative_path,
            exists_on_disk: true,
        })
    }

    /// Search notes using FTS5 `trigram` for `>= 3` characters and bounded `LIKE` fallback for
    /// 1-char and 2-char queries (`S-17`, `M2-K05`, `M2-A-12`).
    pub fn search_notes(
        &self,
        query: &str,
        only_model_accessible: bool,
    ) -> Result<(Vec<NoteDto>, String), StorageError> {
        let trimmed = query.trim();
        let char_count = trimmed.chars().count();
        if char_count == 0 {
            return Ok((self.list_notes(only_model_accessible)?, "empty_all".into()));
        }

        let (matched_ids, mode): (Vec<String>, String) = if char_count >= 3 {
            let escaped = format!("\"{}\"", trimmed.replace('"', "\"\""));
            let mut stmt = self.conn().prepare_cached(
                "SELECT n.id
                 FROM note_fts f
                 JOIN note n ON n.id = f.note_id
                 WHERE note_fts MATCH ?1
                   AND (?2 = 0 OR n.allow_model_access = 1)
                 ORDER BY n.id ASC",
            )?;
            let ids: Vec<String> = stmt
                .query_map(params![escaped, i32::from(only_model_accessible)], |r| {
                    r.get(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            (ids, "fts5_trigram".to_string())
        } else {
            let pattern = escape_sql_like(trimmed);
            let mut stmt = self.conn().prepare_cached(
                "SELECT n.id
                 FROM note_fts f
                 JOIN note n ON n.id = f.note_id
                 WHERE (f.title LIKE ?1 ESCAPE '\\' OR f.body_markdown LIKE ?1 ESCAPE '\\' OR f.tags_text LIKE ?1 ESCAPE '\\')
                   AND (?2 = 0 OR n.allow_model_access = 1)
                 ORDER BY n.id ASC",
            )?;
            let ids: Vec<String> = stmt
                .query_map(params![pattern, i32::from(only_model_accessible)], |r| {
                    r.get(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            (ids, "bounded_like_fallback".to_string())
        };

        let mut notes = Vec::with_capacity(matched_ids.len());
        for id in matched_ids {
            notes.push(self.get_note(&id)?);
        }
        Ok((notes, mode))
    }

    /// Candidate IDs are bounded before full note bodies and links are loaded by the host.
    pub fn search_note_candidate_ids(
        &self,
        query: &str,
        only_model_accessible: bool,
        limit: usize,
    ) -> Result<Vec<String>, StorageError> {
        let query = query.trim();
        let (sql, pattern) = if query.is_empty() {
            (
                "SELECT n.id FROM note n WHERE (?2=0 OR n.allow_model_access=1) AND ?1='' ORDER BY n.id LIMIT ?3",
                String::new(),
            )
        } else if query.chars().count() >= 3 {
            (
                "SELECT n.id FROM note_fts f JOIN note n ON n.id=f.note_id WHERE note_fts MATCH ?1 AND (?2=0 OR n.allow_model_access=1) ORDER BY n.id LIMIT ?3",
                format!("\"{}\"", query.replace('"', "\"\"")),
            )
        } else {
            (
                "SELECT n.id FROM note_fts f JOIN note n ON n.id=f.note_id WHERE (f.title LIKE ?1 ESCAPE '\\' OR f.body_markdown LIKE ?1 ESCAPE '\\' OR f.tags_text LIKE ?1 ESCAPE '\\') AND (?2=0 OR n.allow_model_access=1) ORDER BY n.id LIMIT ?3",
                escape_sql_like(query),
            )
        };
        let mut statement = self.conn().prepare_cached(sql)?;
        let rows = statement.query_map(
            params![
                pattern,
                i32::from(only_model_accessible),
                limit.min(10_001) as i64
            ],
            |row| row.get(0),
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }
}

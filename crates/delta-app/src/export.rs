use super::*;
use rusqlite::{types::ValueRef, Connection};

impl AppService {
    /// Export a coherent read snapshot, including audit journals, FX and company actions.
    /// Publish the manifest only after all files have been written and hashed.
    pub fn export_library(&self, export_dir: &Path) -> Result<ExportManifestDto, ErrorEnvelope> {
        crate::paths::ensure_channel_data_isolation(
            export_dir,
            crate::paths::BuildChannel::current(),
        )
        .map_err(|e| ErrorEnvelope::new("channel_isolation", e.to_string(), false))?;
        let fail = |error: &dyn std::fmt::Display| {
            ErrorEnvelope::new("export_failed", error.to_string(), false)
        };
        if export_dir.exists()
            && fs::read_dir(export_dir)
                .map_err(|e| fail(&e))?
                .next()
                .is_some()
        {
            return Err(ErrorEnvelope::new(
                "export_target_not_empty",
                "export requires an empty or new directory",
                false,
            ));
        }
        let parent = export_dir
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent).map_err(|e| fail(&e))?;
        let canonical_parent = fs::canonicalize(parent).map_err(|e| fail(&e))?;
        let library = fs::canonicalize(self.store.root_dir()).map_err(|e| fail(&e))?;
        if canonical_parent.starts_with(&library) {
            return Err(ErrorEnvelope::new(
                "export_target_in_library",
                "export directory must be outside the library",
                false,
            ));
        }
        let staging = canonical_parent.join(format!(".delta-export-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&staging).map_err(|e| fail(&e))?;
        let result = (|| {
            let snapshot = self
                .store
                .conn()
                .unchecked_transaction()
                .map_err(|e| fail(&e))?;
            let (library_id, ledger_revision): (String, i64) = snapshot
                .query_row("SELECT id,ledger_revision FROM library LIMIT 1", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .map_err(|e| fail(&e))?;
            let dataset_version = snapshot
                .query_row(
                    "SELECT COALESCE(MAX(version_num),0) FROM dataset_publication",
                    [],
                    |r| r.get(0),
                )
                .map_err(|e| fail(&e))?;
            let mut terms_classes = Vec::new();
            let mut statement = snapshot
                .prepare("SELECT DISTINCT terms_class FROM dataset_version ORDER BY terms_class")
                .map_err(|e| fail(&e))?;
            for row in statement
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(|e| fail(&e))?
            {
                let row = row.map_err(|e| fail(&e))?;
                for term in row.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    if !matches!(
                        term,
                        "public_open" | "official_free_personal" | "unofficial_personal"
                    ) {
                        return Err(ErrorEnvelope::new(
                            "invalid_terms_class",
                            "dataset contains an unknown terms class",
                            false,
                        ));
                    }
                    if !terms_classes.iter().any(|s| s == term) {
                        terms_classes.push(term.to_string());
                    }
                }
            }
            drop(statement);
            let mut files = BTreeMap::new();
            for (name, sql) in [
                ("ledger_events.csv", "SELECT * FROM economic_event ORDER BY occurred_at,sequence_no,id"),
                ("journal_postings.csv", "SELECT j.event_id,j.ledger_revision,j.posting_rule_version,j.reversal_of,p.* FROM posting p JOIN journal_transaction j ON j.id=p.transaction_id ORDER BY j.ledger_revision,j.id,p.id"),
                ("cost_lots.csv", "SELECT * FROM cost_lot ORDER BY account_id,asset_id,acquired_at,id"),
                ("bars.csv", "SELECT * FROM bar ORDER BY instrument_id,session_date,source,valid_from_version,id"),
                ("fx_rates.csv", "SELECT * FROM fx_rate ORDER BY base_currency,quote_currency,effective_date,source,dataset_version,id"),
                ("corporate_actions.csv", "SELECT * FROM corporate_action ORDER BY instrument_id,ex_date,source,factor_version,id"),
                ("event_book_fx.csv", "SELECT * FROM event_book_fx ORDER BY event_id,currency"),
                ("dataset_versions.csv", "SELECT * FROM dataset_version ORDER BY version_num"),
            ] {
                let path = staging.join(name);
                export_query_csv(&snapshot, sql, &path)?;
                files.insert(name.into(), sha256_file(&path).map_err(|e| fail(&e))?);
            }

            // Export open-format JSON & Markdown artifacts (`M2-W08`, `M2-A-17`) when M2 entities exist
            let notes = self.store.list_notes(false).unwrap_or_default();
            let mut ctx_stmt = snapshot
                .prepare("SELECT id FROM chart_context ORDER BY id ASC")
                .map_err(|e| fail(&e))?;
            let ctx_ids: Vec<String> = ctx_stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(|e| fail(&e))?
                .filter_map(Result::ok)
                .collect();
            drop(ctx_stmt);

            let mut ai_stmt = snapshot
                .prepare(
                    "SELECT id, library_id, schema_version, active_checkpoint_id, status, created_at, updated_at
                     FROM ai_session ORDER BY id ASC",
                )
                .map_err(|e| fail(&e))?;
            let ai_sessions: Vec<delta_data::AiSessionRecord> = ai_stmt
                .query_map([], |r| {
                    Ok(delta_data::AiSessionRecord {
                        id: r.get(0)?,
                        library_id: r.get(1)?,
                        schema_version: r.get(2)?,
                        active_checkpoint_id: r.get(3)?,
                        status: r.get(4)?,
                        created_at: r.get(5)?,
                        updated_at: r.get(6)?,
                    })
                })
                .map_err(|e| fail(&e))?
                .filter_map(Result::ok)
                .collect();
            drop(ai_stmt);

            let has_settings_or_attachments = ["attachments", "settings"].iter().any(|subdir| {
                let d = self.store.root_dir().join(subdir);
                d.is_dir() && fs::read_dir(d).ok().and_then(|mut it| it.next()).is_some()
            });

            if !notes.is_empty()
                || !ctx_ids.is_empty()
                || !ai_sessions.is_empty()
                || has_settings_or_attachments
            {
                let all_events = load_all_ledger_events(&self.store).map_err(|e| fail(&e))?;
                let ledger_json_path = staging.join("ledger_events.json");
                fs::write(
                    &ledger_json_path,
                    serde_json::to_vec_pretty(&all_events).map_err(|e| fail(&e))?,
                )
                .map_err(|e| fail(&e))?;
                files.insert(
                    "ledger_events.json".into(),
                    sha256_file(&ledger_json_path).map_err(|e| fail(&e))?,
                );

                let notes_dir = staging.join("notes");
                fs::create_dir_all(&notes_dir).map_err(|e| fail(&e))?;
                let notes_index_path = notes_dir.join("index.json");
                fs::write(
                    &notes_index_path,
                    serde_json::to_vec_pretty(&notes).map_err(|e| fail(&e))?,
                )
                .map_err(|e| fail(&e))?;
                files.insert(
                    "notes/index.json".into(),
                    sha256_file(&notes_index_path).map_err(|e| fail(&e))?,
                );

                for note in &notes {
                    let safe_id: String = note
                        .id
                        .chars()
                        .map(|c| {
                            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                                c
                            } else {
                                '_'
                            }
                        })
                        .collect();
                    let rel_md = format!("notes/{safe_id}.md");
                    let md_path = staging.join(&rel_md);
                    let md_content = format!(
                        "---\nid: {}\ntitle: {}\ntemplate: {}\nrevision: {}\ntags: [{}]\n---\n\n{}\n",
                        note.id,
                        note.title,
                        note.template_kind,
                        note.current_revision,
                        note.tags.join(", "),
                        note.body_markdown
                    );
                    fs::write(&md_path, md_content.as_bytes()).map_err(|e| fail(&e))?;
                    files.insert(rel_md, sha256_file(&md_path).map_err(|e| fail(&e))?);
                }

                let mut chart_contexts = Vec::new();
                for cid in ctx_ids {
                    if let Ok(ctx) = self.store.get_chart_context(&cid) {
                        chart_contexts.push(ctx);
                    }
                }
                let ctx_path = staging.join("chart_contexts.json");
                fs::write(
                    &ctx_path,
                    serde_json::to_vec_pretty(&chart_contexts).map_err(|e| fail(&e))?,
                )
                .map_err(|e| fail(&e))?;
                files.insert(
                    "chart_contexts.json".into(),
                    sha256_file(&ctx_path).map_err(|e| fail(&e))?,
                );

                let ai_path = staging.join("ai_sessions.json");
                fs::write(
                    &ai_path,
                    serde_json::to_vec_pretty(&ai_sessions).map_err(|e| fail(&e))?,
                )
                .map_err(|e| fail(&e))?;
                files.insert(
                    "ai_sessions.json".into(),
                    sha256_file(&ai_path).map_err(|e| fail(&e))?,
                );

                for subdir in ["attachments", "settings"] {
                    let src_dir = self.store.root_dir().join(subdir);
                    if src_dir.is_dir() {
                        export_copy_dir_recursive(
                            &src_dir,
                            &staging,
                            &staging.join(subdir),
                            &mut files,
                        )
                        .map_err(|e| fail(&e))?;
                    }
                }
            }

            // Export open-format M4 simulation & training tables (`M4-W03`, `M4-A-11`) when runs or user fee profiles exist
            let sim_bundle = self
                .store
                .export_simulation_tables_snapshot()
                .map_err(|e| fail(&e))?;
            if !sim_bundle.simulation_runs.is_empty()
                || sim_bundle
                    .fee_profile_versions
                    .iter()
                    .any(|fp| !fp.is_builtin)
            {
                let sim_dir = staging.join("simulation");
                fs::create_dir_all(&sim_dir).map_err(|e| fail(&e))?;
                let bundle_path = sim_dir.join("simulation_bundle.json");
                fs::write(
                    &bundle_path,
                    serde_json::to_vec_pretty(&sim_bundle).map_err(|e| fail(&e))?,
                )
                .map_err(|e| fail(&e))?;
                files.insert(
                    "simulation/simulation_bundle.json".into(),
                    sha256_file(&bundle_path).map_err(|e| fail(&e))?,
                );

                for (rel_name, sql) in [
                    (
                        "simulation/fee_profile_versions.csv",
                        "SELECT * FROM fee_profile_version ORDER BY profile_key, version_num, id",
                    ),
                    (
                        "simulation/simulation_runs.csv",
                        "SELECT * FROM simulation_run ORDER BY created_at, run_id",
                    ),
                    (
                        "simulation/training_sessions.csv",
                        "SELECT * FROM training_session ORDER BY run_id",
                    ),
                    (
                        "simulation/sim_orders.csv",
                        "SELECT * FROM sim_order ORDER BY run_id, order_seq, order_id",
                    ),
                    (
                        "simulation/sim_fills.csv",
                        "SELECT * FROM sim_fill ORDER BY run_id, bar_close_at, fill_id",
                    ),
                    (
                        "simulation/simulation_events.csv",
                        "SELECT * FROM simulation_event ORDER BY run_id, seq",
                    ),
                    (
                        "simulation/sim_checkpoints.csv",
                        "SELECT * FROM sim_checkpoint ORDER BY run_id, seq",
                    ),
                    (
                        "simulation/training_notes.csv",
                        "SELECT * FROM training_note ORDER BY run_id, created_at, note_id",
                    ),
                ] {
                    let csv_path = staging.join(rel_name);
                    export_query_csv(&snapshot, sql, &csv_path)?;
                    files.insert(
                        rel_name.into(),
                        sha256_file(&csv_path).map_err(|e| fail(&e))?,
                    );
                }
            }

            snapshot.commit().map_err(|e| fail(&e))?;
            let manifest = ExportManifestDto {
                exported_at: Utc::now().to_rfc3339(),
                library_id,
                ledger_revision,
                dataset_version,
                terms_classes,
                files,
            };
            fs::write(
                staging.join("export_manifest.json"),
                serde_json::to_vec_pretty(&manifest).map_err(|e| fail(&e))?,
            )
            .map_err(|e| fail(&e))?;
            if export_dir.exists() {
                fs::remove_dir(export_dir).map_err(|e| fail(&e))?;
            }
            fs::rename(&staging, export_dir).map_err(|e| fail(&e))?;
            Ok(manifest)
        })();
        if result.is_err() {
            // This exact staging directory was created here and is confined to its parent.
            let _ = fs::remove_dir_all(&staging);
        }
        result
    }
}

fn export_query_csv(conn: &Connection, sql: &str, path: &Path) -> Result<(), ErrorEnvelope> {
    let fail = |error: &dyn std::fmt::Display| {
        ErrorEnvelope::new("export_csv_failed", error.to_string(), false)
    };
    let mut statement = conn.prepare(sql).map_err(|e| fail(&e))?;
    let count = statement.column_count();
    let mut writer = csv::Writer::from_path(path).map_err(|e| fail(&e))?;
    writer
        .write_record(statement.column_names())
        .map_err(|e| fail(&e))?;
    let mut rows = statement.query([]).map_err(|e| fail(&e))?;
    while let Some(row) = rows.next().map_err(|e| fail(&e))? {
        let mut record = Vec::with_capacity(count);
        for column in 0..count {
            record.push(match row.get_ref(column).map_err(|e| fail(&e))? {
                ValueRef::Null => String::new(),
                ValueRef::Integer(value) => value.to_string(),
                ValueRef::Real(_) => {
                    return Err(ErrorEnvelope::new(
                        "invalid_financial_storage",
                        "unexpected floating point value in financial export",
                        false,
                    ))
                }
                ValueRef::Text(value) => std::str::from_utf8(value)
                    .map_err(|e| fail(&e))?
                    .to_string(),
                ValueRef::Blob(value) => hex::encode(value),
            });
        }
        writer.write_record(record).map_err(|e| fail(&e))?;
    }
    writer.flush().map_err(|e| fail(&e))?;
    Ok(())
}

fn export_copy_dir_recursive(
    src_dir: &Path,
    staging_root: &Path,
    dst_dir: &Path,
    files: &mut BTreeMap<String, String>,
) -> std::io::Result<()> {
    fs::create_dir_all(dst_dir)?;
    for entry in fs::read_dir(src_dir)? {
        let entry = entry?;
        let path = entry.path();
        let dst_path = dst_dir.join(entry.file_name());
        if path.is_dir() {
            export_copy_dir_recursive(&path, staging_root, &dst_path, files)?;
        } else if path.is_file() {
            fs::copy(&path, &dst_path)?;
            let rel = dst_path
                .strip_prefix(staging_root)
                .unwrap_or(&dst_path)
                .to_string_lossy()
                .replace('\\', "/");
            let hash = sha256_file(&dst_path).map_err(|e| std::io::Error::other(e.to_string()))?;
            files.insert(rel, hash);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::seed_ac02_fixture_in_service;
    use crate::paths::{resolve_app_paths, BuildChannel};
    use delta_data::{ChartContextDto, StructuredNoteFields};

    #[test]
    fn test_m2_a17_backup_restore_export_delete_and_log_redaction() {
        apply_process_quiet_guards();
        let tmp_root = std::env::temp_dir().join(format!(
            "delta_m2_a17_{}_{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or(1)
        ));
        let _ = fs::remove_dir_all(&tmp_root);
        let data_dir = tmp_root.join("lib_data");
        let paths = resolve_app_paths(Some(BuildChannel::Source), Some(&data_dir)).unwrap();
        let mut svc = AppService::open(paths, "USD", "USD", "UTC", true).unwrap();

        // 1. Seed ledger, notes, attachments, chart contexts, settings, and AI sessions
        let dsv = seed_ac02_fixture_in_service(&mut svc).unwrap();
        let events = load_all_ledger_events(svc.store()).unwrap();
        let first_event_id = events[0].event_id.clone();

        let ctx_id = svc
            .store_mut()
            .save_chart_context(&ChartContextDto {
                id: "CTX-A17-1".to_string(),
                instrument_id: "NASDAQ:AAPL".to_string(),
                venue: "NASDAQ".to_string(),
                timeframe: "1d".to_string(),
                adjustment: AdjustmentKind::Raw,
                visible_start_date: NaiveDate::from_ymd_opt(2026, 1, 2).unwrap(),
                visible_end_date: NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(),
                anchor_date: Some(NaiveDate::from_ymd_opt(2026, 1, 10).unwrap()),
                dataset_version: Some(dsv),
                latest_dataset_version: dsv,
                has_newer_dataset_version: false,
                indicators_json: r#"["SMA20","EMA50"]"#.to_string(),
            })
            .unwrap();

        let secret_note_body = "CONFIDENTIAL_NOTE_BODY_DO_NOT_LOG_998877";
        svc.store_mut()
            .save_note_revision(
                "NOTE-A17-1",
                "AAPL Post-Trade Review",
                "post_trade",
                true,
                secret_note_body,
                &StructuredNoteFields {
                    thesis: "Momentum hold".to_string(),
                    conclusion: "Disciplined exit".to_string(),
                    ..Default::default()
                },
                &["aapl".to_string(), "review".to_string()],
            )
            .unwrap();
        svc.store_mut()
            .add_note_link(
                "NOTE-A17-1",
                "event",
                &first_event_id,
                1,
                None,
                "review_target",
            )
            .unwrap();
        svc.store_mut()
            .add_note_link(
                "NOTE-A17-1",
                "chart_context",
                &ctx_id,
                dsv,
                None,
                "context_snapshot",
            )
            .unwrap();
        let att = svc
            .store_mut()
            .add_note_attachment(
                "NOTE-A17-1",
                "chart_snap.png",
                "image/png",
                b"PNG_BYTES_A17",
            )
            .unwrap();

        svc.store_mut().create_ai_session("SESS-A17-1").unwrap();

        let settings_dir = svc.paths().data_dir.join("settings");
        fs::create_dir_all(&settings_dir).unwrap();
        fs::write(
            settings_dir.join("layout.json"),
            br#"{"version":1,"left_nav":"overview"}"#,
        )
        .unwrap();

        // 2. Write diagnostic audit log and verify redaction of secret token, full account number (`88881234`), and note body
        let api_secret = "sk-live-secret-token-abcdef123456";
        svc.write_audit_log(
            "ai_review_and_sync",
            &format!("Synced account 88881234 with token {api_secret} and note {secret_note_body}"),
            &[api_secret, "88881234"],
        )
        .unwrap();
        let leaks = svc
            .scan_logs_for_sensitive_leaks(&[api_secret, "88881234", secret_note_body])
            .unwrap();
        assert!(
            leaks.is_empty(),
            "logs must not contain secrets, full account numbers, or note bodies: {leaks:?}"
        );
        // Partial text, unknown secrets and an untrusted action name must also stay out
        // of logs; a caller-maintained replacement list cannot guarantee this.
        let unlisted_secret = "unlisted-synthetic-secret";
        let log = svc
            .write_audit_log(
                unlisted_secret,
                &format!(
                    "Authorization: Bearer {unlisted_secret}; note fragment: {}",
                    &secret_note_body[..3]
                ),
                &[],
            )
            .unwrap();
        let text = fs::read_to_string(&log).unwrap();
        assert!(!text.contains(unlisted_secret));
        assert!(!text.contains("Authorization"));
        assert!(text.contains("message_bytes"));
        // Leak diagnostics identify positions without echoing the sensitive value.
        fs::write(
            svc.paths().data_dir.join("logs/injected-test.log"),
            unlisted_secret,
        )
        .unwrap();
        let detected = svc
            .scan_logs_for_sensitive_leaks(&[unlisted_secret])
            .unwrap();
        assert_eq!(detected.len(), 1);
        assert!(!detected[0].contains(unlisted_secret));
        fs::remove_file(svc.paths().data_dir.join("logs/injected-test.log")).unwrap();

        // 3. Create backup and restore into a new directory -> verifies ledger, note, and attachment references
        let backup_dir = tmp_root.join("backup_snap");
        let backup_manifest = svc.create_backup(&backup_dir).unwrap();
        assert!(backup_manifest.file_hashes.contains_key("delta.sqlite"));
        assert!(backup_manifest
            .file_hashes
            .contains_key("settings/layout.json"));

        let restored_dir = tmp_root.join("restored_lib");
        let restored_manifest = AppService::restore_backup(&backup_dir, &restored_dir).unwrap();
        assert_eq!(restored_manifest.library_id, backup_manifest.library_id);

        let restored_paths =
            resolve_app_paths(Some(BuildChannel::Source), Some(&restored_dir)).unwrap();
        let restored_svc = AppService::open(restored_paths, "USD", "USD", "UTC", true).unwrap();
        let restored_note = restored_svc.store().get_note("NOTE-A17-1").unwrap();
        assert_eq!(restored_note.body_markdown, secret_note_body);
        assert_eq!(restored_note.attachments.len(), 1);
        assert_eq!(restored_note.attachments[0].sha256_hash, att.sha256_hash);
        assert!(restored_note.attachments[0].exists_on_disk);
        drop(restored_svc);

        // 4. Export library: verify CSV + JSON + Markdown notes + attachments + chart_contexts + ai_sessions + settings
        let export_dir = tmp_root.join("exported_open_format");
        let export_manifest = svc.export_library(&export_dir).unwrap();
        for expected_key in [
            "ledger_events.csv",
            "ledger_events.json",
            "notes/index.json",
            "notes/NOTE-A17-1.md",
            "chart_contexts.json",
            "ai_sessions.json",
            "settings/layout.json",
        ] {
            assert!(
                export_manifest.files.contains_key(expected_key),
                "export manifest missing {expected_key}"
            );
            assert!(export_dir.join(expected_key).is_file());
        }
        assert!(export_manifest
            .files
            .keys()
            .any(|k| k.starts_with("attachments/")));

        // 5. Dev/Source channel cannot target release roots (`ensure_channel_data_isolation`)
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            let release_root = PathBuf::from(local_app_data).join("delta");
            let err_rel = svc
                .create_backup(&release_root.join("forbidden_backup"))
                .expect_err("dev/source channel must reject release root");
            assert_eq!(err_rel.code, "channel_isolation");
        }

        // 6. Preview and execute library deletion
        drop(svc);
        let preview = AppService::delete_library(&data_dir, false, true).unwrap();
        assert!(preview.dry_run_preview);
        assert!(!preview.deleted);
        assert!(preview.file_count > 0);
        assert!(data_dir.exists());

        let deleted = AppService::delete_library(&data_dir, true, false).unwrap();
        assert!(deleted.deleted);
        assert_eq!(deleted.file_count, preview.file_count);
        assert!(!data_dir.exists());

        let _ = fs::remove_dir_all(&tmp_root);
    }
}

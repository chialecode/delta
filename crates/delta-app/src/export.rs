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

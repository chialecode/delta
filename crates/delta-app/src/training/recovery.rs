//! Training run initial state, deterministic replay, and integrity recovery (S-43, M4-A-09).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use delta_core::{
    SimCommand, SimInstrumentSpec, SimReplayer, SimState, SimStepBar, SIM_ENGINE_VERSION_M5,
};
use delta_data::{
    FeeProfileVersionRecord, SimRunStatus, SimulationRecoveryBundle, SimulationRunRecord,
    TrainingBarPortQuery, TrainingSessionRecord,
};
use rusqlite::params;
use rust_decimal::Decimal;

use super::scrub::{map_sim_err, map_storage_err, record_to_core_fee_profile, ReplayFailure};
use super::RecoveredSim;
use crate::service::{AppService, ErrorEnvelope};

impl AppService {
    pub(crate) fn resolve_training_dataset_version(
        &self,
        requested: Option<i64>,
    ) -> Result<i64, ErrorEnvelope> {
        if let Some(v) = requested {
            if v <= 0 {
                return Err(ErrorEnvelope::new(
                    "INVALID_DATASET_VERSION",
                    "dataset_version must be positive",
                    false,
                ));
            }
            let exists: bool = self
                .store()
                .conn()
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM dataset_version WHERE version_num = ?1)",
                    params![v],
                    |r| r.get(0),
                )
                .map_err(|e| ErrorEnvelope::new("QUERY_VERSION_FAILED", e.to_string(), false))?;
            if !exists {
                return Err(ErrorEnvelope::new(
                    "UNKNOWN_DATASET_VERSION",
                    format!("dataset_version {v} does not exist"),
                    false,
                ));
            }
            return Ok(v);
        }

        let pub_v: i64 = self
            .store()
            .conn()
            .query_row(
                "SELECT COALESCE(MAX(version_num), 0) FROM dataset_publication",
                [],
                |r| r.get(0),
            )
            .map_err(|e| ErrorEnvelope::new("QUERY_VERSION_FAILED", e.to_string(), false))?;
        if pub_v > 0 {
            return Ok(pub_v);
        }

        let dsv: i64 = self
            .store()
            .conn()
            .query_row(
                "SELECT COALESCE(MAX(version_num), 0) FROM dataset_version",
                [],
                |r| r.get(0),
            )
            .map_err(|e| ErrorEnvelope::new("QUERY_VERSION_FAILED", e.to_string(), false))?;
        if dsv <= 0 {
            return Err(ErrorEnvelope::new(
                "NO_DATASET_VERSION",
                "本地行情库尚无可用 dataset_version；请先同步或导入行情数据",
                false,
            ));
        }
        Ok(dsv)
    }

    pub(super) fn load_sim_instrument_info(
        &self,
        real_instrument_id: &str,
        fallback_quote_currency: &str,
    ) -> Result<(SimInstrumentSpec, String, String, bool), ErrorEnvelope> {
        let reg = self
            .store()
            .load_instrument_registry()
            .map_err(|e| map_storage_err("REGISTRY_FAILED", e))?;
        if let Some(inst) = reg.instruments.get(real_instrument_id) {
            let spec = SimInstrumentSpec::from_instrument(inst).map_err(map_sim_err)?;
            let is_crypto = matches!(inst.kind, delta_core::InstrumentKind::SpotCrypto);
            return Ok((spec, inst.venue.clone(), inst.symbol.clone(), is_crypto));
        }

        let parts: Vec<&str> = real_instrument_id.splitn(2, ':').collect();
        let (venue, symbol) = if parts.len() == 2 {
            (parts[0].to_string(), parts[1].to_string())
        } else {
            ("UNKNOWN".to_string(), real_instrument_id.to_string())
        };
        let is_crypto = matches!(venue.as_str(), "OKX" | "BINANCE");
        let spec = SimInstrumentSpec {
            instrument_id: real_instrument_id.to_string(),
            base_asset_id: format!("ASSET:{symbol}"),
            quote_currency: fallback_quote_currency.to_ascii_uppercase(),
            lot_size: if is_crypto {
                Decimal::new(1, 6)
            } else {
                Decimal::ONE
            },
            tick_size: Decimal::new(1, 2),
        };
        spec.validate().map_err(map_sim_err)?;
        Ok((spec, venue, symbol, is_crypto))
    }

    /// Initial state for a run's frozen engine and timeframe (`m4-sim-1` or `m5-sim-2`).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn build_initial_sim_state_for(
        &self,
        engine_version: &str,
        timeframe: &str,
        quote_currency: &str,
        initial_cash: Decimal,
        fee_profile_rec: &FeeProfileVersionRecord,
        real_instrument_id: &str,
        dataset_version: i64,
        history_start_at: DateTime<Utc>,
        initial_visible_until: DateTime<Utc>,
    ) -> Result<SimState, ErrorEnvelope> {
        let core_fp = record_to_core_fee_profile(fee_profile_rec)?;
        let (spec, _, _, _) = self.load_sim_instrument_info(real_instrument_id, quote_currency)?;
        let vis_bars = self
            .store()
            .query_training_visible_final_bars(&TrainingBarPortQuery {
                instrument_id: real_instrument_id.to_string(),
                timeframe: timeframe.to_string(),
                source: None,
                dataset_version,
                warmup_start_at: Some(history_start_at),
                history_start_at: Some(history_start_at),
                visible_until: initial_visible_until,
                previous_visible_until: None,
            })
            .map_err(|e| map_storage_err("TRAINING_PORT_FAILED", e))?;
        let last_bar = vis_bars.last().ok_or_else(|| {
            ErrorEnvelope::new(
                "EMPTY_INITIAL_HISTORY",
                "initial training history window contains zero visible bars",
                false,
            )
        })?;
        let mut initial_closes = BTreeMap::new();
        initial_closes.insert(real_instrument_id.to_string(), last_bar.close);
        if engine_version == SIM_ENGINE_VERSION_M5 {
            let tradable = SimStepBar::from_bar(last_bar.clone()).is_effectively_tradable();
            return SimState::new_m5(
                quote_currency.to_ascii_uppercase(),
                initial_cash,
                core_fp,
                vec![spec],
                Some(initial_visible_until),
                initial_closes,
                timeframe,
                BTreeMap::from([(real_instrument_id.to_string(), tradable)]),
            )
            .map_err(map_sim_err);
        }
        SimState::new(
            quote_currency.to_ascii_uppercase(),
            initial_cash,
            core_fp,
            vec![spec],
            Some(initial_visible_until),
            initial_closes,
        )
        .map_err(map_sim_err)
    }

    pub(super) fn replay_events_on_state(
        &self,
        initial_state: SimState,
        run: &SimulationRunRecord,
        session: &TrainingSessionRecord,
        events: &[delta_data::SimulationEventRecord],
    ) -> Result<(SimState, Vec<(i64, String)>), ReplayFailure> {
        self.replay_events_with_replayer(initial_state, run, session, events)
            .map(|(st, h, _)| (st, h))
    }

    fn replay_events_with_replayer(
        &self,
        initial_state: SimState,
        run: &SimulationRunRecord,
        session: &TrainingSessionRecord,
        events: &[delta_data::SimulationEventRecord],
    ) -> Result<RecoveredSim, ReplayFailure> {
        if events.is_empty() {
            return Ok((
                initial_state.clone(),
                Vec::new(),
                SimReplayer::new(initial_state),
            ));
        }
        let max_vis = events
            .iter()
            .map(|e| e.visible_until)
            .max()
            .unwrap_or(session.visible_until);
        if max_vis > session.visible_until {
            return Err(format!(
                "simulation_event visible_until ({max_vis}) exceeds training_session.visible_until ({})",
                session.visible_until
            )
            .into());
        }

        let needs_bars = events
            .iter()
            .any(|e| e.event_kind == "advance_bar" || e.command_json.contains("advance_bar"));
        let mut bars_by_close = BTreeMap::new();
        if needs_bars {
            let lower_bound = session.train_start_at.min(max_vis);
            let vis_bars = self
                .store()
                .query_training_visible_final_bars(&TrainingBarPortQuery {
                    instrument_id: session.real_instrument_id.clone(),
                    timeframe: session.timeframe.clone(),
                    source: None,
                    dataset_version: run.dataset_version,
                    warmup_start_at: Some(lower_bound),
                    history_start_at: Some(lower_bound),
                    visible_until: max_vis,
                    previous_visible_until: None,
                })
                .map_err(|e| ReplayFailure::Storage(map_storage_err("TRAINING_PORT_FAILED", e)))?;
            for b in vis_bars {
                bars_by_close.insert(b.close_at, b);
            }
        }

        // The replay start is always a bar boundary (initial state, a bar advance, or for
        // `m5-sim-2` a boundary checkpoint), so undo events rebuild exactly (`S-51` ③).
        let mut replayer = SimReplayer::new(initial_state.clone());
        let mut cur_state = initial_state;
        let mut step_hashes = Vec::with_capacity(events.len());
        for ev in events {
            let cmd: SimCommand = serde_json::from_str(&ev.command_json)
                .map_err(|e| format!("corrupt command_json at seq {}: {e}", ev.seq))?;
            let next_state = match &cmd {
                SimCommand::AdvanceBar { .. } => {
                    let bar = bars_by_close.get(&ev.visible_until).ok_or_else(|| {
                        format!(
                            "missing finalized bar at visible_until {} for seq {}",
                            ev.visible_until, ev.seq
                        )
                    })?;
                    let sb = SimStepBar::from_bar(bar.clone());
                    replayer
                        .apply(&cur_state, ev.seq as u64, cmd, Some(&sb))
                        .map_err(|e| format!("replay step failed at seq {}: {e}", ev.seq))?
                        .0
                }
                _ => {
                    replayer
                        .apply(&cur_state, ev.seq as u64, cmd, None)
                        .map_err(|e| format!("replay command failed at seq {}: {e}", ev.seq))?
                        .0
                }
            };
            let h = next_state.state_hash();
            step_hashes.push((ev.seq, h));
            cur_state = next_state;
        }
        Ok((cur_state, step_hashes, replayer))
    }

    /// Recovery that also returns the undo replayer positioned at the latest bar boundary.
    pub(crate) fn compute_recovery_full(
        &self,
        bundle: &SimulationRecoveryBundle,
    ) -> Result<RecoveredSim, ReplayFailure> {
        let session = bundle
            .session
            .as_ref()
            .ok_or_else(|| format!("missing training_session for run '{}'", bundle.run.run_id))?;

        // `m5-sim-2`: undo needs the state at the latest bar boundary, so recovery starts from
        // the latest checkpoint that sits on a boundary (seq 0, a bar advance or finish).
        let boundary_bundle;
        let bundle = if bundle.run.engine_version == SIM_ENGINE_VERSION_M5 {
            match &bundle.latest_checkpoint {
                Some(cp) if cp.seq > 0 => {
                    let at_boundary = bundle
                        .all_events
                        .get((cp.seq - 1) as usize)
                        .is_some_and(|e| matches!(e.event_kind.as_str(), "advance_bar" | "finish"));
                    if at_boundary {
                        bundle
                    } else {
                        let checkpoints = self
                            .store()
                            .list_sim_checkpoints(&bundle.run.run_id)
                            .map_err(|e| {
                                ReplayFailure::Storage(map_storage_err("CHECKPOINT_LOAD_FAILED", e))
                            })?;
                        let chosen = checkpoints
                            .into_iter()
                            .filter(|c| {
                                c.seq == 0
                                    || bundle.all_events.get((c.seq - 1) as usize).is_some_and(
                                        |e| {
                                            matches!(
                                                e.event_kind.as_str(),
                                                "advance_bar" | "finish"
                                            )
                                        },
                                    )
                            })
                            .max_by_key(|c| c.seq);
                        let after = chosen.as_ref().map(|c| c.seq).unwrap_or(0);
                        let mut b = bundle.clone();
                        b.events_after_checkpoint = b
                            .all_events
                            .iter()
                            .filter(|e| e.seq > after)
                            .cloned()
                            .collect();
                        b.latest_checkpoint = chosen;
                        boundary_bundle = b;
                        &boundary_bundle
                    }
                }
                _ => bundle,
            }
        } else {
            bundle
        };
        let (start_state, mut hashes) = match &bundle.latest_checkpoint {
            Some(cp) => {
                let mut cp_state: SimState = serde_json::from_str(&cp.state_json).map_err(|e| {
                    format!("corrupt sim_checkpoint state_json at seq {}: {e}", cp.seq)
                })?;
                cp_state.normalize_fields();
                let actual_cp_hash = cp_state.state_hash();
                if actual_cp_hash != cp.state_hash {
                    return Err(format!(
                        "checkpoint state_json hash ({actual_cp_hash}) != stored checkpoint state_hash ({}) at seq {}",
                        cp.state_hash, cp.seq
                    )
                    .into());
                }
                let mut h = Vec::new();
                if cp.seq == 0 {
                    h.push((0, actual_cp_hash));
                }
                (cp_state, h)
            }
            None => {
                let init_vis = bundle
                    .all_events
                    .first()
                    .and_then(|_| {
                        self.find_initial_visible_until_for_session(&bundle.run, session)
                            .ok()
                    })
                    .unwrap_or(session.visible_until);
                let init_state = self
                    .build_initial_sim_state_for(
                        &bundle.run.engine_version,
                        &session.timeframe,
                        &bundle.run.quote_currency,
                        bundle.run.initial_cash,
                        &bundle.fee_profile,
                        &session.real_instrument_id,
                        bundle.run.dataset_version,
                        session.history_start_at,
                        init_vis,
                    )
                    .map_err(ReplayFailure::Storage)?;
                let h0 = init_state.state_hash();
                (init_state, vec![(0, h0)])
            }
        };

        let (final_state, step_hashes, replayer) = self.replay_events_with_replayer(
            start_state,
            &bundle.run,
            session,
            &bundle.events_after_checkpoint,
        )?;
        hashes.extend(step_hashes);
        if final_state.state_hash() != bundle.run.latest_state_hash {
            return Err(format!(
                "recovered final state_hash ({}) != simulation_run.latest_state_hash ({})",
                final_state.state_hash(),
                bundle.run.latest_state_hash
            )
            .into());
        }
        Ok((final_state, hashes, replayer))
    }

    pub(super) fn find_initial_visible_until_for_session(
        &self,
        run: &SimulationRunRecord,
        session: &TrainingSessionRecord,
    ) -> Result<DateTime<Utc>, ErrorEnvelope> {
        let checkpoints = self
            .store()
            .list_sim_checkpoints(&run.run_id)
            .map_err(|e| map_storage_err("CHECKPOINT_LOAD_FAILED", e))?;
        if let Some(cp0) = checkpoints.iter().find(|cp| cp.seq == 0) {
            if let Ok(st) = serde_json::from_str::<SimState>(&cp0.state_json) {
                if let Some(vis) = st.visible_until {
                    return Ok(vis);
                }
            }
        }
        // Fallback via truncated query up to `train_start_at`: every bar has positive duration, so
        // `close_at <= train_start_at` strictly excludes training bars while retaining the final
        // pre-training history bar even for 24/7 crypto where `last_history.close_at == train_start_at`.
        let hist_bars = self
            .store()
            .query_training_visible_final_bars(&TrainingBarPortQuery {
                instrument_id: session.real_instrument_id.clone(),
                timeframe: session.timeframe.clone(),
                source: None,
                dataset_version: run.dataset_version,
                warmup_start_at: Some(session.history_start_at),
                history_start_at: Some(session.history_start_at),
                visible_until: session.train_start_at.max(session.history_start_at),
                previous_visible_until: None,
            })
            .map_err(|e| map_storage_err("TRAINING_PORT_FAILED", e))?;
        hist_bars.last().map(|b| b.close_at).ok_or_else(|| {
            ErrorEnvelope::new(
                "INITIAL_CLOCK_NOT_FOUND",
                "unable to determine initial history cutoff for training session",
                false,
            )
        })
    }

    /// Structural + replay hash check on a single already-loaded bundle (`S-43`, `TRN-Q01`).
    fn verify_recovery_bundle_readonly(
        &self,
        bundle: &SimulationRecoveryBundle,
    ) -> Result<(SimState, SimReplayer), ReplayFailure> {
        if bundle.run.status == SimRunStatus::Damaged {
            return Err(format!(
                "simulation_run '{}' was already marked damaged",
                bundle.run.run_id
            )
            .into());
        }
        if bundle.all_events.len() as i64 != bundle.run.latest_seq {
            return Err(format!(
                "event count ({}) != latest_seq ({})",
                bundle.all_events.len(),
                bundle.run.latest_seq
            )
            .into());
        }
        for (idx, ev) in bundle.all_events.iter().enumerate() {
            let expected_seq = (idx as i64) + 1;
            if ev.seq != expected_seq {
                return Err(format!(
                    "non-contiguous simulation_event seq: expected {expected_seq}, got {}",
                    ev.seq
                )
                .into());
            }
        }
        if let Some(last_ev) = bundle.all_events.last() {
            if last_ev.state_hash != bundle.run.latest_state_hash {
                return Err(format!(
                    "latest simulation_event hash ({}) != simulation_run.latest_state_hash ({})",
                    last_ev.state_hash, bundle.run.latest_state_hash
                )
                .into());
            }
        }
        if let Some(ref cp) = bundle.latest_checkpoint {
            if cp.seq > bundle.run.latest_seq {
                return Err(format!(
                    "checkpoint seq ({}) exceeds run latest_seq ({})",
                    cp.seq, bundle.run.latest_seq
                )
                .into());
            }
            if cp.seq > 0 {
                let ev = &bundle.all_events[(cp.seq - 1) as usize];
                if ev.state_hash != cp.state_hash {
                    return Err(format!(
                        "checkpoint hash at seq {} ({}) != event hash ({})",
                        cp.seq, cp.state_hash, ev.state_hash
                    )
                    .into());
                }
            }
        }

        let (final_state, recomputed, replayer) = self.compute_recovery_full(bundle)?;

        let stored_by_seq: BTreeMap<i64, &str> = bundle
            .all_events
            .iter()
            .map(|e| (e.seq, e.state_hash.as_str()))
            .collect();
        for (seq, actual_hash) in recomputed {
            if seq == 0 {
                if let Some(ref cp) = bundle.latest_checkpoint {
                    if cp.seq == 0 && cp.state_hash != actual_hash {
                        return Err(format!(
                            "initial checkpoint hash mismatch: stored {}, recomputed {}",
                            cp.state_hash, actual_hash
                        )
                        .into());
                    }
                }
                continue;
            }
            match stored_by_seq.get(&seq) {
                None => {
                    return Err(format!("recomputed hash returned unknown event seq {seq}").into());
                }
                Some(&expected_hash) if expected_hash != actual_hash => {
                    return Err(format!(
                        "state_hash mismatch at seq {seq}: stored {expected_hash}, recomputed {actual_hash}"
                    )
                    .into());
                }
                Some(_) => {}
            }
        }
        Ok((final_state, replayer))
    }

    /// Mutating recovery check returning the undo replayer in a single bundle load & replay.
    /// Marks the run `'damaged'` in SQLite only on integrity failure (never on transient read errors).
    pub(crate) fn verify_and_recover_sim_state_with_replayer(
        &mut self,
        run_id: &str,
    ) -> Result<(SimulationRecoveryBundle, SimState, SimReplayer), ErrorEnvelope> {
        let bundle = self
            .store()
            .load_simulation_recovery_bundle(run_id)
            .map_err(|e| map_storage_err("RUN_NOT_FOUND", e))?;
        match self.verify_recovery_bundle_readonly(&bundle) {
            Ok((state, replayer)) => Ok((bundle, state, replayer)),
            Err(ReplayFailure::Storage(err)) => Err(err),
            Err(ReplayFailure::Integrity(reason)) => {
                if bundle.run.status != SimRunStatus::Damaged {
                    self.store_mut()
                        .mark_simulation_run_damaged(run_id, &reason)
                        .map_err(|e| map_storage_err("RECOVERY_VERIFY_FAILED", e))?;
                }
                Err(ErrorEnvelope::new(
                    "RUN_DAMAGED",
                    format!("训练会话状态校验失败，已标记为损坏（damaged）：{reason}"),
                    false,
                ))
            }
        }
    }

    /// Mutating recovery check: verifies checkpoint + event log hashes and atomically marks the run
    /// `'damaged'` in SQLite if any mismatch is detected (`S-43`, `M4-A-09`).
    pub(crate) fn verify_and_recover_sim_state(
        &mut self,
        run_id: &str,
    ) -> Result<(SimulationRecoveryBundle, SimState), ErrorEnvelope> {
        let (bundle, state, _) = self.verify_and_recover_sim_state_with_replayer(run_id)?;
        Ok((bundle, state))
    }

    /// Non-mutating verified state reader for read-only queries (`show`, `preview`, `report`, `replay`, AI snapshot tools).
    pub(crate) fn read_verified_sim_state(
        &self,
        run_id: &str,
    ) -> Result<(SimulationRecoveryBundle, SimState), ErrorEnvelope> {
        let bundle = self
            .store()
            .load_simulation_recovery_bundle(run_id)
            .map_err(|e| map_storage_err("RUN_NOT_FOUND", e))?;
        let (state, _) = self
            .verify_recovery_bundle_readonly(&bundle)
            .map_err(|failure| failure.into_envelope("RUN_DAMAGED"))?;
        Ok((bundle, state))
    }
}

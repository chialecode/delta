use chrono::Utc;
use delta_core::to_canonical_text;
use rusqlite::{params, Connection, OptionalExtension};
use rust_decimal::Decimal;

use crate::m4_schema::models::{
    map_fee_profile_row, FeeChargingTier, FeeCurrencyMode, FeeProfileVersionRecord,
    FeeRoundingMode, SlippageMode,
};
use crate::m4_schema::{
    BUILTIN_FEE_CRYPTO_RATE_ID, BUILTIN_FEE_US_EQUITY_PER_SHARE_ID, BUILTIN_FEE_ZERO_COST_ID,
};
use crate::storage::{SqliteStore, StorageError};

pub(crate) fn seed_builtin_fee_profiles(tx: &Connection) -> Result<(), StorageError> {
    let seeded_at = "2026-11-12T00:00:00+00:00";
    let builtins = [
        FeeProfileVersionRecord {
            id: BUILTIN_FEE_ZERO_COST_ID.to_string(),
            profile_key: "zero_cost".to_string(),
            version_num: 1,
            name: "零成本（场景假设）".to_string(),
            market_category: "all".to_string(),
            venue: None,
            notional_rate: Decimal::ZERO,
            per_unit_fee: Decimal::ZERO,
            fixed_fee_per_order: Decimal::ZERO,
            min_fee_per_order: None,
            max_fee_per_order: None,
            fee_currency_mode: FeeCurrencyMode::Quote,
            rounding_mode: FeeRoundingMode::HalfUp,
            rounding_scale: 2,
            charging_tier: FeeChargingTier::PerOrder,
            slippage_mode: SlippageMode::None,
            slippage_value: Decimal::ZERO,
            half_spread_bps: Decimal::ZERO,
            rules_json: r#"{"template":"zero_cost","scenario_assumption":true}"#.to_string(),
            is_builtin: true,
            is_scenario_assumption: true,
            official_source_url: None,
            official_retrieved_at: None,
            assumption_note:
                "Scenario assumption template: zero commission, zero spread, zero slippage baseline."
                    .to_string(),
            created_at: seeded_at.to_string(),
        },
        FeeProfileVersionRecord {
            id: BUILTIN_FEE_US_EQUITY_PER_SHARE_ID.to_string(),
            profile_key: "us_equity_per_share_example".to_string(),
            version_num: 1,
            name: "美股按股收费示例（场景假设）".to_string(),
            market_category: "us_equity".to_string(),
            venue: None,
            notional_rate: Decimal::ZERO,
            per_unit_fee: Decimal::new(5, 3), // 0.005 USD / share
            fixed_fee_per_order: Decimal::ZERO,
            min_fee_per_order: Some(Decimal::ONE), // 1.00 USD min per order
            max_fee_per_order: None,
            fee_currency_mode: FeeCurrencyMode::Quote,
            rounding_mode: FeeRoundingMode::HalfUp,
            rounding_scale: 2,
            charging_tier: FeeChargingTier::PerOrder,
            slippage_mode: SlippageMode::Ticks,
            slippage_value: Decimal::ONE,
            half_spread_bps: Decimal::from(2),
            rules_json: r#"{"template":"us_equity_per_share_example","scenario_assumption":true}"#
                .to_string(),
            is_builtin: true,
            is_scenario_assumption: true,
            official_source_url: None,
            official_retrieved_at: None,
            assumption_note:
                "Scenario assumption template (not an official broker fee schedule): 0.005 USD/share, 1.00 USD per-order minimum, 1 tick slippage, 2 bps half-spread."
                    .to_string(),
            created_at: seeded_at.to_string(),
        },
        FeeProfileVersionRecord {
            id: BUILTIN_FEE_CRYPTO_RATE_ID.to_string(),
            profile_key: "crypto_rate_example".to_string(),
            version_num: 1,
            name: "加密按成交额示例（场景假设）".to_string(),
            market_category: "crypto_spot".to_string(),
            venue: None,
            notional_rate: Decimal::new(1, 3), // 0.001 == 10 bps
            per_unit_fee: Decimal::ZERO,
            fixed_fee_per_order: Decimal::ZERO,
            min_fee_per_order: None,
            max_fee_per_order: None,
            fee_currency_mode: FeeCurrencyMode::Quote,
            rounding_mode: FeeRoundingMode::Up,
            rounding_scale: 8,
            charging_tier: FeeChargingTier::PerFill,
            slippage_mode: SlippageMode::Bps,
            slippage_value: Decimal::from(5),
            half_spread_bps: Decimal::from(5),
            rules_json: r#"{"template":"crypto_rate_example","scenario_assumption":true}"#
                .to_string(),
            is_builtin: true,
            is_scenario_assumption: true,
            official_source_url: None,
            official_retrieved_at: None,
            assumption_note:
                "Scenario assumption template (not an official exchange fee schedule): 10 bps notional fee, 5 bps slippage, 5 bps half-spread."
                    .to_string(),
            created_at: seeded_at.to_string(),
        },
    ];

    for fp in &builtins {
        insert_fee_profile_on_conn(tx, fp, true)?;
    }
    Ok(())
}

pub(crate) fn validate_fee_profile(fp: &FeeProfileVersionRecord) -> Result<(), StorageError> {
    if fp.id.trim().is_empty() || fp.profile_key.trim().is_empty() || fp.name.trim().is_empty() {
        return Err(StorageError::IntegrityFailed(
            "fee_profile_version id, profile_key, and name must be non-empty".into(),
        ));
    }
    if fp.version_num < 1 {
        return Err(StorageError::IntegrityFailed(
            "fee_profile_version version_num must be >= 1".into(),
        ));
    }
    if fp.rounding_scale > 18 {
        return Err(StorageError::IntegrityFailed(
            "fee_profile_version rounding_scale must be <= 18".into(),
        ));
    }
    if fp.notional_rate < Decimal::ZERO
        || fp.per_unit_fee < Decimal::ZERO
        || fp.fixed_fee_per_order < Decimal::ZERO
        || fp.slippage_value < Decimal::ZERO
        || fp.half_spread_bps < Decimal::ZERO
    {
        return Err(StorageError::IntegrityFailed(
            "fee_profile_version fee/slippage/spread components must be non-negative".into(),
        ));
    }
    if let Some(min_fee) = fp.min_fee_per_order {
        if min_fee < Decimal::ZERO {
            return Err(StorageError::IntegrityFailed(
                "fee_profile_version min_fee_per_order must be non-negative".into(),
            ));
        }
        if let Some(max_fee) = fp.max_fee_per_order {
            if max_fee < min_fee {
                return Err(StorageError::IntegrityFailed(
                    "fee_profile_version max_fee_per_order cannot be less than min_fee_per_order"
                        .into(),
                ));
            }
        }
    }
    if let Some(max_fee) = fp.max_fee_per_order {
        if max_fee < Decimal::ZERO {
            return Err(StorageError::IntegrityFailed(
                "fee_profile_version max_fee_per_order must be non-negative".into(),
            ));
        }
    }
    if fp.assumption_note.trim().is_empty() {
        return Err(StorageError::IntegrityFailed(
            "fee_profile_version assumption_note must be non-empty".into(),
        ));
    }
    if fp.is_builtin && !fp.is_scenario_assumption {
        let has_url = fp
            .official_source_url
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty());
        let has_date = fp
            .official_retrieved_at
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty());
        if !has_url || !has_date {
            return Err(StorageError::IntegrityFailed(
                "built-in platform fee preset requires official_source_url and official_retrieved_at (S-41)"
                    .into(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn insert_fee_profile_on_conn(
    conn: &Connection,
    fp: &FeeProfileVersionRecord,
    or_ignore: bool,
) -> Result<(), StorageError> {
    validate_fee_profile(fp)?;
    let sql = if or_ignore {
        "INSERT OR IGNORE INTO fee_profile_version (
            id, profile_key, version_num, name, market_category, venue,
            notional_rate, per_unit_fee, fixed_fee_per_order, min_fee_per_order, max_fee_per_order,
            fee_currency_mode, rounding_mode, rounding_scale, charging_tier,
            slippage_mode, slippage_value, half_spread_bps, rules_json,
            is_builtin, is_scenario_assumption, official_source_url, official_retrieved_at,
            assumption_note, created_at
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6,
            ?7, ?8, ?9, ?10, ?11,
            ?12, ?13, ?14, ?15,
            ?16, ?17, ?18, ?19,
            ?20, ?21, ?22, ?23,
            ?24, ?25
         )"
    } else {
        "INSERT INTO fee_profile_version (
            id, profile_key, version_num, name, market_category, venue,
            notional_rate, per_unit_fee, fixed_fee_per_order, min_fee_per_order, max_fee_per_order,
            fee_currency_mode, rounding_mode, rounding_scale, charging_tier,
            slippage_mode, slippage_value, half_spread_bps, rules_json,
            is_builtin, is_scenario_assumption, official_source_url, official_retrieved_at,
            assumption_note, created_at
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6,
            ?7, ?8, ?9, ?10, ?11,
            ?12, ?13, ?14, ?15,
            ?16, ?17, ?18, ?19,
            ?20, ?21, ?22, ?23,
            ?24, ?25
         )"
    };
    let mut stmt = conn.prepare_cached(sql)?;
    stmt.execute(params![
        fp.id,
        fp.profile_key,
        fp.version_num,
        fp.name,
        fp.market_category,
        fp.venue,
        to_canonical_text(fp.notional_rate),
        to_canonical_text(fp.per_unit_fee),
        to_canonical_text(fp.fixed_fee_per_order),
        fp.min_fee_per_order.map(to_canonical_text),
        fp.max_fee_per_order.map(to_canonical_text),
        fp.fee_currency_mode.as_str(),
        fp.rounding_mode.as_str(),
        fp.rounding_scale,
        fp.charging_tier.as_str(),
        fp.slippage_mode.as_str(),
        to_canonical_text(fp.slippage_value),
        to_canonical_text(fp.half_spread_bps),
        fp.rules_json,
        i32::from(fp.is_builtin),
        i32::from(fp.is_scenario_assumption),
        fp.official_source_url,
        fp.official_retrieved_at,
        fp.assumption_note,
        fp.created_at,
    ])?;
    Ok(())
}

impl SqliteStore {
    // ------------------------------------------------------------------------
    // 1. Fee Profile Versioning & Freezing (`S-41`, `M4-A-08`)
    // ------------------------------------------------------------------------

    pub fn insert_fee_profile_version(
        &mut self,
        fp: &FeeProfileVersionRecord,
    ) -> Result<FeeProfileVersionRecord, StorageError> {
        insert_fee_profile_on_conn(self.conn(), fp, false)?;
        self.get_fee_profile_version(&fp.id)
    }

    /// Editing an existing fee profile creates a new immutable version (`version_num = max + 1`)
    /// under the same `profile_key` while leaving existing versions untouched (`S-41`, `M4-A-08`).
    pub fn create_next_fee_profile_version(
        &mut self,
        mut draft: FeeProfileVersionRecord,
    ) -> Result<FeeProfileVersionRecord, StorageError> {
        if draft.profile_key.trim().is_empty() {
            return Err(StorageError::IntegrityFailed(
                "profile_key must be non-empty".into(),
            ));
        }
        let tx = self.conn_mut().savepoint()?;
        let next_ver: i64 = tx.query_row(
            "SELECT COALESCE(MAX(version_num), 0) + 1 FROM fee_profile_version WHERE profile_key = ?1",
            params![draft.profile_key],
            |r| r.get(0),
        )?;
        draft.version_num = next_ver;
        draft.is_builtin = false;
        draft.id = format!(
            "FPV:{}:v{}",
            draft.profile_key.trim().to_ascii_uppercase(),
            next_ver
        );
        if draft.created_at.trim().is_empty() {
            draft.created_at = Utc::now().to_rfc3339();
        }
        insert_fee_profile_on_conn(&tx, &draft, false)?;
        tx.commit()?;
        self.get_fee_profile_version(&draft.id)
    }

    /// Attempting to mutate an in-use or built-in `fee_profile_version` in place is strictly rejected (`M4-A-08`).
    /// An unreferenced non-builtin draft may be updated only before any `simulation_run` references it.
    pub fn mutate_fee_profile_version_in_place(
        &mut self,
        fp: &FeeProfileVersionRecord,
    ) -> Result<FeeProfileVersionRecord, StorageError> {
        validate_fee_profile(fp)?;
        let existing = self.get_fee_profile_version(&fp.id)?;
        if existing.is_builtin {
            return Err(StorageError::IntegrityFailed(format!(
                "built-in fee_profile_version '{}' is immutable; create a new profile version instead",
                fp.id
            )));
        }
        let in_use: bool = self.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM simulation_run WHERE fee_profile_version_id = ?1)",
            params![fp.id],
            |r| r.get(0),
        )?;
        if in_use {
            return Err(StorageError::IntegrityFailed(format!(
                "fee_profile_version '{}' is referenced by an existing simulation_run and is immutable; create a new version instead",
                fp.id
            )));
        }
        self.conn().execute(
            "UPDATE fee_profile_version
             SET name = ?2,
                 market_category = ?3,
                 venue = ?4,
                 notional_rate = ?5,
                 per_unit_fee = ?6,
                 fixed_fee_per_order = ?7,
                 min_fee_per_order = ?8,
                 max_fee_per_order = ?9,
                 fee_currency_mode = ?10,
                 rounding_mode = ?11,
                 rounding_scale = ?12,
                 charging_tier = ?13,
                 slippage_mode = ?14,
                 slippage_value = ?15,
                 half_spread_bps = ?16,
                 rules_json = ?17,
                 is_scenario_assumption = ?18,
                 official_source_url = ?19,
                 official_retrieved_at = ?20,
                 assumption_note = ?21
             WHERE id = ?1",
            params![
                fp.id,
                fp.name,
                fp.market_category,
                fp.venue,
                to_canonical_text(fp.notional_rate),
                to_canonical_text(fp.per_unit_fee),
                to_canonical_text(fp.fixed_fee_per_order),
                fp.min_fee_per_order.map(to_canonical_text),
                fp.max_fee_per_order.map(to_canonical_text),
                fp.fee_currency_mode.as_str(),
                fp.rounding_mode.as_str(),
                fp.rounding_scale,
                fp.charging_tier.as_str(),
                fp.slippage_mode.as_str(),
                to_canonical_text(fp.slippage_value),
                to_canonical_text(fp.half_spread_bps),
                fp.rules_json,
                i32::from(fp.is_scenario_assumption),
                fp.official_source_url,
                fp.official_retrieved_at,
                fp.assumption_note,
            ],
        )?;
        self.get_fee_profile_version(&fp.id)
    }

    pub fn delete_fee_profile_version(&mut self, id: &str) -> Result<(), StorageError> {
        let existing = self.get_fee_profile_version(id)?;
        if existing.is_builtin {
            return Err(StorageError::IntegrityFailed(format!(
                "built-in fee_profile_version '{id}' cannot be deleted"
            )));
        }
        let in_use: bool = self.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM simulation_run WHERE fee_profile_version_id = ?1)",
            params![id],
            |r| r.get(0),
        )?;
        if in_use {
            return Err(StorageError::IntegrityFailed(format!(
                "fee_profile_version '{id}' is referenced by a simulation_run and cannot be deleted"
            )));
        }
        self.conn()
            .execute("DELETE FROM fee_profile_version WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn get_fee_profile_version(
        &self,
        id: &str,
    ) -> Result<FeeProfileVersionRecord, StorageError> {
        let mut stmt = self.conn().prepare_cached(
            "SELECT id, profile_key, version_num, name, market_category, venue,
                    notional_rate, per_unit_fee, fixed_fee_per_order, min_fee_per_order, max_fee_per_order,
                    fee_currency_mode, rounding_mode, rounding_scale, charging_tier,
                    slippage_mode, slippage_value, half_spread_bps, rules_json,
                    is_builtin, is_scenario_assumption, official_source_url, official_retrieved_at,
                    assumption_note, created_at
             FROM fee_profile_version
             WHERE id = ?1",
        )?;
        let row = stmt
            .query_row(params![id], |r| Ok(map_fee_profile_row(r)))
            .optional()?;
        match row {
            Some(res) => res,
            None => Err(StorageError::IntegrityFailed(format!(
                "fee_profile_version '{id}' not found"
            ))),
        }
    }

    pub fn list_fee_profile_versions(&self) -> Result<Vec<FeeProfileVersionRecord>, StorageError> {
        let mut stmt = self.conn().prepare_cached(
            "SELECT id, profile_key, version_num, name, market_category, venue,
                    notional_rate, per_unit_fee, fixed_fee_per_order, min_fee_per_order, max_fee_per_order,
                    fee_currency_mode, rounding_mode, rounding_scale, charging_tier,
                    slippage_mode, slippage_value, half_spread_bps, rules_json,
                    is_builtin, is_scenario_assumption, official_source_url, official_retrieved_at,
                    assumption_note, created_at
             FROM fee_profile_version
             ORDER BY is_builtin DESC, profile_key ASC, version_num DESC",
        )?;
        let rows = stmt.query_map([], |r| Ok(map_fee_profile_row(r)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row??);
        }
        Ok(out)
    }
}

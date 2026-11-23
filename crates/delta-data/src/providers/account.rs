use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, TimeZone, Utc};
use delta_core::parse_decimal_lossless;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::providers::ProviderError;
use crate::registry::InstrumentRegistry;

// ============================================================================
// 10. Read-Only `AccountProvider` Trait & Fixture Implementations (AC-17)
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountBalanceSnapshotDto {
    pub asset_id: String,
    pub quantity: Decimal,
    pub as_of: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountSyncPageDto {
    pub provider: String,
    pub account_id: String,
    pub next_cursor: Option<String>,
    pub balances: Vec<AccountBalanceSnapshotDto>,
    #[serde(default)]
    pub events: Vec<delta_core::LedgerEventInput>,
}

pub trait AccountProvider {
    fn provider_name(&self) -> &str;
    fn fetch_account_page(
        &self,
        account_id: &str,
        cursor: Option<&str>,
    ) -> Result<AccountSyncPageDto, ProviderError>;
}

pub struct FixtureAccountProvider {
    pub provider: String,
    pub pages: Vec<AccountSyncPageDto>,
}

fn json_id_string(row: &serde_json::Value, key: &str) -> String {
    row.get(key)
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .unwrap_or_else(|| v.to_string())
        })
        .unwrap_or_default()
}

impl FixtureAccountProvider {
    /// Normalize synthetic recordings of the native data arrays from balance,
    /// fill, deposit and withdrawal endpoints. No keys or network calls are used.
    pub fn from_recorded_response(
        provider: &str,
        account_id: &str,
        body: &str,
    ) -> Result<Self, ProviderError> {
        let error = |detail: String| ProviderError::Parse {
            provider: provider.into(),
            detail,
        };
        if !matches!(provider, "okx" | "binance") {
            return Err(error("unsupported recorded account provider".into()));
        }
        #[derive(Deserialize)]
        struct Recording {
            as_of: DateTime<Utc>,
            balances: Vec<serde_json::Value>,
            fills: Vec<serde_json::Value>,
            deposits: Vec<serde_json::Value>,
            withdrawals: Vec<serde_json::Value>,
        }
        let recording: Recording = serde_json::from_str(body).map_err(|e| error(e.to_string()))?;
        let string = |row: &serde_json::Value, key: &str| -> Result<String, ProviderError> {
            row.get(key)
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .ok_or_else(|| error(format!("missing string field {key}")))
        };
        let decimal = |row: &serde_json::Value, key: &str| -> Result<Decimal, ProviderError> {
            parse_decimal_lossless(&string(row, key)?).map_err(|e| error(e.to_string()))
        };
        let registry = InstrumentRegistry::new_seeded();
        let mut balances = Vec::new();
        for row in recording.balances {
            let (asset, quantity) = if provider == "okx" {
                (string(&row, "ccy")?, decimal(&row, "cashBal")?)
            } else {
                (
                    string(&row, "asset")?,
                    decimal(&row, "free")? + decimal(&row, "locked")?,
                )
            };
            if quantity < Decimal::ZERO {
                return Err(error("negative balance rejected in spot recording".into()));
            }
            balances.push(AccountBalanceSnapshotDto {
                asset_id: asset,
                quantity,
                as_of: recording.as_of,
            });
        }
        let mut csv = csv::Writer::from_writer(Vec::new());
        csv.write_record([
            "account",
            "event_type",
            "occurred_at",
            "timezone",
            "symbol",
            "side",
            "quantity",
            "price",
            "quote_currency",
            "fee_amount",
            "fee_currency",
            "source_id",
            "cost_known",
            "counterparty_account",
        ])
        .map_err(|e| error(e.to_string()))?;
        for row in recording.fills {
            let raw_symbol = string(
                &row,
                if provider == "okx" {
                    "instId"
                } else {
                    "symbol"
                },
            )?;
            let instrument = registry
                .resolve_symbol(Some(provider), &raw_symbol)
                .ok_or_else(|| error("unknown fill market in recording".into()))?;
            let market = registry
                .instruments
                .get(&instrument)
                .ok_or_else(|| error(format!("missing instrument metadata for {instrument}")))?;
            let side = if provider == "okx" {
                string(&row, "side")?
            } else {
                if row
                    .get("isBuyer")
                    .and_then(|v| v.as_bool())
                    .ok_or_else(|| error("missing isBuyer".into()))?
                {
                    "buy"
                } else {
                    "sell"
                }
                .into()
            };
            if !matches!(side.as_str(), "buy" | "sell") {
                return Err(error("invalid fill side".into()));
            }
            let time = if provider == "okx" {
                string(&row, "ts")?
                    .parse::<i64>()
                    .map_err(|e| error(e.to_string()))?
            } else {
                row.get("time")
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| error("missing fill time".into()))?
            };
            let at = Utc
                .timestamp_millis_opt(time)
                .single()
                .ok_or_else(|| error("invalid fill time".into()))?;
            if at > recording.as_of {
                return Err(error("fill occurs after recording cutoff".into()));
            }
            let raw_fee = decimal(
                &row,
                if provider == "okx" {
                    "fee"
                } else {
                    "commission"
                },
            )?;
            if (provider == "okx" && raw_fee > Decimal::ZERO)
                || (provider == "binance" && raw_fee < Decimal::ZERO)
            {
                return Err(error("rebate requires explicit income recording".into()));
            }
            let fee = if provider == "okx" { -raw_fee } else { raw_fee };
            let id = json_id_string(&row, if provider == "okx" { "tradeId" } else { "id" });
            csv.write_record([
                account_id,
                side.as_str(),
                &at.to_rfc3339(),
                "UTC",
                &instrument,
                side.as_str(),
                &string(&row, if provider == "okx" { "fillSz" } else { "qty" })?,
                &string(&row, if provider == "okx" { "fillPx" } else { "price" })?,
                &market.quote_asset_id,
                &fee.to_string(),
                &string(
                    &row,
                    if provider == "okx" {
                        "feeCcy"
                    } else {
                        "commissionAsset"
                    },
                )?,
                &id,
                "true",
                "",
            ])
            .map_err(|e| error(e.to_string()))?;
        }
        for (outgoing, rows) in [(false, recording.deposits), (true, recording.withdrawals)] {
            for row in rows {
                let completed = if provider == "okx" {
                    row.get("state").and_then(|v| v.as_str()) == Some("2")
                } else {
                    row.get("status").and_then(|v| v.as_u64()) == Some(if outgoing { 6 } else { 1 })
                };
                if !completed {
                    continue;
                }
                let currency = string(&row, if provider == "okx" { "ccy" } else { "coin" })?;
                let quantity = decimal(&row, if provider == "okx" { "amt" } else { "amount" })?;
                let timestamp = string(
                    &row,
                    if provider == "okx" {
                        "ts"
                    } else if outgoing {
                        "applyTime"
                    } else {
                        "insertTime"
                    },
                )
                .or_else(|_| {
                    row.get("insertTime")
                        .and_then(|v| v.as_i64())
                        .map(|t| t.to_string())
                        .ok_or_else(|| error("missing funding timestamp".into()))
                })?;
                let at = timestamp
                    .parse::<i64>()
                    .ok()
                    .and_then(|t| Utc.timestamp_millis_opt(t).single())
                    .or_else(|| {
                        DateTime::parse_from_rfc3339(&timestamp)
                            .ok()
                            .map(|t| t.with_timezone(&Utc))
                    })
                    .or_else(|| {
                        if provider == "binance" {
                            chrono::NaiveDateTime::parse_from_str(&timestamp, "%Y-%m-%d %H:%M:%S")
                                .ok()
                                .map(|t| t.and_utc())
                        } else {
                            None
                        }
                    })
                    .ok_or_else(|| error("invalid recorded funding timestamp".into()))?;
                if at > recording.as_of {
                    return Err(error("funding occurs after recording cutoff".into()));
                }
                let crypto = !delta_core::is_fiat_currency(&currency)
                    && !matches!(currency.as_str(), "USDT" | "USDC");
                let instrument = if crypto {
                    registry
                        .instruments
                        .values()
                        .find(|i| {
                            i.venue.eq_ignore_ascii_case(provider)
                                && i.base_asset_id == currency
                                && i.quote_asset_id == "USDT"
                        })
                        .map(|i| i.id.clone())
                        .ok_or_else(|| error("unknown funding asset market".into()))?
                } else {
                    currency.clone()
                };
                let event = match (outgoing, crypto) {
                    (true, true) => "position_transfer_out",
                    (false, true) => "opening_balance",
                    (true, false) => "withdrawal",
                    (false, false) => "deposit",
                };
                let quote = if crypto { "USD" } else { &currency };
                let id_key = if outgoing {
                    if provider == "okx" {
                        "wdId"
                    } else {
                        "id"
                    }
                } else {
                    "txId"
                };
                let id = json_id_string(&row, id_key);
                csv.write_record([
                    account_id,
                    event,
                    &at.to_rfc3339(),
                    "UTC",
                    &instrument,
                    "",
                    &quantity.to_string(),
                    "",
                    quote,
                    "0",
                    quote,
                    &id,
                    if crypto && !outgoing { "false" } else { "true" },
                    if outgoing && crypto {
                        "external:recorded-wallet"
                    } else {
                        ""
                    },
                ])
                .map_err(|e| error(e.to_string()))?;
                if outgoing {
                    let fee_key = if provider == "okx" {
                        "fee"
                    } else {
                        "transactionFee"
                    };
                    if let Some(fee) = row.get(fee_key).and_then(|v| v.as_str()) {
                        let value =
                            parse_decimal_lossless(fee).map_err(|e| error(e.to_string()))?;
                        if value < Decimal::ZERO {
                            return Err(error("negative withdrawal fee".into()));
                        }
                        if value > Decimal::ZERO {
                            csv.write_record([
                                account_id,
                                "fee_only",
                                &at.to_rfc3339(),
                                "UTC",
                                &currency,
                                "",
                                fee,
                                "",
                                &currency,
                                "0",
                                &currency,
                                &format!("{id}:fee"),
                                "true",
                                "",
                            ])
                            .map_err(|e| error(e.to_string()))?;
                        }
                    }
                }
            }
        }
        let bytes = csv.into_inner().map_err(|e| error(e.to_string()))?;
        let text = String::from_utf8(bytes).map_err(|e| error(e.to_string()))?;
        let batch = format!("recording:{}", hex::encode(Sha256::digest(body.as_bytes())));
        let rows = crate::importers::parse_csv_by_preset(
            crate::importers::StatementPreset::GenericCsv,
            account_id,
            &batch,
            &text,
            &registry,
        )
        .map_err(|e| error(e.to_string()))?;
        let mut events = Vec::new();
        for row in rows {
            events.push(row.normalized.ok_or_else(|| {
                error(
                    row.error_reason
                        .unwrap_or_else(|| "invalid recorded event".into()),
                )
            })?);
        }
        Ok(Self {
            provider: provider.into(),
            pages: vec![AccountSyncPageDto {
                provider: provider.into(),
                account_id: account_id.into(),
                next_cursor: None,
                balances,
                events,
            }],
        })
    }
}

impl AccountProvider for FixtureAccountProvider {
    fn provider_name(&self) -> &str {
        &self.provider
    }

    fn fetch_account_page(
        &self,
        account_id: &str,
        cursor: Option<&str>,
    ) -> Result<AccountSyncPageDto, ProviderError> {
        let idx: usize = cursor
            .map(|c| {
                c.parse().map_err(|_| ProviderError::Parse {
                    provider: self.provider.clone(),
                    detail: "invalid cursor".into(),
                })
            })
            .transpose()?
            .unwrap_or(0);
        let page = self
            .pages
            .get(idx)
            .cloned()
            .ok_or_else(|| ProviderError::Parse {
                provider: self.provider.clone(),
                detail: format!("no page for cursor {idx}"),
            })?;
        if page.account_id != account_id {
            return Err(ProviderError::Parse {
                provider: self.provider.clone(),
                detail: "account scope mismatch".into(),
            });
        }
        Ok(page)
    }
}

fn lookup_expected_balance(
    account_id: &str,
    asset_upper: &str,
    ledger_cash: &BTreeMap<(String, String), Decimal>,
    ledger_holdings: &BTreeMap<(String, String), delta_core::HoldingProjection>,
    reg: &InstrumentRegistry,
) -> (Decimal, Option<String>) {
    if let Some(c) = ledger_cash.get(&(account_id.to_string(), asset_upper.to_string())) {
        return (*c, Some(asset_upper.to_string()));
    }
    if let Some(h) = ledger_holdings.get(&(account_id.to_string(), asset_upper.to_string())) {
        return (h.quantity, Some(asset_upper.to_string()));
    }
    if let Some(resolved) = reg.resolve_symbol(None, asset_upper) {
        if let Some(h) = ledger_holdings.get(&(account_id.to_string(), resolved.clone())) {
            return (h.quantity, Some(resolved));
        }
    }
    (Decimal::ZERO, None)
}

/// Compare observed account balance snapshots against ledger projected balances and emit
/// `ReconciliationIssueRecord`s without mutating ledger history (AC-17 / FR-DATA-05).
/// Note: supports single-asset snapshots (only checks assets included in `observed_snapshots`).
pub fn reconcile_balances(
    account_id: &str,
    ledger_cash: &BTreeMap<(String, String), Decimal>,
    ledger_holdings: &BTreeMap<(String, String), delta_core::HoldingProjection>,
    observed_snapshots: &[AccountBalanceSnapshotDto],
) -> Vec<crate::storage::ReconciliationIssueRecord> {
    let mut issues = Vec::new();
    let reg = InstrumentRegistry::new_seeded();

    for snap in observed_snapshots {
        let asset_upper = snap.asset_id.to_ascii_uppercase();
        let (expected, _) =
            lookup_expected_balance(account_id, &asset_upper, ledger_cash, ledger_holdings, &reg);

        let diff = (snap.quantity - expected).normalize();
        if !diff.is_zero() {
            issues.push(crate::storage::ReconciliationIssueRecord {
                id: format!(
                    "RECON:{account_id}:{asset_upper}:{}",
                    snap.as_of.timestamp()
                ),
                account_id: account_id.to_string(),
                asset_id: asset_upper,
                as_of: snap.as_of.to_rfc3339(),
                expected: delta_core::to_canonical_text(expected),
                observed: delta_core::to_canonical_text(snap.quantity),
                difference: delta_core::to_canonical_text(diff),
                reason:
                    "observed balance differs from ledger projection; ledger preserved unmodified"
                        .to_string(),
                resolution_event_id: None,
            });
        }
    }
    issues
}

/// Full two-way balance reconciliation for a complete account snapshot using the caller's
/// `InstrumentRegistry`: detects both observed-vs-ledger mismatches AND non-zero ledger
/// cash/holdings absent from `observed_snapshots`.
pub fn reconcile_balances_full_snapshot(
    account_id: &str,
    ledger_cash: &BTreeMap<(String, String), Decimal>,
    ledger_holdings: &BTreeMap<(String, String), delta_core::HoldingProjection>,
    observed_snapshots: &[AccountBalanceSnapshotDto],
    as_of: DateTime<Utc>,
    registry: &InstrumentRegistry,
) -> Vec<crate::storage::ReconciliationIssueRecord> {
    let mut issues = Vec::new();
    let mut seen_ledger_keys: BTreeSet<String> = BTreeSet::new();

    for snap in observed_snapshots {
        let asset_upper = snap.asset_id.to_ascii_uppercase();
        let (expected, matched_key) = lookup_expected_balance(
            account_id,
            &asset_upper,
            ledger_cash,
            ledger_holdings,
            registry,
        );
        seen_ledger_keys.insert(asset_upper.clone());
        if let Some(k) = matched_key {
            seen_ledger_keys.insert(k);
        }
        let diff = (snap.quantity - expected).normalize();
        if !diff.is_zero() {
            issues.push(crate::storage::ReconciliationIssueRecord {
                id: format!(
                    "RECON:{account_id}:{asset_upper}:{}",
                    snap.as_of.timestamp()
                ),
                account_id: account_id.to_string(),
                asset_id: asset_upper,
                as_of: snap.as_of.to_rfc3339(),
                expected: delta_core::to_canonical_text(expected),
                observed: delta_core::to_canonical_text(snap.quantity),
                difference: delta_core::to_canonical_text(diff),
                reason:
                    "observed balance differs from ledger projection; ledger preserved unmodified"
                        .to_string(),
                resolution_event_id: None,
            });
        }
    }

    for ((acc, ccy), qty) in ledger_cash {
        if acc != account_id || qty.is_zero() || seen_ledger_keys.contains(ccy) {
            continue;
        }
        issues.push(crate::storage::ReconciliationIssueRecord {
            id: format!("RECON:{account_id}:{ccy}:{}", as_of.timestamp()),
            account_id: account_id.to_string(),
            asset_id: ccy.clone(),
            as_of: as_of.to_rfc3339(),
            expected: delta_core::to_canonical_text(*qty),
            observed: "0".to_string(),
            difference: delta_core::to_canonical_text(-*qty),
            reason: "ledger cash balance missing from complete observed snapshot; ledger preserved unmodified"
                .to_string(),
            resolution_event_id: None,
        });
    }
    for ((acc, asset_id), holding) in ledger_holdings {
        if acc != account_id || holding.quantity.is_zero() || seen_ledger_keys.contains(asset_id) {
            continue;
        }
        issues.push(crate::storage::ReconciliationIssueRecord {
            id: format!("RECON:{account_id}:{asset_id}:{}", as_of.timestamp()),
            account_id: account_id.to_string(),
            asset_id: asset_id.clone(),
            as_of: as_of.to_rfc3339(),
            expected: delta_core::to_canonical_text(holding.quantity),
            observed: "0".to_string(),
            difference: delta_core::to_canonical_text(-holding.quantity),
            reason: "ledger holding missing from complete observed snapshot; ledger preserved unmodified"
                .to_string(),
            resolution_event_id: None,
        });
    }
    issues
}

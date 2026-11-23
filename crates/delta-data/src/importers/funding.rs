use delta_core::{parse_decimal_lossless, EconomicEventType, LedgerEventInput};
use rust_decimal::Decimal;

use crate::importers::{
    normalize_csv_header, parse_optional_fee, parse_timestamp_with_tz, verify_csv_headers,
    wrap_preview_row, ImportError, PreviewRow, StatementPreset,
};
use crate::registry::InstrumentRegistry;

/// Funding CSV presets for Futu, OKX, and Binance.
/// Preserves per-row validation diagnostics (`PreviewRow { validation_state: "error", ... }`)
/// instead of aborting the whole file, honors Futu `symbol`/`代码`, and scopes venue lookup strictly.
pub(crate) fn parse_funding_csv(
    preset: StatementPreset,
    account_id: &str,
    batch_id: &str,
    content: &str,
    registry: &InstrumentRegistry,
) -> Result<Vec<PreviewRow>, ImportError> {
    let mut reader = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .from_reader(content.as_bytes());
    let headers = reader.headers()?.clone();
    let zh = headers
        .iter()
        .map(normalize_csv_header)
        .any(|h| h.contains("币种"));
    let (required, allowed): (&[&str], &[&str]) = match (preset, zh) {
        (StatementPreset::FutuCsv, false) => (
            &["type", "time", "amount", "currency"],
            &[
                "account",
                "type",
                "time",
                "amount",
                "currency",
                "fee",
                "transactionid",
                "counterpartyaccount",
                "symbol",
            ],
        ),
        (StatementPreset::FutuCsv, true) => (
            &["类型", "时间", "金额", "币种"],
            &[
                "账户",
                "类型",
                "时间",
                "金额",
                "币种",
                "手续费",
                "流水号",
                "对方账户",
                "代码",
            ],
        ),
        (StatementPreset::OkxCsv, false) => (
            &["timestamp", "type", "currency", "amount"],
            &[
                "account",
                "billid",
                "timestamp",
                "type",
                "currency",
                "amount",
                "fee",
                "feecurrency",
                "counterpartyaccount",
            ],
        ),
        (StatementPreset::OkxCsv, true) => (
            &["时间", "类型", "币种", "数量"],
            &[
                "账户",
                "账单id",
                "时间",
                "类型",
                "币种",
                "数量",
                "手续费",
                "手续费币种",
                "对方账户",
            ],
        ),
        (StatementPreset::BinanceCsv, _) => (
            &["date(utc)", "type", "coin", "amount"],
            &[
                "account",
                "date(utc)",
                "type",
                "coin",
                "amount",
                "fee",
                "fee coin",
                "transactionid",
                "counterpartyaccount",
            ],
        ),
        _ => unreachable!(),
    };
    let map = verify_csv_headers(preset, &headers, required, allowed)?;
    let mut out = Vec::new();
    let mut row_no = 0usize;

    for record in reader.records() {
        let record = record?;
        let raw = serde_json::to_string(&record.iter().collect::<Vec<_>>()).unwrap();
        let get = |key: &str| {
            map.get(key)
                .and_then(|&i| record.get(i))
                .unwrap_or("")
                .trim()
        };
        let choose = |en: &str, cn: &str| if zh { get(cn) } else { get(en) };

        let parsed = (|| -> Result<(LedgerEventInput, Option<LedgerEventInput>), String> {
            let currency = match preset {
                StatementPreset::BinanceCsv => get("coin"),
                _ => choose("currency", "币种"),
            }
            .to_ascii_uppercase();
            if currency.is_empty() {
                return Err("missing currency".into());
            }
            let amount_str = choose(
                "amount",
                if preset == StatementPreset::FutuCsv {
                    "金额"
                } else {
                    "数量"
                },
            );
            let quantity = parse_decimal_lossless(amount_str).map_err(|e| e.to_string())?;
            let timestamp = match preset {
                StatementPreset::BinanceCsv => get("date(utc)"),
                StatementPreset::OkxCsv => choose("timestamp", "时间"),
                _ => choose("time", "时间"),
            };
            let timezone = if preset == StatementPreset::FutuCsv {
                "America/New_York"
            } else {
                "UTC"
            };
            let occurred_at = parse_timestamp_with_tz(timestamp, timezone)?;
            let kind = choose("type", "类型").to_ascii_lowercase();
            let crypto = !delta_core::is_fiat_currency(&currency)
                && !matches!(currency.as_str(), "USDT" | "USDC");

            let ev_type = match kind.as_str() {
                "deposit" | "充值" | "入金" => {
                    if crypto {
                        EconomicEventType::OpeningBalance
                    } else {
                        EconomicEventType::Deposit
                    }
                }
                "withdrawal" | "withdraw" | "提现" | "提币" | "出金" => {
                    if crypto {
                        EconomicEventType::PositionTransferOut
                    } else {
                        EconomicEventType::Withdrawal
                    }
                }
                "transfer_out" | "转出" => {
                    if crypto {
                        EconomicEventType::PositionTransferOut
                    } else {
                        EconomicEventType::TransferOut
                    }
                }
                "transfer_in" | "转入" => {
                    if crypto {
                        EconomicEventType::PositionTransferIn
                    } else {
                        EconomicEventType::TransferIn
                    }
                }
                "dividend" | "分红" => EconomicEventType::CashDividend,
                "interest" | "利息" => EconomicEventType::Interest,
                "fee" | "手续费" => EconomicEventType::FeeOnly,
                other => return Err(format!("unsupported funding type '{other}'")),
            };

            if quantity.is_zero() {
                return Err("quantity must be non-zero".into());
            }
            if quantity <= Decimal::ZERO
                && !matches!(
                    ev_type,
                    EconomicEventType::Withdrawal
                        | EconomicEventType::PositionTransferOut
                        | EconomicEventType::TransferOut
                        | EconomicEventType::FeeOnly
                )
            {
                return Err("funding inflow must be positive".into());
            }
            if crypto
                && !matches!(
                    ev_type,
                    EconomicEventType::OpeningBalance
                        | EconomicEventType::PositionTransferIn
                        | EconomicEventType::PositionTransferOut
                        | EconomicEventType::FeeOnly
                )
            {
                return Err("crypto income requires an explicit priced ledger event".into());
            }

            let (inst_id, asset_id) = match preset {
                StatementPreset::FutuCsv => {
                    let sym_col = choose("symbol", "代码");
                    if !sym_col.is_empty() {
                        let resolved = registry.resolve_symbol(Some("futu"), sym_col);
                        let asset = resolved
                            .as_ref()
                            .and_then(|id| registry.instruments.get(id))
                            .map(|i| i.base_asset_id.clone())
                            .unwrap_or_else(|| sym_col.to_ascii_uppercase());
                        (resolved, asset)
                    } else if crypto {
                        return Err(format!(
                            "non-fiat funding currency '{currency}' in Futu CSV requires a symbol"
                        ));
                    } else {
                        (None, currency.clone())
                    }
                }
                StatementPreset::OkxCsv | StatementPreset::BinanceCsv => {
                    let provider_venue = if preset == StatementPreset::OkxCsv {
                        "OKX"
                    } else {
                        "BINANCE"
                    };
                    if crypto {
                        let market = registry
                            .instruments
                            .values()
                            .find(|i| {
                                i.venue == provider_venue
                                    && i.base_asset_id == currency
                                    && i.quote_asset_id == "USDT"
                            })
                            .ok_or_else(|| {
                                format!(
                                    "register a {provider_venue}:{currency}/USDT market before importing funding"
                                )
                            })?;
                        (Some(market.id.clone()), market.base_asset_id.clone())
                    } else {
                        (None, currency.clone())
                    }
                }
                _ => (None, currency.clone()),
            };

            let source = match preset {
                StatementPreset::OkxCsv => choose("billid", "账单id"),
                _ => choose("transactionid", "流水号"),
            };
            let source_ref = if source.is_empty() {
                None
            } else {
                Some(source.to_string())
            };
            let cp = choose("counterpartyaccount", "对方账户");
            let counterparty = if cp.is_empty() && ev_type == EconomicEventType::PositionTransferOut
            {
                Some(format!("external:{}", preset.as_str()))
            } else if cp.is_empty() {
                None
            } else {
                Some(cp.to_string())
            };
            let quote = if crypto && ev_type != EconomicEventType::FeeOnly {
                "USD".to_string()
            } else {
                currency.clone()
            };
            let cost_known = !(crypto && ev_type == EconomicEventType::OpeningBalance);

            let fee = parse_optional_fee(
                choose("fee", "手续费"),
                "negative funding fee needs an explicit rebate income row",
            )?;

            let main_row_no = row_no + 1;
            let main_ev_id = format!("EV:{batch_id}:{main_row_no}");
            let main_ev = LedgerEventInput {
                event_id: main_ev_id.clone(),
                account_id: account_id.to_string(),
                event_type: ev_type,
                occurred_at,
                sequence_no: main_row_no as i64,
                asset_id,
                instrument_id: inst_id,
                quantity: quantity.abs(),
                price: None,
                quote_currency: quote.clone(),
                fee_amount: Decimal::ZERO,
                fee_currency: quote,
                cost_known,
                counterparty_account_id: counterparty,
                transfer_group_id: None,
                correction_group_id: Some(format!("CG:{main_ev_id}")),
                revision: 1,
                active_in_correction_group: true,
                source_ref,
            };

            let fee_ev = if fee > Decimal::ZERO {
                let fee_ccy_raw = match preset {
                    StatementPreset::BinanceCsv => get("fee coin"),
                    _ => choose("feecurrency", "手续费币种"),
                };
                let fee_currency = if fee_ccy_raw.is_empty() {
                    currency
                } else {
                    fee_ccy_raw.to_ascii_uppercase()
                };
                let fee_row_no = main_row_no + 1;
                let fee_ev_id = format!("EV:{batch_id}:{fee_row_no}");
                Some(LedgerEventInput {
                    event_id: fee_ev_id.clone(),
                    account_id: account_id.to_string(),
                    event_type: EconomicEventType::FeeOnly,
                    occurred_at,
                    sequence_no: fee_row_no as i64,
                    asset_id: fee_currency.clone(),
                    instrument_id: None,
                    quantity: fee,
                    price: None,
                    quote_currency: fee_currency.clone(),
                    fee_amount: Decimal::ZERO,
                    fee_currency,
                    cost_known: true,
                    counterparty_account_id: None,
                    transfer_group_id: None,
                    correction_group_id: Some(format!("CG:{fee_ev_id}")),
                    revision: 1,
                    active_in_correction_group: true,
                    source_ref: if source.is_empty() {
                        None
                    } else {
                        Some(format!("{source}:fee"))
                    },
                })
            } else {
                None
            };

            Ok((main_ev, fee_ev))
        })();

        match parsed {
            Ok((main_ev, fee_ev)) => {
                row_no += 1;
                out.push(wrap_preview_row(row_no, batch_id, raw.clone(), Ok(main_ev)));
                if let Some(fev) = fee_ev {
                    row_no += 1;
                    out.push(wrap_preview_row(row_no, batch_id, raw, Ok(fev)));
                }
            }
            Err(reason) => {
                row_no += 1;
                out.push(wrap_preview_row(row_no, batch_id, raw, Err(reason)));
            }
        }
    }

    Ok(out)
}

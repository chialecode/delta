use std::collections::HashSet;

use delta_core::{parse_decimal_lossless, EconomicEventType, LedgerEventInput};
use rust_decimal::Decimal;

use crate::importers::funding::parse_funding_csv;
use crate::importers::{
    normalize_csv_header, parse_optional_fee, parse_timestamp_with_tz, verify_csv_headers,
    wrap_preview_row, ImportError, PreviewRow, StatementPreset,
};
use crate::registry::InstrumentRegistry;

pub(crate) fn parse_csv_by_preset(
    preset: StatementPreset,
    account_id: &str,
    batch_id: &str,
    raw_content: &str,
    reg: &InstrumentRegistry,
) -> Result<Vec<PreviewRow>, ImportError> {
    let mut rdr = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .from_reader(raw_content.as_bytes());
    let headers = rdr.headers()?.clone();
    let normalized: HashSet<String> = headers.iter().map(normalize_csv_header).collect();
    if let Some(account_column) = headers
        .iter()
        .position(|h| matches!(normalize_csv_header(h).as_str(), "account" | "账户"))
    {
        let mut source_accounts = HashSet::new();
        let mut scan = csv::ReaderBuilder::new()
            .trim(csv::Trim::All)
            .from_reader(raw_content.as_bytes());
        for row in scan.records() {
            let row = row?;
            if let Some(source) = row.get(account_column).filter(|s| !s.is_empty()) {
                source_accounts.insert(source.to_string());
            }
        }
        if source_accounts.len() > 1 {
            return Err(ImportError::InvalidHeader {
                preset: preset.as_str().into(),
                detail: "one import file must belong to one bound account".into(),
            });
        }
    }
    let funding = match preset {
        StatementPreset::FutuCsv => normalized.contains("amount") || normalized.contains("金额"),
        StatementPreset::OkxCsv => normalized.contains("currency") || normalized.contains("币种"),
        StatementPreset::BinanceCsv => normalized.contains("coin") && !normalized.contains("pair"),
        _ => false,
    };
    if funding {
        return parse_funding_csv(preset, account_id, batch_id, raw_content, reg);
    }

    match preset {
        StatementPreset::GenericCsv => {
            let required = [
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
            ];
            let allowed = [
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
                "cost_known",
                "counterparty_account",
                "source_id",
            ];
            let map = verify_csv_headers(preset, &headers, &required, &allowed)?;
            let mut out = Vec::new();
            for (idx, rec_res) in rdr.records().enumerate() {
                let rec = rec_res?;
                let row_no = idx + 1;
                let raw_line = rec.iter().collect::<Vec<_>>().join(",");
                let get =
                    |k: &str| -> &str { map.get(k).and_then(|&i| rec.get(i)).unwrap_or("").trim() };
                let parsed = (|| -> Result<LedgerEventInput, String> {
                    let ev_type_str = get("event_type").to_ascii_lowercase();
                    let side_str = get("side").to_ascii_lowercase();
                    let ev_type = match ev_type_str.as_str() {
                        "opening_balance" | "opening" => EconomicEventType::OpeningBalance,
                        "buy" => EconomicEventType::Buy,
                        "sell" => EconomicEventType::Sell,
                        "cash_exchange" | "exchange" | "convert" => EconomicEventType::CashExchange,
                        "trade" => match side_str.as_str() {
                            "buy" => EconomicEventType::Buy,
                            "sell" => EconomicEventType::Sell,
                            _ => return Err(format!("unsupported trade side '{side_str}'")),
                        },
                        "deposit" => EconomicEventType::Deposit,
                        "withdrawal" => EconomicEventType::Withdrawal,
                        "transfer_out" => EconomicEventType::TransferOut,
                        "transfer_in" => EconomicEventType::TransferIn,
                        "position_transfer_out" => EconomicEventType::PositionTransferOut,
                        "position_transfer_in" => EconomicEventType::PositionTransferIn,
                        "cash_dividend" | "dividend" => EconomicEventType::CashDividend,
                        "interest" => EconomicEventType::Interest,
                        "fee_only" | "fee" => EconomicEventType::FeeOnly,
                        "split" => EconomicEventType::Split,
                        other => return Err(format!("unsupported event_type '{other}'")),
                    };
                    let occurred_at = parse_timestamp_with_tz(get("occurred_at"), get("timezone"))?;
                    let sym_raw = get("symbol");
                    let inst_id = reg.resolve_symbol(None, sym_raw);
                    let asset_id = inst_id
                        .as_ref()
                        .and_then(|id| reg.instruments.get(id))
                        .map(|inst| inst.base_asset_id.clone())
                        .unwrap_or_else(|| sym_raw.to_ascii_uppercase());
                    let qty = parse_decimal_lossless(get("quantity")).map_err(|e| e.to_string())?;
                    if qty <= Decimal::ZERO {
                        return Err(format!("quantity must be positive, got {qty}"));
                    }
                    let price_str = get("price");
                    let price = if price_str.is_empty() {
                        None
                    } else {
                        Some(parse_decimal_lossless(price_str).map_err(|e| e.to_string())?)
                    };
                    let fee = parse_optional_fee(
                        get("fee_amount"),
                        "generic fees must be non-negative; rebates need an explicit income event",
                    )?;
                    let cost_known_str = get("cost_known");
                    let cost_known = match cost_known_str.to_ascii_lowercase().as_str() {
                        "" | "true" | "1" | "yes" => true,
                        "false" | "0" | "no" => false,
                        other => return Err(format!("invalid cost_known '{other}'")),
                    };
                    let cp = {
                        let s = get("counterparty_account");
                        if s.is_empty() {
                            None
                        } else {
                            Some(s.to_string())
                        }
                    };
                    let src_id = get("source_id");
                    let ev_id = if src_id.is_empty() {
                        format!("EV:{batch_id}:{row_no}")
                    } else {
                        format!("EV:{account_id}:{src_id}:{row_no}")
                    };
                    Ok(LedgerEventInput {
                        event_id: ev_id.clone(),
                        account_id: account_id.to_string(),
                        event_type: ev_type,
                        occurred_at,
                        sequence_no: row_no as i64,
                        asset_id,
                        instrument_id: inst_id,
                        quantity: qty,
                        price,
                        quote_currency: get("quote_currency").to_ascii_uppercase(),
                        fee_amount: fee,
                        fee_currency: get("fee_currency").to_ascii_uppercase(),
                        cost_known,
                        counterparty_account_id: cp,
                        transfer_group_id: None,
                        correction_group_id: Some(format!("CG:{ev_id}")),
                        revision: 1,
                        active_in_correction_group: true,
                        source_ref: if src_id.is_empty() {
                            None
                        } else {
                            Some(src_id.to_string())
                        },
                    })
                })();
                out.push(wrap_preview_row(row_no, batch_id, raw_line, parsed));
            }
            Ok(out)
        }
        StatementPreset::FutuCsv => {
            let is_zh = headers
                .iter()
                .map(normalize_csv_header)
                .any(|h| h.contains("成交数量") || h.contains("方向"));
            let map = if is_zh {
                verify_csv_headers(
                    preset,
                    &headers,
                    &[
                        "账户",
                        "方向",
                        "代码",
                        "成交时间",
                        "成交数量",
                        "成交价格",
                        "币种",
                        "佣金",
                        "平台使用费",
                        "流水号",
                    ],
                    &[
                        "账户",
                        "方向",
                        "代码",
                        "名称",
                        "成交时间",
                        "成交数量",
                        "成交价格",
                        "币种",
                        "佣金",
                        "平台使用费",
                        "交收费",
                        "证监会规费",
                        "交易活动费",
                        "流水号",
                    ],
                )?
            } else {
                verify_csv_headers(
                    preset,
                    &headers,
                    &[
                        "account",
                        "side",
                        "symbol",
                        "tradetime",
                        "quantity",
                        "price",
                        "currency",
                        "commission",
                        "platformfee",
                        "tradeid",
                    ],
                    &[
                        "account",
                        "side",
                        "symbol",
                        "name",
                        "tradetime",
                        "quantity",
                        "price",
                        "currency",
                        "commission",
                        "platformfee",
                        "settlementfee",
                        "regulatoryfee",
                        "activityfee",
                        "tradeid",
                    ],
                )?
            };
            let mut out = Vec::new();
            for (idx, rec_res) in rdr.records().enumerate() {
                let rec = rec_res?;
                let row_no = idx + 1;
                let raw_line = rec.iter().collect::<Vec<_>>().join(",");
                let get_either = |zh: &str, en: &str| -> &str {
                    let key = if is_zh { zh } else { en };
                    map.get(key).and_then(|&i| rec.get(i)).unwrap_or("").trim()
                };
                let parsed = (|| -> Result<LedgerEventInput, String> {
                    let side_raw = get_either("方向", "side").to_ascii_lowercase();
                    let ev_type = if side_raw.contains("买") || side_raw == "buy" {
                        EconomicEventType::Buy
                    } else if side_raw.contains("卖") || side_raw == "sell" {
                        EconomicEventType::Sell
                    } else if side_raw.contains("入金") || side_raw == "deposit" {
                        EconomicEventType::Deposit
                    } else {
                        return Err(format!("unsupported Futu side '{side_raw}'"));
                    };
                    let occurred_at = parse_timestamp_with_tz(
                        get_either("成交时间", "tradetime"),
                        "America/New_York",
                    )?;
                    let sym_raw = get_either("代码", "symbol");
                    let inst_id = reg.resolve_symbol(Some("futu"), sym_raw);
                    let asset_id = inst_id
                        .clone()
                        .unwrap_or_else(|| sym_raw.to_ascii_uppercase());
                    let qty = parse_decimal_lossless(get_either("成交数量", "quantity"))
                        .map_err(|e| e.to_string())?;
                    if qty <= Decimal::ZERO {
                        return Err(format!("quantity must be positive, got {qty}"));
                    }
                    let price_str = get_either("成交价格", "price");
                    let price = if price_str.is_empty() {
                        None
                    } else {
                        Some(parse_decimal_lossless(price_str).map_err(|e| e.to_string())?)
                    };
                    let ccy = get_either("币种", "currency").to_ascii_uppercase();

                    let mut total_fee = Decimal::ZERO;
                    for (zh_f, en_f) in [
                        ("佣金", "commission"),
                        ("平台使用费", "platformfee"),
                        ("交收费", "settlementfee"),
                        ("证监会规费", "regulatoryfee"),
                        ("交易活动费", "activityfee"),
                    ] {
                        total_fee += parse_optional_fee(
                            get_either(zh_f, en_f),
                            "negative Futu fee requires an explicit rebate income row",
                        )?;
                    }
                    let trade_id = get_either("流水号", "tradeid");
                    let ev_id = format!("EV:{account_id}:FUTU:{trade_id}:{row_no}");
                    Ok(LedgerEventInput {
                        event_id: ev_id.clone(),
                        account_id: account_id.to_string(),
                        event_type: ev_type,
                        occurred_at,
                        sequence_no: row_no as i64,
                        asset_id,
                        instrument_id: inst_id,
                        quantity: qty,
                        price,
                        quote_currency: ccy.clone(),
                        fee_amount: total_fee.normalize(),
                        fee_currency: ccy,
                        cost_known: true,
                        counterparty_account_id: None,
                        transfer_group_id: None,
                        correction_group_id: Some(format!("CG:{ev_id}")),
                        revision: 1,
                        active_in_correction_group: true,
                        source_ref: Some(trade_id.to_string()),
                    })
                })();
                out.push(wrap_preview_row(row_no, batch_id, raw_line, parsed));
            }
            Ok(out)
        }
        StatementPreset::OkxCsv => {
            let is_zh = headers
                .iter()
                .map(normalize_csv_header)
                .any(|h| h.contains("账单id") || h.contains("交易对"));
            let map = if is_zh {
                verify_csv_headers(
                    preset,
                    &headers,
                    &[
                        "账户",
                        "账单id",
                        "时间",
                        "交易对",
                        "类型",
                        "方向",
                        "成交数量",
                        "成交均价",
                        "计价币种",
                        "手续费",
                        "手续费币种",
                    ],
                    &[
                        "账户",
                        "账单id",
                        "时间",
                        "交易对",
                        "类型",
                        "方向",
                        "成交数量",
                        "成交均价",
                        "计价币种",
                        "手续费",
                        "手续费币种",
                    ],
                )?
            } else {
                verify_csv_headers(
                    preset,
                    &headers,
                    &[
                        "account",
                        "billid",
                        "timestamp",
                        "instrument",
                        "type",
                        "side",
                        "size",
                        "price",
                        "quotecurrency",
                        "fee",
                        "feecurrency",
                    ],
                    &[
                        "account",
                        "billid",
                        "timestamp",
                        "instrument",
                        "type",
                        "side",
                        "size",
                        "price",
                        "quotecurrency",
                        "fee",
                        "feecurrency",
                    ],
                )?
            };
            let mut out = Vec::new();
            for (idx, rec_res) in rdr.records().enumerate() {
                let rec = rec_res?;
                let row_no = idx + 1;
                let raw_line = rec.iter().collect::<Vec<_>>().join(",");
                let get_either = |zh: &str, en: &str| -> &str {
                    let key = if is_zh { zh } else { en };
                    map.get(key).and_then(|&i| rec.get(i)).unwrap_or("").trim()
                };
                let parsed = (|| -> Result<LedgerEventInput, String> {
                    let type_raw = get_either("类型", "type").to_ascii_lowercase();
                    let side_raw = get_either("方向", "side").to_ascii_lowercase();
                    let ev_type = if type_raw.contains("充") || type_raw == "deposit" {
                        EconomicEventType::Deposit
                    } else if type_raw.contains("提") || type_raw == "withdrawal" {
                        EconomicEventType::Withdrawal
                    } else if side_raw.contains("买") || side_raw == "buy" {
                        EconomicEventType::Buy
                    } else if side_raw.contains("卖") || side_raw == "sell" {
                        EconomicEventType::Sell
                    } else {
                        return Err(format!("unsupported OKX type/side '{type_raw}/{side_raw}'"));
                    };
                    let occurred_at =
                        parse_timestamp_with_tz(get_either("时间", "timestamp"), "UTC")?;
                    let inst_raw = get_either("交易对", "instrument");
                    let inst_id = reg.resolve_symbol(Some("okx"), inst_raw);
                    let base_asset = inst_id
                        .as_ref()
                        .and_then(|id| reg.instruments.get(id))
                        .map(|i| i.base_asset_id.clone())
                        .unwrap_or_else(|| {
                            inst_raw
                                .split(['-', '/'])
                                .next()
                                .unwrap_or(inst_raw)
                                .to_ascii_uppercase()
                        });
                    let qty = parse_decimal_lossless(get_either("成交数量", "size"))
                        .map_err(|e| e.to_string())?;
                    if qty <= Decimal::ZERO {
                        return Err(format!("quantity must be positive, got {qty}"));
                    }
                    let price_str = get_either("成交均价", "price");
                    let price = if price_str.is_empty() {
                        None
                    } else {
                        Some(parse_decimal_lossless(price_str).map_err(|e| e.to_string())?)
                    };
                    let quote_ccy = get_either("计价币种", "quotecurrency").to_ascii_uppercase();
                    let fee = parse_optional_fee(
                        get_either("手续费", "fee"),
                        "negative OKX CSV fee requires an explicit rebate income row",
                    )?;
                    let fee_ccy = get_either("手续费币种", "feecurrency").to_ascii_uppercase();
                    if matches!(
                        ev_type,
                        EconomicEventType::Deposit | EconomicEventType::Withdrawal
                    ) && base_asset != quote_ccy
                    {
                        return Err("crypto funding requires the independent currency/amount funding template".into());
                    }
                    let bill_id = get_either("账单id", "billid");
                    let ev_id = format!("EV:{account_id}:OKX:{bill_id}:{row_no}");

                    Ok(LedgerEventInput {
                        event_id: ev_id.clone(),
                        account_id: account_id.to_string(),
                        event_type: ev_type,
                        occurred_at,
                        sequence_no: row_no as i64,
                        asset_id: base_asset,
                        instrument_id: inst_id,
                        quantity: qty,
                        price,
                        quote_currency: quote_ccy,
                        fee_amount: fee,
                        fee_currency: fee_ccy,
                        cost_known: true,
                        counterparty_account_id: None,
                        transfer_group_id: None,
                        correction_group_id: Some(format!("CG:{ev_id}")),
                        revision: 1,
                        active_in_correction_group: true,
                        source_ref: Some(bill_id.to_string()),
                    })
                })();
                out.push(wrap_preview_row(row_no, batch_id, raw_line, parsed));
            }
            Ok(out)
        }
        StatementPreset::BinanceCsv => {
            let required = [
                "date(utc)",
                "pair",
                "side",
                "price",
                "executed",
                "amount",
                "fee",
                "fee coin",
            ];
            let mut allowed = required.to_vec();
            allowed.extend(["account", "tradeid"]);
            let map = verify_csv_headers(preset, &headers, &required, &allowed)?;
            let mut out = Vec::new();
            for (idx, rec_res) in rdr.records().enumerate() {
                let rec = rec_res?;
                let row_no = idx + 1;
                let raw_line = rec.iter().collect::<Vec<_>>().join(",");
                let get =
                    |k: &str| -> &str { map.get(k).and_then(|&i| rec.get(i)).unwrap_or("").trim() };
                let parsed = (|| -> Result<LedgerEventInput, String> {
                    let side_str = get("side").to_ascii_lowercase();
                    let ev_type = match side_str.as_str() {
                        "buy" => EconomicEventType::Buy,
                        "sell" => EconomicEventType::Sell,
                        "deposit" => EconomicEventType::Deposit,
                        "withdrawal" => EconomicEventType::Withdrawal,
                        other => return Err(format!("unsupported Binance side '{other}'")),
                    };
                    let occurred_at = parse_timestamp_with_tz(get("date(utc)"), "UTC")?;
                    let pair_raw = get("pair");
                    let inst_id = reg.resolve_symbol(Some("binance"), pair_raw);
                    let (base_asset, quote_ccy) = if let Some(inst) =
                        inst_id.as_ref().and_then(|id| reg.instruments.get(id))
                    {
                        (inst.base_asset_id.clone(), inst.quote_asset_id.clone())
                    } else {
                        (pair_raw.to_ascii_uppercase(), "USDT".to_string())
                    };
                    let qty = parse_decimal_lossless(get("executed")).map_err(|e| e.to_string())?;
                    if qty <= Decimal::ZERO {
                        return Err(format!("quantity must be positive, got {qty}"));
                    }
                    let price_str = get("price");
                    let price = if price_str.is_empty() {
                        None
                    } else {
                        Some(parse_decimal_lossless(price_str).map_err(|e| e.to_string())?)
                    };
                    let fee = parse_optional_fee(
                        get("fee"),
                        "negative Binance fee requires an explicit rebate income row",
                    )?;
                    if matches!(
                        ev_type,
                        EconomicEventType::Deposit | EconomicEventType::Withdrawal
                    ) && base_asset != quote_ccy
                    {
                        return Err(
                            "crypto funding requires the independent coin/amount funding template"
                                .into(),
                        );
                    }
                    let fee_ccy = get("fee coin").to_ascii_uppercase();
                    let tid = get("tradeid");
                    let ev_id = format!("EV:{account_id}:BN:{tid}:{row_no}");
                    Ok(LedgerEventInput {
                        event_id: ev_id.clone(),
                        account_id: account_id.to_string(),
                        event_type: ev_type,
                        occurred_at,
                        sequence_no: row_no as i64,
                        asset_id: base_asset,
                        instrument_id: inst_id,
                        quantity: qty,
                        price,
                        quote_currency: quote_ccy,
                        fee_amount: fee,
                        fee_currency: fee_ccy,
                        cost_known: true,
                        counterparty_account_id: None,
                        transfer_group_id: None,
                        correction_group_id: Some(format!("CG:{ev_id}")),
                        revision: 1,
                        active_in_correction_group: true,
                        source_ref: Some(tid.to_string()),
                    })
                })();
                out.push(wrap_preview_row(row_no, batch_id, raw_line, parsed));
            }
            Ok(out)
        }
        StatementPreset::TigerCsv
        | StatementPreset::LongbridgeCsv
        | StatementPreset::SchwabCsv
        | StatementPreset::EastMoneyCsv
        | StatementPreset::IbkrActivityCsv => {
            let provider_hint = match preset {
                StatementPreset::TigerCsv => Some("tiger"),
                StatementPreset::LongbridgeCsv => Some("longbridge"),
                StatementPreset::SchwabCsv => Some("schwab"),
                StatementPreset::EastMoneyCsv => Some("eastmoney"),
                StatementPreset::IbkrActivityCsv => Some("ibkr"),
                _ => None,
            };
            let required = [
                "account", "datetime", "symbol", "action", "quantity", "price", "currency", "fee",
                "tradeid",
            ];
            let map = verify_csv_headers(preset, &headers, &required, &required)?;
            let mut out = Vec::new();
            for (idx, rec_res) in rdr.records().enumerate() {
                let rec = rec_res?;
                let row_no = idx + 1;
                let raw_line = rec.iter().collect::<Vec<_>>().join(",");
                let get =
                    |k: &str| -> &str { map.get(k).and_then(|&i| rec.get(i)).unwrap_or("").trim() };
                let parsed = (|| -> Result<LedgerEventInput, String> {
                    let act = get("action").to_ascii_lowercase();
                    let ev_type = match act.as_str() {
                        "buy" | "买入" | "证券买入" => EconomicEventType::Buy,
                        "sell" | "卖出" | "证券卖出" => EconomicEventType::Sell,
                        "dividend" | "分红" => EconomicEventType::CashDividend,
                        "deposit" | "入金" => EconomicEventType::Deposit,
                        other => return Err(format!("unsupported action '{other}'")),
                    };
                    let occurred_at = parse_timestamp_with_tz(get("datetime"), "America/New_York")?;
                    let sym_raw = get("symbol");
                    let inst_id = reg.resolve_symbol(provider_hint, sym_raw);
                    let asset_id = inst_id
                        .clone()
                        .unwrap_or_else(|| sym_raw.to_ascii_uppercase());
                    let qty = parse_decimal_lossless(get("quantity")).map_err(|e| e.to_string())?;
                    if qty <= Decimal::ZERO {
                        return Err(format!("quantity must be positive, got {qty}"));
                    }
                    let p_str = get("price");
                    let price = if p_str.is_empty() {
                        None
                    } else {
                        Some(parse_decimal_lossless(p_str).map_err(|e| e.to_string())?)
                    };
                    let ccy = get("currency").to_ascii_uppercase();
                    let fee = parse_optional_fee(
                        get("fee"),
                        "negative expense fee requires an explicit rebate income row",
                    )?;
                    let tid = get("tradeid");
                    let ev_id = format!("EV:{account_id}:{}:{tid}:{row_no}", preset.as_str());
                    Ok(LedgerEventInput {
                        event_id: ev_id.clone(),
                        account_id: account_id.to_string(),
                        event_type: ev_type,
                        occurred_at,
                        sequence_no: row_no as i64,
                        asset_id,
                        instrument_id: inst_id,
                        quantity: qty,
                        price,
                        quote_currency: ccy.clone(),
                        fee_amount: fee,
                        fee_currency: ccy,
                        cost_known: true,
                        counterparty_account_id: None,
                        transfer_group_id: None,
                        correction_group_id: Some(format!("CG:{ev_id}")),
                        revision: 1,
                        active_in_correction_group: true,
                        source_ref: Some(tid.to_string()),
                    })
                })();
                out.push(wrap_preview_row(row_no, batch_id, raw_line, parsed));
            }
            Ok(out)
        }
        StatementPreset::IbkrFlexXml => unreachable!(),
    }
}

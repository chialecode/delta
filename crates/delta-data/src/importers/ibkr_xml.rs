use std::collections::{BTreeMap, HashSet};

use delta_core::{parse_decimal_lossless, EconomicEventType, LedgerEventInput};
use quick_xml::events::Event as XmlEvent;
use quick_xml::Reader as XmlReader;
use rust_decimal::Decimal;

use crate::importers::{parse_timestamp_with_tz, wrap_preview_row, ImportError, PreviewRow};
use crate::registry::InstrumentRegistry;

/// Pure Rust IBKR Flex Query XML parser (`<Trade>`, `<CashTransaction>`, `<CorporateAction>`, `<Transfer>`).
pub(crate) fn parse_ibkr_flex_xml(
    account_id: &str,
    batch_id: &str,
    xml_content: &str,
    reg: &InstrumentRegistry,
) -> Result<Vec<PreviewRow>, ImportError> {
    let mut reader = XmlReader::from_str(xml_content);
    reader.config_mut().trim_text(true);
    let mut out = Vec::new();
    let mut row_no = 0usize;
    let mut source_accounts = HashSet::new();

    loop {
        match reader.read_event() {
            Ok(XmlEvent::Empty(e)) | Ok(XmlEvent::Start(e)) => {
                let name_bytes = e.name();
                let xml_error = |detail: String| ImportError::InvalidHeader {
                    preset: "ibkr_flex_xml".into(),
                    detail,
                };
                let tag: &str = name_bytes.as_ref();
                let mut attrs = BTreeMap::new();
                for attr in e.attributes() {
                    let attr = attr.map_err(|e| xml_error(e.to_string()))?;
                    let k = AsRef::<str>::as_ref(&attr.key).to_string();
                    let v = attr
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .map_err(|e| xml_error(e.to_string()))?
                        .to_string();
                    attrs.insert(k, v);
                }
                if let Some(source) = attrs.get("accountId").filter(|s| !s.is_empty()) {
                    source_accounts.insert(source.clone());
                    if source_accounts.len() > 1 {
                        return Err(xml_error(
                            "multi-account Flex files must be split before binding an account"
                                .into(),
                        ));
                    }
                }
                if !matches!(
                    tag,
                    "Trade" | "CashTransaction" | "CorporateAction" | "Transfer"
                ) {
                    continue;
                }
                row_no += 1;
                let raw_line = format!("{tag} {:?}", attrs);
                let parsed = (|| -> Result<LedgerEventInput, String> {
                    match tag {
                        "Trade" => {
                            let side_str = attrs
                                .get("buySell")
                                .map(|s| s.to_ascii_uppercase())
                                .unwrap_or_default();
                            let ev_type = if side_str == "BUY" {
                                EconomicEventType::Buy
                            } else if side_str == "SELL" {
                                EconomicEventType::Sell
                            } else {
                                return Err(format!("invalid IBKR Trade buySell '{side_str}'"));
                            };
                            let dt_raw = attrs
                                .get("dateTime")
                                .or_else(|| attrs.get("tradeDate"))
                                .ok_or("missing dateTime")?;
                            let occurred_at = parse_timestamp_with_tz(dt_raw, "America/New_York")?;
                            let sym = attrs.get("symbol").ok_or("missing symbol")?;
                            let inst_id = reg.resolve_symbol(Some("ibkr"), sym);
                            let asset_id =
                                inst_id.clone().unwrap_or_else(|| sym.to_ascii_uppercase());
                            let qty = parse_decimal_lossless(
                                attrs.get("quantity").ok_or("missing quantity")?,
                            )
                            .map_err(|e| e.to_string())?
                            .abs();
                            if qty <= Decimal::ZERO {
                                return Err("IBKR Trade quantity must be non-zero".into());
                            }
                            let price = parse_decimal_lossless(
                                attrs.get("tradePrice").ok_or("missing tradePrice")?,
                            )
                            .map_err(|e| e.to_string())?;
                            if price <= Decimal::ZERO {
                                return Err("IBKR Trade tradePrice must be positive".into());
                            }
                            let ccy = attrs
                                .get("currency")
                                .map(|s| s.trim().to_ascii_uppercase())
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| "USD".to_string());
                            let comm_str =
                                attrs.get("ibCommission").map(|s| s.as_str()).unwrap_or("0");
                            let commission =
                                parse_decimal_lossless(comm_str).map_err(|e| e.to_string())?;
                            if commission > Decimal::ZERO {
                                return Err(
                                    "positive IBKR commission is a rebate; record explicit income"
                                        .into(),
                                );
                            }
                            let fee = -commission;
                            let fee_ccy = attrs
                                .get("ibCommissionCurrency")
                                .map(|s| s.trim().to_ascii_uppercase())
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| ccy.clone());
                            let tid = attrs.get("tradeID").cloned().unwrap_or_default();
                            let ev_id = format!("EV:{account_id}:IBKR:TRD:{tid}:{row_no}");
                            Ok(LedgerEventInput {
                                event_id: ev_id.clone(),
                                account_id: account_id.to_string(),
                                event_type: ev_type,
                                occurred_at,
                                sequence_no: row_no as i64,
                                asset_id,
                                instrument_id: inst_id,
                                quantity: qty,
                                price: Some(price),
                                quote_currency: ccy,
                                fee_amount: fee,
                                fee_currency: fee_ccy,
                                cost_known: true,
                                counterparty_account_id: None,
                                transfer_group_id: None,
                                correction_group_id: Some(format!("CG:{ev_id}")),
                                revision: 1,
                                active_in_correction_group: true,
                                source_ref: Some(tid),
                            })
                        }
                        "CashTransaction" => {
                            let ctype = attrs
                                .get("type")
                                .map(|s| s.to_ascii_lowercase())
                                .unwrap_or_default();
                            let amt = parse_decimal_lossless(
                                attrs.get("amount").ok_or("missing amount")?,
                            )
                            .map_err(|e| e.to_string())?;
                            if amt.is_zero() {
                                return Err("IBKR CashTransaction amount must be non-zero".into());
                            }
                            let ccy = attrs
                                .get("currency")
                                .map(|s| s.trim().to_ascii_uppercase())
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| "USD".to_string());
                            let dt_raw = attrs.get("dateTime").ok_or("missing dateTime")?;
                            let occurred_at = parse_timestamp_with_tz(dt_raw, "America/New_York")?;
                            let (ev_type, asset_id, inst_id) = if ctype.contains("dividend") {
                                let sym = attrs.get("symbol").map(|s| s.as_str()).unwrap_or(&ccy);
                                let inst = reg.resolve_symbol(Some("ibkr"), sym);
                                let asset =
                                    inst.clone().unwrap_or_else(|| sym.to_ascii_uppercase());
                                (EconomicEventType::CashDividend, asset, inst)
                            } else if ctype.contains("interest") && amt > Decimal::ZERO {
                                (EconomicEventType::Interest, ccy.clone(), None)
                            } else if ctype.contains("fee")
                                || ctype.contains("tax")
                                || (ctype.contains("interest") && amt < Decimal::ZERO)
                            {
                                if amt > Decimal::ZERO {
                                    return Err(
                                        "positive IBKR fee/tax is a refund or rebate; record explicit income"
                                            .into(),
                                    );
                                }
                                (EconomicEventType::FeeOnly, ccy.clone(), None)
                            } else if ctype.contains("deposit") || ctype.contains("withdrawal") {
                                (
                                    if amt > Decimal::ZERO {
                                        EconomicEventType::Deposit
                                    } else {
                                        EconomicEventType::Withdrawal
                                    },
                                    ccy.clone(),
                                    None,
                                )
                            } else {
                                return Err(format!(
                                    "unsupported IBKR cash transaction type '{ctype}'"
                                ));
                            };
                            if ev_type == EconomicEventType::CashDividend && amt <= Decimal::ZERO {
                                return Err("negative dividend requires an explicit withholding/tax fee row".into());
                            }
                            let tid = attrs.get("transactionID").cloned().unwrap_or_default();
                            let ev_id = format!("EV:{account_id}:IBKR:CASH:{tid}:{row_no}");
                            Ok(LedgerEventInput {
                                event_id: ev_id.clone(),
                                account_id: account_id.to_string(),
                                event_type: ev_type,
                                occurred_at,
                                sequence_no: row_no as i64,
                                asset_id,
                                instrument_id: inst_id,
                                quantity: amt.abs(),
                                price: None,
                                quote_currency: ccy.clone(),
                                fee_amount: Decimal::ZERO,
                                fee_currency: ccy,
                                cost_known: true,
                                counterparty_account_id: None,
                                transfer_group_id: None,
                                correction_group_id: Some(format!("CG:{ev_id}")),
                                revision: 1,
                                active_in_correction_group: true,
                                source_ref: Some(tid),
                            })
                        }
                        "CorporateAction" => {
                            if !attrs
                                .get("type")
                                .is_some_and(|s| matches!(s.as_str(), "FS" | "RS" | "split"))
                            {
                                return Err("only explicit split corporate actions are supported in this Flex template".into());
                            }
                            let sym = attrs.get("symbol").ok_or("missing symbol")?;
                            let inst_id = reg.resolve_symbol(Some("ibkr"), sym);
                            let asset_id =
                                inst_id.clone().unwrap_or_else(|| sym.to_ascii_uppercase());
                            let ratio =
                                parse_decimal_lossless(attrs.get("ratio").ok_or("missing ratio")?)
                                    .map_err(|e| e.to_string())?;
                            if ratio <= Decimal::ZERO {
                                return Err(
                                    "IBKR CorporateAction split ratio must be positive".into()
                                );
                            }
                            let ccy = attrs
                                .get("currency")
                                .map(|s| s.trim().to_ascii_uppercase())
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| "USD".to_string());
                            let dt_raw = attrs.get("dateTime").ok_or("missing dateTime")?;
                            let occurred_at = parse_timestamp_with_tz(dt_raw, "America/New_York")?;
                            let ev_id = format!("EV:{account_id}:IBKR:CA:{row_no}");
                            Ok(LedgerEventInput {
                                event_id: ev_id.clone(),
                                account_id: account_id.to_string(),
                                event_type: EconomicEventType::Split,
                                occurred_at,
                                sequence_no: row_no as i64,
                                asset_id,
                                instrument_id: inst_id,
                                quantity: ratio,
                                price: None,
                                quote_currency: ccy.clone(),
                                fee_amount: Decimal::ZERO,
                                fee_currency: ccy,
                                cost_known: true,
                                counterparty_account_id: None,
                                transfer_group_id: None,
                                correction_group_id: Some(format!("CG:{ev_id}")),
                                revision: 1,
                                active_in_correction_group: true,
                                source_ref: attrs.get("actionID").cloned(),
                            })
                        }
                        "Transfer" => {
                            let sym = attrs.get("symbol").ok_or("missing symbol")?;
                            let inst_id = reg.resolve_symbol(Some("ibkr"), sym);
                            let asset_id =
                                inst_id.clone().unwrap_or_else(|| sym.to_ascii_uppercase());
                            let signed_qty = parse_decimal_lossless(
                                attrs.get("quantity").ok_or("missing quantity")?,
                            )
                            .map_err(|e| e.to_string())?;
                            let qty = signed_qty.abs();
                            if qty <= Decimal::ZERO {
                                return Err("IBKR Transfer quantity must be non-zero".into());
                            }
                            let ccy = attrs
                                .get("currency")
                                .map(|s| s.trim().to_ascii_uppercase())
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| "USD".to_string());
                            let dt_raw = attrs.get("dateTime").ok_or("missing dateTime")?;
                            let occurred_at = parse_timestamp_with_tz(dt_raw, "America/New_York")?;
                            let incoming = match attrs
                                .get("direction")
                                .map(|s| s.to_ascii_uppercase())
                                .as_deref()
                            {
                                Some("IN" | "INCOMING") => true,
                                Some("OUT" | "OUTGOING") => false,
                                Some(other) => {
                                    return Err(format!("invalid transfer direction '{other}'"))
                                }
                                None => signed_qty > Decimal::ZERO,
                            };
                            let cp = attrs
                                .get("counterpartyAccount")
                                .cloned()
                                .filter(|s| !s.is_empty());
                            let external_in = incoming && cp.is_none();
                            let cp = if !incoming && cp.is_none() {
                                Some("external:ibkr".into())
                            } else {
                                cp
                            };
                            let ev_id = format!("EV:{account_id}:IBKR:XFER:{row_no}");
                            Ok(LedgerEventInput {
                                event_id: ev_id.clone(),
                                account_id: account_id.to_string(),
                                event_type: if external_in {
                                    EconomicEventType::OpeningBalance
                                } else if incoming {
                                    EconomicEventType::PositionTransferIn
                                } else {
                                    EconomicEventType::PositionTransferOut
                                },
                                occurred_at,
                                sequence_no: row_no as i64,
                                asset_id,
                                instrument_id: inst_id,
                                quantity: qty,
                                price: None,
                                quote_currency: ccy.clone(),
                                fee_amount: Decimal::ZERO,
                                fee_currency: ccy,
                                cost_known: !external_in,
                                counterparty_account_id: cp,
                                transfer_group_id: None,
                                correction_group_id: Some(format!("CG:{ev_id}")),
                                revision: 1,
                                active_in_correction_group: true,
                                source_ref: attrs.get("transferID").cloned(),
                            })
                        }
                        _ => unreachable!(),
                    }
                })();
                out.push(wrap_preview_row(row_no, batch_id, raw_line, parsed));
            }
            Ok(XmlEvent::Eof) => break,
            Err(e) => {
                return Err(ImportError::InvalidHeader {
                    preset: "ibkr_flex_xml".to_string(),
                    detail: e.to_string(),
                })
            }
            _ => {}
        }
    }
    if row_no == 0 {
        return Err(ImportError::InvalidHeader {
            preset: "ibkr_flex_xml".to_string(),
            detail: "no Flex statements/trades found".to_string(),
        });
    }
    Ok(out)
}

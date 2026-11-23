use crate::decimal::{canon_decimal, to_canonical_text};
use crate::model::{
    deduplicate_corporate_actions, AdjustmentKind, BarRecord, CorporateActionRecord,
    CorporateActionType,
};
use crate::sim::fees::{
    round_price_adverse_tick, settle_order_fees, validate_step_multiple, FeeCurrencyMode,
    FeeProfileVersion, SyntheticFillInput,
};
use crate::sim::{
    BarPathStage, CashReservation, FillRule, OrderCostPreview, OrderGroupRole, OrderRejection,
    OrderSide, OrderStatus, OrderSubmitKind, OrderSubmitRequest, PositionReservation,
    SimBarSnapshot, SimCommand, SimError, SimEvent, SimFill, SimLot, SimOrder, SimOrderType,
    SimRejectCode, SimState, SimStepBar, TimeInForce, MARKET_BUY_RESERVATION_BUFFER_RATE,
    SIM_ENGINE_VERSION_M5,
};
use chrono::{DateTime, Utc};
use rust_decimal::prelude::*;
use rust_decimal::Decimal;
use std::collections::{BTreeMap, BTreeSet};

fn checked_mul_sim(a: Decimal, b: Decimal, ctx: &str) -> Result<Decimal, SimError> {
    a.checked_mul(b)
        .ok_or_else(|| SimError::ArithmeticOverflow(ctx.to_string()))
}

fn checked_add_sim(a: Decimal, b: Decimal, ctx: &str) -> Result<Decimal, SimError> {
    a.checked_add(b)
        .ok_or_else(|| SimError::ArithmeticOverflow(ctx.to_string()))
}

fn reject(code: SimRejectCode, message: String, field: Option<&str>) -> SimError {
    SimError::rejected(OrderRejection {
        code,
        message,
        field: field.map(str::to_string),
        nearest_lower: None,
        nearest_upper: None,
        required: None,
        available: None,
    })
}

fn reject_amount(
    code: SimRejectCode,
    message: String,
    field: Option<&str>,
    required: Decimal,
    available: Decimal,
) -> SimError {
    SimError::rejected(OrderRejection {
        code,
        message,
        field: field.map(str::to_string),
        nearest_lower: None,
        nearest_upper: None,
        required: Some(required),
        available: Some(available),
    })
}

fn validate_tick_field(
    value: Decimal,
    tick_size: Decimal,
    field_name: &str,
) -> Result<Decimal, Box<OrderRejection>> {
    validate_step_multiple(value, tick_size).map_err(|v| {
        Box::new(OrderRejection {
            code: SimRejectCode::InvalidTickSize,
            message: format!(
                "{field_name} {} violates tick_size {}; nearest valid lower={:?}, upper={:?}",
                to_canonical_text(v.invalid_value),
                to_canonical_text(v.step_size),
                v.nearest_lower.map(to_canonical_text),
                v.nearest_upper.map(to_canonical_text),
            ),
            field: Some(field_name.to_string()),
            nearest_lower: v.nearest_lower,
            nearest_upper: v.nearest_upper,
            required: None,
            available: None,
        })
    })
}

/// Validate an `OrderSubmitRequest` against instrument tick/lot rules, spot long-only rules,
/// and cash/position reservation limits without mutating `state` (`S-39`, `S-40`, `M4-A-04`, `M4-A-06`).
pub fn preview_order_submission(
    state: &SimState,
    req: &OrderSubmitRequest,
) -> Result<OrderCostPreview, SimError> {
    preview_order_submission_with_available(
        state,
        req,
        state.available_cash(),
        state.available_position(&req.instrument_id),
    )
}

fn preview_order_submission_with_available(
    state: &SimState,
    req: &OrderSubmitRequest,
    avail_cash: Decimal,
    avail_pos: Decimal,
) -> Result<OrderCostPreview, SimError> {
    if state.finished {
        return Err(SimError::SessionFinished);
    }
    let spec = state
        .instruments
        .get(&req.instrument_id)
        .ok_or_else(|| SimError::UnknownInstrument(req.instrument_id.clone()))?;

    let qty = validate_step_multiple(req.quantity, spec.lot_size).map_err(|v| {
        SimError::rejected(OrderRejection {
            code: SimRejectCode::InvalidLotSize,
            message: format!(
                "quantity {} violates lot_size {}; nearest valid lower={:?}, upper={:?}",
                to_canonical_text(v.invalid_value),
                to_canonical_text(v.step_size),
                v.nearest_lower.map(to_canonical_text),
                v.nearest_upper.map(to_canonical_text),
            ),
            field: Some("quantity".to_string()),
            nearest_lower: v.nearest_lower,
            nearest_upper: v.nearest_upper,
            required: None,
            available: None,
        })
    })?;

    let buffer_mult =
        Decimal::ONE + Decimal::from_str_exact(MARKET_BUY_RESERVATION_BUFFER_RATE).unwrap();

    match &req.kind {
        OrderSubmitKind::Single {
            side,
            order_type,
            attached_bracket,
        } => {
            if matches!(order_type, SimOrderType::StopLimit { .. }) {
                return Err(reject(
                    SimRejectCode::UnsupportedOrderType,
                    "stop_limit orders are not supported in M4".to_string(),
                    Some("order_type"),
                ));
            }
            if *side == OrderSide::Sell && attached_bracket.is_some() {
                return Err(reject(
                    SimRejectCode::InvalidOrderParams,
                    "attached bracket is only supported on buy entry orders in spot long-only mode"
                        .to_string(),
                    Some("attached_bracket"),
                ));
            }

            let entry_explicit_price = match order_type {
                SimOrderType::Market => None,
                SimOrderType::Limit { limit_price } => Some(
                    validate_tick_field(*limit_price, spec.tick_size, "limit_price")
                        .map_err(SimError::OrderRejected)?,
                ),
                SimOrderType::StopMarket { stop_price } => Some(
                    validate_tick_field(*stop_price, spec.tick_size, "stop_price")
                        .map_err(SimError::OrderRejected)?,
                ),
                SimOrderType::StopLimit { .. } => unreachable!(),
            };

            if let Some(bracket) = attached_bracket {
                if bracket.take_profit_limit.is_none() && bracket.stop_loss_stop.is_none() {
                    return Err(reject(
                        SimRejectCode::InvalidOrderParams,
                        "attached bracket must specify take_profit_limit and/or stop_loss_stop"
                            .to_string(),
                        Some("attached_bracket"),
                    ));
                }
                let tp_opt = match bracket.take_profit_limit {
                    Some(tp) => Some(
                        validate_tick_field(tp, spec.tick_size, "take_profit_limit")
                            .map_err(SimError::OrderRejected)?,
                    ),
                    None => None,
                };
                let sl_opt = match bracket.stop_loss_stop {
                    Some(sl) => Some(
                        validate_tick_field(sl, spec.tick_size, "stop_loss_stop")
                            .map_err(SimError::OrderRejected)?,
                    ),
                    None => None,
                };
                if let (Some(tp), Some(sl)) = (tp_opt, sl_opt) {
                    if tp <= sl {
                        return Err(reject(
                            SimRejectCode::InvalidOrderParams,
                            "attached take_profit_limit must be strictly greater than stop_loss_stop"
                                .to_string(),
                            Some("attached_bracket"),
                        ));
                    }
                }
                if let Some(entry_p) = entry_explicit_price {
                    if let Some(tp) = tp_opt {
                        if tp <= entry_p {
                            return Err(reject(
                                SimRejectCode::InvalidOrderParams,
                                "attached take_profit_limit must exceed entry order price"
                                    .to_string(),
                                Some("take_profit_limit"),
                            ));
                        }
                    }
                    if let Some(sl) = sl_opt {
                        if sl >= entry_p {
                            return Err(reject(
                                SimRejectCode::InvalidOrderParams,
                                "attached stop_loss_stop must be below entry order price"
                                    .to_string(),
                                Some("stop_loss_stop"),
                            ));
                        }
                    }
                }
            }

            match side {
                OrderSide::Buy => {
                    let reserve_unit_price = match order_type {
                        SimOrderType::Limit { .. } => entry_explicit_price.unwrap(),
                        SimOrderType::Market => {
                            let last_close = state
                                .last_close_by_instrument
                                .get(&req.instrument_id)
                                .copied()
                                .ok_or_else(|| {
                                    reject(
                                        SimRejectCode::MissingReferencePrice,
                                        format!(
                                            "missing visible reference close for market buy on {}",
                                            req.instrument_id
                                        ),
                                        None,
                                    )
                                })?;
                            canon_decimal(checked_mul_sim(
                                last_close,
                                buffer_mult,
                                "market_buy_buffer",
                            )?)
                        }
                        SimOrderType::StopMarket { .. } => {
                            let stop_p = entry_explicit_price.unwrap();
                            let buffered_close = match state
                                .last_close_by_instrument
                                .get(&req.instrument_id)
                                .copied()
                            {
                                Some(c) => checked_mul_sim(c, buffer_mult, "stop_buy_buffer")?,
                                None => checked_mul_sim(stop_p, buffer_mult, "stop_buy_buffer")?,
                            };
                            canon_decimal(stop_p.max(buffered_close))
                        }
                        SimOrderType::StopLimit { .. } => unreachable!(),
                    };

                    let settlement = settle_order_fees(
                        &state.fee_profile,
                        OrderSide::Buy,
                        &spec.base_asset_id,
                        &spec.quote_currency,
                        &[SyntheticFillInput {
                            fill_id: "PREVIEW".to_string(),
                            quantity: qty,
                            fill_price: reserve_unit_price,
                        }],
                    )?;
                    let cash_fee = if state.fee_profile.fee_currency_mode == FeeCurrencyMode::Quote
                    {
                        settlement.total_fee_amount
                    } else {
                        Decimal::ZERO
                    };
                    let notional =
                        checked_mul_sim(qty, reserve_unit_price, "buy_reservation_notional")?;
                    let required_cash =
                        canon_decimal(checked_add_sim(notional, cash_fee, "buy_reservation_cash")?);
                    if required_cash > avail_cash {
                        return Err(reject_amount(
                            SimRejectCode::InsufficientFunds,
                            format!(
                                "insufficient available cash: required {}, available {}",
                                to_canonical_text(required_cash),
                                to_canonical_text(avail_cash),
                            ),
                            Some("quantity"),
                            required_cash,
                            avail_cash,
                        ));
                    }
                    Ok(OrderCostPreview {
                        instrument_id: req.instrument_id.clone(),
                        quantity: qty,
                        reference_price: Some(reserve_unit_price),
                        estimated_notional: Some(canon_decimal(notional)),
                        estimated_fee: Some(settlement.total_fee_amount),
                        fee_currency: settlement.fee_currency,
                        cash_reservation_required: required_cash,
                        position_reservation_required: Decimal::ZERO,
                        available_cash_before: avail_cash,
                        available_position_before: avail_pos,
                    })
                }
                OrderSide::Sell => {
                    if qty > avail_pos {
                        return Err(reject_amount(
                            SimRejectCode::InsufficientPosition,
                            format!(
                                "insufficient available position for {}: required {}, available {}",
                                req.instrument_id,
                                to_canonical_text(qty),
                                to_canonical_text(avail_pos),
                            ),
                            Some("quantity"),
                            qty,
                            avail_pos,
                        ));
                    }
                    let ref_p = entry_explicit_price.or_else(|| {
                        state
                            .last_close_by_instrument
                            .get(&req.instrument_id)
                            .copied()
                    });
                    let est_notional = match ref_p {
                        Some(p) => Some(canon_decimal(checked_mul_sim(
                            qty,
                            p,
                            "sell_preview_notional",
                        )?)),
                        None => None,
                    };
                    let est_fee = match ref_p {
                        Some(p) if p > Decimal::ZERO => Some(
                            settle_order_fees(
                                &state.fee_profile,
                                OrderSide::Sell,
                                &spec.base_asset_id,
                                &spec.quote_currency,
                                &[SyntheticFillInput {
                                    fill_id: "PREVIEW".to_string(),
                                    quantity: qty,
                                    fill_price: p,
                                }],
                            )?
                            .total_fee_amount,
                        ),
                        _ => None,
                    };
                    Ok(OrderCostPreview {
                        instrument_id: req.instrument_id.clone(),
                        quantity: qty,
                        reference_price: ref_p.map(canon_decimal),
                        estimated_notional: est_notional,
                        estimated_fee: est_fee,
                        fee_currency: spec.quote_currency.clone(),
                        cash_reservation_required: Decimal::ZERO,
                        position_reservation_required: qty,
                        available_cash_before: avail_cash,
                        available_position_before: avail_pos,
                    })
                }
            }
        }
        OrderSubmitKind::Oco {
            take_profit_limit,
            stop_loss_stop,
            ..
        } => {
            let tp = validate_tick_field(*take_profit_limit, spec.tick_size, "take_profit_limit")
                .map_err(SimError::OrderRejected)?;
            let sl = validate_tick_field(*stop_loss_stop, spec.tick_size, "stop_loss_stop")
                .map_err(SimError::OrderRejected)?;
            if tp <= sl {
                return Err(reject(
                    SimRejectCode::InvalidOrderParams,
                    "OCO take_profit_limit must be strictly greater than stop_loss_stop"
                        .to_string(),
                    Some("take_profit_limit"),
                ));
            }
            if qty > avail_pos {
                return Err(reject_amount(
                    SimRejectCode::InsufficientPosition,
                    format!(
                        "insufficient available position for OCO on {}: required {}, available {}",
                        req.instrument_id,
                        to_canonical_text(qty),
                        to_canonical_text(avail_pos),
                    ),
                    Some("quantity"),
                    qty,
                    avail_pos,
                ));
            }
            let est_notional = canon_decimal(checked_mul_sim(qty, tp, "oco_preview_notional")?);
            let settlement = settle_order_fees(
                &state.fee_profile,
                OrderSide::Sell,
                &spec.base_asset_id,
                &spec.quote_currency,
                &[SyntheticFillInput {
                    fill_id: "PREVIEW".to_string(),
                    quantity: qty,
                    fill_price: tp,
                }],
            )?;
            Ok(OrderCostPreview {
                instrument_id: req.instrument_id.clone(),
                quantity: qty,
                reference_price: Some(tp),
                estimated_notional: Some(est_notional),
                estimated_fee: Some(settlement.total_fee_amount),
                fee_currency: spec.quote_currency.clone(),
                cash_reservation_required: Decimal::ZERO,
                position_reservation_required: qty,
                available_cash_before: avail_cash,
                available_position_before: avail_pos,
            })
        }
    }
}

fn release_order_reservation(state: &mut SimState, order_id: &str) {
    let Some(res_id) = state
        .orders
        .get(order_id)
        .and_then(|o| o.reservation_id.clone())
    else {
        return;
    };
    state.cash_reservations.remove(&res_id);
    let remove_pos_res = if let Some(pos_res) = state.position_reservations.get_mut(&res_id) {
        pos_res.order_ids.retain(|id| id != order_id);
        pos_res.order_ids.is_empty()
    } else {
        false
    };
    if remove_pos_res {
        state.position_reservations.remove(&res_id);
    }
}

fn consume_position_reservation_fully(state: &mut SimState, res_id: &str) {
    state.position_reservations.remove(res_id);
}

fn cancel_order_internal(
    state: &mut SimState,
    order_id: &str,
    reason: &str,
    updated_at: DateTime<Utc>,
    events: &mut Vec<SimEvent>,
) {
    let Some(order) = state.orders.get_mut(order_id) else {
        return;
    };
    if !order.status.is_open_or_pending() {
        return;
    }
    let inst_id = order.instrument_id.clone();
    order.status = OrderStatus::Cancelled;
    order.cancel_reason = Some(reason.to_string());
    order.updated_at = updated_at;
    release_order_reservation(state, order_id);
    events.push(SimEvent::OrderCancelled {
        order_id: order_id.to_string(),
        instrument_id: inst_id,
        reason: reason.to_string(),
    });
}

fn cancel_pending_children(
    state: &mut SimState,
    parent_order_id: &str,
    reason: &str,
    updated_at: DateTime<Utc>,
    events: &mut Vec<SimEvent>,
) {
    let child_ids: Vec<String> = state
        .orders
        .values()
        .filter(|c| {
            c.parent_order_id.as_deref() == Some(parent_order_id)
                && c.status == OrderStatus::PendingParent
        })
        .map(|c| c.order_id.clone())
        .collect();
    for child_id in child_ids {
        cancel_order_internal(state, &child_id, reason, updated_at, events);
    }
}

fn next_order_id(order_seq: u64) -> String {
    format!("SIM-ORD-{order_seq:06}")
}

fn next_fill_id(fill_seq: u64) -> String {
    format!("SIM-FILL-{fill_seq:06}")
}

fn next_group_id(group_seq: u64) -> String {
    format!("SIM-GRP-{group_seq:06}")
}

fn next_res_id(res_seq: u64) -> String {
    format!("SIM-RES-{res_seq:06}")
}

fn require_m5(state: &SimState, feature: &str) -> Result<(), SimError> {
    if state.is_m5() {
        Ok(())
    } else {
        Err(SimError::EngineFeatureUnavailable {
            feature: feature.to_string(),
            required: SIM_ENGINE_VERSION_M5.to_string(),
            actual: state.engine_version.clone(),
        })
    }
}

/// Quick order (`S-51` ①): a market order filled inside this command at the current visible
/// close plus adverse spread/slippage, adverse tick rounding and fees. Zero-volume or
/// untradable bars are rejected; insufficient cash or position is rejected (never shrunk).
fn quick_fill_into_state(
    state: &mut SimState,
    req: OrderSubmitRequest,
) -> Result<Vec<SimEvent>, SimError> {
    require_m5(state, "visible_close quick fill")?;
    if state.finished {
        return Err(SimError::SessionFinished);
    }
    let OrderSubmitKind::Single {
        side,
        order_type,
        attached_bracket,
    } = &req.kind
    else {
        return Err(reject(
            SimRejectCode::InvalidOrderParams,
            "quick fills are single market orders".into(),
            Some("kind"),
        ));
    };
    if *order_type != SimOrderType::Market || attached_bracket.is_some() {
        return Err(reject(
            SimRejectCode::InvalidOrderParams,
            "quick fills are plain market orders without attached exits".into(),
            Some("order_type"),
        ));
    }
    let side = *side;
    let spec = state
        .instruments
        .get(&req.instrument_id)
        .cloned()
        .ok_or_else(|| SimError::UnknownInstrument(req.instrument_id.clone()))?;
    let qty = validate_step_multiple(req.quantity, spec.lot_size).map_err(|v| {
        SimError::rejected(OrderRejection {
            code: SimRejectCode::InvalidLotSize,
            message: format!(
                "quantity {} violates lot_size {}",
                to_canonical_text(v.invalid_value),
                to_canonical_text(v.step_size)
            ),
            field: Some("quantity".into()),
            nearest_lower: v.nearest_lower,
            nearest_upper: v.nearest_upper,
            required: None,
            available: None,
        })
    })?;
    let close = state
        .last_close_by_instrument
        .get(&req.instrument_id)
        .copied()
        .ok_or_else(|| {
            reject(
                SimRejectCode::MissingReferencePrice,
                format!("no visible close for {}", req.instrument_id),
                None,
            )
        })?;
    if !state
        .last_bar_tradable
        .get(&req.instrument_id)
        .copied()
        .unwrap_or(false)
    {
        return Err(reject(
            SimRejectCode::UntradableBar,
            "the current bar has zero volume or is not tradable; quick fill rejected".into(),
            None,
        ));
    }
    let close_at = state.visible_until.ok_or_else(|| {
        reject(
            SimRejectCode::MissingReferencePrice,
            "no visible bar".into(),
            None,
        )
    })?;
    let probe = SimOrder {
        order_id: String::new(),
        order_seq: 0,
        instrument_id: req.instrument_id.clone(),
        side,
        order_type: SimOrderType::Market,
        tif: req.tif,
        priority: req.priority,
        quantity: qty,
        parent_group_id: None,
        parent_order_id: None,
        oco_peer_order_id: None,
        reservation_id: None,
        group_role: OrderGroupRole::Standalone,
        has_trade_plan: false,
        status: OrderStatus::Active,
        reject_code: None,
        cancel_reason: None,
        submitted_at: close_at,
        eligible_at: close_at,
        updated_at: close_at,
        activated_bar_close_at: None,
        activated_stage: None,
        activated_path_price: None,
    };
    let fill_price =
        compute_execution_price_for_order(&probe, close, spec.tick_size, &state.fee_profile);
    let settlement = settle_order_fees(
        &state.fee_profile,
        side,
        &spec.base_asset_id,
        &spec.quote_currency,
        &[SyntheticFillInput {
            fill_id: "QUICK".into(),
            quantity: qty,
            fill_price,
        }],
    )?;
    match side {
        OrderSide::Buy => {
            let need = -settlement.fills[0].net_quote_cash_delta;
            let avail = state.available_cash();
            if need > avail {
                return Err(reject_amount(
                    SimRejectCode::InsufficientFunds,
                    format!(
                        "insufficient available cash: required {}, available {}",
                        to_canonical_text(need),
                        to_canonical_text(avail)
                    ),
                    Some("quantity"),
                    need,
                    avail,
                ));
            }
        }
        OrderSide::Sell => {
            let avail = state.available_position(&req.instrument_id);
            if qty > avail {
                return Err(reject_amount(
                    SimRejectCode::InsufficientPosition,
                    format!(
                        "insufficient available position: required {}, available {}",
                        to_canonical_text(qty),
                        to_canonical_text(avail)
                    ),
                    Some("quantity"),
                    qty,
                    avail,
                ));
            }
        }
    }
    let seq = state.next_order_seq;
    state.next_order_seq += 1;
    let order_id = next_order_id(seq);
    let mut order = probe;
    order.order_id = order_id.clone();
    order.order_seq = seq;
    // A sell needs its quantity locked like any sell order until the fill consumes it.
    if side == OrderSide::Sell {
        let r = next_res_id(state.next_reservation_seq);
        state.next_reservation_seq += 1;
        state.position_reservations.insert(
            r.clone(),
            PositionReservation {
                reservation_id: r.clone(),
                instrument_id: req.instrument_id.clone(),
                quantity: qty,
                order_ids: vec![order_id.clone()],
            },
        );
        order.reservation_id = Some(r);
    }
    state.orders.insert(order_id.clone(), order);
    let mut events = vec![SimEvent::OrderSubmitted {
        order_ids: vec![order_id.clone()],
        parent_group_id: None,
        cash_reserved: Decimal::ZERO,
        position_reserved: if side == OrderSide::Sell {
            qty
        } else {
            Decimal::ZERO
        },
    }];
    // Prefer the exact stepped bar's `open_at` / `session_date` when present; otherwise derive
    // `session_date` from `close_at - 1s` so both US equity 20:00/21:00Z and 24/7 crypto 00:00Z
    // closes land on the bar's actual trading date.
    let (open_at, session_date) = state
        .bar_snapshots
        .last()
        .filter(|s| s.instrument_id == req.instrument_id && s.close_at == close_at)
        .map(|s| (s.open_at, s.session_date))
        .unwrap_or_else(|| {
            (
                close_at - state.timeframe_duration(),
                (close_at - chrono::Duration::seconds(1)).date_naive(),
            )
        });
    let bar = BarRecord {
        id: format!(
            "VISIBLE-CLOSE:{}:{}",
            req.instrument_id,
            close_at.to_rfc3339()
        ),
        instrument_id: req.instrument_id.clone(),
        timeframe: state.timeframe.clone().unwrap_or_else(|| "1d".into()),
        session: "visible_close".into(),
        session_date,
        open_at,
        close_at,
        open: close,
        high: close,
        low: close,
        close,
        volume: Decimal::ONE,
        source: "visible_close".into(),
        adjustment: AdjustmentKind::Raw,
        is_final: true,
        valid_from_version: 0,
        valid_to_version: None,
        raw_payload_hash: None,
    };
    let trig = TriggerMatch {
        order_id: order_id.clone(),
        priority: req.priority,
        instrument_id: req.instrument_id.clone(),
        order_seq: seq,
        trigger_path_price: close,
        distance_from_cur: Decimal::ZERO,
        fill_price,
    };
    let mut buys = BTreeMap::new();
    let mut sells = BTreeMap::new();
    execute_triggered_order(
        state,
        &trig,
        BarPathStage::HighToClose,
        &bar,
        &mut buys,
        &mut sells,
        &mut events,
    )?;
    if state.orders.get(&order_id).map(|o| o.status) != Some(OrderStatus::Filled) {
        return Err(reject(
            SimRejectCode::InsufficientFundsAtFill,
            "quick fill could not be executed".into(),
            None,
        ));
    }
    // Refresh the latest stepped snapshot when the quick fill occurs at that bar's close so
    // `equity_curve` and `exposed_bars` stay aligned with `ending_equity` after quick-fill costs.
    let updated_cash = state.cash_balance;
    let updated_pos = state.position_quantity(&req.instrument_id);
    let updated_cost = state.position_cost(&req.instrument_id);
    let updated_equity = state.total_equity_at_last_close();
    if let Some(last_snap) = state
        .bar_snapshots
        .last_mut()
        .filter(|s| s.instrument_id == req.instrument_id && s.close_at == close_at)
    {
        if side == OrderSide::Buy {
            last_snap.had_buy_fill = true;
        } else {
            last_snap.had_sell_fill = true;
        }
        last_snap.cash_after = updated_cash;
        last_snap.position_qty_after = updated_pos;
        last_snap.position_cost_after = updated_cost;
        last_snap.equity_at_close = updated_equity;
    }
    state.quick_fill_count += 1;
    Ok(events)
}

/// Modify (`S-51` ②): price and/or quantity of an open order, equivalent to an atomic cancel
/// and re-place. The order keeps its id, group and OCO links; reservations are recomputed and it
/// becomes eligible from the next bar open. Filled, expired, cancelled or rejected orders refuse.
fn modify_order_into_state(
    state: &mut SimState,
    order_id: &str,
    quantity: Option<Decimal>,
    limit_price: Option<Decimal>,
    stop_price: Option<Decimal>,
) -> Result<Vec<SimEvent>, SimError> {
    require_m5(state, "modify order")?;
    if state.finished {
        return Err(SimError::SessionFinished);
    }
    let order = state
        .orders
        .get(order_id)
        .cloned()
        .ok_or_else(|| SimError::UnknownOrder(order_id.to_string()))?;
    if !order.status.is_open_or_pending() {
        return Err(SimError::OrderNotModifiable {
            order_id: order_id.to_string(),
            status: order.status,
        });
    }
    if quantity.is_none() && limit_price.is_none() && stop_price.is_none() {
        return Err(reject(
            SimRejectCode::InvalidOrderParams,
            "modify needs a new quantity or price".into(),
            None,
        ));
    }
    let new_type = match (order.order_type, limit_price, stop_price) {
        (SimOrderType::Limit { limit_price: old }, lp, None) => SimOrderType::Limit {
            limit_price: lp.unwrap_or(old),
        },
        (SimOrderType::StopMarket { stop_price: old }, None, sp) => SimOrderType::StopMarket {
            stop_price: sp.unwrap_or(old),
        },
        (SimOrderType::Market, None, None) => SimOrderType::Market,
        _ => return Err(reject(
            SimRejectCode::InvalidOrderParams,
            "the new price must match the order type (limit price for limit, stop price for stop)"
                .into(),
            Some("price"),
        )),
    }
    .canonical();
    let linked = order.parent_order_id.is_some() || order.oco_peer_order_id.is_some();
    let new_qty = quantity.unwrap_or(order.quantity);
    if linked && new_qty != order.quantity {
        return Err(reject(
            SimRejectCode::InvalidOrderParams,
            "quantity of OCO / attached exit legs follows their group; change the price only"
                .into(),
            Some("quantity"),
        ));
    }
    let spec = state.instruments[&order.instrument_id].clone();
    if let Some(p) = new_type.limit_price().or(new_type.stop_price()) {
        validate_tick_field(p, spec.tick_size, "price").map_err(SimError::OrderRejected)?;
    }
    // Keep the take-profit strictly above the stop-loss within an open/pending OCO / bracket pair.
    if let Some(peer) = order
        .oco_peer_order_id
        .as_ref()
        .and_then(|id| state.orders.get(id))
        .filter(|p| p.status.is_open_or_pending())
    {
        let (tp, sl) = match (new_type, peer.order_type) {
            (SimOrderType::Limit { limit_price }, SimOrderType::StopMarket { stop_price }) => {
                (limit_price, stop_price)
            }
            (SimOrderType::StopMarket { stop_price }, SimOrderType::Limit { limit_price }) => {
                (limit_price, stop_price)
            }
            _ => (Decimal::ONE, Decimal::ZERO),
        };
        if tp <= sl {
            return Err(reject(
                SimRejectCode::InvalidOrderParams,
                "take-profit must stay strictly above the stop-loss".into(),
                Some("price"),
            ));
        }
    }
    // Keep a pending attached exit leg consistent with its parent's explicit entry price.
    if let Some(parent) = order
        .parent_order_id
        .as_ref()
        .and_then(|id| state.orders.get(id))
        .filter(|p| p.status.is_open_or_pending())
    {
        if let Some(entry_p) = parent
            .order_type
            .limit_price()
            .or(parent.order_type.stop_price())
        {
            if let SimOrderType::Limit { limit_price: tp } = new_type {
                if tp <= entry_p {
                    return Err(reject(
                        SimRejectCode::InvalidOrderParams,
                        "attached take_profit_limit must exceed entry order price".into(),
                        Some("take_profit_limit"),
                    ));
                }
            }
            if let SimOrderType::StopMarket { stop_price: sl } = new_type {
                if sl >= entry_p {
                    return Err(reject(
                        SimRejectCode::InvalidOrderParams,
                        "attached stop_loss_stop must be below entry order price".into(),
                        Some("stop_loss_stop"),
                    ));
                }
            }
        }
    }
    // Keep a BracketEntry parent's new explicit price consistent with its pending attached exits.
    let pending_children: Vec<(String, OrderGroupRole, SimOrderType)> = state
        .orders
        .values()
        .filter(|c| {
            c.parent_order_id.as_deref() == Some(order_id) && c.status == OrderStatus::PendingParent
        })
        .map(|c| (c.order_id.clone(), c.group_role, c.order_type))
        .collect();
    if let Some(entry_p) = new_type.limit_price().or(new_type.stop_price()) {
        for (_, role, child_type) in &pending_children {
            match (role, child_type) {
                (OrderGroupRole::BracketTakeProfit, SimOrderType::Limit { limit_price: tp })
                    if *tp <= entry_p =>
                {
                    return Err(reject(
                        SimRejectCode::InvalidOrderParams,
                        "entry order price must stay below attached take_profit_limit".into(),
                        Some("price"),
                    ));
                }
                (OrderGroupRole::BracketStopLoss, SimOrderType::StopMarket { stop_price: sl })
                    if *sl >= entry_p =>
                {
                    return Err(reject(
                        SimRejectCode::InvalidOrderParams,
                        "entry order price must stay above attached stop_loss_stop".into(),
                        Some("price"),
                    ));
                }
                _ => {}
            }
        }
    }

    let at = state.visible_until.unwrap_or(order.submitted_at);
    // Re-validate funds / position directly with this order's existing reservation excluded
    // (avoids cloning the entire SimState).
    let mut final_qty = order.quantity;
    if order.status != OrderStatus::PendingParent && !linked {
        let old_cash_res = order
            .reservation_id
            .as_ref()
            .and_then(|rid| state.cash_reservations.get(rid))
            .map(|r| r.amount)
            .unwrap_or(Decimal::ZERO);
        let old_pos_res = order
            .reservation_id
            .as_ref()
            .and_then(|rid| state.position_reservations.get(rid))
            .filter(|r| r.order_ids.len() == 1 && r.order_ids[0] == order_id)
            .map(|r| r.quantity)
            .unwrap_or(Decimal::ZERO);
        let avail_cash = canon_decimal(
            (state.cash_balance - (state.reserved_cash() - old_cash_res)).max(Decimal::ZERO),
        );
        let avail_pos = canon_decimal(
            (state.position_quantity(&order.instrument_id)
                - (state.reserved_position(&order.instrument_id) - old_pos_res))
                .max(Decimal::ZERO),
        );
        let req = OrderSubmitRequest {
            instrument_id: order.instrument_id.clone(),
            quantity: new_qty,
            tif: order.tif,
            priority: order.priority,
            kind: OrderSubmitKind::Single {
                side: order.side,
                order_type: new_type,
                attached_bracket: None,
            },
            eligible_at_hint: None,
            fill_rule: None,
        };
        let preview = preview_order_submission_with_available(state, &req, avail_cash, avail_pos)?;
        release_order_reservation(state, order_id);
        let res_id = next_res_id(state.next_reservation_seq);
        state.next_reservation_seq += 1;
        if preview.cash_reservation_required > Decimal::ZERO {
            state.cash_reservations.insert(
                res_id.clone(),
                CashReservation {
                    reservation_id: res_id.clone(),
                    order_id: order_id.to_string(),
                    instrument_id: order.instrument_id.clone(),
                    amount: preview.cash_reservation_required,
                },
            );
        } else {
            state.position_reservations.insert(
                res_id.clone(),
                PositionReservation {
                    reservation_id: res_id.clone(),
                    instrument_id: order.instrument_id.clone(),
                    quantity: preview.position_reservation_required,
                    order_ids: vec![order_id.to_string()],
                },
            );
        }
        final_qty = preview.quantity;
        if let Some(o) = state.orders.get_mut(order_id) {
            o.reservation_id = Some(res_id);
            o.quantity = final_qty;
        }
    }
    // Keep PendingParent bracket exit legs' displayed quantity aligned when entry quantity changes.
    if final_qty != order.quantity {
        for (cid, _, _) in &pending_children {
            if let Some(child) = state.orders.get_mut(cid) {
                child.quantity = final_qty;
                child.updated_at = at;
            }
        }
    }
    let o = state.orders.get_mut(order_id).expect("order exists");
    o.order_type = new_type;
    o.submitted_at = at;
    o.eligible_at = at;
    o.updated_at = at;
    if o.status == OrderStatus::Active {
        o.status = OrderStatus::Accepted;
    }
    let event = SimEvent::OrderModified {
        order_id: order_id.to_string(),
        instrument_id: o.instrument_id.clone(),
        quantity: o.quantity,
        limit_price: new_type.limit_price(),
        stop_price: new_type.stop_price(),
        eligible_at: at,
    };
    state.modify_count += 1;
    Ok(vec![event])
}

/// Undo-aware command reducer (`S-51` ③). Since the latest bar boundary (initial state, a bar
/// advance, or a boundary checkpoint) it keeps each live undoable command together with the
/// exact state before it. `UndoLast` voids the most recent live command by restoring that state,
/// so cash, position, fees, reservations and the ids of every surviving order/fill are restored
/// exactly (no re-numbering), while id counters only move forward. Nothing at or before a bar
/// advance can be undone.
#[derive(Debug, Clone)]
pub struct SimReplayer {
    live: Vec<(u64, SimCommand, SimState)>,
}

impl SimReplayer {
    /// `boundary` is the state at the latest bar boundary; undo never reaches behind it.
    pub fn new(boundary: SimState) -> Self {
        let _ = boundary;
        Self { live: Vec::new() }
    }

    /// The seq the next `UndoLast` would void, if any.
    pub fn undo_target(&self) -> Option<(u64, &SimCommand)> {
        self.live.last().map(|(s, c, _)| (*s, c))
    }

    pub fn apply(
        &mut self,
        current: &SimState,
        seq: u64,
        command: SimCommand,
        bar: Option<&SimStepBar>,
    ) -> Result<(SimState, Vec<SimEvent>), SimError> {
        match command {
            SimCommand::UndoLast { target_seq } => {
                require_m5(current, "undo")?;
                if current.finished {
                    return Err(SimError::SessionFinished);
                }
                let Some((last_seq, last_cmd, before)) = self.live.last().cloned() else {
                    return Err(SimError::UndoUnavailable(
                        "no command since the last bar advance".into(),
                    ));
                };
                if last_seq != target_seq {
                    return Err(SimError::UndoUnavailable(format!(
                        "only the latest command (seq {last_seq}) can be undone, not {target_seq}"
                    )));
                }
                self.live.pop();
                let mut rebuilt = before;
                // Counters never move backwards: ids voided by an undo are not reused.
                rebuilt.command_seq = current.command_seq + 1;
                rebuilt.next_order_seq = rebuilt.next_order_seq.max(current.next_order_seq);
                rebuilt.next_fill_seq = rebuilt.next_fill_seq.max(current.next_fill_seq);
                rebuilt.next_group_seq = rebuilt.next_group_seq.max(current.next_group_seq);
                rebuilt.next_reservation_seq = rebuilt
                    .next_reservation_seq
                    .max(current.next_reservation_seq);
                rebuilt.undo_count = current.undo_count + 1;
                rebuilt.normalize_fields();
                Ok((
                    rebuilt,
                    vec![SimEvent::CommandUndone {
                        target_seq,
                        target_command: last_cmd.kind_str().to_string(),
                    }],
                ))
            }
            cmd @ (SimCommand::AdvanceBar { .. } | SimCommand::Finish) => {
                let out = step(current, cmd, bar)?;
                self.live.clear();
                Ok(out)
            }
            cmd => {
                let undoable = cmd.is_undoable();
                let out = step(current, cmd.clone(), None)?;
                if undoable && current.is_m5() {
                    self.live.push((seq, cmd, current.clone()));
                }
                Ok(out)
            }
        }
    }
}

fn submit_order_into_state(
    state: &mut SimState,
    req: OrderSubmitRequest,
) -> Result<Vec<SimEvent>, SimError> {
    if req.fill_rule == Some(FillRule::VisibleClose) {
        return quick_fill_into_state(state, req);
    }
    let preview = preview_order_submission(state, &req)?;
    let submitted_at = state
        .visible_until
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).unwrap());
    let eligible_at = req
        .eligible_at_hint
        .filter(|&t| t >= submitted_at)
        .unwrap_or(submitted_at);

    let mut created_ids = Vec::new();
    let mut parent_group_id_out = None;

    match req.kind {
        OrderSubmitKind::Single {
            side,
            order_type,
            attached_bracket,
        } => {
            let order_type = order_type.canonical();
            let entry_seq = state.next_order_seq;
            state.next_order_seq += 1;
            let entry_id = next_order_id(entry_seq);

            let has_plan = attached_bracket.is_some();
            let group_id = if has_plan {
                let g = next_group_id(state.next_group_seq);
                state.next_group_seq += 1;
                parent_group_id_out = Some(g.clone());
                Some(g)
            } else {
                None
            };

            let res_id = if preview.cash_reservation_required > Decimal::ZERO {
                let r = next_res_id(state.next_reservation_seq);
                state.next_reservation_seq += 1;
                state.cash_reservations.insert(
                    r.clone(),
                    CashReservation {
                        reservation_id: r.clone(),
                        order_id: entry_id.clone(),
                        instrument_id: req.instrument_id.clone(),
                        amount: preview.cash_reservation_required,
                    },
                );
                Some(r)
            } else if preview.position_reservation_required > Decimal::ZERO {
                let r = next_res_id(state.next_reservation_seq);
                state.next_reservation_seq += 1;
                state.position_reservations.insert(
                    r.clone(),
                    PositionReservation {
                        reservation_id: r.clone(),
                        instrument_id: req.instrument_id.clone(),
                        quantity: preview.position_reservation_required,
                        order_ids: vec![entry_id.clone()],
                    },
                );
                Some(r)
            } else {
                None
            };

            state.orders.insert(
                entry_id.clone(),
                SimOrder {
                    order_id: entry_id.clone(),
                    order_seq: entry_seq,
                    instrument_id: req.instrument_id.clone(),
                    side,
                    order_type,
                    tif: req.tif,
                    priority: req.priority,
                    quantity: preview.quantity,
                    parent_group_id: group_id.clone(),
                    parent_order_id: None,
                    oco_peer_order_id: None,
                    reservation_id: res_id,
                    group_role: if has_plan {
                        OrderGroupRole::BracketEntry
                    } else {
                        OrderGroupRole::Standalone
                    },
                    has_trade_plan: has_plan,
                    status: OrderStatus::Accepted,
                    reject_code: None,
                    cancel_reason: None,
                    submitted_at,
                    eligible_at,
                    updated_at: submitted_at,
                    activated_bar_close_at: None,
                    activated_stage: None,
                    activated_path_price: None,
                },
            );
            created_ids.push(entry_id.clone());

            if let Some(bracket) = attached_bracket {
                let bracket = bracket.canonical();
                let tp_id = bracket.take_profit_limit.map(|_| {
                    let s = state.next_order_seq;
                    state.next_order_seq += 1;
                    (s, next_order_id(s))
                });
                let sl_id = bracket.stop_loss_stop.map(|_| {
                    let s = state.next_order_seq;
                    state.next_order_seq += 1;
                    (s, next_order_id(s))
                });

                if let (Some(tp_price), Some((tp_seq, tp_ord_id))) =
                    (bracket.take_profit_limit, tp_id.clone())
                {
                    state.orders.insert(
                        tp_ord_id.clone(),
                        SimOrder {
                            order_id: tp_ord_id.clone(),
                            order_seq: tp_seq,
                            instrument_id: req.instrument_id.clone(),
                            side: OrderSide::Sell,
                            order_type: SimOrderType::Limit {
                                limit_price: tp_price,
                            },
                            tif: bracket.tif,
                            priority: req.priority,
                            quantity: preview.quantity,
                            parent_group_id: group_id.clone(),
                            parent_order_id: Some(entry_id.clone()),
                            oco_peer_order_id: sl_id.as_ref().map(|(_, id)| id.clone()),
                            reservation_id: None,
                            group_role: OrderGroupRole::BracketTakeProfit,
                            has_trade_plan: true,
                            status: OrderStatus::PendingParent,
                            reject_code: None,
                            cancel_reason: None,
                            submitted_at,
                            eligible_at,
                            updated_at: submitted_at,
                            activated_bar_close_at: None,
                            activated_stage: None,
                            activated_path_price: None,
                        },
                    );
                    created_ids.push(tp_ord_id);
                }
                if let (Some(sl_price), Some((sl_seq, sl_ord_id))) = (bracket.stop_loss_stop, sl_id)
                {
                    state.orders.insert(
                        sl_ord_id.clone(),
                        SimOrder {
                            order_id: sl_ord_id.clone(),
                            order_seq: sl_seq,
                            instrument_id: req.instrument_id.clone(),
                            side: OrderSide::Sell,
                            order_type: SimOrderType::StopMarket {
                                stop_price: sl_price,
                            },
                            tif: bracket.tif,
                            priority: req.priority,
                            quantity: preview.quantity,
                            parent_group_id: group_id,
                            parent_order_id: Some(entry_id),
                            oco_peer_order_id: tp_id.map(|(_, id)| id),
                            reservation_id: None,
                            group_role: OrderGroupRole::BracketStopLoss,
                            has_trade_plan: true,
                            status: OrderStatus::PendingParent,
                            reject_code: None,
                            cancel_reason: None,
                            submitted_at,
                            eligible_at,
                            updated_at: submitted_at,
                            activated_bar_close_at: None,
                            activated_stage: None,
                            activated_path_price: None,
                        },
                    );
                    created_ids.push(sl_ord_id);
                }
            }
        }
        OrderSubmitKind::Oco {
            take_profit_limit,
            stop_loss_stop,
            is_trade_plan,
        } => {
            let group_id = next_group_id(state.next_group_seq);
            state.next_group_seq += 1;
            parent_group_id_out = Some(group_id.clone());

            let tp_seq = state.next_order_seq;
            state.next_order_seq += 1;
            let tp_id = next_order_id(tp_seq);

            let sl_seq = state.next_order_seq;
            state.next_order_seq += 1;
            let sl_id = next_order_id(sl_seq);

            let res_id = next_res_id(state.next_reservation_seq);
            state.next_reservation_seq += 1;
            state.position_reservations.insert(
                res_id.clone(),
                PositionReservation {
                    reservation_id: res_id.clone(),
                    instrument_id: req.instrument_id.clone(),
                    quantity: preview.quantity,
                    order_ids: vec![tp_id.clone(), sl_id.clone()],
                },
            );

            state.orders.insert(
                tp_id.clone(),
                SimOrder {
                    order_id: tp_id.clone(),
                    order_seq: tp_seq,
                    instrument_id: req.instrument_id.clone(),
                    side: OrderSide::Sell,
                    order_type: SimOrderType::Limit {
                        limit_price: canon_decimal(take_profit_limit),
                    },
                    tif: req.tif,
                    priority: req.priority,
                    quantity: preview.quantity,
                    parent_group_id: Some(group_id.clone()),
                    parent_order_id: None,
                    oco_peer_order_id: Some(sl_id.clone()),
                    reservation_id: Some(res_id.clone()),
                    group_role: OrderGroupRole::OcoTakeProfit,
                    has_trade_plan: is_trade_plan,
                    status: OrderStatus::Accepted,
                    reject_code: None,
                    cancel_reason: None,
                    submitted_at,
                    eligible_at,
                    updated_at: submitted_at,
                    activated_bar_close_at: None,
                    activated_stage: None,
                    activated_path_price: None,
                },
            );
            state.orders.insert(
                sl_id.clone(),
                SimOrder {
                    order_id: sl_id.clone(),
                    order_seq: sl_seq,
                    instrument_id: req.instrument_id,
                    side: OrderSide::Sell,
                    order_type: SimOrderType::StopMarket {
                        stop_price: canon_decimal(stop_loss_stop),
                    },
                    tif: req.tif,
                    priority: req.priority,
                    quantity: preview.quantity,
                    parent_group_id: Some(group_id),
                    parent_order_id: None,
                    oco_peer_order_id: Some(tp_id.clone()),
                    reservation_id: Some(res_id),
                    group_role: OrderGroupRole::OcoStopLoss,
                    has_trade_plan: is_trade_plan,
                    status: OrderStatus::Accepted,
                    reject_code: None,
                    cancel_reason: None,
                    submitted_at,
                    eligible_at,
                    updated_at: submitted_at,
                    activated_bar_close_at: None,
                    activated_stage: None,
                    activated_path_price: None,
                },
            );
            created_ids.push(tp_id);
            created_ids.push(sl_id);
        }
    }

    Ok(vec![SimEvent::OrderSubmitted {
        order_ids: created_ids,
        parent_group_id: parent_group_id_out,
        cash_reserved: preview.cash_reservation_required,
        position_reserved: preview.position_reservation_required,
    }])
}

fn cancel_order_command(
    state: &mut SimState,
    order_id: &str,
    reason: Option<String>,
) -> Result<Vec<SimEvent>, SimError> {
    if state.finished {
        return Err(SimError::SessionFinished);
    }
    let Some(order) = state.orders.get(order_id).cloned() else {
        return Err(SimError::UnknownOrder(order_id.to_string()));
    };
    if !order.status.is_open_or_pending() {
        return Err(SimError::OrderNotCancellable {
            order_id: order_id.to_string(),
            status: order.status,
        });
    }

    let updated_at = state.visible_until.unwrap_or(order.submitted_at);
    let reason_str = reason.unwrap_or_else(|| "user_cancelled".to_string());
    let mut events = Vec::new();

    cancel_order_internal(state, order_id, &reason_str, updated_at, &mut events);

    // Cancel OCO peer if present
    if let Some(peer_id) = order.oco_peer_order_id {
        cancel_order_internal(
            state,
            &peer_id,
            "oco_peer_cancelled",
            updated_at,
            &mut events,
        );
    }

    cancel_pending_children(
        state,
        order_id,
        "parent_order_cancelled",
        updated_at,
        &mut events,
    );

    Ok(events)
}

#[derive(Debug, Clone)]
struct TriggerMatch {
    order_id: String,
    priority: i32,
    instrument_id: String,
    order_seq: u64,
    trigger_path_price: Decimal,
    distance_from_cur: Decimal,
    fill_price: Decimal,
}

fn compute_execution_price_for_order(
    order: &SimOrder,
    trigger_path_price: Decimal,
    tick_size: Decimal,
    fee_profile: &FeeProfileVersion,
) -> Decimal {
    let spread_delta = (trigger_path_price * fee_profile.half_spread_bps) / Decimal::from(10_000);
    let slip_delta = fee_profile
        .slippage
        .delta_for_price(trigger_path_price, tick_size);

    match order.order_type {
        SimOrderType::Market => {
            let raw = match order.side {
                OrderSide::Buy => trigger_path_price + spread_delta + slip_delta,
                OrderSide::Sell => trigger_path_price - spread_delta - slip_delta,
            };
            round_price_adverse_tick(raw, order.side, tick_size)
        }
        SimOrderType::Limit { limit_price } => match order.side {
            OrderSide::Buy => {
                if trigger_path_price < limit_price {
                    let raw = trigger_path_price + slip_delta;
                    round_price_adverse_tick(raw, OrderSide::Buy, tick_size).min(limit_price)
                } else {
                    canon_decimal(limit_price)
                }
            }
            OrderSide::Sell => {
                if trigger_path_price > limit_price {
                    let raw = trigger_path_price - slip_delta;
                    round_price_adverse_tick(raw, OrderSide::Sell, tick_size).max(limit_price)
                } else {
                    canon_decimal(limit_price)
                }
            }
        },
        SimOrderType::StopMarket { .. } => {
            let raw = match order.side {
                OrderSide::Buy => trigger_path_price + slip_delta,
                OrderSide::Sell => trigger_path_price - slip_delta,
            };
            round_price_adverse_tick(raw, order.side, tick_size)
        }
        SimOrderType::StopLimit { .. } => unreachable!("StopLimit rejected at submit"),
    }
}

fn check_order_trigger_on_subsegment(
    order: &SimOrder,
    stage: BarPathStage,
    cur: Decimal,
    seg_end: Decimal,
    tick_size: Decimal,
    fee_profile: &FeeProfileVersion,
) -> Option<TriggerMatch> {
    let trigger_opt = match order.order_type {
        SimOrderType::Market => (stage == BarPathStage::AtOpen).then_some(cur),
        SimOrderType::Limit { limit_price } => match order.side {
            OrderSide::Buy => {
                if cur <= limit_price {
                    Some(cur)
                } else if seg_end < cur && seg_end <= limit_price {
                    Some(limit_price)
                } else {
                    None
                }
            }
            OrderSide::Sell => {
                if cur >= limit_price {
                    Some(cur)
                } else if seg_end > cur && seg_end >= limit_price {
                    Some(limit_price)
                } else {
                    None
                }
            }
        },
        SimOrderType::StopMarket { stop_price } => match order.side {
            OrderSide::Buy => {
                if cur >= stop_price {
                    Some(cur)
                } else if seg_end > cur && seg_end >= stop_price {
                    Some(stop_price)
                } else {
                    None
                }
            }
            OrderSide::Sell => {
                if cur <= stop_price {
                    Some(cur)
                } else if seg_end < cur && seg_end <= stop_price {
                    Some(stop_price)
                } else {
                    None
                }
            }
        },
        SimOrderType::StopLimit { .. } => None,
    }?;

    let distance = (trigger_opt - cur).abs();
    let fill_price = compute_execution_price_for_order(order, trigger_opt, tick_size, fee_profile);
    Some(TriggerMatch {
        order_id: order.order_id.clone(),
        priority: order.priority,
        instrument_id: order.instrument_id.clone(),
        order_seq: order.order_seq,
        trigger_path_price: canon_decimal(trigger_opt),
        distance_from_cur: canon_decimal(distance),
        fill_price,
    })
}

/// Check whether an order's OCO peer would ALSO have been reachable on the remaining bar path
/// starting from the pair's activation point on this bar (`S-39`, `M4-A-05`).
fn is_oco_dual_touch_ambiguous(
    state: &SimState,
    order: &SimOrder,
    bar: &BarRecord,
    current_stage: BarPathStage,
) -> bool {
    let Some(peer_id) = &order.oco_peer_order_id else {
        return false;
    };
    let Some(peer) = state.orders.get(peer_id) else {
        return false;
    };
    if !peer.status.is_open_or_pending() {
        return false;
    }

    let (act_stage, act_price) = if order.activated_bar_close_at == Some(bar.close_at) {
        (
            order.activated_stage.unwrap_or(BarPathStage::AtOpen),
            order.activated_path_price.unwrap_or(bar.open),
        )
    } else {
        (BarPathStage::AtOpen, bar.open)
    };

    if current_stage == BarPathStage::AtOpen && act_stage == BarPathStage::AtOpen {
        return false;
    }

    let (min_rem, max_rem) = remaining_bar_extrema(bar, act_stage, act_price);
    let self_reachable = order_would_reach_extrema(order, act_price, min_rem, max_rem);
    let peer_reachable = order_would_reach_extrema(peer, act_price, min_rem, max_rem);
    self_reachable && peer_reachable
}

fn remaining_bar_extrema(
    bar: &BarRecord,
    from_stage: BarPathStage,
    from_price: Decimal,
) -> (Decimal, Decimal) {
    match from_stage {
        BarPathStage::AtOpen => (bar.low, bar.high),
        BarPathStage::OpenToLow => (from_price.min(bar.low), from_price.max(bar.high)),
        BarPathStage::LowToHigh => (from_price.min(bar.close), from_price.max(bar.high)),
        BarPathStage::HighToClose => (from_price.min(bar.close), from_price.max(bar.close)),
    }
}

fn order_would_reach_extrema(
    order: &SimOrder,
    act_price: Decimal,
    min_rem: Decimal,
    max_rem: Decimal,
) -> bool {
    match (order.side, order.order_type) {
        (OrderSide::Sell, SimOrderType::Limit { limit_price }) => {
            act_price >= limit_price || max_rem >= limit_price
        }
        (OrderSide::Sell, SimOrderType::StopMarket { stop_price }) => {
            act_price <= stop_price || min_rem <= stop_price
        }
        (OrderSide::Buy, SimOrderType::Limit { limit_price }) => {
            act_price <= limit_price || min_rem <= limit_price
        }
        (OrderSide::Buy, SimOrderType::StopMarket { stop_price }) => {
            act_price >= stop_price || max_rem >= stop_price
        }
        (_, SimOrderType::Market) => true,
        _ => false,
    }
}

fn allocate_fifo_lots_for_sell(
    lots: &mut Vec<SimLot>,
    sell_qty: Decimal,
) -> Result<Decimal, SimError> {
    let avail: Decimal = lots.iter().map(|l| l.remaining_quantity).sum();
    if avail < sell_qty {
        return Err(reject_amount(
            SimRejectCode::InsufficientPosition,
            format!(
                "FIFO allocation oversell: needed {}, available {}",
                to_canonical_text(sell_qty),
                to_canonical_text(avail),
            ),
            Some("quantity"),
            sell_qty,
            avail,
        ));
    }
    let mut rem = sell_qty;
    let mut total_cost = Decimal::ZERO;
    for lot in lots.iter_mut() {
        if rem.is_zero() {
            break;
        }
        if lot.remaining_quantity.is_zero() {
            continue;
        }
        let take = rem.min(lot.remaining_quantity);
        let cost = if take == lot.remaining_quantity {
            lot.remaining_cost_quote
        } else {
            let rounded = canon_decimal(
                ((lot.remaining_cost_quote * take) / lot.remaining_quantity)
                    .round_dp_with_strategy(8, RoundingStrategy::MidpointAwayFromZero),
            );
            if lot.remaining_cost_quote >= Decimal::ZERO {
                rounded.min(lot.remaining_cost_quote)
            } else {
                rounded
            }
        };
        lot.remaining_quantity = canon_decimal(lot.remaining_quantity - take);
        lot.remaining_cost_quote = canon_decimal(lot.remaining_cost_quote - cost);
        rem = canon_decimal(rem - take);
        total_cost = canon_decimal(total_cost + cost);
    }
    lots.retain(|l| l.remaining_quantity > Decimal::ZERO);
    Ok(total_cost)
}

fn apply_pre_open_corporate_actions_for_bar(
    state: &mut SimState,
    bar: &BarRecord,
    corporate_actions: &[CorporateActionRecord],
    events: &mut Vec<SimEvent>,
) -> Result<(), SimError> {
    let relevant: Vec<CorporateActionRecord> = corporate_actions
        .iter()
        .filter(|ca| ca.instrument_id == bar.instrument_id && ca.ex_date == bar.session_date)
        .cloned()
        .collect();
    if relevant.is_empty() {
        return Ok(());
    }
    let dedup = deduplicate_corporate_actions(&relevant);

    for action in dedup {
        let action_key = (
            action.instrument_id.clone(),
            action.ex_date,
            action.action_type.as_str().to_string(),
        );
        if state.applied_corporate_actions.contains(&action_key) {
            continue;
        }

        match action.action_type {
            CorporateActionType::Split => {
                let ratio = canon_decimal(action.ratio_or_amount);
                if ratio <= Decimal::ZERO {
                    return Err(SimError::InvalidCorporateAction {
                        action_id: action.id.clone(),
                        reason: format!("split ratio must be positive, got {ratio}"),
                    });
                }
                let qty_before = state.position_quantity(&bar.instrument_id);
                if let Some(lots) = state.lots_by_instrument.get_mut(&bar.instrument_id) {
                    for lot in lots.iter_mut() {
                        lot.original_quantity =
                            canon_decimal(lot.original_quantity.checked_mul(ratio).ok_or_else(
                                || SimError::ArithmeticOverflow("split_orig_qty".into()),
                            )?);
                        lot.remaining_quantity =
                            canon_decimal(lot.remaining_quantity.checked_mul(ratio).ok_or_else(
                                || SimError::ArithmeticOverflow("split_rem_qty".into()),
                            )?);
                        // Total cost basis (`remaining_cost_quote`) is strictly unchanged (`S-42`)!
                    }
                }
                let qty_after = state.position_quantity(&bar.instrument_id);

                // Cancel all open/pending orders for this instrument with reason `corporate_action_split` (`S-42`, `M4-A-03`)
                let open_ids: Vec<String> = state
                    .orders
                    .values()
                    .filter(|o| {
                        o.instrument_id == bar.instrument_id && o.status.is_open_or_pending()
                    })
                    .map(|o| o.order_id.clone())
                    .collect();
                let mut cancel_events = Vec::new();
                for ord_id in &open_ids {
                    cancel_order_internal(
                        state,
                        ord_id,
                        "corporate_action_split",
                        bar.open_at,
                        &mut cancel_events,
                    );
                }

                state.applied_corporate_actions.insert(action_key);
                events.push(SimEvent::CorporateActionApplied {
                    action_id: action.id.clone(),
                    instrument_id: bar.instrument_id.clone(),
                    action_type: CorporateActionType::Split,
                    ex_date: action.ex_date,
                    ratio_or_amount: ratio,
                    position_qty_before: qty_before,
                    position_qty_after: qty_after,
                    cash_credited: Decimal::ZERO,
                    cancelled_order_ids: open_ids,
                    assumption_marker: None,
                });
                events.extend(cancel_events);
            }
            CorporateActionType::CashDividend => {
                let div_per_share = canon_decimal(action.ratio_or_amount);
                if div_per_share < Decimal::ZERO {
                    return Err(SimError::InvalidCorporateAction {
                        action_id: action.id.clone(),
                        reason: format!("cash dividend cannot be negative, got {div_per_share}"),
                    });
                }
                let qty = state.position_quantity(&bar.instrument_id);
                let cash_credited = if qty > Decimal::ZERO && div_per_share > Decimal::ZERO {
                    let credit =
                        canon_decimal(qty.checked_mul(div_per_share).ok_or_else(|| {
                            SimError::ArithmeticOverflow("dividend_credit".into())
                        })?);
                    state.cash_balance = canon_decimal(state.cash_balance + credit);
                    state.dividend_income = canon_decimal(state.dividend_income + credit);
                    state.ex_date_dividend_assumed = true;
                    credit
                } else {
                    Decimal::ZERO
                };

                let marker = if cash_credited > Decimal::ZERO {
                    Some("ex_date_pre_open_cash_credit".to_string())
                } else {
                    None
                };
                state.applied_corporate_actions.insert(action_key);
                events.push(SimEvent::CorporateActionApplied {
                    action_id: action.id.clone(),
                    instrument_id: bar.instrument_id.clone(),
                    action_type: CorporateActionType::CashDividend,
                    ex_date: action.ex_date,
                    ratio_or_amount: div_per_share,
                    position_qty_before: qty,
                    position_qty_after: qty,
                    cash_credited,
                    cancelled_order_ids: Vec::new(),
                    assumption_marker: marker,
                });
            }
        }
    }
    Ok(())
}

fn validate_bar_ohlc(bar: &BarRecord) -> Result<(), SimError> {
    if !bar.is_final {
        return Err(SimError::InvalidBar {
            bar_id: bar.id.clone(),
            reason: "simulation only accepts closed final bars (is_final = true)".into(),
        });
    }
    if bar.open <= Decimal::ZERO
        || bar.high <= Decimal::ZERO
        || bar.low <= Decimal::ZERO
        || bar.close <= Decimal::ZERO
    {
        return Err(SimError::InvalidBar {
            bar_id: bar.id.clone(),
            reason: "bar OHLC prices must all be strictly positive".into(),
        });
    }
    if bar.low > bar.open
        || bar.low > bar.close
        || bar.high < bar.open
        || bar.high < bar.close
        || bar.low > bar.high
    {
        return Err(SimError::InvalidBar {
            bar_id: bar.id.clone(),
            reason: format!(
                "invalid OHLC envelope: O={}, H={}, L={}, C={}",
                bar.open, bar.high, bar.low, bar.close
            ),
        });
    }
    if bar.volume < Decimal::ZERO {
        return Err(SimError::InvalidBar {
            bar_id: bar.id.clone(),
            reason: "bar volume cannot be negative".into(),
        });
    }
    if bar.close_at < bar.open_at {
        return Err(SimError::InvalidBar {
            bar_id: bar.id.clone(),
            reason: "bar close_at precedes open_at".into(),
        });
    }
    Ok(())
}

/// Process a batch of simultaneous bars (single bar in M4 training, or multi-instrument bars
/// sharing `event_time` in M6 backtest) with deterministic ordering `(event_time, priority, instrument_id, order_seq)`.
pub fn step_multi_bars(
    state: &SimState,
    step_bars: &[SimStepBar],
    corporate_actions: &[CorporateActionRecord],
) -> Result<(SimState, Vec<SimEvent>), SimError> {
    if state.finished {
        return Err(SimError::SessionFinished);
    }
    if step_bars.is_empty() {
        return Err(SimError::MissingBarInput);
    }

    let mut next = state.clone();
    next.command_seq += 1;

    // Sort incoming bars deterministically by `(close_at, open_at, instrument_id)`
    let mut sorted_bars: Vec<&SimStepBar> = step_bars.iter().collect();
    sorted_bars.sort_by(|a, b| {
        a.bar
            .close_at
            .cmp(&b.bar.close_at)
            .then_with(|| a.bar.open_at.cmp(&b.bar.open_at))
            .then_with(|| a.bar.instrument_id.cmp(&b.bar.instrument_id))
    });

    // Validate instruments, OHLC invariants, and strict per-instrument + global replay clock monotonicity
    let mut seen_instruments = BTreeSet::new();
    for sb in &sorted_bars {
        if !next.instruments.contains_key(&sb.bar.instrument_id) {
            return Err(SimError::UnknownInstrument(sb.bar.instrument_id.clone()));
        }
        if !seen_instruments.insert(sb.bar.instrument_id.clone()) {
            return Err(SimError::InvalidBar {
                bar_id: sb.bar.id.clone(),
                reason: format!("duplicate bar in same step for {}", sb.bar.instrument_id),
            });
        }
        validate_bar_ohlc(&sb.bar)?;
        if let Some(prev_inst_close) = next.last_close_at_by_instrument.get(&sb.bar.instrument_id) {
            if sb.bar.close_at <= *prev_inst_close {
                return Err(SimError::ClockNotMonotonic {
                    current: Some(*prev_inst_close),
                    incoming: sb.bar.close_at,
                });
            }
        }
        if let Some(prev_global) = next.visible_until {
            if sb.bar.close_at <= prev_global {
                return Err(SimError::ClockNotMonotonic {
                    current: Some(prev_global),
                    incoming: sb.bar.close_at,
                });
            }
        }
    }

    let mut events = Vec::new();
    let mut pos_before_by_inst: BTreeMap<String, Decimal> = BTreeMap::new();
    let mut had_buy_by_inst: BTreeMap<String, bool> = BTreeMap::new();
    let mut had_sell_by_inst: BTreeMap<String, bool> = BTreeMap::new();

    for sb in &sorted_bars {
        pos_before_by_inst.insert(
            sb.bar.instrument_id.clone(),
            next.position_quantity(&sb.bar.instrument_id),
        );
        had_buy_by_inst.insert(sb.bar.instrument_id.clone(), false);
        had_sell_by_inst.insert(sb.bar.instrument_id.clone(), false);
        apply_pre_open_corporate_actions_for_bar(
            &mut next,
            &sb.bar,
            corporate_actions,
            &mut events,
        )?;
    }

    // Activate eligible Accepted orders across tradable/untradable bars
    let bar_map: BTreeMap<String, &SimStepBar> = sorted_bars
        .iter()
        .map(|sb| (sb.bar.instrument_id.clone(), *sb))
        .collect();

    for order in next.orders.values_mut() {
        if order.status == OrderStatus::Accepted {
            if let Some(sb) = bar_map.get(&order.instrument_id) {
                if sb.bar.close_at > order.submitted_at && sb.bar.open_at >= order.eligible_at {
                    order.status = OrderStatus::Active;
                    order.updated_at = sb.bar.open_at;
                }
            }
        }
    }

    // Traverse stages 0..=3 synchronously across all tradable bars in deterministic order
    let stages = [
        BarPathStage::AtOpen,
        BarPathStage::OpenToLow,
        BarPathStage::LowToHigh,
        BarPathStage::HighToClose,
    ];

    for stage in stages {
        let mut cur_price_by_inst: BTreeMap<String, (Decimal, Decimal)> = BTreeMap::new();
        for sb in &sorted_bars {
            if !sb.is_effectively_tradable() {
                continue;
            }
            let (seg_start, seg_end) = match stage {
                BarPathStage::AtOpen => (sb.bar.open, sb.bar.open),
                BarPathStage::OpenToLow => (sb.bar.open, sb.bar.low),
                BarPathStage::LowToHigh => (sb.bar.low, sb.bar.high),
                BarPathStage::HighToClose => (sb.bar.high, sb.bar.close),
            };
            cur_price_by_inst.insert(sb.bar.instrument_id.clone(), (seg_start, seg_end));
        }

        let max_iters = next.orders.len().saturating_mul(4).saturating_add(16);
        for _ in 0..max_iters {
            // Select the earliest path trigger per instrument by `(distance_from_cur, priority, order_seq)`,
            // then choose the single global minimum across instruments by `(open_at, priority, instrument_id, order_seq)`
            // (`S-36`). This two-level selection is strictly transitive and O(k).
            let mut best_per_inst: BTreeMap<String, TriggerMatch> = BTreeMap::new();

            for order in next.orders.values() {
                if order.status != OrderStatus::Active {
                    continue;
                }
                let Some(&(cur, seg_end)) = cur_price_by_inst.get(&order.instrument_id) else {
                    continue;
                };
                let Some(sb) = bar_map.get(&order.instrument_id) else {
                    continue;
                };
                if !(sb.bar.close_at > order.submitted_at && sb.bar.open_at >= order.eligible_at) {
                    continue;
                }
                // Enforce attached bracket post-entry activation rule (`S-39`, `M4-A-05`):
                // If activated on this bar at `act_stage`, never trigger on an earlier stage,
                // and at `AtOpen` an order activated BY an `AtOpen` fill only runs on `OpenToLow`+.
                if order.activated_bar_close_at == Some(sb.bar.close_at) {
                    if let Some(act_stage) = order.activated_stage {
                        if stage < act_stage
                            || (stage == BarPathStage::AtOpen && act_stage == BarPathStage::AtOpen)
                        {
                            continue;
                        }
                    }
                }

                let spec = &next.instruments[&order.instrument_id];
                if let Some(m) = check_order_trigger_on_subsegment(
                    order,
                    stage,
                    cur,
                    seg_end,
                    spec.tick_size,
                    &next.fee_profile,
                ) {
                    let replace = match best_per_inst.get(&m.instrument_id) {
                        None => true,
                        Some(existing) => {
                            (m.distance_from_cur, m.priority, m.order_seq)
                                < (
                                    existing.distance_from_cur,
                                    existing.priority,
                                    existing.order_seq,
                                )
                        }
                    };
                    if replace {
                        best_per_inst.insert(m.instrument_id.clone(), m);
                    }
                }
            }

            let Some(chosen) = best_per_inst.into_values().min_by(|a, b| {
                let ta = bar_map[&a.instrument_id].bar.open_at;
                let tb = bar_map[&b.instrument_id].bar.open_at;
                ta.cmp(&tb)
                    .then_with(|| a.priority.cmp(&b.priority))
                    .then_with(|| a.instrument_id.cmp(&b.instrument_id))
                    .then_with(|| a.order_seq.cmp(&b.order_seq))
            }) else {
                break;
            };

            if let Some(entry) = cur_price_by_inst.get_mut(&chosen.instrument_id) {
                entry.0 = chosen.trigger_path_price;
            }

            execute_triggered_order(
                &mut next,
                &chosen,
                stage,
                &bar_map[&chosen.instrument_id].bar,
                &mut had_buy_by_inst,
                &mut had_sell_by_inst,
                &mut events,
            )?;
        }
    }

    // End of bar: process simultaneous bars grouped by `close_at`. Updating all close prices for
    // the group before snapshotting guarantees consistent multi-instrument portfolio equity.
    let intraday = next.is_intraday();
    let mut group_start = 0;
    while group_start < sorted_bars.len() {
        let group_close_at = sorted_bars[group_start].bar.close_at;
        let mut group_end = group_start + 1;
        while group_end < sorted_bars.len() && sorted_bars[group_end].bar.close_at == group_close_at
        {
            group_end += 1;
        }
        let group = &sorted_bars[group_start..group_end];

        for sb in group {
            if next.is_m5() {
                next.last_bar_tradable
                    .insert(sb.bar.instrument_id.clone(), sb.is_effectively_tradable());
            }
            next.last_close_by_instrument
                .insert(sb.bar.instrument_id.clone(), canon_decimal(sb.bar.close));
            next.last_close_at_by_instrument
                .insert(sb.bar.instrument_id.clone(), sb.bar.close_at);
            next.visible_until = Some(match next.visible_until {
                Some(prev) => prev.max(sb.bar.close_at),
                None => sb.bar.close_at,
            });
        }

        for sb in group {
            // Expire remaining active/accepted DAY orders on each bar (including untradable/zero-volume bars, `M4-A-06`).
            // Daily bars: after the first eligible bar. `m5-sim-2` intraday (`S-51` ④): at the end of
            // the UTC calendar day containing `eligible_at`.
            let day_expire_ids: Vec<String> = next
                .orders
                .values()
                .filter(|o| {
                    o.instrument_id == sb.bar.instrument_id
                        && matches!(o.status, OrderStatus::Active | OrderStatus::Accepted)
                        && o.tif == TimeInForce::Day
                        && sb.bar.close_at > o.submitted_at
                        && sb.bar.open_at >= o.eligible_at
                        && (!intraday || sb.bar.close_at >= utc_day_end(o.eligible_at))
                })
                .map(|o| o.order_id.clone())
                .collect();

            for ord_id in day_expire_ids {
                let Some(ord_snapshot) = next.orders.get(&ord_id).cloned() else {
                    continue;
                };
                if !matches!(
                    ord_snapshot.status,
                    OrderStatus::Active | OrderStatus::Accepted
                ) {
                    continue;
                }
                if let Some(o) = next.orders.get_mut(&ord_id) {
                    o.status = OrderStatus::Expired;
                    o.updated_at = sb.bar.close_at;
                }
                release_order_reservation(&mut next, &ord_id);
                events.push(SimEvent::OrderExpired {
                    order_id: ord_id.clone(),
                    instrument_id: sb.bar.instrument_id.clone(),
                    bar_close_at: sb.bar.close_at,
                });

                cancel_pending_children(
                    &mut next,
                    &ord_id,
                    "parent_order_expired",
                    sb.bar.close_at,
                    &mut events,
                );
            }

            next.step_count += 1;

            let pos_after = next.position_quantity(&sb.bar.instrument_id);
            let cost_after = next.position_cost(&sb.bar.instrument_id);
            let equity = next.total_equity_at_last_close();
            let snapshot = SimBarSnapshot {
                step_index: next.step_count,
                instrument_id: sb.bar.instrument_id.clone(),
                session_date: sb.bar.session_date,
                open_at: sb.bar.open_at,
                close_at: sb.bar.close_at,
                open: canon_decimal(sb.bar.open),
                high: canon_decimal(sb.bar.high),
                low: canon_decimal(sb.bar.low),
                close: canon_decimal(sb.bar.close),
                volume: canon_decimal(sb.bar.volume),
                tradable: sb.is_effectively_tradable(),
                had_buy_fill: *had_buy_by_inst.get(&sb.bar.instrument_id).unwrap_or(&false),
                had_sell_fill: *had_sell_by_inst
                    .get(&sb.bar.instrument_id)
                    .unwrap_or(&false),
                position_qty_before: *pos_before_by_inst
                    .get(&sb.bar.instrument_id)
                    .unwrap_or(&Decimal::ZERO),
                cash_after: next.cash_balance,
                position_qty_after: pos_after,
                position_cost_after: cost_after,
                equity_at_close: equity,
            };
            next.bar_snapshots.push(snapshot.clone());
            events.push(SimEvent::BarAdvanced {
                snapshot: Box::new(snapshot),
            });
        }

        group_start = group_end;
    }

    next.normalize_fields();
    Ok((next, events))
}

/// End (exclusive) of the UTC calendar day containing `at`.
fn utc_day_end(at: DateTime<Utc>) -> DateTime<Utc> {
    at.date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight")
        .and_utc()
        + chrono::Duration::days(1)
}

fn execute_triggered_order(
    state: &mut SimState,
    trig: &TriggerMatch,
    stage: BarPathStage,
    bar: &BarRecord,
    had_buy_by_inst: &mut BTreeMap<String, bool>,
    had_sell_by_inst: &mut BTreeMap<String, bool>,
    events: &mut Vec<SimEvent>,
) -> Result<(), SimError> {
    let Some(order) = state.orders.get(&trig.order_id).cloned() else {
        return Ok(());
    };
    if order.status != OrderStatus::Active {
        return Ok(());
    }
    let spec = state.instruments[&order.instrument_id].clone();

    let settlement = settle_order_fees(
        &state.fee_profile,
        order.side,
        &spec.base_asset_id,
        &spec.quote_currency,
        &[SyntheticFillInput {
            fill_id: "EXEC".to_string(),
            quantity: order.quantity,
            fill_price: trig.fill_price,
        }],
    )?;
    let settled_fill = &settlement.fills[0];

    // Check `INSUFFICIENT_FUNDS_AT_FILL` (`S-40`, `M4-A-06`):
    // Total cash required at fill must not exceed `available_cash + this_order_cash_reservation`.
    let order_cash_reserved = order
        .reservation_id
        .as_ref()
        .and_then(|rid| state.cash_reservations.get(rid))
        .map(|r| r.amount)
        .unwrap_or(Decimal::ZERO);
    let max_spendable_for_order = canon_decimal(state.available_cash() + order_cash_reserved);

    if settled_fill.net_quote_cash_delta < Decimal::ZERO {
        let cash_needed = -settled_fill.net_quote_cash_delta;
        if cash_needed > max_spendable_for_order {
            if let Some(o) = state.orders.get_mut(&order.order_id) {
                o.status = OrderStatus::Rejected;
                o.reject_code = Some(SimRejectCode::InsufficientFundsAtFill);
                o.updated_at = bar.close_at;
            }
            release_order_reservation(state, &order.order_id);
            let rejection = OrderRejection {
                code: SimRejectCode::InsufficientFundsAtFill,
                message: format!(
                    "whole order rejected at fill: required cash {}, spendable (available + order reservation) {}",
                    to_canonical_text(cash_needed),
                    to_canonical_text(max_spendable_for_order),
                ),
                field: None,
                nearest_lower: None,
                nearest_upper: None,
                required: Some(cash_needed),
                available: Some(max_spendable_for_order),
            };
            events.push(SimEvent::OrderRejected {
                order_id: order.order_id.clone(),
                instrument_id: order.instrument_id.clone(),
                rejection,
            });
            if let Some(peer_id) = &order.oco_peer_order_id {
                cancel_order_internal(state, peer_id, "oco_peer_rejected", bar.close_at, events);
            }
            cancel_pending_children(
                state,
                &order.order_id,
                "parent_order_rejected",
                bar.close_at,
                events,
            );
            return Ok(());
        }
    }

    let ambiguous = is_oco_dual_touch_ambiguous(state, &order, bar, stage);
    if ambiguous {
        state.ambiguous_fill_count += 1;
    }

    let fill_seq = state.next_fill_seq;
    state.next_fill_seq += 1;
    let fill_id = next_fill_id(fill_seq);

    let (net_base_qty, realized_pnl_delta) = match order.side {
        OrderSide::Buy => {
            release_order_reservation(state, &order.order_id);
            state.cash_balance =
                canon_decimal(state.cash_balance + settled_fill.net_quote_cash_delta);
            let lot_cost = canon_decimal(-settled_fill.net_quote_cash_delta);
            let net_qty = settled_fill.net_received_base_quantity;
            state
                .lots_by_instrument
                .entry(order.instrument_id.clone())
                .or_default()
                .push(SimLot {
                    lot_id: format!("SIM-LOT-{fill_seq:06}"),
                    source_fill_id: fill_id.clone(),
                    instrument_id: order.instrument_id.clone(),
                    acquired_at: bar.close_at,
                    original_quantity: net_qty,
                    remaining_quantity: net_qty,
                    original_cost_quote: lot_cost,
                    remaining_cost_quote: lot_cost,
                });
            had_buy_by_inst.insert(order.instrument_id.clone(), true);
            (net_qty, Decimal::ZERO)
        }
        OrderSide::Sell => {
            if let Some(rid) = &order.reservation_id {
                consume_position_reservation_fully(state, rid);
            }
            let lots = state
                .lots_by_instrument
                .entry(order.instrument_id.clone())
                .or_default();
            let allocated_cost = allocate_fifo_lots_for_sell(lots, order.quantity)?;
            let net_proceeds = settled_fill.net_quote_cash_delta;
            state.cash_balance = canon_decimal(state.cash_balance + net_proceeds);
            let pnl = canon_decimal(net_proceeds - allocated_cost);
            state.realized_pnl = canon_decimal(state.realized_pnl + pnl);
            had_sell_by_inst.insert(order.instrument_id.clone(), true);
            (order.quantity, pnl)
        }
    };

    state.total_fees_quote = canon_decimal(state.total_fees_quote + settled_fill.fee_quote_value);

    if let Some(o) = state.orders.get_mut(&order.order_id) {
        o.status = OrderStatus::Filled;
        o.updated_at = bar.close_at;
    }

    let sim_fill = SimFill {
        fill_id,
        fill_seq,
        order_id: order.order_id.clone(),
        order_seq: order.order_seq,
        parent_group_id: order.parent_group_id.clone(),
        group_role: order.group_role,
        has_trade_plan: order.has_trade_plan,
        instrument_id: order.instrument_id.clone(),
        bar_session_date: bar.session_date,
        bar_open_at: bar.open_at,
        bar_close_at: bar.close_at,
        path_stage: stage,
        raw_path_price: trig.trigger_path_price,
        fill_price: settled_fill.fill_price,
        gross_quantity: order.quantity,
        net_base_quantity: net_base_qty,
        side: order.side,
        fee_amount: settled_fill.fee_amount,
        fee_currency: settled_fill.fee_currency.clone(),
        fee_quote_value: settled_fill.fee_quote_value,
        realized_pnl_delta,
        ambiguous,
    };
    state.fills.push(sim_fill.clone());
    events.push(SimEvent::OrderFilled {
        fill: Box::new(sim_fill),
    });

    // If this order had an OCO peer, cancel the other OCO leg immediately (`S-39`, `M4-A-05`)
    if let Some(peer_id) = &order.oco_peer_order_id {
        cancel_order_internal(state, peer_id, "oco_peer_filled", bar.close_at, events);
    }

    // If this was a Buy entry with PendingParent attached bracket leg(s), activate them NOW
    // for path segments strictly AFTER `(stage, trig.trigger_path_price)` (`S-39`, `M4-A-05`)!
    if order.side == OrderSide::Buy {
        let mut child_ids: Vec<String> = state
            .orders
            .values()
            .filter(|c| {
                c.parent_order_id.as_deref() == Some(order.order_id.as_str())
                    && c.status == OrderStatus::PendingParent
            })
            .map(|c| c.order_id.clone())
            .collect();
        child_ids.sort();

        if !child_ids.is_empty() {
            let rem_lot = net_base_qty % spec.lot_size;
            let exit_qty = canon_decimal(net_base_qty - rem_lot);
            if exit_qty <= Decimal::ZERO
                || exit_qty > state.available_position(&order.instrument_id)
            {
                for cid in child_ids {
                    cancel_order_internal(
                        state,
                        &cid,
                        "insufficient_received_lot_for_bracket",
                        bar.close_at,
                        events,
                    );
                }
            } else {
                let res_id = next_res_id(state.next_reservation_seq);
                state.next_reservation_seq += 1;
                state.position_reservations.insert(
                    res_id.clone(),
                    PositionReservation {
                        reservation_id: res_id.clone(),
                        instrument_id: order.instrument_id.clone(),
                        quantity: exit_qty,
                        order_ids: child_ids.clone(),
                    },
                );
                let is_m5 = state.is_m5();
                for cid in child_ids {
                    if let Some(c) = state.orders.get_mut(&cid) {
                        c.quantity = exit_qty;
                        c.reservation_id = Some(res_id.clone());
                        c.status = OrderStatus::Active;
                        c.updated_at = bar.close_at;
                        // In `m5-sim-2` intraday runs, refresh `eligible_at` to the fill bar's
                        // open so `DAY` bracket legs activated by a GTC entry on a later day
                        // live until the end of that UTC day instead of expiring immediately.
                        if is_m5 {
                            c.eligible_at = bar.open_at;
                        }
                        c.activated_bar_close_at = Some(bar.close_at);
                        c.activated_stage = Some(stage);
                        c.activated_path_price = Some(trig.trigger_path_price);
                    }
                    events.push(SimEvent::OrderActivated {
                        order_id: cid,
                        instrument_id: order.instrument_id.clone(),
                        quantity: exit_qty,
                        at_stage: stage,
                        at_path_price: trig.trigger_path_price,
                    });
                }
            }
        }
    }

    Ok(())
}

/// Pure single-step state transition function (`S-36`):
/// `step(state, command, bar) -> Result<(SimState, Vec<SimEvent>), SimError>`.
pub fn step(
    state: &SimState,
    command: SimCommand,
    bar: Option<&SimStepBar>,
) -> Result<(SimState, Vec<SimEvent>), SimError> {
    match command {
        SimCommand::SubmitOrder(req) => {
            let mut next = state.clone();
            let events = submit_order_into_state(&mut next, req)?;
            next.command_seq += 1;
            next.normalize_fields();
            Ok((next, events))
        }
        SimCommand::CancelOrder { order_id, reason } => {
            let mut next = state.clone();
            let events = cancel_order_command(&mut next, &order_id, reason)?;
            next.command_seq += 1;
            next.normalize_fields();
            Ok((next, events))
        }
        SimCommand::AdvanceBar { corporate_actions } => {
            let sb = bar.ok_or(SimError::MissingBarInput)?;
            step_multi_bars(state, std::slice::from_ref(sb), &corporate_actions)
        }
        SimCommand::ModifyOrder {
            order_id,
            quantity,
            limit_price,
            stop_price,
        } => {
            let mut next = state.clone();
            let events =
                modify_order_into_state(&mut next, &order_id, quantity, limit_price, stop_price)?;
            next.command_seq += 1;
            next.normalize_fields();
            Ok((next, events))
        }
        SimCommand::UndoLast { .. } => Err(SimError::UndoUnavailable(
            "undo is applied by SimReplayer with its bar-boundary state".into(),
        )),
        SimCommand::Finish => {
            if state.finished {
                return Err(SimError::SessionFinished);
            }
            let mut next = state.clone();
            next.command_seq += 1;
            let now = next
                .visible_until
                .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).unwrap());
            let open_ids: Vec<String> = next
                .orders
                .values()
                .filter(|o| o.status.is_open_or_pending())
                .map(|o| o.order_id.clone())
                .collect();
            let mut events = Vec::new();
            for oid in &open_ids {
                cancel_order_internal(&mut next, oid, "session_finished", now, &mut events);
            }
            next.finished = true;
            events.push(SimEvent::SessionFinished {
                visible_until: next.visible_until,
                cancelled_order_ids: open_ids,
            });
            next.normalize_fields();
            Ok((next, events))
        }
    }
}

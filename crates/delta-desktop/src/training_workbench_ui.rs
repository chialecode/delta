//! M5 chart-trading workbench rendering and input glue (`S-54`, `M5-W05`).
//!
//! The pure interaction rules live in [`crate::training_workbench`]; this module only maps GPUI
//! events onto them and renders the toolbar, the "练习账本" ledger, the draw-to-order draft,
//! confirmation bars and the history / analysis / transfer panels. Trading actions are created
//! only by mouse clicks on explicit buttons or by `Ctrl+Enter` on a confirmed draft.

use std::time::Duration;

#[cfg(test)]
#[path = "training_workbench_ui_tests.rs"]
mod tests;

use delta_app::{
    to_canonical_text, ModifyTrainingOrderRequest, OrderSide, PracticeTransferKind, SimOrderType,
    SimRunStatus, Timeframe, TrainingWorkspaceSnapshotDto,
};
use gpui_kit::{
    base::TestTextExt as _,
    component::{
        button::{Button, ButtonVariants as _},
        h_flex,
        tag::Tag,
        v_flex, ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, StyledExt as _,
    },
    div,
    prelude::FluentBuilder as _,
    px, AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, Styled as _, TestSupportExt as _, Window,
};
use rust_decimal::prelude::*;

use crate::{
    chart_canvas::{parse_hex_color, ChartOverlay, OverlayHLine, OverlayLineStyle, OverlaySegment},
    training_ui::{TrainingSidePanel, TrainingUiAction, TrainingViewMode},
    training_workbench::{
        buy_quantity, command_for_key, floor_to_lot, sell_quantity, ChartClickOutcome, ConfirmKind,
        KeyMods, WorkbenchCommand, WorkbenchState, WorkbenchTool, BUY_FRACTIONS, MA_PERIODS,
        PLAY_SPEEDS_MS, SELL_FRACTIONS,
    },
    workbench::{CenterWorkspacePanel, WorkbenchSharedState},
};

/// Pointer distance (px) within which a mouse-down grabs an order line for dragging.
const ORDER_LINE_GRAB_PX: f32 = 5.;
/// Share of available cash a quick buy may spend, leaving room for fees and adverse rounding.
const QUICK_BUY_CASH_SHARE: Decimal = Decimal::from_parts(995, 0, 0, false, 3);

fn dec_f64(value: Decimal) -> f64 {
    value.to_f64().unwrap_or(f64::NAN)
}

/// The limit/stop price an open order line is drawn at.
pub(crate) fn order_line_price(order_type: &SimOrderType) -> Option<Decimal> {
    match order_type {
        SimOrderType::Limit { limit_price } => Some(*limit_price),
        SimOrderType::StopMarket { stop_price } => Some(*stop_price),
        _ => None,
    }
}

/// Builds the price-panel overlay for the current snapshot and workbench state.
pub(crate) fn workbench_overlay(
    ws: &TrainingWorkspaceSnapshotDto,
    wb: &WorkbenchState,
    aggregated_view: bool,
) -> ChartOverlay {
    let mut overlay = ChartOverlay {
        hide_volume: !wb.show_volume,
        ..ChartOverlay::default()
    };
    for order in &ws.open_orders {
        if let Some(price) = order_line_price(&order.order_type) {
            let display_price = wb
                .dragging_order
                .as_ref()
                .filter(|(id, _, _)| id == &order.order_id)
                .map(|(_, _, preview)| *preview)
                .unwrap_or(price);
            overlay.hlines.push(OverlayHLine {
                price: dec_f64(display_price),
                color: if order.side == OrderSide::Buy {
                    0x1e88e5
                } else {
                    0xe53935
                },
                style: if wb.locked_orders.contains(&order.order_id) {
                    OverlayLineStyle::Solid
                } else {
                    OverlayLineStyle::Dashed
                },
                thick: true,
            });
        }
    }
    if let Some(pos) = ws.position.as_ref().filter(|p| !p.quantity.is_zero()) {
        overlay.hlines.push(OverlayHLine {
            price: dec_f64(pos.avg_unit_cost),
            color: 0x9e9e9e,
            style: OverlayLineStyle::Dashed,
            thick: false,
        });
    }
    if let Some(draft) = &wb.draft {
        overlay.hlines.push(OverlayHLine {
            price: dec_f64(draft.price),
            color: 0xf0b90b,
            style: OverlayLineStyle::Dashed,
            thick: true,
        });
    }
    if aggregated_view {
        return overlay;
    }
    if wb.show_drawings {
        for d in &wb.drawings {
            let color = parse_hex_color(&d.color);
            let style = OverlayLineStyle::from_drawing_style(&d.line_style);
            let thick = wb.selected_drawing.as_deref() == Some(d.drawing_id.as_str());
            match (d.kind.as_str(), d.anchors.as_slice()) {
                ("horizontal", [(_, p), ..]) => overlay.hlines.push(OverlayHLine {
                    price: dec_f64(*p),
                    color,
                    style,
                    thick,
                }),
                (kind @ ("ray" | "segment"), [(b0, p0), (b1, p1), ..]) => {
                    overlay.segments.push(OverlaySegment {
                        from: (*b0 as f64, dec_f64(*p0)),
                        to: (*b1 as f64, dec_f64(*p1)),
                        ray: kind == "ray",
                        color,
                        style,
                        thick,
                    })
                }
                _ => {}
            }
        }
    }
    if let (Some((b0, p0)), Some((b1, p1))) = (wb.measure.start, wb.measure.end) {
        overlay.segments.push(OverlaySegment {
            from: (b0 as f64, dec_f64(p0)),
            to: (b1 as f64, dec_f64(p1)),
            ray: false,
            color: 0x8e24aa,
            style: OverlayLineStyle::Dashed,
            thick: false,
        });
    }
    if let Some((_b, p)) = wb.pending_anchors.first() {
        overlay.hlines.push(OverlayHLine {
            price: dec_f64(*p),
            color: 0x8e24aa,
            style: OverlayLineStyle::Dashed,
            thick: false,
        });
    }
    overlay
}

/// Quick-trade quantities (buy, sell, flatten) for the ledger buttons. All `Decimal`, floored to
/// the lot size; the buy budget is `min(equity / fraction, available cash × 99.5%)`.
pub(crate) fn quick_quantities(
    ws: &TrainingWorkspaceSnapshotDto,
    wb: &WorkbenchState,
) -> (Decimal, Decimal, Decimal) {
    let close = ws
        .chart_slice
        .bars
        .last()
        .map(|b| b.close)
        .unwrap_or_default();
    let lot = ws.account.lot_size;
    let budget = (ws.account.total_equity / Decimal::from(wb.buy_fraction.max(1)))
        .min(ws.account.available_cash * QUICK_BUY_CASH_SHARE);
    let buy = buy_quantity(budget, 1, close, lot);
    let available = ws
        .position
        .as_ref()
        .map(|p| p.available_quantity)
        .unwrap_or_default();
    let sell = sell_quantity(available, wb.sell_fraction, lot);
    (buy, sell, floor_to_lot(available, lot))
}

impl CenterWorkspacePanel {
    /// Pointer → `(global bar index, price)` via the shared `chart_canvas` projection (`UI-Q04`).
    pub(crate) fn training_pointer_bar_price(
        &self,
        position: gpui_kit::Point<gpui_kit::Pixels>,
    ) -> Option<(usize, f64)> {
        let bounds = (*crate::lock_or_recover(&self.training_plot_bounds))?;
        let g = crate::lock_or_recover(&self.state);
        let slice = &g.training.workspace.as_ref()?.chart_slice;
        crate::chart_canvas::pointer_bar_price(position, bounds, slice)
    }

    pub(crate) fn price_to_y(&self, price: f64) -> Option<f32> {
        let bounds = (*crate::lock_or_recover(&self.training_plot_bounds))?;
        let g = crate::lock_or_recover(&self.state);
        let slice = &g.training.workspace.as_ref()?.chart_slice;
        crate::chart_canvas::price_to_y(price, bounds, slice)
    }

    /// Mouse-down on the chart. Returns `true` when the workbench consumed it (order-line grab,
    /// drawing anchor, measure point or draw-to-order draft) so the pan gesture must not start.
    /// Never submits anything.
    pub(crate) fn workbench_chart_down(
        &mut self,
        position: gpui_kit::Point<gpui_kit::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((index, price_f)) = self.training_pointer_bar_price(position) else {
            return false;
        };
        let (run_id, open_lines, aggregated, tick, close, default_qty) = {
            let g = crate::lock_or_recover(&self.state);
            let Some(ws) = g.training.workspace.as_ref() else {
                return false;
            };
            if ws.run.status != SimRunStatus::Active {
                return false;
            }
            let lines: Vec<(String, Decimal)> = ws
                .open_orders
                .iter()
                .filter_map(|o| order_line_price(&o.order_type).map(|p| (o.order_id.clone(), p)))
                .collect();
            let close = ws
                .chart_slice
                .bars
                .last()
                .map(|b| b.close)
                .unwrap_or_default();
            (
                ws.run.run_id.clone(),
                lines,
                g.training.wb.view_timeframe.is_some(),
                ws.account.tick_size,
                close,
                quick_quantities(ws, &g.training.wb).0,
            )
        };
        if aggregated {
            return false;
        }
        let pointer_y = f32::from(position.y);
        for (order_id, price) in open_lines {
            let Some(line_y) = self.price_to_y(dec_f64(price)) else {
                continue;
            };
            if (line_y - pointer_y).abs() <= ORDER_LINE_GRAB_PX {
                let mut g = crate::lock_or_recover(&self.state);
                if g.training.wb.locked_orders.contains(&order_id) {
                    g.ui_notice = "该挂单已锁定，解锁后才能拖动改价".into();
                } else {
                    g.training.wb.dragging_order = Some((order_id, price, price));
                }
                cx.notify();
                return true;
            }
        }
        let Some(price_dec) = Decimal::from_f64(price_f) else {
            return false;
        };
        let price = crate::training_workbench::round_to_tick(price_dec, tick);
        let outcome = {
            let mut g = crate::lock_or_recover(&self.state);
            let wb = &mut g.training.wb;
            if wb.tool == WorkbenchTool::None && !wb.draw_order_enabled {
                return false;
            }
            wb.chart_click(index as i64, price, close, default_qty)
        };
        match outcome {
            ChartClickOutcome::SaveDrawing(drawing) => {
                // Dispatch immediately on mouse-down so dragging/releasing outside the chart
                // can never drop a completed drawing (`UI-B07`).
                self.dispatch_training(
                    Ok(TrainingUiAction::SaveDrawing { run_id, drawing }),
                    window,
                    cx,
                );
            }
            ChartClickOutcome::DraftOpened => {
                crate::lock_or_recover(&self.state).ui_notice =
                    "已生成下单草稿：选择方向后点“确认下单”或按 Ctrl+Enter；Esc 取消".into();
            }
            _ => {}
        }
        cx.notify();
        true
    }

    /// Updates live preview position while dragging an unlocked order line (`UI-B07`).
    pub(crate) fn workbench_chart_drag_move(
        &mut self,
        position: gpui_kit::Point<gpui_kit::Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some((_, price_f)) = self.training_pointer_bar_price(position) else {
            return;
        };
        let Some(price_dec) = Decimal::from_f64(price_f) else {
            return;
        };
        let mut g = crate::lock_or_recover(&self.state);
        let Some(tick) = g.training.workspace.as_ref().map(|ws| ws.account.tick_size) else {
            return;
        };
        let preview = crate::training_workbench::round_to_tick(price_dec, tick);
        if preview <= Decimal::ZERO {
            return;
        }
        if let Some((_, _, cur_preview)) = g.training.wb.dragging_order.as_mut() {
            if *cur_preview != preview {
                *cur_preview = preview;
                drop(g);
                cx.notify();
            }
        }
    }

    /// Cancels an active order-line drag if the pointer is released outside the chart (`UI-B07`).
    pub(crate) fn workbench_chart_cancel_drag(&mut self, cx: &mut Context<Self>) {
        if crate::lock_or_recover(&self.state)
            .training
            .wb
            .dragging_order
            .take()
            .is_some()
        {
            cx.notify();
        }
    }

    /// Mouse-up on the chart: finishes an order-line drag (a modify request, never a fill).
    pub(crate) fn workbench_chart_up(
        &mut self,
        position: gpui_kit::Point<gpui_kit::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((order_id, old_price, _preview)) = crate::lock_or_recover(&self.state)
            .training
            .wb
            .dragging_order
            .take()
        else {
            return;
        };
        let Some((_, price_f)) = self.training_pointer_bar_price(position) else {
            cx.notify();
            return;
        };
        let Some(new_price_dec) = Decimal::from_f64(price_f) else {
            cx.notify();
            return;
        };
        let request = {
            let g = crate::lock_or_recover(&self.state);
            let Some(ws) = g.training.workspace.as_ref() else {
                drop(g);
                cx.notify();
                return;
            };
            let Some(order) = ws.open_orders.iter().find(|o| o.order_id == order_id) else {
                drop(g);
                cx.notify();
                return;
            };
            let Some((id, new_price)) = g.training.wb.drag_release(
                &order_id,
                old_price,
                new_price_dec,
                ws.account.tick_size,
            ) else {
                drop(g);
                cx.notify();
                return;
            };
            let (limit_price, stop_price) = match order.order_type {
                SimOrderType::Limit { .. } => (Some(new_price), None),
                _ => (None, Some(new_price)),
            };
            (
                ws.run.run_id.clone(),
                ModifyTrainingOrderRequest {
                    order_id: id,
                    quantity: None,
                    limit_price,
                    stop_price,
                },
            )
        };
        self.dispatch_training(
            Ok(TrainingUiAction::ModifyOrder {
                run_id: request.0,
                request: request.1,
            }),
            window,
            cx,
        );
    }

    /// Workbench keyboard handling. Returns `true` when the key was consumed.
    pub(crate) fn workbench_key_down(
        &mut self,
        event: &gpui_kit::KeyDownEvent,
        run_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let m = &event.keystroke.modifiers;
        let mods = KeyMods {
            control: m.control || m.secondary(cx),
            alt: m.alt,
            shift: m.shift,
            platform: m.platform && !cfg!(target_os = "macos"),
        };
        let focused = self.is_any_training_text_input_focused(window, cx);
        let command = {
            let g = self.state.lock().unwrap();
            command_for_key(&g.training.wb, &event.keystroke.key, mods, focused)
        };
        let Some(command) = command else {
            // An open draft owns Ctrl+Enter: without a chosen option it must not fall through to
            // the order ticket's submit shortcut.
            let ctrl_enter = mods.control && event.keystroke.key.eq_ignore_ascii_case("enter");
            let mut g = self.state.lock().unwrap();
            if ctrl_enter && g.training.wb.draft.is_some() {
                g.ui_notice = "请先在下单草稿中选择方向，再确认".into();
                cx.notify();
                return true;
            }
            return false;
        };
        let run_id = run_id.to_string();
        match command {
            WorkbenchCommand::Advance => {
                self.dispatch_training(Ok(TrainingUiAction::AdvanceBar(run_id)), window, cx);
            }
            WorkbenchCommand::TogglePlay => self.toggle_training_play(window, cx),
            WorkbenchCommand::SelectTool(tool) => {
                self.state.lock().unwrap().training.wb.select_tool(tool);
                cx.notify();
            }
            WorkbenchCommand::DeleteSelectedDrawing(drawing_id) => self.dispatch_training(
                Ok(TrainingUiAction::DeleteDrawing { run_id, drawing_id }),
                window,
                cx,
            ),
            WorkbenchCommand::Undo => {
                self.dispatch_training(Ok(TrainingUiAction::UndoLast(run_id)), window, cx)
            }
            WorkbenchCommand::ConfirmDraft => self.confirm_workbench_draft(&run_id, window, cx),
            WorkbenchCommand::Cancel => {
                {
                    let mut g = self.state.lock().unwrap();
                    let wb = &mut g.training.wb;
                    wb.draft = None;
                    wb.pending_confirm = None;
                    wb.pending_anchors.clear();
                    wb.dragging_order = None;
                    if wb.tool != WorkbenchTool::None {
                        wb.tool = WorkbenchTool::None;
                    }
                }
                self.clear_training_crosshair_and_focus_workspace(window, cx);
            }
        }
        true
    }

    pub(crate) fn confirm_workbench_draft(
        &mut self,
        run_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = self.state.lock().unwrap().training.wb.confirm_draft();
        if let Some(request) = request {
            self.dispatch_training(
                Ok(TrainingUiAction::SubmitOrder {
                    run_id: run_id.to_string(),
                    request,
                }),
                window,
                cx,
            );
        }
    }

    pub(crate) fn toggle_training_play(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let idle = self.controller.read(cx).is_idle();
        {
            let mut g = crate::lock_or_recover(&self.state);
            let pb = &mut g.training.wb.playback;
            pb.playing = !pb.playing;
            if idle {
                pb.in_flight = false;
            }
            pb.last_pause_reason = None;
        }
        self.training_play_last = None;
        self.ensure_training_ticker(window, cx);
        cx.notify();
    }

    /// Starts the playback / data-preparation ticker if it is not already running. The loop exits
    /// by itself once nothing is playing, no preparation job runs and the controller is idle.
    pub(crate) fn ensure_training_ticker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.training_ticker.is_some() {
            return;
        }
        let executor = cx.background_executor().clone();
        self.training_ticker = Some(cx.spawn_in(window, async move |panel, cx| loop {
            executor.timer(Duration::from_millis(30)).await;
            let keep = panel
                .update_in(cx, |panel, window, cx| panel.training_tick(window, cx))
                .unwrap_or(false);
            if !keep {
                let _ = panel.update(cx, |panel, _| panel.training_ticker = None);
                break;
            }
        }));
    }

    /// One ticker pass; returns whether the ticker should keep running.
    pub(crate) fn training_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let idle = self.controller.read(cx).is_idle();
        // Executor clock: real time in the app, the controllable test clock in tests.
        let now = cx.background_executor().now();
        let (playing, interval, run_id, prepare) = {
            let g = crate::lock_or_recover(&self.state);
            // Playback may only advance bars while the user is actually viewing the training
            // workspace; navigating away or switching mode pauses playback cleanly (`UI-B02`).
            let in_workspace = g.layout.active_nav == "nav.training"
                && g.training.mode == TrainingViewMode::Workspace;
            let run_id = in_workspace
                .then(|| {
                    g.training
                        .workspace
                        .as_ref()
                        .filter(|ws| ws.run.status == SimRunStatus::Active)
                        .map(|ws| ws.run.run_id.clone())
                })
                .flatten();
            (
                g.training.wb.playback.playing,
                g.training.wb.playback.interval_ms(),
                run_id,
                g.training.prepare_job.is_some(),
            )
        };
        if playing {
            match run_id {
                Some(run_id)
                    if idle
                        && self.training_play_last.is_none_or(|last| {
                            now.duration_since(last) >= Duration::from_millis(interval)
                        }) =>
                {
                    let start = crate::lock_or_recover(&self.state)
                        .training
                        .wb
                        .playback
                        .tick();
                    if start {
                        self.training_play_last = Some(now);
                        self.dispatch_training(Ok(TrainingUiAction::PlayStep(run_id)), window, cx);
                    }
                }
                None => {
                    let mut g = crate::lock_or_recover(&self.state);
                    g.training.wb.playback.playing = false;
                    g.training.wb.playback.in_flight = false;
                    drop(g);
                    cx.notify();
                }
                _ => {}
            }
        }
        if prepare
            && idle
            && self
                .training_prepare_last
                .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(500))
        {
            self.training_prepare_last = Some(now);
            self.dispatch_training(Ok(TrainingUiAction::PollPrepareJob), window, cx);
        }
        let g = crate::lock_or_recover(&self.state);
        g.training.wb.playback.playing || g.training.prepare_job.is_some() || !idle
    }

    pub(crate) fn render_workbench_toolbar(
        &self,
        state: &WorkbenchSharedState,
        ws: &TrainingWorkspaceSnapshotDto,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wb = &state.training.wb;
        let run_id = ws.run.run_id.clone();
        let active = ws.run.status == SimRunStatus::Active;
        let playing = wb.playback.playing;
        let play_label = if playing {
            "暂停播放 (空格)"
        } else {
            "播放 (空格)"
        };
        let tool_btn =
            |id: &'static str, label: &'static str, tool: WorkbenchTool, cx: &mut Context<Self>| {
                Button::new(id)
                    .xsmall()
                    .outline()
                    .selected(wb.tool == tool)
                    .label(label)
                    .on_click(cx.listener(move |panel, _, window, cx| {
                        panel.state.lock().unwrap().training.wb.select_tool(tool);
                        panel.training_workspace_focus.focus(window, cx);
                        cx.notify();
                    }))
            };
        let base_tf = Timeframe::parse(&ws.run.timeframe).unwrap_or_default();
        let higher: Vec<Timeframe> = Timeframe::ALL
            .iter()
            .copied()
            .filter(|t| t.millis() > base_tf.millis())
            .collect();
        let rid_view = run_id.clone();
        let rid_undo = run_id.clone();
        let saved_text = SharedString::from(format!(
            "已保存在本机 · 第 {}/{} 步",
            ws.run.current_step_index, ws.run.total_steps
        ));
        let mut row = h_flex()
            .id("trn-wb-toolbar")
            .test_support()
            .w_full()
            .gap_1()
            .flex_wrap()
            .items_center()
            .child(
                Button::new("trn-wb-play")
                    .xsmall()
                    .primary()
                    .disabled(!active)
                    .label(play_label)
                    .on_click(cx.listener(|panel, _, window, cx| {
                        panel.toggle_training_play(window, cx);
                        panel.training_workspace_focus.focus(window, cx);
                    })),
            );
        for (i, ms) in PLAY_SPEEDS_MS.iter().enumerate() {
            let label = SharedString::from(format!("{}x", i + 1));
            row = row.child(
                Button::new(SharedString::from(format!("trn-wb-speed-{i}")))
                    .xsmall()
                    .ghost()
                    .selected(wb.playback.speed_index == i)
                    .label(label)
                    .tooltip(format!("每根 {ms} 毫秒"))
                    .on_click(cx.listener(move |panel, _, _, cx| {
                        panel.state.lock().unwrap().training.wb.playback.speed_index = i;
                        cx.notify();
                    })),
            );
        }
        row = row
            .child(div().w(px(8.)))
            .child(tool_btn(
                "trn-wb-tool-ray",
                "射线 Q",
                WorkbenchTool::Ray,
                cx,
            ))
            .child(tool_btn(
                "trn-wb-tool-hline",
                "水平线 W",
                WorkbenchTool::Horizontal,
                cx,
            ))
            .child(tool_btn(
                "trn-wb-tool-segment",
                "线段 E",
                WorkbenchTool::Segment,
                cx,
            ))
            .child(tool_btn(
                "trn-wb-tool-measure",
                "测量",
                WorkbenchTool::Measure,
                cx,
            ))
            .child(
                Button::new("trn-wb-draw-order")
                    .xsmall()
                    .outline()
                    .selected(wb.draw_order_enabled)
                    .label("画线下单")
                    .tooltip("开启后在图上点击价格生成下单草稿，仍需确认才会提交")
                    .on_click(cx.listener(|panel, _, _, cx| {
                        let mut g = panel.state.lock().unwrap();
                        g.training.wb.draw_order_enabled = !g.training.wb.draw_order_enabled;
                        g.training.wb.draft = None;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("trn-wb-show-drawings")
                    .xsmall()
                    .ghost()
                    .selected(wb.show_drawings)
                    .label("显示画线")
                    .on_click(cx.listener(|panel, _, _, cx| {
                        let mut g = panel.state.lock().unwrap();
                        g.training.wb.show_drawings = !g.training.wb.show_drawings;
                        cx.notify();
                    })),
            )
            .child(div().w(px(8.)));
        for (i, period) in MA_PERIODS.iter().enumerate() {
            let rid = run_id.clone();
            row = row.child(
                Button::new(SharedString::from(format!("trn-wb-ma-{period}")))
                    .xsmall()
                    .ghost()
                    .selected(wb.ma_enabled[i])
                    .label(SharedString::from(format!("MA{period}")))
                    .on_click(cx.listener(move |panel, _, window, cx| {
                        let viewport = {
                            let mut g = panel.state.lock().unwrap();
                            g.training.wb.ma_enabled[i] = !g.training.wb.ma_enabled[i];
                            let periods: Vec<usize> = MA_PERIODS
                                .iter()
                                .zip(g.training.wb.ma_enabled)
                                .filter_map(|(p, on)| on.then_some(*p))
                                .collect();
                            let Some(mut vp) = g.training.chart_viewport.clone().or_else(|| {
                                g.training.workspace.as_ref().map(|w| w.viewport.clone())
                            }) else {
                                return;
                            };
                            vp.indicator_config.sma_periods = periods;
                            g.training.chart_viewport = Some(vp.clone());
                            vp
                        };
                        panel.dispatch_training(
                            Ok(TrainingUiAction::UpdateChartViewport {
                                run_id: rid.clone(),
                                viewport,
                            }),
                            window,
                            cx,
                        );
                    })),
            );
        }
        row = row.child(
            Button::new("trn-wb-volume")
                .xsmall()
                .ghost()
                .selected(wb.show_volume)
                .label("成交量")
                .on_click(cx.listener(|panel, _, _, cx| {
                    let mut g = panel.state.lock().unwrap();
                    g.training.wb.show_volume = !g.training.wb.show_volume;
                    cx.notify();
                })),
        );
        if !higher.is_empty() {
            let rid = rid_view.clone();
            row = row.child(div().w(px(8.))).child(
                Button::new("trn-wb-view-base")
                    .xsmall()
                    .ghost()
                    .selected(wb.view_timeframe.is_none())
                    .label(SharedString::from(base_tf.as_str().to_string()))
                    .on_click(cx.listener(move |panel, _, window, cx| {
                        panel.dispatch_training(
                            Ok(TrainingUiAction::SetViewTimeframe {
                                run_id: rid.clone(),
                                view: None,
                            }),
                            window,
                            cx,
                        );
                    })),
            );
            for tf in higher {
                let rid = rid_view.clone();
                row = row.child(
                    Button::new(SharedString::from(format!("trn-wb-view-{}", tf.as_str())))
                        .xsmall()
                        .ghost()
                        .selected(wb.view_timeframe == Some(tf))
                        .label(SharedString::from(tf.as_str().to_string()))
                        .on_click(cx.listener(move |panel, _, window, cx| {
                            panel.dispatch_training(
                                Ok(TrainingUiAction::SetViewTimeframe {
                                    run_id: rid.clone(),
                                    view: Some(tf),
                                }),
                                window,
                                cx,
                            );
                        })),
                );
            }
        }
        row = row
            .child(div().w(px(8.)))
            .child(
                Button::new("trn-wb-undo")
                    .xsmall()
                    .outline()
                    .disabled(!active)
                    .label("撤销 Ctrl+Z")
                    .on_click(cx.listener(move |panel, _, window, cx| {
                        panel.dispatch_training(
                            Ok(TrainingUiAction::UndoLast(rid_undo.clone())),
                            window,
                            cx,
                        );
                    })),
            )
            .child(
                Button::new("trn-wb-clear-drawings")
                    .xsmall()
                    .ghost()
                    .label("清除画线")
                    .on_click(cx.listener(|panel, _, _, cx| {
                        panel.state.lock().unwrap().training.wb.pending_confirm =
                            Some(ConfirmKind::ClearDrawings);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("trn-wb-restart")
                    .xsmall()
                    .ghost()
                    .disabled(!active || ws.run.practice_account_id.is_none())
                    .label("重新开局")
                    .on_click(cx.listener(|panel, _, _, cx| {
                        panel.state.lock().unwrap().training.wb.pending_confirm =
                            Some(ConfirmKind::Restart);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("trn-wb-abandon")
                    .xsmall()
                    .ghost()
                    .disabled(!active)
                    .label("放弃本局")
                    .on_click(cx.listener(|panel, _, _, cx| {
                        panel.state.lock().unwrap().training.wb.pending_confirm =
                            Some(ConfirmKind::NewRun);
                        cx.notify();
                    })),
            )
            .child(self.side_panel_button(
                "trn-wb-open-history",
                "训练历史",
                TrainingSidePanel::History,
                cx,
            ))
            .child(self.side_panel_button(
                "trn-wb-open-analysis",
                "账户分析",
                TrainingSidePanel::Analysis,
                cx,
            ))
            .when(ws.run.practice_account_id.is_some(), |row| {
                row.child(self.side_panel_button(
                    "trn-wb-open-transfer",
                    "资金划转",
                    TrainingSidePanel::Transfer,
                    cx,
                ))
            })
            .child(
                div()
                    .id("trn-wb-saved")
                    .test_support()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(saved_text.clone())
                    .test_text(saved_text),
            );
        let mut col = v_flex().w_full().gap_1().child(row);
        if let Some(reason) = wb.playback.last_pause_reason.as_ref().filter(|_| !playing) {
            let text = SharedString::from(reason.clone());
            col = col.child(
                div()
                    .id("trn-wb-pause-reason")
                    .test_support()
                    .child(Tag::warning().small().child(text.clone()))
                    .test_text(text),
            );
        }
        if let Some(label) = wb.measure.label() {
            let text = SharedString::from(format!("测量：{label}"));
            col = col.child(
                div()
                    .id("trn-wb-measure")
                    .test_support()
                    .text_xs()
                    .font_family(cx.theme().mono_font_family.clone())
                    .child(text.clone())
                    .test_text(text),
            );
        }
        col.when_some(
            self.render_workbench_confirm(state, &run_id, cx),
            |c, el| c.child(el),
        )
        .when_some(self.render_workbench_draft(state, &run_id, cx), |c, el| {
            c.child(el)
        })
        .into_any_element()
    }

    fn side_panel_button(
        &self,
        id: &'static str,
        label: &'static str,
        panel_kind: TrainingSidePanel,
        cx: &mut Context<Self>,
    ) -> Button {
        Button::new(id)
            .xsmall()
            .ghost()
            .label(label)
            .on_click(cx.listener(move |panel, _, window, cx| {
                panel.dispatch_training(
                    Ok(TrainingUiAction::OpenSidePanel(panel_kind)),
                    window,
                    cx,
                );
            }))
    }

    fn render_workbench_confirm(
        &self,
        state: &WorkbenchSharedState,
        run_id: &str,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let kind = state.training.wb.pending_confirm?;
        let (text, action) = match kind {
            ConfirmKind::ClearDrawings => (
                "确认清除本局全部画线？此操作不可撤销。",
                TrainingUiAction::ClearDrawings(run_id.to_string()),
            ),
            ConfirmKind::Restart => (
                "确认放弃本局并以相同设置重新开局？本局会照常结算并计入分析。",
                TrainingUiAction::RestartRun(run_id.to_string()),
            ),
            ConfirmKind::NewRun => (
                "确认放弃本局？本局按当前收盘价结算到练习账户并计入分析。",
                TrainingUiAction::AbandonRun(run_id.to_string()),
            ),
        };
        let action = std::sync::Arc::new(std::sync::Mutex::new(Some(action)));
        Some(
            h_flex()
                .id("trn-wb-confirm")
                .test_support()
                .gap_2()
                .items_center()
                .p_1p5()
                .border_1()
                .border_color(cx.theme().warning)
                .rounded(cx.theme().radius)
                .child(div().text_xs().child(text))
                .child(
                    Button::new("trn-wb-confirm-yes")
                        .xsmall()
                        .warning()
                        .label("确认")
                        .on_click(cx.listener(move |panel, _, window, cx| {
                            panel.state.lock().unwrap().training.wb.pending_confirm = None;
                            if let Some(action) = action.lock().unwrap().take() {
                                panel.dispatch_training(Ok(action), window, cx);
                            }
                        })),
                )
                .child(
                    Button::new("trn-wb-confirm-no")
                        .xsmall()
                        .ghost()
                        .label("取消")
                        .on_click(cx.listener(|panel, _, _, cx| {
                            panel.state.lock().unwrap().training.wb.pending_confirm = None;
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }

    fn render_workbench_draft(
        &self,
        state: &WorkbenchSharedState,
        run_id: &str,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let draft = state.training.wb.draft.clone()?;
        let ws = state.training.workspace.as_ref()?;
        let (buy_qty, sell_qty, _) = quick_quantities(ws, &state.training.wb);
        let header = SharedString::from(format!(
            "下单草稿 · 价格 {} · 数量 {}",
            to_canonical_text(draft.price),
            to_canonical_text(draft.quantity)
        ));
        let mut row = h_flex()
            .id("trn-wb-draft")
            .test_support()
            .gap_1p5()
            .items_center()
            .flex_wrap()
            .p_1p5()
            .border_1()
            .border_color(cx.theme().primary)
            .rounded(cx.theme().radius)
            .child(div().text_xs().child(header.clone()).test_text(header));
        for option in draft.options.clone() {
            let qty = match option {
                crate::training_workbench::DraftOption::BuyLimit
                | crate::training_workbench::DraftOption::BuyStop => buy_qty,
                _ => sell_qty,
            };
            row = row.child(
                Button::new(SharedString::from(format!("trn-wb-draft-{option:?}")))
                    .xsmall()
                    .outline()
                    .selected(draft.selected == Some(option))
                    .label(option.label())
                    .on_click(cx.listener(move |panel, _, _, cx| {
                        let mut g = panel.state.lock().unwrap();
                        if let Some(d) = g.training.wb.draft.as_mut() {
                            d.selected = Some(option);
                            d.quantity = qty;
                        }
                        cx.notify();
                    })),
            );
        }
        let rid = run_id.to_string();
        row = row
            .child(
                Button::new("trn-wb-draft-confirm")
                    .xsmall()
                    .primary()
                    .disabled(draft.selected.is_none() || draft.quantity <= Decimal::ZERO)
                    .label("确认下单 Ctrl+Enter")
                    .on_click(cx.listener(move |panel, _, window, cx| {
                        panel.confirm_workbench_draft(&rid, window, cx);
                    })),
            )
            .child(
                Button::new("trn-wb-draft-cancel")
                    .xsmall()
                    .ghost()
                    .label("取消 Esc")
                    .on_click(cx.listener(|panel, _, _, cx| {
                        panel.state.lock().unwrap().training.wb.draft = None;
                        cx.notify();
                    })),
            );
        Some(row.into_any_element())
    }

    /// Right-column "练习账本": account, position, proportions, quick buttons, open orders, fills.
    pub(crate) fn render_practice_ledger(
        &self,
        state: &WorkbenchSharedState,
        ws: &TrainingWorkspaceSnapshotDto,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wb = &state.training.wb;
        let ccy = ws.account.quote_currency.clone();
        let active = ws.run.status == SimRunStatus::Active;
        let (buy_qty, sell_qty, flat_qty) = quick_quantities(ws, wb);
        let run_id = ws.run.run_id.clone();
        let funding = match (
            &ws.run.practice_account_id,
            &state.training.practice_account,
        ) {
            (Some(_), Some(acc)) => format!(
                "练习账户 {} · 账户可用 {} · 本局分配 {}",
                acc.quote_currency,
                to_canonical_text(acc.available),
                to_canonical_text(ws.run.initial_cash)
            ),
            _ => format!(
                "独立资金 · 初始 {} {ccy}",
                to_canonical_text(ws.run.initial_cash)
            ),
        };
        let funding = SharedString::from(funding);
        let pos_text = SharedString::from(
            match ws.position.as_ref().filter(|p| !p.quantity.is_zero()) {
                Some(p) => format!(
                    "持仓 {} · 成本 {} · 浮动盈亏 {} {ccy}",
                    to_canonical_text(p.quantity),
                    to_canonical_text(p.avg_unit_cost),
                    to_canonical_text(ws.account.unrealized_pnl)
                ),
                None => "当前无持仓".to_string(),
            },
        );
        let equity_text = SharedString::from(format!(
            "权益 {} · 可用现金 {} · 已实现 {} · 费用 {} {ccy}",
            to_canonical_text(ws.account.total_equity),
            to_canonical_text(ws.account.available_cash),
            to_canonical_text(ws.account.realized_pnl),
            to_canonical_text(ws.account.total_fees_quote)
        ));
        let mut buy_row = h_flex()
            .gap_1()
            .flex_wrap()
            .child(div().text_xs().child("买入比例"));
        for (frac, label) in BUY_FRACTIONS {
            buy_row = buy_row.child(
                Button::new(SharedString::from(format!("trn-wb-buyfrac-{frac}")))
                    .xsmall()
                    .ghost()
                    .selected(wb.buy_fraction == frac)
                    .label(label)
                    .on_click(cx.listener(move |panel, _, _, cx| {
                        panel.state.lock().unwrap().training.wb.buy_fraction = frac;
                        cx.notify();
                    })),
            );
        }
        let mut sell_row = h_flex()
            .gap_1()
            .flex_wrap()
            .child(div().text_xs().child("卖出比例"));
        for (frac, label) in SELL_FRACTIONS {
            sell_row = sell_row.child(
                Button::new(SharedString::from(format!("trn-wb-sellfrac-{frac}")))
                    .xsmall()
                    .ghost()
                    .selected(wb.sell_fraction == frac)
                    .label(label)
                    .on_click(cx.listener(move |panel, _, _, cx| {
                        panel.state.lock().unwrap().training.wb.sell_fraction = frac;
                        cx.notify();
                    })),
            );
        }
        let quick = |id: &'static str,
                     label: String,
                     side: OrderSide,
                     qty: Decimal,
                     cx: &mut Context<Self>| {
            let rid = run_id.clone();
            Button::new(id)
                .small()
                .when(side == OrderSide::Buy, |b| b.primary())
                .when(side == OrderSide::Sell, |b| b.danger())
                .disabled(!active || qty <= Decimal::ZERO)
                .label(SharedString::from(label))
                // Mouse click only: no key binding maps to a quick trade (`S-54`).
                .on_click(cx.listener(move |panel, _, window, cx| {
                    panel.dispatch_training(
                        Ok(TrainingUiAction::QuickTrade {
                            run_id: rid.clone(),
                            side,
                            quantity: qty,
                        }),
                        window,
                        cx,
                    );
                }))
        };
        let buttons = h_flex()
            .gap_1p5()
            .child(quick(
                "trn-wb-quick-buy",
                format!("买入 {}", to_canonical_text(buy_qty)),
                OrderSide::Buy,
                buy_qty,
                cx,
            ))
            .child(quick(
                "trn-wb-quick-sell",
                format!("卖出 {}", to_canonical_text(sell_qty)),
                OrderSide::Sell,
                sell_qty,
                cx,
            ))
            .child(quick(
                "trn-wb-quick-flat",
                "清仓".to_string(),
                OrderSide::Sell,
                flat_qty,
                cx,
            ));

        let mut orders = v_flex().gap_1();
        for order in &ws.open_orders {
            let price = order_line_price(&order.order_type);
            let locked = wb.locked_orders.contains(&order.order_id);
            let oid = order.order_id.clone();
            let text = SharedString::from(format!(
                "{} {} {} @ {}",
                if order.side == OrderSide::Buy {
                    "买"
                } else {
                    "卖"
                },
                match order.order_type {
                    SimOrderType::Limit { .. } => "限价",
                    SimOrderType::StopMarket { .. } => "止损",
                    _ => "市价",
                },
                to_canonical_text(order.quantity),
                price.map(to_canonical_text).unwrap_or_else(|| "—".into())
            ));
            let tick = ws.account.tick_size;
            let mut line = h_flex()
                .id(SharedString::from(format!(
                    "trn-wb-order-{}",
                    order.order_id
                )))
                .test_support()
                .gap_1()
                .items_center()
                .child(div().text_xs().child(text.clone()).test_text(text))
                .child(
                    Button::new(SharedString::from(format!(
                        "trn-wb-lock-{}",
                        order.order_id
                    )))
                    .xsmall()
                    .ghost()
                    .selected(locked)
                    .label(if locked { "已锁定" } else { "锁定" })
                    .on_click(cx.listener({
                        let oid = oid.clone();
                        move |panel, _, _, cx| {
                            let mut g = panel.state.lock().unwrap();
                            let set = &mut g.training.wb.locked_orders;
                            if !set.remove(&oid) {
                                set.insert(oid.clone());
                            }
                            cx.notify();
                        }
                    })),
                );
            if let (Some(price), false, true) = (price, locked, active) {
                for (suffix, delta) in [("down", -tick), ("up", tick)] {
                    let rid = run_id.clone();
                    let oid = oid.clone();
                    let is_limit = matches!(order.order_type, SimOrderType::Limit { .. });
                    line = line.child(
                        Button::new(SharedString::from(format!("trn-wb-nudge-{suffix}-{oid}")))
                            .xsmall()
                            .ghost()
                            .label(if suffix == "up" { "+1 跳" } else { "−1 跳" })
                            .on_click(cx.listener(move |panel, _, window, cx| {
                                let new_price = price + delta;
                                if new_price <= Decimal::ZERO {
                                    return;
                                }
                                panel.dispatch_training(
                                    Ok(TrainingUiAction::ModifyOrder {
                                        run_id: rid.clone(),
                                        request: ModifyTrainingOrderRequest {
                                            order_id: oid.clone(),
                                            quantity: None,
                                            limit_price: is_limit.then_some(new_price),
                                            stop_price: (!is_limit).then_some(new_price),
                                        },
                                    }),
                                    window,
                                    cx,
                                );
                            })),
                    );
                }
            }
            if active {
                let rid = run_id.clone();
                line = line.child(
                    Button::new(SharedString::from(format!("trn-wb-cancel-{oid}")))
                        .xsmall()
                        .ghost()
                        .label("撤单")
                        .on_click(cx.listener(move |panel, _, window, cx| {
                            panel.dispatch_training(
                                Ok(TrainingUiAction::CancelOrder {
                                    run_id: rid.clone(),
                                    order_id: oid.clone(),
                                }),
                                window,
                                cx,
                            );
                        })),
                );
            }
            orders = orders.child(line);
        }
        let fills: Vec<SharedString> = ws
            .fills
            .iter()
            .rev()
            .take(8)
            .map(|f| {
                SharedString::from(format!(
                    "{} {} @ {} · 费 {}",
                    if f.side == OrderSide::Buy {
                        "买"
                    } else {
                        "卖"
                    },
                    to_canonical_text(f.gross_quantity),
                    to_canonical_text(f.fill_price),
                    to_canonical_text(f.fee_amount)
                ))
            })
            .collect();
        v_flex()
            .id("trn-wb-ledger")
            .test_support()
            .w_full()
            .gap_1p5()
            .p_2()
            .border_1()
            .border_color(cx.theme().border)
            .rounded(cx.theme().radius)
            .child(div().text_sm().font_semibold().child("练习账本"))
            .child(
                div()
                    .id("trn-wb-funding")
                    .test_support()
                    .text_xs()
                    .child(funding.clone())
                    .test_text(funding),
            )
            .child(
                div()
                    .id("trn-wb-equity")
                    .test_support()
                    .text_xs()
                    .font_family(cx.theme().mono_font_family.clone())
                    .child(equity_text.clone())
                    .test_text(equity_text),
            )
            .child(
                div()
                    .id("trn-wb-position")
                    .test_support()
                    .text_xs()
                    .child(pos_text.clone())
                    .test_text(pos_text),
            )
            .child(buy_row)
            .child(sell_row)
            .child(buttons)
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        "快捷买卖按当前可见收盘价成交（不利方向取整）；可 Ctrl+Z 撤销到本根推进前",
                    ),
            )
            .when(!ws.open_orders.is_empty(), |c| {
                c.child(
                    div()
                        .text_xs()
                        .font_semibold()
                        .child("挂单（可在图上拖动价格线改价）"),
                )
                .child(orders)
            })
            .when(!fills.is_empty(), |c| {
                c.child(div().text_xs().font_semibold().child("最近成交"))
                    .children(fills.into_iter().map(|t| {
                        div()
                            .text_xs()
                            .font_family(cx.theme().mono_font_family.clone())
                            .child(t)
                    }))
            })
            .into_any_element()
    }

    /// History / analysis / transfer overlay panel (`S-53`).
    pub(crate) fn render_training_side_panel(
        &self,
        state: &WorkbenchSharedState,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let panel_kind = state.training.side_panel;
        let body: AnyElement = match panel_kind {
            TrainingSidePanel::None => return None,
            TrainingSidePanel::History => {
                let rows: Vec<SharedString> = state
                    .training
                    .history
                    .iter()
                    .map(|r| {
                        let window = match (r.train_start_date, r.train_end_date) {
                            (Some(a), Some(b)) => format!("{a}～{b}"),
                            _ => "（隐藏模式，结束揭示前不显示日期）".into(),
                        };
                        SharedString::from(format!(
                            "#{} {} {} {} · {} · {}/{} 步 · 收益 {}% · 盈亏 {} · 平仓 {} 笔{}",
                            r.index,
                            r.display_symbol,
                            r.timeframe,
                            window,
                            crate::training_ui::status_label(r.status).0,
                            r.current_step,
                            r.total_steps,
                            to_canonical_text(r.total_return_pct),
                            to_canonical_text(r.total_pnl),
                            r.closed_trade_count,
                            r.win_rate_pct
                                .map(|w| format!(" · 胜率 {}%", to_canonical_text(w)))
                                .unwrap_or_default()
                        ))
                    })
                    .collect();
                v_flex()
                    .gap_1()
                    .when(rows.is_empty(), |c| {
                        c.child(div().text_xs().child("还没有训练记录"))
                    })
                    .children(rows.into_iter().enumerate().map(|(i, t)| {
                        div()
                            .id(SharedString::from(format!("trn-history-row-{i}")))
                            .test_support()
                            .text_xs()
                            .child(t.clone())
                            .test_text(t)
                    }))
                    .into_any_element()
            }
            TrainingSidePanel::Analysis => match &state.training.analysis {
                None => div().text_xs().child("暂无分析数据").into_any_element(),
                Some(rep) => {
                    let m = &rep.groups;
                    let opt =
                        |v: Option<Decimal>| v.map(to_canonical_text).unwrap_or_else(|| "—".into());
                    let ratio = crate::training_ui::ratio_metric_text;
                    let mut lines = vec![
                        format!(
                            "练习账户 {} · 余额 {} · 权益 {} · 净存入 {} · 累计盈亏 {}",
                            rep.quote_currency,
                            to_canonical_text(rep.balance),
                            to_canonical_text(rep.equity),
                            to_canonical_text(rep.net_deposits),
                            to_canonical_text(rep.cumulative_pnl)
                        ),
                        format!(
                            "共 {} 局（已结束 {}，盈利 {}）· 局胜率 {} · 费用 {}",
                            rep.run_count,
                            rep.ended_run_count,
                            rep.winning_run_count,
                            opt(rep.run_win_rate),
                            to_canonical_text(rep.total_fees)
                        ),
                        format!(
                            "平仓 {} 组 · 胜率 {} · 盈亏比 {} · 利润因子 {} · 极值盈亏比 {} · 期望 {}",
                            m.win_rate.closed_trade_count,
                            opt(m.win_rate.win_rate),
                            ratio(&m.payoff_ratio),
                            ratio(&m.profit_factor),
                            ratio(&m.extreme_payoff_ratio),
                            opt(m.expectancy.value)
                        ),
                        format!(
                            "平均持仓 {} 根 · 计划执行率 {} · 撤销 {} 次",
                            opt(m.average_holding_bars),
                            opt(rep.plan_execution_rate),
                            rep.undo_count
                        ),
                    ];
                    for b in rep.by_timeframe.iter().chain(&rep.by_market_category) {
                        lines.push(format!(
                            "分组 {} · {} 局 · 盈亏 {} · 平仓 {} 组",
                            b.key,
                            b.run_count,
                            to_canonical_text(b.pnl),
                            b.metrics.win_rate.closed_trade_count
                        ));
                    }
                    if m.sample_insufficient {
                        lines.push(format!(
                            "样本不足：平仓少于 {} 组，比例指标仅供参考",
                            delta_app::ACCOUNT_REPORT_MIN_GROUPS
                        ));
                    }
                    v_flex()
                        .gap_1()
                        .children(lines.into_iter().enumerate().map(|(i, t)| {
                            let t = SharedString::from(t);
                            div()
                                .id(SharedString::from(format!("trn-analysis-line-{i}")))
                                .test_support()
                                .text_xs()
                                .child(t.clone())
                                .test_text(t)
                        }))
                        .into_any_element()
                }
            },
            TrainingSidePanel::Transfer => {
                let acc = state.training.practice_account.clone();
                let text = SharedString::from(match &acc {
                    Some(a) => format!(
                        "{} · 可用 {} · 本局占用 {} · 权益 {}",
                        a.name,
                        to_canonical_text(a.available),
                        to_canonical_text(a.allocated_to_open_run),
                        to_canonical_text(a.equity)
                    ),
                    None => "尚无练习账户".into(),
                });
                let ccy = acc
                    .as_ref()
                    .map(|a| a.quote_currency.clone())
                    .unwrap_or_else(|| "USDT".into());
                let mut row = h_flex().gap_1().items_center();
                for (id, label, kind, amount) in [
                    (
                        "trn-transfer-dep-10k",
                        "存入 10000",
                        PracticeTransferKind::Deposit,
                        10_000,
                    ),
                    (
                        "trn-transfer-dep-100k",
                        "存入 100000",
                        PracticeTransferKind::Deposit,
                        100_000,
                    ),
                    (
                        "trn-transfer-wd-10k",
                        "取出 10000",
                        PracticeTransferKind::Withdrawal,
                        10_000,
                    ),
                ] {
                    let ccy = ccy.clone();
                    row = row.child(Button::new(id).xsmall().outline().label(label).on_click(
                        cx.listener(move |panel, _, window, cx| {
                            panel.dispatch_training(
                                Ok(TrainingUiAction::TransferFunds {
                                    currency: ccy.clone(),
                                    kind,
                                    amount: Decimal::from(amount),
                                }),
                                window,
                                cx,
                            );
                        }),
                    ));
                }
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .id("trn-transfer-account")
                            .test_support()
                            .text_xs()
                            .child(text.clone())
                            .test_text(text),
                    )
                    .child(row)
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(
                            "练习资金只在练习账户内流转，与真实资产账本隔离；取出不能超过可用余额",
                        ),
                    )
                    .into_any_element()
            }
        };
        let title = match panel_kind {
            TrainingSidePanel::History => "训练历史",
            TrainingSidePanel::Analysis => "练习账户分析",
            TrainingSidePanel::Transfer => "练习资金划转",
            TrainingSidePanel::None => "",
        };
        Some(
            v_flex()
                .id("trn-side-panel")
                .test_support()
                .w_full()
                .gap_1p5()
                .p_2()
                .border_1()
                .border_color(cx.theme().border)
                .rounded(cx.theme().radius)
                .child(
                    h_flex()
                        .justify_between()
                        .child(div().text_sm().font_semibold().child(title))
                        .child(
                            Button::new("trn-side-panel-close")
                                .xsmall()
                                .ghost()
                                .label("关闭")
                                .on_click(cx.listener(|panel, _, window, cx| {
                                    panel.dispatch_training(
                                        Ok(TrainingUiAction::OpenSidePanel(
                                            TrainingSidePanel::None,
                                        )),
                                        window,
                                        cx,
                                    );
                                })),
                        ),
                )
                .child(body)
                .into_any_element(),
        )
    }
}

/// Higher-timeframe chart slice built only from the host's aggregated (already truncated) candles.
/// Masked runs keep the scrubbed template dates, so no real date reaches the view.
pub(crate) fn aggregated_slice(
    ws: &TrainingWorkspaceSnapshotDto,
    agg: &delta_app::TrainingAggregatedViewDto,
) -> Option<delta_app::KlineWindowSliceDto> {
    let template = ws.chart_slice.bars.last()?.clone();
    let mut slice = ws.chart_slice.clone();
    slice.timeframe = agg.view_timeframe.clone();
    slice.visible_start_index = 0;
    slice.visible_end_index = agg.candles.len();
    slice.total_visible_bars = agg.candles.len();
    slice.sma_series.clear();
    slice.ema_series.clear();
    slice.macd_series.clear();
    slice.rsi_series.clear();
    slice.markers.clear();
    slice.gap_indices.clear();
    slice.has_incomplete_tail = agg.candles.last().is_some_and(|c| !c.complete);
    slice.bars = agg
        .candles
        .iter()
        .map(|c| {
            let mut bar = template.clone();
            if let Some(open_at) = c.open_at.filter(|_| !ws.run.is_identity_masked) {
                bar.open_at = open_at;
                bar.session_date = open_at.date_naive();
            }
            bar.timeframe = agg.view_timeframe.clone();
            bar.open = c.open;
            bar.high = c.high;
            bar.low = c.low;
            bar.close = c.close;
            bar.volume = c.volume;
            bar.is_final = c.complete;
            bar
        })
        .collect();
    slice.lod_candles = slice
        .bars
        .iter()
        .zip(&agg.candles)
        .enumerate()
        .map(|(i, (bar, c))| delta_app::LodCandleDto {
            start_bar_index: i,
            end_bar_index: i + 1,
            session_date: bar.session_date,
            open: dec_f64(c.open),
            high: dec_f64(c.high),
            low: dec_f64(c.low),
            close: dec_f64(c.close),
            volume: dec_f64(c.volume),
            has_gap_before: false,
            is_incomplete: !c.complete,
        })
        .collect();
    Some(slice)
}

/// Info row: crosshair (or latest) bar OHLC and change, plus playback progress (`S-54`).
/// Uses `display_slice` and `display_tf` so higher-timeframe aggregated views report matching
/// timeframe and OHLC values (`UI-B14`).
pub(crate) fn render_info_row(
    ws: &TrainingWorkspaceSnapshotDto,
    display_slice: &delta_app::KlineWindowSliceDto,
    display_tf: &str,
    viewport: Option<&delta_app::KlineViewport>,
    cx: &mut Context<CenterWorkspacePanel>,
) -> AnyElement {
    let bars = &display_slice.bars;
    let local = viewport
        .and_then(|vp| vp.crosshair_index)
        .and_then(|g| g.checked_sub(display_slice.visible_start_index))
        .filter(|&i| i < bars.len())
        .or_else(|| bars.len().checked_sub(1));
    let ohlc = local
        .and_then(|i| {
            let bar = bars.get(i)?;
            let prev = i.checked_sub(1).and_then(|p| bars.get(p));
            let change = prev
                .filter(|p| !p.close.is_zero())
                .map(|p| {
                    format!(
                        " · 涨跌 {}%",
                        to_canonical_text(
                            ((bar.close - p.close) / p.close * Decimal::from(100)).round_dp(2)
                        )
                    )
                })
                .unwrap_or_default();
            Some(format!(
                "开 {} 高 {} 低 {} 收 {}{change}",
                to_canonical_text(bar.open),
                to_canonical_text(bar.high),
                to_canonical_text(bar.low),
                to_canonical_text(bar.close)
            ))
        })
        .unwrap_or_else(|| "—".into());
    let remaining = (ws.run.total_steps - ws.run.current_step_index).max(0);
    let text = SharedString::from(format!(
        "{} · {} · 第 {} / {} 根 · 还剩 {} 根 · {ohlc}",
        ws.run.display_symbol, display_tf, ws.run.current_step_index, ws.run.total_steps, remaining
    ));
    div()
        .id("trn-wb-info-row")
        .test_support()
        .w_full()
        .px_2()
        .py_1()
        .rounded(cx.theme().radius)
        .bg(cx.theme().muted)
        .font_family(cx.theme().mono_font_family.clone())
        .text_xs()
        .child(text.clone())
        .test_text(text)
        .into_any_element()
}

impl CenterWorkspacePanel {
    /// List-view card: start a default BTC/USDT practice run or prepare the data first (`S-48`).
    pub(crate) fn render_default_btc_start(
        &self,
        state: &WorkbenchSharedState,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let hidden = state.training.new_run_draft.hidden_mode;
        let preparing = state.training.prepare_job.is_some();
        let status = SharedString::from(match &state.training.data_status {
            Some(s) if preparing => {
                let ready: Vec<&str> = s
                    .timeframes
                    .iter()
                    .filter(|t| t.candidate_ready == Some(true))
                    .map(|t| t.timeframe.as_str())
                    .collect();
                if ready.is_empty() {
                    "正在准备比特币训练数据…（可离开本页，完成后自动可用）".to_string()
                } else {
                    format!(
                        "正在准备比特币训练数据…已可开局：{}（其余级别完成后自动可用）",
                        ready.join("、")
                    )
                }
            }
            None if preparing => {
                "正在准备比特币训练数据…（可离开本页，完成后自动可用）".to_string()
            }
            Some(s) => s
                .timeframes
                .iter()
                .map(|t| {
                    format!(
                        "{} {} 根{}",
                        t.timeframe.as_str(),
                        t.bar_count,
                        match (t.candidate_ready, t.stale) {
                            (Some(true), false) => "（可开局）",
                            (Some(true), true) => "（可开局，后台待更新）",
                            _ => "（需准备）",
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join(" · "),
            None => "比特币 BTC/USDT（币安为主、欧易核验）：开局时自动检查数据".to_string(),
        });
        let mut row = h_flex().gap_1p5().items_center().flex_wrap();
        for tf in Timeframe::ALL {
            row = row.child(
                Button::new(SharedString::from(format!("trn-start-btc-{}", tf.as_str())))
                    .small()
                    .when(tf == Timeframe::D1, |b| b.primary())
                    .when(tf != Timeframe::D1, |b| b.outline())
                    .disabled(preparing && !state.training.default_timeframe_ready(tf))
                    .label(SharedString::from(format!(
                        "新一局 · 比特币 {}",
                        tf.as_str()
                    )))
                    .on_click(cx.listener(move |panel, _, window, cx| {
                        panel.dispatch_training(
                            Ok(TrainingUiAction::StartDefaultRun {
                                timeframe: tf,
                                hidden_mode: hidden,
                            }),
                            window,
                            cx,
                        );
                        panel.ensure_training_ticker(window, cx);
                    })),
            );
        }
        row = row
            .child(
                Button::new("trn-start-hidden")
                    .small()
                    .ghost()
                    .selected(hidden)
                    .label(if hidden {
                        "隐藏模式：开"
                    } else {
                        "隐藏模式：关"
                    })
                    .on_click(cx.listener(|panel, _, _, cx| {
                        let mut g = panel.state.lock().unwrap();
                        g.training.new_run_draft.hidden_mode =
                            !g.training.new_run_draft.hidden_mode;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("trn-prepare-data")
                    .small()
                    .ghost()
                    .disabled(preparing)
                    .label("准备 / 更新训练数据")
                    .on_click(cx.listener(|panel, _, window, cx| {
                        panel.dispatch_training(
                            Ok(TrainingUiAction::PrepareDefaultData),
                            window,
                            cx,
                        );
                        panel.ensure_training_ticker(window, cx);
                    })),
            )
            .child(self.side_panel_button(
                "trn-list-open-history",
                "训练历史",
                TrainingSidePanel::History,
                cx,
            ))
            .child(self.side_panel_button(
                "trn-list-open-analysis",
                "账户分析",
                TrainingSidePanel::Analysis,
                cx,
            ))
            .child(self.side_panel_button(
                "trn-list-open-transfer",
                "资金划转",
                TrainingSidePanel::Transfer,
                cx,
            ));
        v_flex()
            .id("trn-default-btc-card")
            .test_support()
            .w_full()
            .gap_1p5()
            .p_2()
            .border_1()
            .border_color(cx.theme().primary)
            .rounded(cx.theme().radius)
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .child("比特币训练（练习账户 USDT）"),
            )
            .child(row)
            .child(
                div()
                    .id("trn-data-status")
                    .test_support()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(status.clone())
                    .test_text(status),
            )
            .into_any_element()
    }
}

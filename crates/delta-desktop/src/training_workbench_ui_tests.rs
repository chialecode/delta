//! M5-W05 headless workbench flow (`S-54`, AC-11/26/33) on loopback market fixtures.
//! Synthetic fixture data only; no public network.

use std::fs;
use std::sync::Arc;
use std::time::Duration;

use chrono::{NaiveDate, TimeZone as _, Utc};
use delta_app::{
    apply_process_quiet_guards, market_fixture::MarketFixtureServer, resolve_app_paths, AppHost,
    AppService, BuildChannel, OrderSide, PrepareTrainingDataRequest, SimOrderType, SimRunStatus,
    Timeframe,
};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{px, Focusable as _, TestAppContext};
use rust_decimal::prelude::*;

use crate::controller::WorkbenchUpdated;
use crate::test_support::{click_id, has_rendered_id, temp_test_dir};
use crate::training_ui::TrainingViewMode;
use crate::workbench::DeltaWorkbench;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    temp_test_dir("delta_m5_desktop", tag)
}

/// Wall-clock audit stamps of the run itself (not market data) are allowed.
fn strip_wall_clock(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.remove("created_at");
            map.remove("updated_at");
            map.remove("finished_at");
            map.values_mut().for_each(strip_wall_clock);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(strip_wall_clock),
        _ => {}
    }
}

fn no_real_identity_json(value: impl serde::Serialize) {
    let mut value = serde_json::to_value(value).unwrap();
    strip_wall_clock(&mut value);
    no_real_identity(&value.to_string());
}

fn no_real_identity(json: &str) {
    for leak in ["BTC", "BINANCE", "OKX"] {
        assert!(!json.contains(leak), "hidden mode leaked {leak}");
    }
    for year in 2017..=2026 {
        if let Some(at) = json.find(&format!("\"{year}-")) {
            let start = json[..at]
                .char_indices()
                .rev()
                .nth(60)
                .map_or(0, |(i, _)| i);
            panic!(
                "hidden mode leaked a {year} date: {}",
                &json[start..(at + 40).min(json.len())]
            );
        }
    }
}

#[gpui_kit::test]
async fn test_m5_w05_headless_workbench_flow(cx: &mut TestAppContext) {
    apply_process_quiet_guards();
    let tmp = temp_dir("w05");
    let fixture =
        MarketFixtureServer::start(Utc.with_ymd_and_hms(2026, 11, 10, 0, 0, 0).unwrap()).unwrap();
    let paths = resolve_app_paths(Some(BuildChannel::Source), Some(&tmp)).unwrap();
    let mut svc = AppService::open(paths, "USD", "USD", "UTC", true).unwrap();
    let summary = svc
        .prepare_training_data(&PrepareTrainingDataRequest {
            timeframes: vec![Timeframe::D1],
            base_urls: fixture.base_urls(),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(summary.failed, 0);
    let host = AppHost::from_service(svc);

    cx.update(gpui_kit::init);
    let (workbench, cx) = cx.add_window_view(|window, cx| {
        DeltaWorkbench::new(
            host.clone(),
            tmp.clone(),
            NaiveDate::from_ymd_opt(2026, 11, 10).unwrap(),
            window,
            cx,
        )
    });
    cx.simulate_resize(gpui_kit::size(px(1600.), px(3400.)));
    let (controller, center, state) = cx.update(|_, cx| {
        let view = workbench.read(cx);
        (
            view.controller.clone(),
            view.center_panel().clone(),
            Arc::clone(view.shared_state()),
        )
    });
    macro_rules! idle {
        () => {
            controller
                .condition::<WorkbenchUpdated>(cx, |ctl, _| ctl.is_idle())
                .await
        };
    }
    idle!();

    // 1. Default BTC/USDT practice run from the list card (hidden mode is the default).
    click_id(cx, "nav-btn-nav-training");
    idle!();
    assert!(has_rendered_id(cx, "trn-default-btc-card"));
    click_id(cx, "trn-start-btc-1d");
    controller
        .condition::<WorkbenchUpdated>(cx, |ctl, _| {
            ctl.is_idle() && ctl.state.lock().unwrap().training.mode == TrainingViewMode::Workspace
        })
        .await;
    for id in [
        "trn-wb-toolbar",
        "trn-wb-info-row",
        "trn-wb-ledger",
        "trn-wb-saved",
        "trn-wb-quick-buy",
        "training-kline-chart",
    ] {
        assert!(has_rendered_id(cx, id), "{id} not rendered");
    }
    let run_id = {
        let g = state.lock().unwrap();
        let ws = g.training.workspace.as_ref().unwrap();
        assert!(ws.run.is_identity_masked);
        assert!(ws.run.practice_account_id.is_some());
        assert_eq!(ws.run.timeframe, "1d");
        no_real_identity_json(&**ws);
        assert!(g.training.practice_account.is_some());
        ws.run.run_id.clone()
    };
    let snapshot = |state: &Arc<std::sync::Mutex<crate::workbench::WorkbenchSharedState>>| {
        state.lock().unwrap().training.workspace.clone().unwrap()
    };

    // 2. Single keys never trade: Q/W/E/Space/N with no draft create no order or fill.
    let ws0 = snapshot(&state);
    assert!(ws0.all_orders.is_empty() && ws0.fills.is_empty());

    // 3. Quick buy (mouse click only) fills at the visible close, then Ctrl+Z undoes it.
    let close = ws0.chart_slice.bars.last().unwrap().close;
    click_id(cx, "trn-wb-quick-buy");
    idle!();
    let ws1 = snapshot(&state);
    assert_eq!(ws1.fills.len(), 1);
    // Visible close plus the built-in crypto profile's adverse slippage (10 bps), never better.
    let fill = ws1.fills[0].fill_price;
    assert!(
        fill >= close && fill <= close * Decimal::new(1002, 3),
        "{fill} vs {close}"
    );
    assert_eq!(ws1.fills[0].side, OrderSide::Buy);
    // 1/10 of equity by default, floored to the lot size.
    assert!(ws1.fills[0].gross_quantity * close <= ws0.account.total_equity / Decimal::from(10));
    cx.simulate_keystrokes("ctrl-z");
    idle!();
    let ws2 = snapshot(&state);
    assert!(ws2.fills.is_empty());
    assert_eq!(ws2.account.total_equity, ws0.account.total_equity);
    click_id(cx, "trn-wb-quick-buy");
    idle!();
    assert_eq!(snapshot(&state).fills.len(), 1);

    // 4. Draw-to-order: a chart click opens a draft; nothing is submitted until confirm.
    click_id(cx, "trn-wb-draw-order");
    let chart_bounds = cx.update(|window, cx| {
        window.render_frame(cx);
        center
            .read(cx)
            .training_plot_bounds
            .lock()
            .unwrap()
            .unwrap()
    });
    let below = close * Decimal::new(995, 3);
    let y_below = cx.update(|_, cx| center.read(cx).price_to_y(below.to_f64().unwrap()).unwrap());
    let click_at = gpui_kit::point(chart_bounds.center().x, px(y_below));
    cx.simulate_click(click_at, gpui_kit::Modifiers::default());
    cx.run_until_parked();
    {
        let g = state.lock().unwrap();
        let draft = g.training.wb.draft.as_ref().expect("draft opened");
        assert_eq!(draft.options.len(), 2);
        assert!(draft.price < close);
        assert_eq!(g.training.workspace.as_ref().unwrap().open_orders.len(), 0);
    }
    // Ctrl+Enter without a chosen option does nothing — even when focus is inside the order ticket (`UI-B03`).
    cx.simulate_keystrokes("ctrl-enter");
    idle!();
    assert!(snapshot(&state).open_orders.is_empty());
    cx.update(|window, cx| {
        center.update(cx, |panel, cx| {
            let fh = panel
                .fields
                .get("form.trn_order_qty")
                .read(cx)
                .focus_handle(cx);
            fh.focus(window, cx);
        })
    });
    cx.simulate_keystrokes("ctrl-enter");
    idle!();
    assert!(
        snapshot(&state).open_orders.is_empty(),
        "UI-B03: open draft with no option must block order-ticket Ctrl+Enter submit"
    );
    click_id(cx, "trn-wb-draft-BuyLimit");
    cx.simulate_keystrokes("ctrl-enter");
    idle!();
    let ws3 = snapshot(&state);
    assert_eq!(
        ws3.open_orders.len(),
        1,
        "draft confirmed into one limit order"
    );
    let order = ws3.open_orders[0].clone();
    let SimOrderType::Limit { limit_price } = order.order_type else {
        panic!("expected a limit order")
    };
    click_id(cx, "trn-wb-draw-order");
    assert!(!state.lock().unwrap().training.wb.draw_order_enabled);

    // 5. Drag the order line: release = ModifyOrder (never a fill), locked lines don't move.
    let y_line = cx.update(|window, cx| {
        window.render_frame(cx);
        center
            .read(cx)
            .price_to_y(limit_price.to_f64().unwrap())
            .unwrap()
    });
    let x = chart_bounds.center().x;
    cx.simulate_mouse_down(
        gpui_kit::point(x, px(y_line)),
        gpui_kit::MouseButton::Left,
        gpui_kit::Modifiers::default(),
    );
    cx.simulate_mouse_move(
        gpui_kit::point(x, px(y_line + 12.)),
        Some(gpui_kit::MouseButton::Left),
        gpui_kit::Modifiers::default(),
    );
    cx.simulate_mouse_up(
        gpui_kit::point(x, px(y_line + 12.)),
        gpui_kit::MouseButton::Left,
        gpui_kit::Modifiers::default(),
    );
    idle!();
    let ws4 = snapshot(&state);
    assert_eq!(ws4.open_orders.len(), 1);
    assert_eq!(ws4.fills.len(), ws3.fills.len());
    let SimOrderType::Limit { limit_price: moved } = ws4.open_orders[0].order_type else {
        panic!()
    };
    assert!(moved < limit_price, "dragging down lowers the limit price");
    // Undo restores the pre-drag price.
    cx.simulate_keystrokes("ctrl-z");
    idle!();
    let SimOrderType::Limit {
        limit_price: restored,
    } = snapshot(&state).open_orders[0].order_type
    else {
        panic!()
    };
    assert_eq!(restored, limit_price);
    // Lock: a drag does nothing.
    click_id(cx, &format!("trn-wb-lock-{}", order.order_id));
    cx.simulate_mouse_down(
        gpui_kit::point(x, px(y_line)),
        gpui_kit::MouseButton::Left,
        gpui_kit::Modifiers::default(),
    );
    cx.simulate_mouse_up(
        gpui_kit::point(x, px(y_line + 20.)),
        gpui_kit::MouseButton::Left,
        gpui_kit::Modifiers::default(),
    );
    idle!();
    let SimOrderType::Limit { limit_price: still } = snapshot(&state).open_orders[0].order_type
    else {
        panic!()
    };
    assert_eq!(still, limit_price);

    // 6. Drawings: W = horizontal (one click), Q = ray (two clicks), Delete removes the selected.
    // Click rows well away from the (locked) order line so no click grabs it.
    let free_y = |offset: f32| {
        let top = chart_bounds.origin.y + px(offset);
        let bottom = chart_bounds.origin.y + chart_bounds.size.height - px(offset);
        if (top - px(y_line)).abs() > px(30.) {
            top
        } else {
            bottom
        }
    };
    cx.simulate_keystrokes("w");
    let tool = state.lock().unwrap().training.wb.tool;
    assert_eq!(tool, crate::training_workbench::WorkbenchTool::Horizontal);
    cx.simulate_click(
        gpui_kit::point(chart_bounds.center().x, free_y(25.)),
        gpui_kit::Modifiers::default(),
    );
    idle!();
    assert_eq!(state.lock().unwrap().training.wb.drawings.len(), 1);
    cx.simulate_keystrokes("q");
    cx.simulate_click(
        gpui_kit::point(chart_bounds.origin.x + px(40.), free_y(45.)),
        gpui_kit::Modifiers::default(),
    );
    cx.run_until_parked();
    cx.simulate_click(
        gpui_kit::point(chart_bounds.center().x, free_y(60.)),
        gpui_kit::Modifiers::default(),
    );
    idle!();
    {
        let g = state.lock().unwrap();
        assert_eq!(g.training.wb.drawings.len(), 2);
        let ray = g
            .training
            .wb
            .drawings
            .iter()
            .find(|d| d.kind == "ray")
            .unwrap();
        assert_eq!(ray.anchors.len(), 2);
        // Anchors are (bar index, price): never a date.
        no_real_identity_json(&g.training.wb.drawings);
    }
    cx.simulate_keystrokes("delete");
    idle!();
    assert_eq!(state.lock().unwrap().training.wb.drawings.len(), 1);
    assert_eq!(
        snapshot(&state).fills.len(),
        ws3.fills.len(),
        "drawing never trades"
    );
    cx.simulate_keystrokes("escape");

    // 7. Measure: label shows price diff, percent and bar count, no date.
    click_id(cx, "trn-wb-tool-measure");
    cx.simulate_click(
        gpui_kit::point(chart_bounds.origin.x + px(20.), free_y(40.)),
        gpui_kit::Modifiers::default(),
    );
    cx.simulate_click(
        gpui_kit::point(chart_bounds.center().x, free_y(70.)),
        gpui_kit::Modifiers::default(),
    );
    cx.run_until_parked();
    let label = state.lock().unwrap().training.wb.measure.label().unwrap();
    assert!(label.contains("根") && label.contains('%'));
    no_real_identity(&label);
    assert!(has_rendered_id(cx, "trn-wb-measure"));
    cx.simulate_keystrokes("escape");

    // 8. Continuous play at 150 ms: atomic steps, auto-pause on a fill or trigger.
    click_id(cx, "trn-wb-speed-3");
    let start_step = snapshot(&state).run.current_step_index;
    click_id(cx, "trn-wb-play");
    // Long enough for the whole run at 150 ms per bar; stops at the first auto-pause.
    for _ in 0..4000 {
        cx.executor().advance_clock(Duration::from_millis(50));
        cx.run_until_parked();
        if !state.lock().unwrap().training.wb.playback.playing {
            break;
        }
    }
    idle!();
    {
        let g = state.lock().unwrap();
        assert!(!g.training.wb.playback.playing, "playback auto-paused");
        let reason = g.training.wb.playback.last_pause_reason.clone().unwrap();
        assert!(
            reason.contains("成交") || reason.contains("训练结束"),
            "pause reason: {reason}"
        );
    }
    let ws5 = snapshot(&state);
    assert!(ws5.run.current_step_index > start_step);
    assert_eq!(
        ws5.chart_slice.visible_end_index, ws5.run.visible_bar_count,
        "UI-B01: PlayStep must advance chart viewport to reveal newly played bars"
    );
    // Leaving Workspace pauses playback cleanly (`UI-B02`).
    if ws5.run.status == SimRunStatus::Active {
        let step_before_nav = ws5.run.current_step_index;
        click_id(cx, "trn-wb-play");
        assert!(state.lock().unwrap().training.wb.playback.playing);
        click_id(cx, "nav-btn-nav-overview");
        idle!();
        for _ in 0..10 {
            cx.executor().advance_clock(Duration::from_millis(50));
            cx.run_until_parked();
        }
        assert!(
            !state.lock().unwrap().training.wb.playback.playing,
            "UI-B02: navigating away from training workspace must pause playback"
        );
        click_id(cx, "nav-btn-nav-training");
        idle!();
        assert_eq!(snapshot(&state).run.current_step_index, step_before_nav);
    }
    // Undo never crosses an advance.
    cx.simulate_keystrokes("ctrl-z");
    idle!();
    assert_eq!(
        snapshot(&state).run.current_step_index,
        ws5.run.current_step_index
    );

    // 9. Focused input swallows shortcuts: N / Space do nothing.
    cx.update(|window, cx| {
        center.update(cx, |panel, cx| {
            let fh = panel
                .fields
                .get("form.trn_order_qty")
                .read(cx)
                .focus_handle(cx);
            fh.focus(window, cx);
        })
    });
    let before = snapshot(&state).run.current_step_index;
    cx.simulate_keystrokes("n space");
    idle!();
    assert_eq!(snapshot(&state).run.current_step_index, before);
    assert!(!state.lock().unwrap().training.wb.playback.playing);
    cx.simulate_keystrokes("escape");

    // 10. Finish → report → reveal; the practice account was settled.
    click_id(cx, "training-finish-btn");
    idle!();
    assert_eq!(
        state.lock().unwrap().training.mode,
        TrainingViewMode::Report
    );
    {
        let g = state.lock().unwrap();
        no_real_identity_json(g.training.report.as_ref().unwrap());
    }
    click_id(cx, "training-reveal-btn");
    idle!();
    {
        let g = state.lock().unwrap();
        let rev = g.training.reveal.as_ref().unwrap();
        assert_eq!(rev.real_instrument_id, "BINANCE:BTC/USDT");
    }
    let account = host.show_practice_account("USDT").unwrap();
    assert!(account.open_run_id.is_none());
    let finished = host.get_training_report(&run_id).unwrap();
    assert_eq!(finished.run.status, SimRunStatus::Finished);

    // 11. History and analysis panels render from the list.
    click_id(cx, "training-report-back-btn");
    idle!();
    click_id(cx, "trn-list-open-history");
    idle!();
    assert!(has_rendered_id(cx, "trn-history-row-0"));
    click_id(cx, "trn-list-open-analysis");
    idle!();
    assert!(has_rendered_id(cx, "trn-analysis-line-0"));

    cx.update(|window, _| window.remove_window());
    host.shutdown().unwrap();
    drop(fixture);
    let _ = fs::remove_dir_all(&tmp);
}

/// `S-48` / `M5-A-01`: while the preparation job still runs, a timeframe whose batch has already
/// published (daily first) can start; the others stay disabled until ready.
#[test]
fn prepared_timeframes_start_before_the_whole_job_finishes() {
    use crate::training_ui::TrainingUiState;
    use delta_app::{Timeframe, TrainingDataStatusDto, TrainingDataTimeframeStatusDto};
    let tf = |timeframe, ready| TrainingDataTimeframeStatusDto {
        timeframe,
        bar_count: if ready { 3000 } else { 0 },
        first_open_at: None,
        last_close_at: None,
        stale: false,
        candidate_ready: Some(ready),
    };
    let mut state = TrainingUiState::default();
    assert!(
        !state.default_timeframe_ready(Timeframe::D1),
        "no status yet"
    );
    state.data_status = Some(TrainingDataStatusDto {
        instrument_id: "BINANCE:BTC/USDT".into(),
        dataset_version: Some(2),
        timeframes: vec![
            tf(Timeframe::D1, true),
            tf(Timeframe::H4, false),
            tf(Timeframe::H1, false),
        ],
        needs_preparation: false,
        needs_update: false,
    });
    assert!(state.default_timeframe_ready(Timeframe::D1));
    assert!(!state.default_timeframe_ready(Timeframe::H4));
    assert!(!state.default_timeframe_ready(Timeframe::H1));
}

//! M5 hidden native-window smoke and `M5-K02` continuous-play latency bench (`S-54`, `S-57`).
//!
//! Both run on loopback market fixtures (synthetic data shaped like Binance/OKX public klines) in a
//! temporary library; no public network, no `delta`/`delta-dev` data (D-26), hidden and
//! never-activated windows (D-25).

use std::{
    path::Path,
    sync::{atomic::Ordering, Arc, Mutex},
    time::{Duration, Instant},
};

use chrono::{NaiveDate, TimeZone as _, Utc};
use delta_app::{
    count_visible_windows_for_current_process, market_fixture::MarketFixtureServer, AppHost,
    CreateTrainingRunRequest, ErrorEnvelope, OrderSide, PrepareTrainingDataRequest, SimOrderType,
    SimRunStatus, TimeInForce, Timeframe, TrainingDrawingDto, TrainingMarketCategory,
    TrainingOrderRequestDto, TrainingWorkspaceSnapshotDto, BUILTIN_FEE_CRYPTO_RATE_ID,
    DEFAULT_TRAINING_INSTRUMENT,
};
use gpui_kit::component::{v_flex, ActiveTheme as _, Root};
use gpui_kit::{
    AppContext as _, Context, IntoElement, ParentElement as _, Render, Styled as _, Window,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::chart_canvas::PAINTED_CHART_FRAMES;
use crate::{
    controller::UiAction,
    training_ui::{TrainingUiAction, TrainingViewMode},
    training_workbench::WorkbenchState,
    training_workbench_ui::workbench_overlay,
    ui_verification::{
        hidden_options, initialize_native_window, native_window, paint_native_window, percentiles,
        quiet_window_check, wait_workbench_idle_and_paint,
    },
    workbench::{verification_app_paths, DeltaWorkbench, VerificationDirectory},
    KlineChartView, MarketColorConvention,
};

/// `M5-K02` budgets: per-step p95 and actual interval p95 at 150 ms/bar.
pub const M5_K02_STEP_P95_BUDGET_MS: f64 = 100.0;
pub const M5_K02_INTERVAL_P95_BUDGET_MS: f64 = 200.0;
const PLAY_INTERVAL_MS: u64 = 150;

fn fixture_end() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 10, 0, 0, 0).unwrap()
}

fn window_err(e: impl std::fmt::Display) -> ErrorEnvelope {
    ErrorEnvelope::new("NATIVE_WINDOW_FAILED", e.to_string(), false)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct M5UiSmokeReport {
    pub status: String,
    pub mode: String,
    pub data_source: String,
    pub executed_at: String,
    pub build_profile: String,
    pub native_window_created: bool,
    pub visible_windows: usize,
    pub focus_stolen: bool,
    pub run_id: String,
    pub timeframe: String,
    pub practice_account: bool,
    pub played_steps: i64,
    pub quick_fill_then_undo: bool,
    pub drawings_saved: usize,
    pub chart_paints: usize,
    pub finished_status: String,
    pub revealed_instrument: String,
    pub elapsed_ms: u64,
    pub build_info: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct M5PlaybackBenchReport {
    pub status: String,
    pub case_id: String,
    pub kpi_id: String,
    pub data_source: String,
    pub executed_at: String,
    pub build_profile: String,
    pub power_throttling: String,
    pub hardware_logical_cores: usize,
    pub cpu: String,
    pub gpu: String,
    pub window_size: String,
    pub visible_windows: usize,
    pub focus_stolen: bool,
    pub timeframe: String,
    pub visible_bars: usize,
    pub drawings: usize,
    pub order_lines: usize,
    pub fill_markers: usize,
    pub play_interval_ms: u64,
    pub warmup_steps: usize,
    pub measured_steps: usize,
    pub native_chart_paints: usize,
    pub step_p50_ms: f64,
    pub step_p95_ms: f64,
    pub step_p99_ms: f64,
    pub interval_p50_ms: f64,
    pub interval_p95_ms: f64,
    pub interval_p99_ms: f64,
    pub step_budget_p95_ms: f64,
    pub interval_budget_p95_ms: f64,
    pub passed: bool,
    /// p95 per phase (ms): advance, snapshot, drawings, account, render+paint.
    pub phase_p95_ms: std::collections::BTreeMap<String, f64>,
    pub step_times_us: Vec<u64>,
    pub interval_times_us: Vec<u64>,
    pub measurement: String,
    pub build_info: Value,
}

/// Prepares BTC/USDT from the loopback fixture into a fresh temporary library.
fn prepared_host(
    dir: &Path,
    timeframes: Vec<Timeframe>,
    from: NaiveDate,
) -> Result<(AppHost, MarketFixtureServer), ErrorEnvelope> {
    let fixture = MarketFixtureServer::start(fixture_end())
        .map_err(|e| ErrorEnvelope::new("FIXTURE_SERVER", e.to_string(), false))?;
    let mut svc =
        delta_app::AppService::open(verification_app_paths(dir), "USD", "USD", "UTC", true)?;
    let summary = svc.prepare_training_data(&PrepareTrainingDataRequest {
        timeframes,
        from: Some(from),
        base_urls: fixture.base_urls(),
        ..Default::default()
    })?;
    if summary.failed > 0 {
        return Err(ErrorEnvelope::new(
            "M5_FIXTURE_PREPARE_FAILED",
            format!("夹具训练数据准备失败批次 {}", summary.failed),
            false,
        ));
    }
    Ok((AppHost::from_service(svc), fixture))
}

/// Hidden-window smoke of the M5 workbench: default BTC run → play → quick buy → undo →
/// drawing → finish → reveal, all through the real controller and panel.
pub fn run_m5_ui_smoke(cli_data_dir: Option<&Path>) -> Result<M5UiSmokeReport, ErrorEnvelope> {
    let started = Instant::now();
    delta_app::apply_process_quiet_guards();
    let base = cli_data_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    let owned = VerificationDirectory::create(&base, "m5-smoke")?;
    let tmp_dir = owned.path.clone();

    let inner = (|| -> Result<M5UiSmokeReport, ErrorEnvelope> {
        let (host, fixture) = prepared_host(
            &tmp_dir,
            vec![Timeframe::D1],
            NaiveDate::from_ymd_opt(2023, 1, 1).unwrap(),
        )?;
        let host_for_ui = host.clone();
        let data_dir = tmp_dir.clone();
        let output: Arc<Mutex<Option<Result<M5UiSmokeReport, ErrorEnvelope>>>> =
            Arc::new(Mutex::new(None));
        let slot = Arc::clone(&output);

        gpui_kit::application()
            .with_assets(gpui_kit::assets::Assets)
            .run(move |cx| {
                gpui_kit::init(cx);
                crate::theme::apply_delta_theme(cx);
                let wb = Arc::new(Mutex::new(None));
                let captured = Arc::clone(&wb);
                let opened = cx.open_window(hidden_options(), move |window, cx| {
                    let view = cx.new(|cx| {
                        DeltaWorkbench::new(
                            host_for_ui,
                            data_dir,
                            NaiveDate::from_ymd_opt(2026, 11, 10).unwrap(),
                            window,
                            cx,
                        )
                    });
                    *captured.lock().unwrap() = Some(view.clone());
                    cx.new(|cx| Root::new(view, window, cx))
                });
                let handle = match opened {
                    Ok(h) => h,
                    Err(e) => {
                        *slot.lock().unwrap() = Some(Err(window_err(e)));
                        cx.quit();
                        return;
                    }
                };
                let hwnd = match native_window() {
                    Ok(h) => h,
                    Err(e) => {
                        *slot.lock().unwrap() = Some(Err(e));
                        cx.quit();
                        return;
                    }
                };
                cx.spawn(async move |cx| {
                    let outcome: Result<M5UiSmokeReport, ErrorEnvelope> = async {
                        initialize_native_window(hwnd)?;
                        let deadline = Instant::now() + Duration::from_secs(90);
                        let paint_start = PAINTED_CHART_FRAMES.load(Ordering::Relaxed);
                        wait_workbench_idle_and_paint(cx, handle, &wb, hwnd, deadline).await?;
                        let dispatch = |cx: &mut gpui_kit::AsyncApp, action: TrainingUiAction| {
                            cx.update_window(handle.into(), |_, window, cx| {
                                let view = wb.lock().unwrap().as_ref().unwrap().clone();
                                view.update(cx, |view, cx| {
                                    view.controller.update(cx, |ctl, cx| {
                                        ctl.dispatch(
                                            UiAction::Training(Box::new(action)),
                                            window,
                                            cx,
                                        )
                                    });
                                });
                            })
                            .map_err(window_err)
                        };
                        let read = |cx: &mut gpui_kit::AsyncApp| {
                            cx.update_window(handle.into(), |_, _, cx| {
                                let view = wb.lock().unwrap().as_ref().unwrap().clone();
                                let state = view.read(cx).shared_state().clone();
                                let g = state.lock().unwrap();
                                g.training.clone()
                            })
                            .map_err(window_err)
                        };
                        cx.update_window(handle.into(), |_, window, cx| {
                            let view = wb.lock().unwrap().as_ref().unwrap().clone();
                            view.update(cx, |view, cx| {
                                view.controller
                                    .update(cx, |ctl, cx| ctl.navigate("nav.training", window, cx))
                            });
                        })
                        .map_err(window_err)?;
                        wait_workbench_idle_and_paint(cx, handle, &wb, hwnd, deadline).await?;
                        dispatch(
                            cx,
                            TrainingUiAction::StartDefaultRun {
                                timeframe: Timeframe::D1,
                                hidden_mode: true,
                            },
                        )?;
                        wait_workbench_idle_and_paint(cx, handle, &wb, hwnd, deadline).await?;
                        let t = read(cx)?;
                        let ws = t.workspace.clone().ok_or_else(|| {
                            ErrorEnvelope::new("M5_SMOKE_NO_RUN", "默认比特币局没有开出来", false)
                        })?;
                        let run_id = ws.run.run_id.clone();
                        if t.mode != TrainingViewMode::Workspace || ws.run.practice_account_id.is_none()
                        {
                            return Err(ErrorEnvelope::new(
                                "M5_SMOKE_RUN_SHAPE",
                                "默认局应进入工作台并使用练习账户",
                                false,
                            ));
                        }
                        // Continuous play through the panel ticker for a few steps.
                        cx.update_window(handle.into(), |_, window, cx| {
                            let view = wb.lock().unwrap().as_ref().unwrap().clone();
                            let center = view.read(cx).center_panel().clone();
                            center.update(cx, |panel, cx| {
                                panel.state.lock().unwrap().training.wb.playback.speed_index = 3;
                                panel.toggle_training_play(window, cx);
                            });
                        })
                        .map_err(window_err)?;
                        loop {
                            cx.background_executor().timer(Duration::from_millis(50)).await;
                            paint_native_window(hwnd)?;
                            let t = read(cx)?;
                            let step = t.workspace.as_ref().map_or(0, |w| w.run.current_step_index);
                            if step >= 5 || !t.wb.playback.playing {
                                break;
                            }
                            if Instant::now() >= deadline {
                                return Err(ErrorEnvelope::new("M5_SMOKE_TIMEOUT", "连续播放超时", false));
                            }
                        }
                        cx.update_window(handle.into(), |_, window, cx| {
                            let view = wb.lock().unwrap().as_ref().unwrap().clone();
                            let center = view.read(cx).center_panel().clone();
                            center.update(cx, |panel, cx| {
                                if panel.state.lock().unwrap().training.wb.playback.playing {
                                    panel.toggle_training_play(window, cx);
                                }
                            });
                        })
                        .map_err(window_err)?;
                        wait_workbench_idle_and_paint(cx, handle, &wb, hwnd, deadline).await?;
                        let played = read(cx)?
                            .workspace
                            .as_ref()
                            .map_or(0, |w| w.run.current_step_index);

                        // Quick buy then undo: fill count goes 0 → 1 → 0.
                        let before = read(cx)?.workspace.unwrap();
                        let close = before.chart_slice.bars.last().map(|b| b.close).unwrap_or_default();
                        let qty = crate::training_workbench::floor_to_lot(
                            Decimal::from(1000) / close,
                            before.account.lot_size,
                        );
                        let fills_before = before.fills.len();
                        dispatch(cx, TrainingUiAction::QuickTrade { run_id: run_id.clone(), side: OrderSide::Buy, quantity: qty })?;
                        wait_workbench_idle_and_paint(cx, handle, &wb, hwnd, deadline).await?;
                        let after_buy = read(cx)?.workspace.unwrap().fills.len();
                        dispatch(cx, TrainingUiAction::UndoLast(run_id.clone()))?;
                        wait_workbench_idle_and_paint(cx, handle, &wb, hwnd, deadline).await?;
                        let after_undo = read(cx)?.workspace.unwrap().fills.len();
                        let quick_fill_then_undo =
                            after_buy == fills_before + 1 && after_undo == fills_before;

                        dispatch(
                            cx,
                            TrainingUiAction::SaveDrawing {
                                run_id: run_id.clone(),
                                drawing: TrainingDrawingDto {
                                    drawing_id: String::new(),
                                    kind: "horizontal".into(),
                                    anchors: vec![(1, close)],
                                    color: "#f0b90b".into(),
                                    line_style: "solid".into(),
                                },
                            },
                        )?;
                        wait_workbench_idle_and_paint(cx, handle, &wb, hwnd, deadline).await?;
                        let drawings_saved = read(cx)?.wb.drawings.len();

                        dispatch(cx, TrainingUiAction::FinishRun(run_id.clone()))?;
                        wait_workbench_idle_and_paint(cx, handle, &wb, hwnd, deadline).await?;
                        dispatch(cx, TrainingUiAction::RevealRun(run_id.clone()))?;
                        wait_workbench_idle_and_paint(cx, handle, &wb, hwnd, deadline).await?;
                        let t = read(cx)?;
                        let finished_status = t
                            .report
                            .as_ref()
                            .map(|r| format!("{:?}", r.run.status))
                            .unwrap_or_default();
                        let revealed_instrument = t
                            .reveal
                            .as_ref()
                            .map(|r| r.real_instrument_id.clone())
                            .unwrap_or_default();
                        let chart_paints = PAINTED_CHART_FRAMES.load(Ordering::Relaxed) - paint_start;
                        quiet_window_check()?;
                        let ok = played >= 1
                            && quick_fill_then_undo
                            && drawings_saved == 1
                            && chart_paints > 0
                            && revealed_instrument == DEFAULT_TRAINING_INSTRUMENT;
                        Ok(M5UiSmokeReport {
                            status: if ok { "passed" } else { "failed" }.into(),
                            mode: "m5-native-hidden-window-smoke".into(),
                            data_source: "loopback market fixture (synthetic, Binance/OKX-shaped); not a live service".into(),
                            executed_at: Utc::now().to_rfc3339(),
                            build_profile: if cfg!(debug_assertions) { "dev" } else { "release" }.into(),
                            native_window_created: true,
                            visible_windows: count_visible_windows_for_current_process(),
                            focus_stolen: false,
                            run_id: run_id.clone(),
                            timeframe: ws.run.timeframe.clone(),
                            practice_account: true,
                            played_steps: played,
                            quick_fill_then_undo,
                            drawings_saved,
                            chart_paints,
                            finished_status,
                            revealed_instrument,
                            elapsed_ms: started.elapsed().as_millis() as u64,
                            build_info: delta_app::build_info("delta-desktop"),
                        })
                    }
                    .await;
                    *slot.lock().unwrap() = Some(outcome);
                    let _ = cx.update_window(handle.into(), |_, window, cx| {
                        window.remove_window();
                        cx.quit();
                    });
                })
                .detach();
            });
        let _ = host.shutdown();
        drop(fixture);
        let taken = output.lock().unwrap().take();
        let rep = taken.ok_or_else(|| {
            ErrorEnvelope::new("M5_SMOKE_INTERRUPTED", "M5 原生窗口验证没有产生结果", false)
        })??;
        if rep.status != "passed" {
            return Err(ErrorEnvelope::new(
                "M5_SMOKE_FAILED",
                serde_json::to_string(&rep).unwrap_or_default(),
                false,
            ));
        }
        Ok(rep)
    })();
    owned.cleanup()?;
    inner
}

struct M5PlaybackBenchView {
    snapshot: TrainingWorkspaceSnapshotDto,
    wb: WorkbenchState,
}

impl Render for M5PlaybackBenchView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ws = &self.snapshot;
        let chart = KlineChartView::new(
            "m5-k02-bench-chart",
            ws.chart_slice.clone(),
            MarketColorConvention::RedUpGreenDown,
        )
        .with_masked_training_labels(ws.run.display_symbol.clone(), ws.bar_labels.clone())
        .with_crosshair(ws.viewport.visible_end.checked_sub(1))
        .with_overlay(workbench_overlay(ws, &self.wb, false));
        v_flex()
            .size_full()
            .p_3()
            .gap_2()
            .bg(cx.theme().background)
            .child(chart.render_chart(cx))
    }
}

/// `M5-K02`: release-only continuous-play latency on a `1h` practice run with 1,000 visible bars,
/// MA5/10/20/60, 20 drawings, 5 order lines and fill markers, playing at 150 ms/bar for 20 warmup
/// and 300 measured steps. Each step = atomic `advance_training_bar`, workspace snapshot, drawings,
/// practice account read, overlay, GPUI layout/paint and synchronous `WM_PAINT`; the interval is
/// measured between actual step starts scheduled on a 150 ms cadence (never queued).
pub fn run_m5_k02_playback_bench(
    cli_data_dir: Option<&Path>,
) -> Result<M5PlaybackBenchReport, ErrorEnvelope> {
    if cfg!(debug_assertions) {
        return Err(ErrorEnvelope::new(
            "RELEASE_BUILD_REQUIRED",
            "M5-K02 连续播放基准必须使用 release 构建执行",
            false,
        ));
    }
    delta_app::apply_process_quiet_guards();
    let power_throttling = delta_app::opt_out_of_execution_speed_throttling();
    let base = cli_data_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    let owned = VerificationDirectory::create(&base, "m5-k02-bench")?;
    let tmp_dir = owned.path.clone();

    const VISIBLE: usize = 1000;
    const WARMUP: usize = 20;
    const MEASURED: usize = 300;

    let inner = (|| -> Result<M5PlaybackBenchReport, ErrorEnvelope> {
        let (host, fixture) = prepared_host(
            &tmp_dir,
            vec![Timeframe::H1],
            NaiveDate::from_ymd_opt(2026, 3, 1).unwrap(),
        )?;
        let run = host.create_training_run(CreateTrainingRunRequest {
            market_category: TrainingMarketCategory::CryptoSpot,
            instrument_id: Some(DEFAULT_TRAINING_INSTRUMENT.to_string()),
            warmup_bars: 200,
            history_bars: VISIBLE,
            training_bars: WARMUP + MEASURED + 10,
            reveal_bars: 60,
            seed: Some(20261114),
            fee_profile_version_id: BUILTIN_FEE_CRYPTO_RATE_ID.to_string(),
            hidden_mode: true,
            create_and_start: true,
            timeframe: Timeframe::H1,
            use_practice_account: true,
            ..CreateTrainingRunRequest::default()
        })?;
        let run_id = run.run_id.clone();
        let snap0 = host.get_training_workspace_snapshot(&run_id, None, 1200)?;
        let close = snap0
            .chart_slice
            .bars
            .last()
            .map(|b| b.close)
            .unwrap_or_default();
        let lot = snap0.account.lot_size;
        // Position + fill marker, then 5 resting order lines far from the market.
        let qty = crate::training_workbench::floor_to_lot(Decimal::from(5_000) / close, lot);
        host.submit_training_order(&run_id, TrainingOrderRequestDto::quick(OrderSide::Buy, qty))?;
        let small = crate::training_workbench::floor_to_lot(qty / Decimal::from(4), lot);
        for (side, factor) in [
            (OrderSide::Buy, Decimal::new(5, 1)),
            (OrderSide::Buy, Decimal::new(6, 1)),
            (OrderSide::Sell, Decimal::from(3)),
            (OrderSide::Sell, Decimal::new(35, 1)),
            (OrderSide::Sell, Decimal::from(4)),
        ] {
            let price =
                crate::training_workbench::round_to_tick(close * factor, snap0.account.tick_size);
            host.submit_training_order(
                &run_id,
                TrainingOrderRequestDto::single(
                    side,
                    SimOrderType::Limit { limit_price: price },
                    small,
                    TimeInForce::Gtc,
                    None,
                ),
            )?;
        }
        for i in 0..20i64 {
            let kind = ["horizontal", "ray", "segment"][(i % 3) as usize];
            let p = close * (Decimal::ONE + Decimal::new(i - 10, 3));
            let anchors = if kind == "horizontal" {
                vec![(900 + i, p)]
            } else {
                vec![(800 + i * 5, p), (950 + i * 2, p * Decimal::new(1005, 3))]
            };
            host.save_training_drawing(
                &run_id,
                TrainingDrawingDto {
                    drawing_id: String::new(),
                    kind: kind.into(),
                    anchors,
                    color: "#1e88e5".into(),
                    line_style: "solid".into(),
                },
            )?;
        }
        let wb0 = WorkbenchState {
            drawings: host.list_training_drawings(&run_id)?,
            ..WorkbenchState::default()
        };
        let mut vp = snap0.viewport.clone();
        vp.indicator_config.sma_periods = vec![5, 10, 20, 60];
        let initial = host.get_training_workspace_snapshot(&run_id, Some(vp.clone()), 1200)?;

        let host_b = host.clone();
        let run_b = run_id.clone();
        let power_b = power_throttling.clone();
        let output: Arc<Mutex<Option<Result<M5PlaybackBenchReport, ErrorEnvelope>>>> =
            Arc::new(Mutex::new(None));
        let slot = Arc::clone(&output);

        gpui_kit::application()
            .with_assets(gpui_kit::assets::Assets)
            .run(move |cx| {
                gpui_kit::init(cx);
                crate::theme::apply_delta_theme(cx);
                let captured = Arc::new(Mutex::new(None));
                let entity_slot = Arc::clone(&captured);
                let opened = cx.open_window(hidden_options(), move |window, cx| {
                    let view = cx.new(|_| M5PlaybackBenchView { snapshot: initial, wb: wb0 });
                    *entity_slot.lock().unwrap() = Some(view.clone());
                    cx.new(|cx| Root::new(view, window, cx))
                });
                let handle = match opened {
                    Ok(h) => h,
                    Err(e) => {
                        *slot.lock().unwrap() = Some(Err(window_err(e)));
                        cx.quit();
                        return;
                    }
                };
                let hwnd = match native_window() {
                    Ok(h) => h,
                    Err(e) => {
                        *slot.lock().unwrap() = Some(Err(e));
                        cx.quit();
                        return;
                    }
                };
                cx.spawn(async move |cx| {
                    let outcome: Result<M5PlaybackBenchReport, ErrorEnvelope> = async {
                        initialize_native_window(hwnd)?;
                        let view = captured.lock().unwrap().take().unwrap();
                        cx.update_window(handle.into(), |_, window, _| {
                            quiet_window_check()?;
                            window.refresh();
                            Ok::<(), ErrorEnvelope>(())
                        })
                        .map_err(window_err)??;
                        paint_native_window(hwnd)?;
                        let mut step_us = Vec::with_capacity(MEASURED);
                        let mut interval_us = Vec::with_capacity(MEASURED);
                        let mut paint_start = 0;
                        let mut last_start: Option<Instant> = None;
                        let mut stats = (0usize, 0usize, 0usize, 0usize);
                        let mut phases: [Vec<u64>; 5] = Default::default();
                        for step in 1..=(WARMUP + MEASURED) {
                            if let Some(prev) = last_start {
                                let due = prev + Duration::from_millis(PLAY_INTERVAL_MS);
                                let now = Instant::now();
                                if due > now {
                                    cx.background_executor().timer(due - now).await;
                                }
                            }
                            if step == WARMUP + 1 {
                                paint_start = PAINTED_CHART_FRAMES.load(Ordering::Relaxed);
                            }
                            let t0 = Instant::now();
                            let receipt = host_b.advance_training_bar(&run_b)?;
                            let t_adv = Instant::now();
                            if receipt.run.status != SimRunStatus::Active {
                                return Err(ErrorEnvelope::new("M5_BENCH_RUN_ENDED", "基准局提前结束", false));
                            }
                            let total = receipt.run.visible_bar_count;
                            let mut vp = vp.clone();
                            vp.total_bars = total;
                            vp.visible_start = total.saturating_sub(VISIBLE);
                            vp.visible_end = total;
                            let snap = host_b.get_training_workspace_snapshot(&run_b, Some(vp), 1200)?;
                            let t_snap = Instant::now();
                            let drawings = host_b.list_training_drawings(&run_b)?;
                            let t_draw = Instant::now();
                            let _account = host_b.show_practice_account(&snap.run.quote_currency)?;
                            let t_acc = Instant::now();
                            stats = (
                                snap.chart_slice.bars.len(),
                                drawings.len(),
                                snap.open_orders.len(),
                                snap.fills.len(),
                            );
                            cx.update_window(handle.into(), |_, window, cx| {
                                view.update(cx, |v, cx| {
                                    v.snapshot = snap;
                                    v.wb.drawings = drawings;
                                    cx.notify();
                                });
                                window.refresh();
                            })
                            .map_err(window_err)?;
                            paint_native_window(hwnd)?;
                            let elapsed = t0.elapsed().as_micros() as u64;
                            if step > WARMUP {
                                let us = |a: Instant, b: Instant| b.duration_since(a).as_micros() as u64;
                                let end = Instant::now();
                                for (i, v) in [us(t0, t_adv), us(t_adv, t_snap), us(t_snap, t_draw), us(t_draw, t_acc), us(t_acc, end)].into_iter().enumerate() {
                                    phases[i].push(v);
                                }
                                step_us.push(elapsed);
                                if let Some(prev) = last_start {
                                    interval_us.push(t0.duration_since(prev).as_micros() as u64);
                                }
                            }
                            last_start = Some(t0);
                        }
                        let paints = PAINTED_CHART_FRAMES.load(Ordering::Relaxed) - paint_start;
                        let (s50, s95, s99) = percentiles(&step_us);
                        let (i50, i95, i99) = percentiles(&interval_us);
                        let ms = |us: u64| us as f64 / 1000.0;
                        let passed = ms(s95) <= M5_K02_STEP_P95_BUDGET_MS
                            && ms(i95) <= M5_K02_INTERVAL_P95_BUDGET_MS
                            && stats.0 == VISIBLE
                            && stats.1 == 20
                            && stats.2 == 5
                            && paints >= MEASURED;
                        let rep = cx
                            .update_window(handle.into(), |_, window, _| {
                                quiet_window_check()?;
                                Ok::<_, ErrorEnvelope>(M5PlaybackBenchReport {
                                    status: if passed { "passed" } else { "failed" }.into(),
                                    case_id: "M5-K02".into(),
                                    kpi_id: "M5-K02".into(),
                                    data_source: "loopback market fixture (synthetic 1h BTC/USDT); not a live service".into(),
                                    executed_at: Utc::now().to_rfc3339(),
                                    build_profile: "release".into(),
                                    power_throttling: power_b.clone(),
                                    hardware_logical_cores: std::thread::available_parallelism()
                                        .map(|n| n.get())
                                        .unwrap_or(1),
                                    cpu: std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "unknown".into()),
                                    gpu: format!("{:?}", window.gpu_specs()),
                                    window_size: format!("{:?}", window.bounds().size),
                                    visible_windows: count_visible_windows_for_current_process(),
                                    focus_stolen: false,
                                    timeframe: "1h".into(),
                                    visible_bars: stats.0,
                                    drawings: stats.1,
                                    order_lines: stats.2,
                                    fill_markers: stats.3,
                                    play_interval_ms: PLAY_INTERVAL_MS,
                                    warmup_steps: WARMUP,
                                    measured_steps: step_us.len(),
                                    native_chart_paints: paints,
                                    step_p50_ms: ms(s50),
                                    step_p95_ms: ms(s95),
                                    step_p99_ms: ms(s99),
                                    interval_p50_ms: ms(i50),
                                    interval_p95_ms: ms(i95),
                                    interval_p99_ms: ms(i99),
                                    step_budget_p95_ms: M5_K02_STEP_P95_BUDGET_MS,
                                    interval_budget_p95_ms: M5_K02_INTERVAL_P95_BUDGET_MS,
                                    passed,
                                    phase_p95_ms: ["advance", "snapshot", "drawings", "account", "render_paint"]
                                        .iter()
                                        .zip(&phases)
                                        .map(|(k, v)| (k.to_string(), ms(percentiles(v).1)))
                                        .collect(),
                                    step_times_us: step_us.clone(),
                                    interval_times_us: interval_us.clone(),
                                    measurement: "release; hidden non-activating HWND 1400x950; 1h practice run, 1000 visible bars, MA5/10/20/60 + MACD/RSI, 20 drawings, 5 order lines, cost line and fill markers; 20 warmup + 300 measured steps scheduled every 150 ms; step = advance_training_bar (atomic save + matching) + snapshot (indicators + slice) + drawings + practice account + overlay + GPUI layout/paint + synchronous WM_PAINT".into(),
                                    build_info: delta_app::build_info("delta-desktop"),
                                })
                            })
                            .map_err(window_err)??;
                        Ok(rep)
                    }
                    .await;
                    *slot.lock().unwrap() = Some(outcome);
                    let _ = cx.update_window(handle.into(), |_, window, cx| {
                        window.remove_window();
                        cx.quit();
                    });
                })
                .detach();
            });
        let _ = host.shutdown();
        drop(fixture);
        let taken = output.lock().unwrap().take();
        let rep = taken.ok_or_else(|| {
            ErrorEnvelope::new("M5_BENCH_INTERRUPTED", "M5-K02 基准没有产生结果", false)
        })??;
        Ok(rep)
    })();
    owned.cleanup()?;
    // The report is returned even when the budget is missed; the caller decides the exit code.
    inner
}

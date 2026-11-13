//! Explicit native-window validation. Windows stay hidden and never activate.

use std::{
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc, Mutex},
    time::{Duration, Instant},
};

use chrono::NaiveDate;
use delta_app::{
    count_visible_windows_for_current_process, to_canonical_text, AdjustmentKind, AppHost,
    BarRecord, CreateTrainingRunRequest, ErrorEnvelope, KlineViewport, KlineWindowSliceDto,
    OrderSide, SimOrderType, TimeInForce, TrainingMarketCategory, TrainingOrderRequestDto,
    TrainingWorkspaceSnapshotDto, BUILTIN_FEE_CRYPTO_RATE_ID, SIMULATED_TRAINING_BADGE,
};
use gpui_kit::component::{
    h_flex,
    table::{DataTable, TableState},
    tag::Tag,
    v_flex, ActiveTheme as _, Root, Sizable as _, StyledExt as _,
};
use gpui_kit::{
    div, point, px, size, AppContext as _, Bounds, Context, Entity, IntoElement,
    ParentElement as _, Render, Styled as _, Window, WindowBounds, WindowOptions,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::chart_canvas::PAINTED_CHART_FRAMES;
use crate::{
    controller::UiAction,
    theme::Chrome,
    training_ui::{write_synthetic_training_fixtures, TrainingUiAction},
    workbench::{
        verification_app_paths, DeltaWorkbench, TransactionsTableDelegate, UiBenchReport,
        VerificationDirectory, RENDERED_TABLE_CELLS,
    },
    KlineChartView, MarketColorConvention,
};

fn hidden_options() -> WindowOptions {
    WindowOptions {
        focus: false,
        show: false,
        inactive_frame_interval: None,
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            point(px(0.), px(0.)),
            size(px(1400.), px(950.)),
        ))),
        ..WindowOptions::default()
    }
}

fn quiet_window_check() -> Result<(), ErrorEnvelope> {
    if count_visible_windows_for_current_process() > 0 {
        return Err(ErrorEnvelope::new(
            "VISIBLE_VERIFICATION_WINDOW",
            "验证意外创建了可见窗口",
            false,
        ));
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetForegroundWindow, GetWindowThreadProcessId,
        };
        let mut pid = 0;
        GetWindowThreadProcessId(GetForegroundWindow(), &mut pid);
        if pid == std::process::id() {
            return Err(ErrorEnvelope::new(
                "VERIFICATION_FOCUS_STOLEN",
                "验证窗口意外取得了前台焦点",
                false,
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn native_window() -> Result<usize, ErrorEnvelope> {
    use windows_sys::Win32::{
        Foundation::{HWND, LPARAM, RECT},
        UI::WindowsAndMessaging::{EnumWindows, GetClientRect, GetWindowThreadProcessId},
    };
    unsafe extern "system" fn find(hwnd: HWND, data: LPARAM) -> i32 {
        let mut pid = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut pid);
        }
        let mut bounds: RECT = unsafe { std::mem::zeroed() };
        if pid == std::process::id()
            && unsafe { GetClientRect(hwnd, &mut bounds) } != 0
            && bounds.right - bounds.left >= 600
            && bounds.bottom - bounds.top >= 400
        {
            unsafe {
                *(data as *mut usize) = hwnd as usize;
            }
            return 0;
        }
        1
    }
    let mut hwnd = 0usize;
    unsafe {
        EnumWindows(Some(find), &mut hwnd as *mut usize as LPARAM);
    }
    if hwnd == 0 {
        return Err(ErrorEnvelope::new(
            "NATIVE_WINDOW_NOT_FOUND",
            "没有找到本次验证的隐藏窗口",
            false,
        ));
    }
    Ok(hwnd)
}

#[cfg(not(windows))]
fn native_window() -> Result<usize, ErrorEnvelope> {
    Err(ErrorEnvelope::new(
        "WINDOWS_VERIFICATION_REQUIRED",
        "原生帧基准需要 Windows",
        false,
    ))
}

fn paint_native_window(hwnd: usize) -> Result<(), ErrorEnvelope> {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_PAINT,
        };
        let mut message_result = 0;
        if SendMessageTimeoutW(
            hwnd as _,
            WM_PAINT,
            0,
            0,
            SMTO_ABORTIFHUNG,
            30_000,
            &mut message_result,
        ) == 0
        {
            return Err(ErrorEnvelope::new(
                "NATIVE_PAINT_FAILED",
                "隐藏窗口绘制未完成",
                false,
            ));
        }
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
    }
    quiet_window_check()
}

fn initialize_native_window(hwnd: usize) -> Result<(), ErrorEnvelope> {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SetWindowPos, SWP_NOACTIVATE, SWP_NOOWNERZORDER, SWP_NOZORDER,
        };
        // Hidden HWNDs do not receive the normal first-show size notification. Explicitly
        // initialize their client bounds without showing or activating the window.
        if SetWindowPos(
            hwnd as _,
            std::ptr::null_mut(),
            0,
            0,
            1400,
            950,
            SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOOWNERZORDER,
        ) == 0
        {
            return Err(ErrorEnvelope::new(
                "NATIVE_WINDOW_SIZE",
                "无法初始化隐藏窗口尺寸",
                false,
            ));
        }
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
    }
    quiet_window_check()
}

pub(crate) fn run_native_smoke(
    host: AppHost,
    data_dir: PathBuf,
    entry_started: Instant,
    navigation_cycles: usize,
) -> Result<Value, ErrorEnvelope> {
    let output = Arc::new(Mutex::new(None));
    let result = Arc::clone(&output);
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
        gpui_kit::init(cx);
        crate::theme::apply_delta_theme(cx);
        let wb = Arc::new(Mutex::new(None));
        let captured = Arc::clone(&wb);
        let opened = cx.open_window(hidden_options(), move |window, cx| {
            let view = cx.new(|cx| DeltaWorkbench::new(host, data_dir, NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(), window, cx));
            *captured.lock().unwrap() = Some(view.clone());
            cx.new(|cx| Root::new(view, window, cx))
        });
        let handle = match opened { Ok(handle) => handle, Err(error) => {
            *result.lock().unwrap() = Some(Err(ErrorEnvelope::new("NATIVE_WINDOW_FAILED", error.to_string(), false))); cx.quit(); return;
        }};
        let hwnd = match native_window() { Ok(hwnd) => hwnd, Err(error) => { *result.lock().unwrap() = Some(Err(error)); cx.quit(); return; } };
        cx.spawn(async move |cx| {
            if let Err(error) = initialize_native_window(hwnd) {
                *result.lock().unwrap() = Some(Err(error)); cx.update(|cx| cx.quit()); return;
            }
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut pages = 0;
            let mut first_screen_ms = None;
            let mut navigation_started: Option<Instant> = None;
            let mut navigation_times_us = Vec::new();
            const PAGES: [&str; 6] = ["nav.overview", "nav.market", "nav.assets", "nav.journal", "nav.data_center", "nav.settings"];
            loop {
                cx.background_executor().timer(Duration::from_millis(20)).await;
                let check = cx.update_window(handle.into(), |_, window, cx| {
                    quiet_window_check()?;
                    let view = wb.lock().unwrap().as_ref().unwrap().clone();
                    let ready = view.update(cx, |view, cx| {
                        !view.controller.read(cx).busy && view.state.lock().unwrap().overview.is_some()
                    });
                    if Instant::now() >= deadline { return Err(ErrorEnvelope::new("SMOKE_TIMEOUT", "工作台未在期限内完成初始化", false)); }
                    window.refresh();
                    Ok(ready)
                });
                let ready = match check {
                    Ok(Ok(ready)) => ready,
                    Ok(Err(error)) => { *result.lock().unwrap() = Some(Err(error)); break; }
                    Err(error) => { *result.lock().unwrap() = Some(Err(ErrorEnvelope::new("NATIVE_WINDOW_FAILED", error.to_string(), false))); break; }
                };
                if let Err(error) = paint_native_window(hwnd) { *result.lock().unwrap() = Some(Err(error)); break; }
                if ready {
                    first_screen_ms.get_or_insert_with(|| entry_started.elapsed().as_millis() as u64);
                    if let Some(started) = navigation_started.take() {
                        navigation_times_us.push(started.elapsed().as_micros() as u64);
                    }
                    if pages >= PAGES.len() * navigation_cycles {
                        let value = cx.update_window(handle.into(), |_, window, cx| {
                            let view = wb.lock().unwrap().as_ref().unwrap().clone();
                            let nav = view.read(cx).state.lock().unwrap().overview.as_ref().unwrap().value.net_worth;
                            if delta_app::to_canonical_text(nav) != "2068" { return Err(ErrorEnvelope::new("SMOKE_NAV", "AC-02 NAV 不符合手算期望", false)); }
                            Ok(json!({"status":"passed","mode":"native-hidden-window-smoke", "native_window_created":true,"rendered_pages":PAGES.len(),
                                "navigation_actions":pages,"first_screen_ms":first_screen_ms,"navigation_times_us":navigation_times_us,
                                "overview_nav":"2068","visible_windows":0,"focus_stolen":false,"window_size":format!("{:?}",window.bounds().size),
                                "gpu":format!("{:?}",window.gpu_specs()),"msvc_stack_bytes":8_388_608,"scale_factor":window.scale_factor(),
                                "first_screen_fixture":"AC-02 local library; Rust entry through host load and first native paint, including synthetic fixture setup; excludes OS executable loader",
                                "interaction_measurement":"navigation dispatch through settings persistence, controller completion and native paint; includes the 20 ms observer interval"}))
                        });
                        *result.lock().unwrap() = Some(value.map_err(|error| ErrorEnvelope::new("SMOKE_WINDOW", error.to_string(), false)).and_then(|value| value));
                        break;
                    }
                    navigation_started = Some(Instant::now());
                    if let Err(error) = cx.update_window(handle.into(), |_, window, cx| {
                        let view = wb.lock().unwrap().as_ref().unwrap().clone();
                        view.update(cx, |view, cx| view.controller.update(cx, |controller, cx| controller.navigate(PAGES[pages % PAGES.len()], window, cx)));
                    }) { *result.lock().unwrap() = Some(Err(ErrorEnvelope::new("SMOKE_WINDOW", error.to_string(), false))); break; }
                    pages += 1;
                }
            }
            let _ = cx.update_window(handle.into(), |_, window, cx| { window.remove_window(); cx.quit(); });
        }).detach();
    });
    let result = output.lock().unwrap().take().ok_or_else(|| {
        ErrorEnvelope::new("SMOKE_INTERRUPTED", "原生窗口验证没有产生结果", false)
    })?;
    result
}

struct NativeBenchmark {
    bars: Vec<BarRecord>,
    viewport: KlineViewport,
    slice: KlineWindowSliceDto,
    table: Entity<TableState<TransactionsTableDelegate>>,
    table_phase: bool,
    frames: usize,
    chart_intervals: Vec<u64>,
    table_intervals: Vec<u64>,
    chart_updates: Vec<u64>,
    chart_paint_start: usize,
    table_cells_start: usize,
}

impl NativeBenchmark {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let bars = delta_app::kline::synthetic_benchmark_bars(100_000);
        let viewport = KlineViewport::new(bars.len(), 1000);
        let slice = viewport.build_window_slice(
            "NASDAQ:AAPL",
            "XNAS",
            "1d",
            AdjustmentKind::Raw,
            &bars,
            None,
            &[],
            &[],
            1200,
        );
        let table = cx.new(|cx| {
            TableState::new(
                TransactionsTableDelegate::new(TransactionsTableDelegate::synthetic_100k_rows()),
                window,
                cx,
            )
        });
        Self {
            bars,
            viewport,
            slice,
            table,
            table_phase: false,
            frames: 0,
            chart_intervals: vec![],
            table_intervals: vec![],
            chart_updates: vec![],
            chart_paint_start: PAINTED_CHART_FRAMES.load(Ordering::Relaxed),
            table_cells_start: RENDERED_TABLE_CELLS.load(Ordering::Relaxed),
        }
    }

    fn next_frame(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<Option<UiBenchReport>, ErrorEnvelope> {
        quiet_window_check()?;
        if window.bounds().size.width <= px(0.) || window.bounds().size.height <= px(0.) {
            return Err(ErrorEnvelope::new(
                "NATIVE_WINDOW_SIZE",
                "隐藏窗口尚无有效的绘制尺寸",
                false,
            ));
        }
        self.frames += 1;
        if self.frames > 320 {
            if self.table_phase {
                let report = self.report(window);
                if report.native_chart_paints < 300 || report.native_table_cells < 300 * 8 {
                    return Err(ErrorEnvelope::new(
                        "NATIVE_RENDER_INCOMPLETE",
                        "实际图表或表格绘制次数不足，不能作为性能证据",
                        false,
                    ));
                }
                return Ok(Some(report));
            }
            self.table_phase = true;
            self.frames = 1;
        }
        let start = Instant::now();
        if self.table_phase {
            self.table.update(cx, |table, cx| {
                table.scroll_to_row((self.frames * 331) % 99_950, cx)
            });
        } else {
            self.viewport
                .pan(if self.frames % 80 < 40 { -11 } else { 11 });
            self.viewport.zoom(
                if self.frames.is_multiple_of(2) {
                    1.001
                } else {
                    0.999
                },
                0.5,
            );
            self.slice = self.viewport.build_window_slice(
                "NASDAQ:AAPL",
                "XNAS",
                "1d",
                AdjustmentKind::Raw,
                &self.bars,
                None,
                &[],
                &[],
                1200,
            );
            if self.frames > 20 {
                self.chart_updates.push(start.elapsed().as_micros() as u64);
            }
        }
        window.refresh();
        cx.notify();
        Ok(None)
    }

    fn record_frame_time(&mut self, duration: Duration) {
        if self.frames > 20 {
            let times = if self.table_phase {
                &mut self.table_intervals
            } else {
                &mut self.chart_intervals
            };
            times.push(duration.as_micros() as u64);
        }
    }

    fn report(&self, window: &Window) -> UiBenchReport {
        let (chart_p50, chart_p95, chart_p99) = percentiles(&self.chart_intervals);
        let (table_p50, table_p95, table_p99) = percentiles(&self.table_intervals);
        UiBenchReport { executed_at: chrono::Utc::now().to_rfc3339(), hardware_logical_cores: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
            transaction_rows: 100_000, table_scroll_frames: self.table_intervals.len(), table_scroll_p50_us: table_p50, table_scroll_p95_us: table_p95,
            kline_total_bars: self.bars.len(), kline_visible_bars: self.viewport.visible_len(), kline_frames: self.chart_intervals.len(),
            kline_p50_us: chart_p50, kline_p95_us: chart_p95, kline_p99_us: chart_p99, kline_equivalent_fps_at_p95: 1_000_000. / chart_p95.max(1) as f64,
            cancel_ui_latency_ms: 0, cancel_backend_latency_ms: 0, table_scroll_p99_us: table_p99,
            chart_frame_times_us: self.chart_intervals.clone(), table_frame_times_us: self.table_intervals.clone(), chart_update_times_us: self.chart_updates.clone(),
            native_chart_paints: PAINTED_CHART_FRAMES.load(Ordering::Relaxed) - self.chart_paint_start,
            native_table_cells: RENDERED_TABLE_CELLS.load(Ordering::Relaxed) - self.table_cells_start,
            native_window_created: true, focus_stolen: false, visible_windows: 0, build_profile: if cfg!(debug_assertions) { "dev" } else { "release" }.into(),
            cpu: std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "unknown".into()), gpu: format!("{:?}", window.gpu_specs()),
            window_size: format!("{:?}", window.bounds().size), scale_factor: window.scale_factor(),
            first_screen_times_ms: vec![], first_screen_p50_ms: 0, first_screen_p95_ms: 0, first_screen_p99_ms: 0,
            interaction_times_us: vec![], interaction_p50_us: 0, interaction_p95_us: 0, interaction_p99_us: 0,
            workbench_measurement: String::new(), power_throttling: String::new(),
            measurement: "native hidden Windows window: scripted update through synchronous WM_PAINT, including GPUI layout/paint and DirectX render/presentation; excludes idle timer; 20 warm-up frames per component; FPS is the reciprocal frame budget".into() }
    }
}

impl Render for NativeBenchmark {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = div().size_full().p_3().bg(cx.theme().background);
        if self.table_phase {
            root = root.child(DataTable::new(&self.table).bordered(true));
        } else {
            root = root.child(
                KlineChartView::new(
                    "native-benchmark-chart",
                    self.slice.clone(),
                    MarketColorConvention::RedUpGreenDown,
                )
                .render_chart(cx),
            );
        }
        root
    }
}

pub(crate) fn percentiles(times: &[u64]) -> (u64, u64, u64) {
    let mut sorted = times.to_vec();
    sorted.sort_unstable();
    let percentile = |percent: usize| {
        sorted
            .get((sorted.len() * percent).div_ceil(100).saturating_sub(1))
            .copied()
            .unwrap_or(0)
    };
    (percentile(50), percentile(95), percentile(99))
}

pub(crate) fn run_native_benchmark() -> Result<UiBenchReport, ErrorEnvelope> {
    let output = Arc::new(Mutex::new(None));
    let result = Arc::clone(&output);
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            crate::theme::apply_delta_theme(cx);
            let captured = Arc::new(Mutex::new(None));
            let entity = Arc::clone(&captured);
            let opened = cx.open_window(hidden_options(), move |window, cx| {
                let view = cx.new(|cx| NativeBenchmark::new(window, cx));
                *entity.lock().unwrap() = Some(view.clone());
                cx.new(|cx| Root::new(view, window, cx))
            });
            let handle = match opened {
                Ok(handle) => handle,
                Err(error) => {
                    *result.lock().unwrap() = Some(Err(ErrorEnvelope::new(
                        "NATIVE_WINDOW_FAILED",
                        error.to_string(),
                        false,
                    )));
                    cx.quit();
                    return;
                }
            };
            let hwnd = match native_window() {
                Ok(hwnd) => hwnd,
                Err(error) => {
                    *result.lock().unwrap() = Some(Err(error));
                    cx.quit();
                    return;
                }
            };
            cx.spawn(async move |cx| {
                if let Err(error) = initialize_native_window(hwnd) {
                    *result.lock().unwrap() = Some(Err(error));
                    cx.update(|cx| cx.quit());
                    return;
                }
                let deadline = Instant::now() + Duration::from_secs(90);
                let view = captured.lock().unwrap().take().unwrap();
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(1))
                        .await;
                    let start = Instant::now();
                    let update = cx.update_window(handle.into(), |_, window, cx| {
                        view.update(cx, |view, cx| view.next_frame(window, cx))
                    });
                    match update {
                        Ok(Ok(Some(report))) => {
                            *result.lock().unwrap() = Some(Ok(report));
                            break;
                        }
                        Ok(Err(error)) => {
                            *result.lock().unwrap() = Some(Err(error));
                            break;
                        }
                        Err(error) => {
                            *result.lock().unwrap() = Some(Err(ErrorEnvelope::new(
                                "NATIVE_WINDOW_FAILED",
                                error.to_string(),
                                false,
                            )));
                            break;
                        }
                        Ok(Ok(None)) => {}
                    }
                    if let Err(error) = paint_native_window(hwnd) {
                        *result.lock().unwrap() = Some(Err(error));
                        break;
                    }
                    let elapsed = start.elapsed();
                    view.update(cx, |view, _| view.record_frame_time(elapsed));
                    if Instant::now() >= deadline {
                        *result.lock().unwrap() = Some(Err(ErrorEnvelope::new(
                            "BENCH_TIMEOUT",
                            "原生窗口未完成规定帧数",
                            false,
                        )));
                        break;
                    }
                }
                let _ = cx.update_window(handle.into(), |_, window, cx| {
                    window.remove_window();
                    cx.quit();
                });
            })
            .detach();
        });
    let result = output.lock().unwrap().take().ok_or_else(|| {
        ErrorEnvelope::new("BENCH_INTERRUPTED", "原生窗口验证没有产生结果", false)
    })?;
    result
}

/// Gate-only resident options (`M3-A-02`, `M3-A-04`, `M3-A-08`). The resident app runs the real
/// workbench in a hidden, never-activated window with the real control pipe and updater.
pub(crate) struct ResidentOptions {
    pub seed: bool,
    pub pending_note: Option<String>,
    pub busy_sync_ms: Option<u64>,
    pub apply_update: bool,
    pub ignore_control: bool,
    pub max_seconds: u64,
}

pub(crate) const RESIDENT_SEED_NOTE: &str = "note:m3-seed";
pub(crate) const RESIDENT_PENDING_NOTE_TITLE: &str = "M3 pending note";

fn seed_resident_library(host: &AppHost) -> Result<(), ErrorEnvelope> {
    if host
        .list_notes(false)?
        .iter()
        .any(|note| note.id == RESIDENT_SEED_NOTE)
    {
        return Ok(());
    }
    host.save_note_revision(
        RESIDENT_SEED_NOTE,
        "M3 seed note",
        "post_trade",
        false,
        "Upgrade must keep this note.",
        delta_app::StructuredNoteFields::default(),
        vec!["m3".into()],
    )?;
    host.add_note_attachment(
        RESIDENT_SEED_NOTE,
        "m3-attachment.txt",
        "text/plain",
        b"M3 attachment bytes".to_vec(),
    )?;
    Ok(())
}

pub(crate) fn run_native_resident(
    paths: delta_app::ResolvedAppPaths,
    options: ResidentOptions,
) -> Result<(), ErrorEnvelope> {
    use crate::lifecycle::{emit_event, update_state, UpdateStatus, APPLY_REQUESTED};
    let data_dir = paths.data_dir.clone();
    let host = AppHost::from_service(delta_app::AppService::open(
        paths, "USD", "USD", "UTC", true,
    )?);
    if options.seed {
        if host.list_accounts()?.is_empty() {
            host.execute(|service| delta_app::seed_ac02_fixture_in_service(service).map(|_| ()))?;
        }
        seed_resident_library(&host)?;
    }
    crate::lifecycle::IGNORE_CONTROL.store(options.ignore_control, Ordering::SeqCst);
    crate::lifecycle::initialize_updates(data_dir.clone(), options.apply_update);
    let failure = Arc::new(Mutex::new(None));
    let failed = Arc::clone(&failure);
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            crate::theme::apply_delta_theme(cx);
            let wb = Arc::new(Mutex::new(None));
            let captured = Arc::clone(&wb);
            let as_of = chrono::Utc::now().date_naive();
            let opened = cx.open_window(hidden_options(), move |window, cx| {
                let view = cx.new(|cx| DeltaWorkbench::new(host, data_dir, as_of, window, cx));
                *captured.lock().unwrap() = Some(view.clone());
                cx.new(|cx| Root::new(view, window, cx))
            });
            let handle = match opened {
                Ok(handle) => handle,
                Err(error) => {
                    *failed.lock().unwrap() = Some(ErrorEnvelope::new("NATIVE_WINDOW_FAILED", error.to_string(), false));
                    cx.quit();
                    return;
                }
            };
            let hwnd = match native_window() {
                Ok(hwnd) => hwnd,
                Err(error) => {
                    *failed.lock().unwrap() = Some(error);
                    cx.quit();
                    return;
                }
            };
            cx.spawn(async move |cx| {
                let outcome: Result<(), ErrorEnvelope> = async {
                    initialize_native_window(hwnd)?;
                    let started = Instant::now();
                    let deadline = started + Duration::from_secs(options.max_seconds);
                    let mut ready = false;
                    let mut apply_sent = false;
                    let mut busy_prompted = false;
                    loop {
                        cx.background_executor().timer(Duration::from_millis(100)).await;
                        if Instant::now() >= deadline {
                            emit_event("resident-timeout", json!({}));
                            break;
                        }
                        let view = wb.lock().unwrap().as_ref().unwrap().clone();
                        let (idle, has_job) = cx
                            .update_window(handle.into(), |_, window, cx| {
                                window.refresh();
                                view.update(cx, |view, cx| {
                                    let state = view.state.lock().unwrap();
                                    (
                                        !view.controller.read(cx).busy && state.overview.is_some(),
                                        state.active_sync_job.is_some(),
                                    )
                                })
                            })
                            .map_err(|e| ErrorEnvelope::new("NATIVE_WINDOW_FAILED", e.to_string(), false))?;
                        paint_native_window(hwnd)?;
                        if !ready {
                            if !idle {
                                continue;
                            }
                            ready = true;
                            cx.update_window(handle.into(), |_, _, cx| {
                                view.update(cx, |view, _| {
                                    let mut state = view.state.lock().unwrap();
                                    if let Some(text) = &options.pending_note {
                                        // An edit whose autosave has not run yet.
                                        state.note_editor.title_buffer = RESIDENT_PENDING_NOTE_TITLE.into();
                                        state.note_editor.body_buffer = text.clone();
                                        state.note_editor.status = delta_app::AutosaveStatus::Editing;
                                    }
                                    if let Some(ms) = options.busy_sync_ms {
                                        state.start_verification_cancellation_job(ms)?;
                                    }
                                    Ok::<_, ErrorEnvelope>(())
                                })
                            })
                            .map_err(|e| ErrorEnvelope::new("NATIVE_WINDOW_FAILED", e.to_string(), false))??;
                            emit_event(
                                "resident-ready",
                                json!({"process_id": std::process::id(), "build_info": delta_app::build_info("delta-desktop"),
                                    "pending_note": options.pending_note.is_some(), "busy_sync": options.busy_sync_ms.is_some()}),
                            );
                        }
                        if options.apply_update {
                            match update_state().map(|state| state.status) {
                                Some(UpdateStatus::Ready { .. }) if !apply_sent => {
                                    apply_sent = true;
                                    APPLY_REQUESTED.store(true, Ordering::SeqCst);
                                }
                                Some(UpdateStatus::BusyConfirm { .. }) => {
                                    // The prompt is shown and nothing happens without
                                    // confirmation; once the work has finished the
                                    // simulated user asks again.
                                    busy_prompted = true;
                                    if !has_job {
                                        emit_event("update-retry-when-idle", json!({}));
                                        crate::lifecycle::set_update_status(UpdateStatus::Ready {
                                            version: update_state()
                                                .and_then(|s| s.offer.map(|o| o.manifest.version))
                                                .unwrap_or_default(),
                                        });
                                        APPLY_REQUESTED.store(true, Ordering::SeqCst);
                                    }
                                }
                                _ => {}
                            }
                        }
                        let _ = busy_prompted;
                    }
                    Ok(())
                }
                .await;
                if let Err(error) = outcome {
                    *failed.lock().unwrap() = Some(error);
                }
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    let outcome = failure.lock().unwrap().take();
    match outcome {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

// ============================================================================
// M4 Hidden Native-Window Smoke & M4-K03 / M4-A-17 Next-Bar Latency Benchmark
// ============================================================================

/// M4-K03 / M4-A-17 budget: p95 end-to-end `"下一根"` latency <= 100.0 ms.
pub const M4_K03_P95_BUDGET_MS: f64 = 100.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct M4UiSmokeReport {
    pub status: String,
    pub mode: String,
    pub executed_at: String,
    pub build_profile: String,
    pub power_throttling: String,
    pub native_window_created: bool,
    pub visible_windows: usize,
    pub focus_stolen: bool,
    pub window_size: String,
    pub scale_factor: f32,
    pub gpu: String,
    pub run_id: String,
    pub simulated_badge: String,
    pub workspace_chart_paints: usize,
    pub total_chart_paints: usize,
    pub revealed_post_finish_bars: usize,
    pub elapsed_ms: u64,
    pub build_info: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct M4NextBarBenchReport {
    pub status: String,
    pub case_id: String,
    pub kpi_id: String,
    pub executed_at: String,
    pub build_profile: String,
    pub power_throttling: String,
    pub hardware_logical_cores: usize,
    pub cpu: String,
    pub gpu: String,
    pub window_size: String,
    pub scale_factor: f32,
    pub native_window_created: bool,
    pub visible_windows: usize,
    pub focus_stolen: bool,
    pub history_bars: usize,
    pub kline_visible_bars: usize,
    pub warmup_steps: usize,
    pub measured_steps: usize,
    pub native_chart_paints: usize,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub budget_p95_ms: f64,
    pub passed: bool,
    pub step_times_ms: Vec<f64>,
    pub step_times_us: Vec<u64>,
    pub measurement: String,
    pub build_info: Value,
}

async fn wait_workbench_idle_and_paint(
    cx: &mut gpui_kit::AsyncApp,
    handle: gpui_kit::WindowHandle<Root>,
    wb: &Arc<Mutex<Option<Entity<DeltaWorkbench>>>>,
    hwnd: usize,
    deadline: Instant,
) -> Result<(), ErrorEnvelope> {
    loop {
        cx.background_executor()
            .timer(Duration::from_millis(15))
            .await;
        let idle = cx
            .update_window(handle.into(), |_, window, cx| {
                quiet_window_check()?;
                window.refresh();
                let view = wb.lock().unwrap().as_ref().unwrap().clone();
                Ok::<bool, ErrorEnvelope>(view.read(cx).controller.read(cx).is_idle())
            })
            .map_err(|e| ErrorEnvelope::new("SMOKE_WINDOW", e.to_string(), false))??;
        paint_native_window(hwnd)?;
        if idle {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(ErrorEnvelope::new(
                "SMOKE_TIMEOUT",
                "M4 隐藏窗口工作台未在期限内空闲",
                false,
            ));
        }
    }
}

/// Runs an end-to-end M4 hidden native-window smoke check:
/// creates a hidden-mode training run, opens `DeltaWorkbench` in a non-activating hidden HWND,
/// navigates to `"nav.training"`, paints the active workspace K-line chart + order ticket,
/// advances 1 bar, finishes the run, renders the unrevealed report, and triggers `"揭示"` + 60-bar
/// post-finish chart paint with zero visible windows and zero focus theft.
pub fn run_m4_ui_smoke(cli_data_dir: Option<&Path>) -> Result<M4UiSmokeReport, ErrorEnvelope> {
    let started = Instant::now();
    delta_app::apply_process_quiet_guards();
    let power_throttling = delta_app::opt_out_of_execution_speed_throttling();
    let base = cli_data_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    let owned = VerificationDirectory::create(&base, "m4-smoke")?;
    let tmp_dir = owned.path.clone();

    let inner_res = (|| -> Result<M4UiSmokeReport, ErrorEnvelope> {
        let fix_dir = tmp_dir.join("fixtures");
        write_synthetic_training_fixtures(&fix_dir, 600)?;
        let paths = verification_app_paths(&tmp_dir);
        let mut svc = delta_app::AppService::open(paths, "USD", "USD", "UTC", true)?;
        svc.sync_from_fixtures(&fix_dir)?;
        let host = AppHost::from_service(svc);

        let created = host.create_training_run(CreateTrainingRunRequest {
            run_id: None,
            market_category: TrainingMarketCategory::CryptoSpot,
            instrument_id: Some("OKX:BTC/USDT".to_string()),
            start_date: None,
            end_date: None,
            warmup_bars: 200,
            history_bars: 120,
            training_bars: 50,
            reveal_bars: 60,
            seed: Some(20260401),
            dataset_version: None,
            initial_cash: Decimal::from(100_000),
            fee_profile_version_id: BUILTIN_FEE_CRYPTO_RATE_ID.to_string(),
            hidden_mode: true,
            create_and_start: true,
        })?;
        let run_id = created.run_id.clone();

        let host_for_ui = host.clone();
        let data_dir_for_ui = tmp_dir.clone();
        let run_id_for_ui = run_id.clone();
        let power_for_ui = power_throttling.clone();
        let output: Arc<Mutex<Option<Result<M4UiSmokeReport, ErrorEnvelope>>>> =
            Arc::new(Mutex::new(None));
        let result_slot = Arc::clone(&output);

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
                            data_dir_for_ui,
                            NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(),
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
                        *result_slot.lock().unwrap() = Some(Err(ErrorEnvelope::new(
                            "NATIVE_WINDOW_FAILED",
                            e.to_string(),
                            false,
                        )));
                        cx.quit();
                        return;
                    }
                };
                let hwnd = match native_window() {
                    Ok(h) => h,
                    Err(e) => {
                        *result_slot.lock().unwrap() = Some(Err(e));
                        cx.quit();
                        return;
                    }
                };
                cx.spawn(async move |cx| {
                    let outcome: Result<M4UiSmokeReport, ErrorEnvelope> = async {
                        initialize_native_window(hwnd)?;
                        let deadline = Instant::now() + Duration::from_secs(45);
                        let paint_start = PAINTED_CHART_FRAMES.load(Ordering::Relaxed);

                        // 1. Wait initial load, navigate to `nav.training`, and open active workspace
                        wait_workbench_idle_and_paint(cx, handle, &wb, hwnd, deadline).await?;
                        cx.update_window(handle.into(), |_, window, cx| {
                            let view = wb.lock().unwrap().as_ref().unwrap().clone();
                            view.update(cx, |view, cx| {
                                view.controller.update(cx, |ctl, cx| {
                                    ctl.navigate("nav.training", window, cx);
                                    ctl.dispatch(
                                        UiAction::Training(Box::new(
                                            TrainingUiAction::ResumeOrOpenRun(
                                                run_id_for_ui.clone(),
                                            ),
                                        )),
                                        window,
                                        cx,
                                    );
                                });
                            });
                        })
                        .map_err(|e| ErrorEnvelope::new("SMOKE_WINDOW", e.to_string(), false))?;
                        wait_workbench_idle_and_paint(cx, handle, &wb, hwnd, deadline).await?;
                        // Extra paint so chart canvas commits bounds & paints
                        cx.update_window(handle.into(), |_, window, _| window.refresh())
                            .map_err(|e| {
                                ErrorEnvelope::new("SMOKE_WINDOW", e.to_string(), false)
                            })?;
                        paint_native_window(hwnd)?;
                        let workspace_chart_paints =
                            PAINTED_CHART_FRAMES.load(Ordering::Relaxed) - paint_start;
                        if workspace_chart_paints == 0 {
                            return Err(ErrorEnvelope::new(
                                "M4_SMOKE_CHART_NOT_PAINTED",
                                "模拟训练工作台 K 线未产生原生绘制帧",
                                false,
                            ));
                        }

                        // 2. Advance 1 bar, finish, and reveal
                        for act in [
                            TrainingUiAction::AdvanceBar(run_id_for_ui.clone()),
                            TrainingUiAction::FinishRun(run_id_for_ui.clone()),
                            TrainingUiAction::RevealRun(run_id_for_ui.clone()),
                        ] {
                            cx.update_window(handle.into(), |_, window, cx| {
                                let view = wb.lock().unwrap().as_ref().unwrap().clone();
                                view.update(cx, |view, cx| {
                                    view.controller.update(cx, |ctl, cx| {
                                        ctl.dispatch(UiAction::Training(Box::new(act)), window, cx);
                                    });
                                });
                            })
                            .map_err(|e| {
                                ErrorEnvelope::new("SMOKE_WINDOW", e.to_string(), false)
                            })?;
                            wait_workbench_idle_and_paint(cx, handle, &wb, hwnd, deadline).await?;
                        }
                        cx.update_window(handle.into(), |_, window, _| window.refresh())
                            .map_err(|e| {
                                ErrorEnvelope::new("SMOKE_WINDOW", e.to_string(), false)
                            })?;
                        paint_native_window(hwnd)?;
                        let total_chart_paints =
                            PAINTED_CHART_FRAMES.load(Ordering::Relaxed) - paint_start;

                        let rep = cx
                            .update_window(handle.into(), |_, window, cx| {
                                quiet_window_check()?;
                                let view = wb.lock().unwrap().as_ref().unwrap().clone();
                                let revealed_bars = view
                                    .read(cx)
                                    .state
                                    .lock()
                                    .unwrap()
                                    .training
                                    .reveal
                                    .as_ref()
                                    .map(|r| r.post_finish_bars.len())
                                    .unwrap_or(0);
                                if revealed_bars != 60 {
                                    return Err(ErrorEnvelope::new(
                                        "M4_SMOKE_REVEAL_BARS",
                                        format!("揭示后续 K 线数量异常: {revealed_bars}"),
                                        false,
                                    ));
                                }
                                Ok(M4UiSmokeReport {
                                    status: "passed".to_string(),
                                    mode: "m4-native-hidden-window-smoke".to_string(),
                                    executed_at: chrono::Utc::now().to_rfc3339(),
                                    build_profile: if cfg!(debug_assertions) {
                                        "dev"
                                    } else {
                                        "release"
                                    }
                                    .to_string(),
                                    power_throttling: power_for_ui.clone(),
                                    native_window_created: true,
                                    visible_windows: count_visible_windows_for_current_process(),
                                    focus_stolen: false,
                                    window_size: format!("{:?}", window.bounds().size),
                                    scale_factor: window.scale_factor(),
                                    gpu: format!("{:?}", window.gpu_specs()),
                                    run_id: run_id_for_ui.clone(),
                                    simulated_badge: SIMULATED_TRAINING_BADGE.to_string(),
                                    workspace_chart_paints,
                                    total_chart_paints,
                                    revealed_post_finish_bars: revealed_bars,
                                    elapsed_ms: started.elapsed().as_millis() as u64,
                                    build_info: delta_app::build_info("delta-desktop"),
                                })
                            })
                            .map_err(|e| {
                                ErrorEnvelope::new("SMOKE_WINDOW", e.to_string(), false)
                            })??;
                        Ok(rep)
                    }
                    .await;
                    *result_slot.lock().unwrap() = Some(outcome);
                    let _ = cx.update_window(handle.into(), |_, window, cx| {
                        window.remove_window();
                        cx.quit();
                    });
                })
                .detach();
            });

        let _ = host.shutdown();
        let taken = output.lock().unwrap().take();
        taken.ok_or_else(|| {
            ErrorEnvelope::new("M4_SMOKE_INTERRUPTED", "M4 原生窗口验证没有产生结果", false)
        })?
    })();

    owned.cleanup()?;
    inner_res
}

struct M4NextBarBenchView {
    snapshot: TrainingWorkspaceSnapshotDto,
}

impl Render for M4NextBarBenchView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ws = &self.snapshot;
        let ccy = &ws.account.quote_currency;
        let chart = KlineChartView::new(
            "m4-k03-bench-chart",
            ws.chart_slice.clone(),
            MarketColorConvention::RedUpGreenDown,
        )
        .with_masked_training_labels(ws.run.display_symbol.clone(), ws.bar_labels.clone())
        .with_crosshair(
            ws.viewport
                .crosshair_index
                .or_else(|| ws.viewport.visible_end.checked_sub(1)),
        );

        v_flex()
            .size_full()
            .p_3()
            .gap_2()
            .bg(cx.theme().background)
            .child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .items_center()
                    .px_3()
                    .py_1p5()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().warning)
                    .bg(cx.theme().muted)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(Tag::warning().small().child(SIMULATED_TRAINING_BADGE))
                            .child(div().text_xs().font_semibold().child(format!(
                                "{} · 进度 {}/{} 根",
                                ws.run.display_symbol,
                                ws.run.current_step_index,
                                ws.run.total_steps
                            ))),
                    )
                    .child(
                        div()
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_xs()
                            .text_color(Chrome::caption())
                            .child(format!(
                                "模拟总权益 {} {ccy} · 可用现金 {} {ccy} · 活动挂单 {}",
                                to_canonical_text(ws.account.total_equity),
                                to_canonical_text(ws.account.available_cash),
                                ws.open_orders.len()
                            )),
                    ),
            )
            .child(chart.render_chart(cx))
    }
}

/// Executes the `M4-K03` / `M4-A-17` release-only next-bar latency benchmark in a hidden,
/// never-activated native HWND:
/// - Rejects non-release (`cfg!(debug_assertions)`) builds.
/// - Seeds 1,800 continuous `OKX:BTC/USDT` daily bars and starts a hidden-mode training run with
///   `history_bars = 1000` + default indicators (`SMA 5/20`, `EMA 12/26`, `MACD 12/26/9`, `RSI 14`).
/// - Places an active position + resting GTC buy/sell limit orders so open-order matching and
///   mark-to-market valuation execute on every step.
/// - Runs 20 warmup steps + 300 measured end-to-end `"下一根"` steps (`advance_training_bar` SQLite
///   save transaction + matching + PIT indicators + 1,000-bar chart slice + GPUI layout/paint +
///   synchronous DirectX `WM_PAINT`), asserting `p95 <= 100.0 ms`, `native_chart_paints >= 300`,
///   `visible_windows == 0`, and `focus_stolen == false`.
pub fn run_m4_k03_next_bar_bench(
    cli_data_dir: Option<&Path>,
) -> Result<M4NextBarBenchReport, ErrorEnvelope> {
    if cfg!(debug_assertions) {
        return Err(ErrorEnvelope::new(
            "RELEASE_BUILD_REQUIRED",
            "M4-K03 下一根延迟基准必须使用 release 构建 (--release) 执行，禁止使用 debug/dev 构建代替",
            false,
        ));
    }
    delta_app::apply_process_quiet_guards();
    let power_throttling = delta_app::opt_out_of_execution_speed_throttling();

    let base = cli_data_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    let owned = VerificationDirectory::create(&base, "m4-k03-bench")?;
    let tmp_dir = owned.path.clone();

    let inner_res = (|| -> Result<M4NextBarBenchReport, ErrorEnvelope> {
        let fix_dir = tmp_dir.join("fixtures");
        write_synthetic_training_fixtures(&fix_dir, 1800)?;
        let paths = verification_app_paths(&tmp_dir);
        let mut svc = delta_app::AppService::open(paths, "USD", "USD", "UTC", true)?;
        svc.sync_from_fixtures(&fix_dir)?;
        let host = AppHost::from_service(svc);

        const HISTORY_BARS: usize = 1000;
        const WARMUP_STEPS: usize = 20;
        const MEASURED_STEPS: usize = 300;
        const TOTAL_STEPS: usize = WARMUP_STEPS + MEASURED_STEPS;

        let created = host.create_training_run(CreateTrainingRunRequest {
            run_id: None,
            market_category: TrainingMarketCategory::CryptoSpot,
            instrument_id: Some("OKX:BTC/USDT".to_string()),
            start_date: None,
            end_date: None,
            warmup_bars: 200,
            history_bars: HISTORY_BARS,
            training_bars: 340,
            reveal_bars: 60,
            seed: Some(20260417),
            dataset_version: None,
            initial_cash: Decimal::from(100_000),
            fee_profile_version_id: BUILTIN_FEE_CRYPTO_RATE_ID.to_string(),
            hidden_mode: true,
            create_and_start: true,
        })?;
        let run_id = created.run_id.clone();

        // Submit an initial market buy (fills on warmup step 1 -> open position) plus resting GTC
        // buy & sell limit orders so matching + reservation checks execute on every measured bar.
        let _ = host.submit_training_order(
            &run_id,
            TrainingOrderRequestDto::single(
                OrderSide::Buy,
                SimOrderType::Market,
                Decimal::new(1, 1), // 0.1 BTC
                TimeInForce::Day,
                None,
            ),
        )?;
        let _ = host.submit_training_order(
            &run_id,
            TrainingOrderRequestDto::single(
                OrderSide::Buy,
                SimOrderType::Limit {
                    limit_price: Decimal::from(10_000),
                },
                Decimal::new(5, 2), // 0.05 BTC resting GTC buy
                TimeInForce::Gtc,
                None,
            ),
        )?;

        let mut init_vp = KlineViewport::new(HISTORY_BARS, HISTORY_BARS);
        init_vp.visible_start = 0;
        init_vp.visible_end = HISTORY_BARS;
        let initial_snap = host.get_training_workspace_snapshot(&run_id, Some(init_vp), 1200)?;

        let host_for_bench = host.clone();
        let run_id_for_bench = run_id.clone();
        let power_for_bench = power_throttling.clone();
        let output: Arc<Mutex<Option<Result<M4NextBarBenchReport, ErrorEnvelope>>>> =
            Arc::new(Mutex::new(None));
        let result_slot = Arc::clone(&output);

        gpui_kit::application()
            .with_assets(gpui_kit::assets::Assets)
            .run(move |cx| {
                gpui_kit::init(cx);
                crate::theme::apply_delta_theme(cx);
                let captured = Arc::new(Mutex::new(None));
                let entity_slot = Arc::clone(&captured);
                let opened = cx.open_window(hidden_options(), move |window, cx| {
                    let view = cx.new(|_| M4NextBarBenchView {
                        snapshot: initial_snap,
                    });
                    *entity_slot.lock().unwrap() = Some(view.clone());
                    cx.new(|cx| Root::new(view, window, cx))
                });
                let handle = match opened {
                    Ok(h) => h,
                    Err(e) => {
                        *result_slot.lock().unwrap() = Some(Err(ErrorEnvelope::new(
                            "NATIVE_WINDOW_FAILED",
                            e.to_string(),
                            false,
                        )));
                        cx.quit();
                        return;
                    }
                };
                let hwnd = match native_window() {
                    Ok(h) => h,
                    Err(e) => {
                        *result_slot.lock().unwrap() = Some(Err(e));
                        cx.quit();
                        return;
                    }
                };
                cx.spawn(async move |cx| {
                    let outcome: Result<M4NextBarBenchReport, ErrorEnvelope> = async {
                        initialize_native_window(hwnd)?;
                        let view = captured.lock().unwrap().take().unwrap();
                        // Initial settle paint
                        cx.update_window(handle.into(), |_, window, _| {
                            quiet_window_check()?;
                            if window.bounds().size.width <= px(0.)
                                || window.bounds().size.height <= px(0.)
                            {
                                return Err(ErrorEnvelope::new(
                                    "NATIVE_WINDOW_SIZE",
                                    "隐藏窗口尚无有效的绘制尺寸",
                                    false,
                                ));
                            }
                            window.refresh();
                            Ok(())
                        })
                        .map_err(|e| {
                            ErrorEnvelope::new("NATIVE_WINDOW_FAILED", e.to_string(), false)
                        })??;
                        paint_native_window(hwnd)?;

                        let mut step_times_us = Vec::with_capacity(MEASURED_STEPS);
                        let mut measured_paint_start = 0usize;
                        let mut last_visible_bars = 0usize;

                        for step in 1..=TOTAL_STEPS {
                            // Yield briefly outside timed section so OS message queue stays clean
                            cx.background_executor()
                                .timer(Duration::from_micros(200))
                                .await;
                            if step == WARMUP_STEPS + 1 {
                                measured_paint_start =
                                    PAINTED_CHART_FRAMES.load(Ordering::Relaxed);
                            }
                            if step == 2 {
                                // After warmup step 1 fills the 0.1 BTC market buy, place a resting GTC sell limit too
                                let _ = host_for_bench.submit_training_order(
                                    &run_id_for_bench,
                                    TrainingOrderRequestDto::single(
                                        OrderSide::Sell,
                                        SimOrderType::Limit {
                                            limit_price: Decimal::from(500_000),
                                        },
                                        Decimal::new(5, 2),
                                        TimeInForce::Gtc,
                                        None,
                                    ),
                                )?;
                            }

                            let t0 = Instant::now();
                            // 1. SQLite save transaction + matching engine (`advance_training_bar`)
                            let receipt =
                                host_for_bench.advance_training_bar(&run_id_for_bench)?;
                            // 2. PIT adjustment + SMA/EMA/MACD/RSI indicators + 1,000-bar chart slice
                            let total_vis = receipt.run.visible_bar_count;
                            let mut vp = KlineViewport::new(total_vis, HISTORY_BARS);
                            vp.visible_start = total_vis.saturating_sub(HISTORY_BARS);
                            vp.visible_end = total_vis;
                            let snap = host_for_bench.get_training_workspace_snapshot(
                                &run_id_for_bench,
                                Some(vp),
                                1200,
                            )?;
                            last_visible_bars = snap.chart_slice.bars.len();

                            // 3. GPUI view update + layout + synchronous DirectX `WM_PAINT`
                            cx.update_window(handle.into(), |_, window, cx| {
                                view.update(cx, |v, cx| {
                                    v.snapshot = snap;
                                    cx.notify();
                                });
                                window.refresh();
                            })
                            .map_err(|e| {
                                ErrorEnvelope::new("NATIVE_WINDOW_FAILED", e.to_string(), false)
                            })?;
                            paint_native_window(hwnd)?;
                            let elapsed_us = t0.elapsed().as_micros() as u64;

                            if step > WARMUP_STEPS {
                                step_times_us.push(elapsed_us);
                            }
                        }

                        let native_chart_paints =
                            PAINTED_CHART_FRAMES.load(Ordering::Relaxed) - measured_paint_start;
                        if step_times_us.len() != MEASURED_STEPS {
                            return Err(ErrorEnvelope::new(
                                "M4_BENCH_INCOMPLETE",
                                format!(
                                    "采样步数不足: 期望 {MEASURED_STEPS}, 实际 {}",
                                    step_times_us.len()
                                ),
                                false,
                            ));
                        }
                        if last_visible_bars != HISTORY_BARS {
                            return Err(ErrorEnvelope::new(
                                "M4_BENCH_VISIBLE_BARS",
                                format!(
                                    "可见 K 线根数不符合 1000 根规格: 实际 {last_visible_bars}"
                                ),
                                false,
                            ));
                        }
                        if native_chart_paints < MEASURED_STEPS {
                            return Err(ErrorEnvelope::new(
                                "M4_BENCH_PAINT_COUNT",
                                format!(
                                    "原生 K 线实际绘制帧数不足: 期望 >= {MEASURED_STEPS}, 实际 {native_chart_paints}"
                                ),
                                false,
                            ));
                        }

                        let (p50_us, p95_us, p99_us) = percentiles(&step_times_us);
                        let max_us = step_times_us.iter().copied().max().unwrap_or(0);
                        let p50_ms = (p50_us as f64) / 1000.0;
                        let p95_ms = (p95_us as f64) / 1000.0;
                        let p99_ms = (p99_us as f64) / 1000.0;
                        let max_ms = (max_us as f64) / 1000.0;
                        let step_times_ms: Vec<f64> = step_times_us
                            .iter()
                            .map(|&us| (us as f64) / 1000.0)
                            .collect();
                        let passed = p95_ms <= M4_K03_P95_BUDGET_MS;

                        let rep = cx
                            .update_window(handle.into(), |_, window, _| {
                                quiet_window_check()?;
                                Ok::<M4NextBarBenchReport, ErrorEnvelope>(M4NextBarBenchReport {
                                    status: if passed { "passed" } else { "failed" }.to_string(),
                                    case_id: "M4-A-17".to_string(),
                                    kpi_id: "M4-K03".to_string(),
                                    executed_at: chrono::Utc::now().to_rfc3339(),
                                    build_profile: if cfg!(debug_assertions) {
                                        "dev"
                                    } else {
                                        "release"
                                    }
                                    .to_string(),
                                    power_throttling: power_for_bench.clone(),
                                    hardware_logical_cores: std::thread::available_parallelism()
                                        .map(|n| n.get())
                                        .unwrap_or(1),
                                    cpu: std::env::var("PROCESSOR_IDENTIFIER")
                                        .unwrap_or_else(|_| "unknown".to_string()),
                                    gpu: format!("{:?}", window.gpu_specs()),
                                    window_size: format!("{:?}", window.bounds().size),
                                    scale_factor: window.scale_factor(),
                                    native_window_created: true,
                                    visible_windows: count_visible_windows_for_current_process(),
                                    focus_stolen: false,
                                    history_bars: HISTORY_BARS,
                                    kline_visible_bars: last_visible_bars,
                                    warmup_steps: WARMUP_STEPS,
                                    measured_steps: step_times_us.len(),
                                    native_chart_paints,
                                    p50_ms,
                                    p95_ms,
                                    p99_ms,
                                    max_ms,
                                    budget_p95_ms: M4_K03_P95_BUDGET_MS,
                                    passed,
                                    step_times_ms,
                                    step_times_us,
                                    measurement: "release build; hidden non-activating HWND (1400x950); 20 warmup + 300 measured end-to-end next-bar steps covering SQLite advance_training_bar save transaction + matching engine + PIT adjustment + default SMA(5,20)/EMA(12,26)/MACD(12,26,9)/RSI(14) indicators + 1000-bar KlineChartView slice + GPUI layout/paint + synchronous DirectX WM_PAINT".to_string(),
                                    build_info: delta_app::build_info("delta-desktop"),
                                })
                            })
                            .map_err(|e| {
                                ErrorEnvelope::new("NATIVE_WINDOW_FAILED", e.to_string(), false)
                            })??;

                        if !rep.passed {
                            return Err(ErrorEnvelope::new(
                                "M4_K03_BUDGET_EXCEEDED",
                                format!(
                                    "M4-K03 下一根延迟超过预算: p95={:.3} ms > {:.1} ms (p50={:.3} ms, p99={:.3} ms)",
                                    rep.p95_ms, rep.budget_p95_ms, rep.p50_ms, rep.p99_ms
                                ),
                                false,
                            ));
                        }
                        Ok(rep)
                    }
                    .await;
                    *result_slot.lock().unwrap() = Some(outcome);
                    let _ = cx.update_window(handle.into(), |_, window, cx| {
                        window.remove_window();
                        cx.quit();
                    });
                })
                .detach();
            });

        let _ = host.shutdown();
        let taken = output.lock().unwrap().take();
        taken.ok_or_else(|| {
            ErrorEnvelope::new("M4_BENCH_INTERRUPTED", "M4-K03 基准测量没有产生结果", false)
        })?
    })();

    owned.cleanup()?;
    inner_res
}

/// Explicit visual capture for design review: renders every workbench page in the same hidden,
/// never-activated native window as `--ui-smoke` and writes PNG frames to `out_dir`. Test-only
/// because `Window::render_to_image` exists only with `gpui-kit/test-support`.
#[cfg(all(test, windows))]
pub(crate) fn run_native_capture(
    host: AppHost,
    data_dir: PathBuf,
    out_dir: PathBuf,
) -> Result<Vec<PathBuf>, ErrorEnvelope> {
    let output = Arc::new(Mutex::new(None));
    let result = Arc::clone(&output);
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
                        host,
                        data_dir,
                        NaiveDate::from_ymd_opt(2026, 1, 15).unwrap(),
                        window,
                        cx,
                    )
                });
                *captured.lock().unwrap() = Some(view.clone());
                cx.new(|cx| Root::new(view, window, cx))
            });
            let fail = |message: String| Err(ErrorEnvelope::new("CAPTURE_FAILED", message, false));
            let handle = match opened {
                Ok(handle) => handle,
                Err(error) => {
                    *result.lock().unwrap() = Some(fail(error.to_string()));
                    cx.quit();
                    return;
                }
            };
            let hwnd = match native_window() {
                Ok(hwnd) => hwnd,
                Err(error) => {
                    *result.lock().unwrap() = Some(Err(error));
                    cx.quit();
                    return;
                }
            };
            cx.spawn(async move |cx| {
                const PAGES: [&str; 6] = [
                    "nav.overview",
                    "nav.market",
                    "nav.assets",
                    "nav.journal",
                    "nav.data_center",
                    "nav.settings",
                ];
                let outcome = async {
                    initialize_native_window(hwnd)?;
                    let mut written = Vec::new();
                    for page in PAGES {
                        cx.update_window(handle.into(), |_, window, cx| {
                            let view = wb.lock().unwrap().as_ref().unwrap().clone();
                            view.update(cx, |view, cx| {
                                view.controller
                                    .update(cx, |ctl, cx| ctl.navigate(page, window, cx))
                            });
                        })
                        .map_err(|e| ErrorEnvelope::new("CAPTURE_FAILED", e.to_string(), false))?;
                        let deadline = Instant::now() + Duration::from_secs(30);
                        loop {
                            cx.background_executor()
                                .timer(Duration::from_millis(50))
                                .await;
                            let ready = cx
                                .update_window(handle.into(), |_, window, cx| {
                                    let view = wb.lock().unwrap().as_ref().unwrap().clone();
                                    window.refresh();
                                    view.update(cx, |view, cx| {
                                        !view.controller.read(cx).busy
                                            && view.state.lock().unwrap().overview.is_some()
                                    })
                                })
                                .map_err(|e| {
                                    ErrorEnvelope::new("CAPTURE_FAILED", e.to_string(), false)
                                })?;
                            paint_native_window(hwnd)?;
                            if ready || Instant::now() >= deadline {
                                break;
                            }
                        }
                        // Two more frames so late layout (tables, chart bounds) settles.
                        for _ in 0..2 {
                            cx.background_executor()
                                .timer(Duration::from_millis(50))
                                .await;
                            cx.update_window(handle.into(), |_, window, _| window.refresh())
                                .map_err(|e| {
                                    ErrorEnvelope::new("CAPTURE_FAILED", e.to_string(), false)
                                })?;
                            paint_native_window(hwnd)?;
                        }
                        quiet_window_check()?;
                        let image = cx
                            .update_window(handle.into(), |_, window, _| window.render_to_image())
                            .map_err(|e| {
                                ErrorEnvelope::new("CAPTURE_FAILED", e.to_string(), false)
                            })?
                            .map_err(|e| {
                                ErrorEnvelope::new("CAPTURE_FAILED", e.to_string(), false)
                            })?;
                        let path = out_dir.join(format!("{}.png", page.replace('.', "-")));
                        image.save(&path).map_err(|e| {
                            ErrorEnvelope::new("CAPTURE_FAILED", e.to_string(), false)
                        })?;
                        written.push(path);
                    }
                    Ok(written)
                }
                .await;
                *result.lock().unwrap() = Some(outcome);
                let _ = cx.update_window(handle.into(), |_, window, cx| {
                    window.remove_window();
                    cx.quit();
                });
            })
            .detach();
        });
    let result = output.lock().unwrap().take().ok_or_else(|| {
        ErrorEnvelope::new("CAPTURE_INTERRUPTED", "原生窗口截图没有产生结果", false)
    })?;
    result
}

#[cfg(all(test, windows))]
mod capture_tests {
    /// Explicit only: `DELTA_UI_CAPTURE_DIR=<dir> cargo test -p delta-desktop --lib
    /// capture_workbench_pages -- --ignored`. Hidden window, no activation (D-25).
    #[test]
    #[ignore = "explicit visual capture; requires DELTA_UI_CAPTURE_DIR"]
    fn capture_workbench_pages() {
        let out = std::path::PathBuf::from(
            std::env::var_os("DELTA_UI_CAPTURE_DIR").expect("DELTA_UI_CAPTURE_DIR"),
        );
        std::fs::create_dir_all(&out).unwrap();
        let root = out.join(format!("lib-{}", uuid::Uuid::new_v4()));
        let documents_dir = root.join("documents");
        let paths = delta_app::ResolvedAppPaths {
            channel: delta_app::BuildChannel::Source,
            data_dir: root.clone(),
            documents_dir: documents_dir.clone(),
            default_export_dir: documents_dir.join("exports"),
            default_backup_dir: root.join("backups"),
            credential_prefix: format!("delta-test-capture-{}", uuid::Uuid::new_v4()),
        };
        let mut svc = delta_app::AppService::open(paths, "USD", "USD", "UTC", true).unwrap();
        delta_app::seed_ac02_fixture_in_service(&mut svc).unwrap();
        let host = super::AppHost::from_service(svc);
        let written = super::run_native_capture(host, root.clone(), out).unwrap();
        assert_eq!(written.len(), 6);
        std::fs::remove_dir_all(root).unwrap();
    }
}

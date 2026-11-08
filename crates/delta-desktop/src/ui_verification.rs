//! Explicit native-window validation. Windows stay hidden and never activate.

use std::{
    path::PathBuf,
    sync::{atomic::Ordering, Arc, Mutex},
    time::{Duration, Instant},
};

use chrono::NaiveDate;
use delta_app::{
    count_visible_windows_for_current_process, AdjustmentKind, AppHost, BarRecord, ErrorEnvelope,
    KlineViewport, KlineWindowSliceDto,
};
use gpui_kit::component::{
    table::{DataTable, TableState},
    ActiveTheme as _, Root,
};
use gpui_kit::{
    div, point, px, size, AppContext as _, Bounds, Context, Entity, IntoElement,
    ParentElement as _, Render, Styled as _, Window, WindowBounds, WindowOptions,
};
use serde_json::{json, Value};

use crate::chart_canvas::PAINTED_CHART_FRAMES;
use crate::{
    workbench::{DeltaWorkbench, TransactionsTableDelegate, UiBenchReport, RENDERED_TABLE_CELLS},
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
            workbench_measurement: String::new(),
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

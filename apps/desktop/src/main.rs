//! DELTA desktop host (GPUI Kit). POC-01/02 evidence: window, navigation,
//! table, chart canvas with 1000 visible candles, wheel zoom, drag pan and
//! crosshair. The UI renders state only; no financial computation happens
//! here.

mod chart;
mod pages;
mod perf;
mod tasks;
mod theme;

use gpui_kit::gpui::{px, size, Bounds, WindowBounds, WindowOptions};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    gpui_kit::platform::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx: &mut gpui_kit::gpui::App| {
            gpui_kit::init(cx);
            gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Light, None, cx);
            let bounds = Bounds::centered(None, size(px(1536.), px(1024.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(960.), px(640.))),
                    ..gpui_kit::component::TitleBar::window_options()
                },
                |window, cx| {
                    use gpui_kit::gpui::AppContext as _;
                    let view = cx.new(|cx| pages::Workspace::new(window, cx));
                    let weak = view.downgrade();
                    window.on_window_should_close(cx, move |_window, cx| {
                        let allowed = weak
                            .update(cx, |view, cx| view.may_close(cx))
                            .unwrap_or(true);
                        if allowed {
                            perf::mark_close_requested();
                        }
                        allowed
                    });
                    view
                },
            )
            .expect("open window");
            cx.on_window_closed(|cx, _window_id| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            cx.activate(true);
        });
    perf::log_summary();
}

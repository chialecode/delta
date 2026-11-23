//! Shared headless GPUI test helpers (`UI-Q04`).

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{px, SharedString};

pub(crate) fn temp_test_dir(prefix: &str, tag: &str) -> PathBuf {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let base = std::env::var_os("DELTA_TEST_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!(
        "{prefix}_{tag}_{}_{}_{}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed),
        Utc::now().timestamp_nanos_opt().unwrap_or(1)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

pub(crate) fn click_id(cx: &mut gpui_kit::VisualTestContext, id: &str) {
    let id_str = SharedString::from(id.to_string());
    let position = cx.update(move |window, cx| {
        window.render_frame(cx);
        let bounds = window.find(id_str.clone()).bounds();
        assert!(
            bounds.size.width > px(0.) && bounds.size.height > px(0.),
            "control '{id_str}' has no rendered bounds"
        );
        bounds.center()
    });
    cx.simulate_click(position, gpui_kit::Modifiers::default());
    cx.run_until_parked();
}

pub(crate) fn has_rendered_id(cx: &mut gpui_kit::VisualTestContext, id: &str) -> bool {
    let id_str = SharedString::from(id.to_string());
    cx.update(move |window, cx| {
        window.render_frame(cx);
        let bounds = window.find(id_str.clone()).bounds();
        bounds.size.width > px(0.) && bounds.size.height > px(0.)
    })
}

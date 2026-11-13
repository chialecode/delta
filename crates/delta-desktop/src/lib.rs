//! DELTA desktop workbench built on `gpui-kit 0.9.0` (`S-12`, `S-15`, `S-16`, `S-24`).

extern crate gpui_kit as gpui;

mod chart_canvas;
pub mod components;
pub mod controller;
pub mod i18n;
pub mod lifecycle;
pub mod theme;
pub mod training_ui;
pub mod training_workbench;
pub mod training_workbench_ui;
mod ui_forms;
pub mod ui_verification;
pub mod ui_verification_m5;
pub mod workbench;

pub use components::*;
pub use i18n::*;
pub use training_ui::*;
pub use ui_verification::{
    run_m4_k03_next_bar_bench, run_m4_ui_smoke, M4NextBarBenchReport, M4UiSmokeReport,
    M4_K03_P95_BUDGET_MS,
};
pub use ui_verification_m5::{
    run_m5_k02_playback_bench, run_m5_ui_smoke, M5PlaybackBenchReport, M5UiSmokeReport,
    M5_K02_INTERVAL_P95_BUDGET_MS, M5_K02_STEP_P95_BUDGET_MS,
};
pub use workbench::*;

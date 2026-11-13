//! DELTA desktop workbench built on `gpui-kit 0.9.0` (`S-12`, `S-15`, `S-16`, `S-24`).

extern crate gpui_kit as gpui;

mod chart_canvas;
pub mod components;
pub mod controller;
pub mod i18n;
pub mod lifecycle;
pub mod theme;
pub mod training_ui;
mod ui_forms;
pub mod ui_verification;
pub mod workbench;

pub use components::*;
pub use i18n::*;
pub use training_ui::*;
pub use ui_verification::{
    run_m4_k03_next_bar_bench, run_m4_ui_smoke, M4NextBarBenchReport, M4UiSmokeReport,
    M4_K03_P95_BUDGET_MS,
};
pub use workbench::*;

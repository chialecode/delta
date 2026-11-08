//! DELTA desktop workbench built on `gpui-kit 0.9.0` (`S-12`, `S-15`, `S-16`, `S-24`).

extern crate gpui_kit as gpui;

mod chart_canvas;
pub mod components;
pub mod controller;
pub mod i18n;
pub mod theme;
mod ui_forms;
mod ui_verification;
pub mod workbench;

pub use components::*;
pub use i18n::*;
pub use workbench::*;

//! Shared workbench tokens. Values are visual, never financial facts.
use gpui_kit::gpui::{div, px, rgb, Div, ParentElement, Styled};

pub const BG: u32 = 0xf5f7fa;
pub const PANEL: u32 = 0xffffff;
pub const INK: u32 = 0x1c2943;
pub const MUTED: u32 = 0x8290a6;
pub const LINE: u32 = 0xe9edf3;
pub const ORANGE: u32 = 0xff8908;
pub const TINT: u32 = 0xfff3e5;
pub const GREEN: u32 = 0x00aa7b;
pub const RED: u32 = 0xef5664;

pub fn row() -> Div {
    div().flex().items_center()
}
pub fn column() -> Div {
    div().flex().flex_col()
}
pub fn card() -> Div {
    column()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(LINE))
        .rounded(px(9.))
        .overflow_hidden()
}
pub fn label(text: impl Into<String>, size: f32, color: u32) -> Div {
    div()
        .text_size(px(size))
        .line_height(px(size * 1.5))
        .text_color(rgb(color))
        .child(text.into())
}
pub fn badge(text: &'static str) -> Div {
    label(text, 10., ORANGE)
        .px_2()
        .py_1()
        .rounded(px(5.))
        .bg(rgb(TINT))
}

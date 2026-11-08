//! DELTA desktop visual theme: a light, dense trading-terminal palette in the spirit of the
//! Longbridge desktop client (white surfaces, hairline borders, dark primary actions, compact
//! 13 px type). Only presentation tokens live here; financial colors still go through
//! [`crate::MarketColorConvention`] so up/down always carries a sign and a word.

use gpui_kit::component::theme::{Theme, ThemeMode};
use gpui_kit::{px, rgb, App, Hsla};

/// Fixed shell metrics shared by the workbench chrome.
pub const TOOLBAR_HEIGHT: f32 = 40.;
pub const NAV_RAIL_WIDTH: f32 = 60.;
pub const STATUS_BAR_HEIGHT: f32 = 26.;
pub const PANEL_HEADER_HEIGHT: f32 = 36.;

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

/// Neutral surfaces used by the shell that are not part of the `gpui-kit` token set.
pub struct Chrome;

impl Chrome {
    /// Rail, toolbar and status bar background (slightly off-white).
    pub fn shell() -> Hsla {
        c(0xF7F8FA)
    }
    /// Hover / selected item background in lists and the rail.
    pub fn hover() -> Hsla {
        c(0xEEF0F3)
    }
    /// Section header text (muted, compact).
    pub fn caption() -> Hsla {
        c(0x8A919C)
    }
    /// Accent used for the active filter pill and the AI entry, Longbridge-style teal.
    pub fn accent() -> Hsla {
        c(0x14B8A6)
    }
    /// Translucent accent fill behind the active pill.
    pub fn accent_soft() -> Hsla {
        c(0xE6F7F5)
    }
    /// Chart line / link blue.
    pub fn blue() -> Hsla {
        c(0x2F7CF6)
    }
}

/// Applies the DELTA workbench palette on top of the `gpui-kit` defaults.
pub fn apply_delta_theme(cx: &mut App) {
    Theme::change(ThemeMode::Light, None, cx);
    Theme::update(cx, |theme| {
        theme.font_size = px(13.);
        theme.mono_font_size = px(12.);
        theme.radius = px(4.);
        theme.radius_lg = px(6.);
        theme.shadow = false;

        theme.background = c(0xFFFFFF);
        theme.foreground = c(0x1F2329);
        theme.muted = c(0xF5F6F8);
        theme.muted_foreground = c(0x8A919C);
        theme.border = c(0xEBEDF0);
        theme.input = c(0xE3E5E8);
        theme.ring = c(0x2F7CF6);
        theme.selection = c(0xD6E6FE);
        theme.caret = c(0x1F2329);

        theme.primary = c(0x1F2329);
        theme.primary_hover = c(0x34393F);
        theme.primary_active = c(0x111316);
        theme.primary_foreground = c(0xFFFFFF);
        theme.secondary = c(0xF5F6F8);
        theme.secondary_hover = c(0xEEF0F3);
        theme.secondary_active = c(0xE6E8EB);
        theme.secondary_foreground = c(0x1F2329);
        theme.accent = c(0xF2F3F5);
        theme.accent_foreground = c(0x1F2329);

        theme.link = c(0x2F7CF6);
        theme.link_hover = c(0x1E66D9);
        theme.link_active = c(0x1A57B8);
        theme.info = c(0x2F7CF6);
        theme.success = c(0x13A776);
        theme.warning = c(0xF29D0C);
        theme.danger = c(0xEE3D54);
        theme.red = c(0xEE3D54);
        theme.red_light = c(0xFDECEE);
        theme.green = c(0x13A776);
        theme.green_light = c(0xE7F6F0);
        theme.blue = c(0x2F7CF6);
        theme.blue_light = c(0xEAF2FE);
        theme.yellow = c(0xF29D0C);
        theme.yellow_light = c(0xFEF5E6);

        theme.sidebar = c(0xF7F8FA);
        theme.sidebar_border = c(0xEBEDF0);
        theme.sidebar_foreground = c(0x1F2329);
        theme.sidebar_accent = c(0xEEF0F3);
        theme.sidebar_accent_foreground = c(0x1F2329);
        theme.title_bar = c(0xF7F8FA);
        theme.title_bar_border = c(0xEBEDF0);
        theme.status_bar = c(0xF7F8FA);
        theme.status_bar_border = c(0xEBEDF0);

        theme.tab_bar = c(0xFFFFFF);
        theme.tab = c(0xFFFFFF);
        theme.tab_active = c(0xFFFFFF);
        theme.tab_foreground = c(0x6B7280);
        theme.tab_active_foreground = c(0x1F2329);

        theme.table = c(0xFFFFFF);
        theme.table_head = c(0xFAFAFB);
        theme.table_head_foreground = c(0x8A919C);
        theme.table_even = c(0xFCFCFD);
        theme.table_hover = c(0xF5F7FA);
        theme.table_active = c(0xEAF2FE);
        theme.table_active_border = c(0x2F7CF6);
        theme.table_row_border = c(0xF0F1F3);
        theme.list_hover = c(0xF5F7FA);
        theme.list_active = c(0xEAF2FE);
        theme.list_active_border = c(0x2F7CF6);

        theme.chart_grid = c(0xF0F1F3);
        theme.chart_1 = c(0x2F7CF6);
        theme.chart_2 = c(0xF29D0C);
        theme.chart_3 = c(0x8B5CF6);
        theme.chart_4 = c(0x14B8A6);
        theme.chart_5 = c(0xEC4899);

        theme.scrollbar_thumb = c(0xD5D8DC);
        theme.scrollbar_thumb_hover = c(0xB9BDC3);
    });
}

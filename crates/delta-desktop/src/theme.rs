//! DELTA desktop visual theme: a light, dense trading-terminal palette in the spirit of the
//! Longbridge desktop client (white surfaces, hairline borders, dark primary actions, compact
//! 15 px type). Only presentation tokens live here; financial colors still go through
//! [`crate::MarketColorConvention`] so up/down always carries a sign and a word.

use gpui_kit::component::theme::{Theme, ThemeMode};
use gpui_kit::{px, rgb, App, Global, Hsla, SharedString};

/// Fixed shell metrics shared by the workbench chrome.
pub const TOOLBAR_HEIGHT: f32 = 40.;
pub const NAV_RAIL_WIDTH: f32 = 60.;
pub const STATUS_BAR_HEIGHT: f32 = 26.;
pub const PANEL_HEADER_HEIGHT: f32 = 36.;

/// Preferred UI sans-serif family when installed (`gpui-kit` keeps `.SystemUIFont` otherwise).
pub const PREFERRED_UI_FONT_FAMILIES: &[&str] = &["Inter"];

/// Preferred monospace families in priority order; DirectWrite provides CJK fallback once loaded.
pub const PREFERRED_MONO_FONT_FAMILIES: &[&str] = &["JetBrains Mono", "Cascadia Mono", "Consolas"];

#[derive(Clone)]
struct CachedInstalledFontNames(Vec<String>);

impl Global for CachedInstalledFontNames {}

fn installed_font_names(cx: &mut App) -> Vec<String> {
    if let Some(cached) = cx.try_global::<CachedInstalledFontNames>() {
        return cached.0.clone();
    }
    let names = cx.text_system().all_font_names();
    cx.set_global(CachedInstalledFontNames(names.clone()));
    names
}

/// Select the first candidate present in `installed_families`, preserving `fallback` when none are
/// installed so GPUI never lays out text against a missing font family.
pub fn select_installed_font_family(
    candidates: &[&str],
    installed_families: &[String],
    fallback: &SharedString,
) -> SharedString {
    for candidate in candidates {
        if let Some(installed) = installed_families
            .iter()
            .find(|name| name.eq_ignore_ascii_case(candidate))
        {
            return SharedString::from(installed.clone());
        }
    }
    fallback.clone()
}

/// Resolve DELTA's `(font_family, mono_font_family)` pair against `installed_families` without
/// risking a GPUI missing-font panic.
pub fn resolve_delta_font_families(
    installed_families: &[String],
    default_ui_family: &SharedString,
    default_mono_family: &SharedString,
) -> (SharedString, SharedString) {
    (
        select_installed_font_family(
            PREFERRED_UI_FONT_FAMILIES,
            installed_families,
            default_ui_family,
        ),
        select_installed_font_family(
            PREFERRED_MONO_FONT_FAMILIES,
            installed_families,
            default_mono_family,
        ),
    )
}

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
    let installed_fonts = installed_font_names(cx);
    Theme::update(cx, |theme| {
        let (ui_font, mono_font) = resolve_delta_font_families(
            &installed_fonts,
            &theme.font_family,
            &theme.mono_font_family,
        );
        theme.font_family = ui_font;
        theme.mono_font_family = mono_font;
        theme.font_size = px(15.);
        theme.mono_font_size = px(14.);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn families(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn ui_font_prefers_inter_only_when_installed() {
        let fallback = SharedString::from(".SystemUIFont");
        assert_eq!(
            select_installed_font_family(
                PREFERRED_UI_FONT_FAMILIES,
                &families(&["Segoe UI", "Inter", "Microsoft YaHei UI"]),
                &fallback,
            ),
            SharedString::from("Inter")
        );
        assert_eq!(
            select_installed_font_family(
                PREFERRED_UI_FONT_FAMILIES,
                &families(&["Segoe UI", "Microsoft YaHei UI"]),
                &fallback,
            ),
            fallback
        );
        assert_eq!(
            select_installed_font_family(PREFERRED_UI_FONT_FAMILIES, &[], &fallback),
            fallback
        );
    }

    #[test]
    fn mono_font_walks_jetbrains_then_cascadia_then_consolas_then_fallback() {
        let fallback = SharedString::from(".SystemUIFont");
        assert_eq!(
            select_installed_font_family(
                PREFERRED_MONO_FONT_FAMILIES,
                &families(&["Consolas", "Cascadia Mono", "JetBrains Mono"]),
                &fallback,
            ),
            SharedString::from("JetBrains Mono")
        );
        assert_eq!(
            select_installed_font_family(
                PREFERRED_MONO_FONT_FAMILIES,
                &families(&["Consolas", "Cascadia Mono"]),
                &fallback,
            ),
            SharedString::from("Cascadia Mono")
        );
        assert_eq!(
            select_installed_font_family(
                PREFERRED_MONO_FONT_FAMILIES,
                &families(&["Consolas", "Courier New"]),
                &fallback,
            ),
            SharedString::from("Consolas")
        );
        assert_eq!(
            select_installed_font_family(
                PREFERRED_MONO_FONT_FAMILIES,
                &families(&["Courier New"]),
                &fallback,
            ),
            fallback
        );
    }

    #[test]
    fn resolve_delta_font_families_preserves_registered_casing_and_headless_defaults() {
        let default_ui = SharedString::from(".SystemUIFont");
        let default_mono = SharedString::from(".SystemUIFont");
        assert_eq!(
            resolve_delta_font_families(&[], &default_ui, &default_mono),
            (default_ui.clone(), default_mono.clone())
        );

        let installed = families(&["inter", "Cascadia Mono", "Consolas"]);
        assert_eq!(
            resolve_delta_font_families(&installed, &default_ui, &SharedString::from("Consolas")),
            (
                SharedString::from("inter"),
                SharedString::from("Cascadia Mono")
            )
        );
    }
}

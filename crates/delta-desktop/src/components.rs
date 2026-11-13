//! Self-contained Longbridge-style desktop UI components (`S-15`, `S-16`, `DESIGN.md`):
//! - [`MetricCard`]: Overview and breakdown metric cards modeled after the `gpui-kit 0.9.0` story
//!   card pattern (`M2-K01`, `M2-A-03`), never fabricating `0` when `primary_value` is `None`.
//! - [`StatusBanner`] & [`WorkbenchSystemState`]: Honest status banners for all required `DESIGN.md`
//!   system states with icon, reason message, and actionable next-step button.
//! - [`KlineChartView`]: Custom GPUI K-line viewport slice renderer (`S-16`, `M2-W05`, `M2-K02`)
//!   with header badges, tabular crosshair OHLCV + indicator readout, explicit `"GAP"` spacers for
//!   missing sessions, clickable trade/note/plan markers (`M2-A-11`), volume sub-panel, and
//!   MACD/RSI sub-panel.

use crate::i18n::{ChangeDirection, I18nCatalog, Locale, MarketColorConvention};
#[allow(unused_imports)]
use delta_app::kline::{
    ChartMarkerDto, ChartMarkerLayout, IndicatorConfig, KlineViewport, KlineWindowSliceDto,
    LodCandleDto, MarkerKind,
};
use delta_app::{to_canonical_text, QualityStatus};
use gpui_kit::base::{ObservedElement, TestTextExt as _};
#[allow(unused_imports)]
use gpui_kit::component::{
    badge::Badge,
    button::{Button, ButtonVariants as _},
    h_flex,
    separator::Separator,
    tag::Tag,
    v_flex, ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    div, AnyElement, App, Div, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, TestSupportExt as _,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Type alias for an interactive `Div` registered with `gpui-kit` headless test observation.
/// In non-test builds this resolves to `gpui_kit::Stateful<gpui_kit::Div>`.
pub type ObservedStatefulDiv = ObservedElement<Stateful<Div>>;

/// Overview and P&L breakdown card modeled after the Longbridge `gpui-kit 0.9.0` story card (`S-15`, `M2-K01`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricCard {
    pub id: SharedString,
    pub title: SharedString,
    pub primary_value: Option<Decimal>,
    pub fallback_text: SharedString,
    pub currency: SharedString,
    pub quality: QualityStatus,
    pub period_label: SharedString,
    pub note: SharedString,
    pub is_signed_pnl: bool,
    pub color_convention: MarketColorConvention,
    pub drilldown_target: Option<SharedString>,
}

impl MetricCard {
    /// Creates a new [`MetricCard`] with Chinese-first defaults (`S-15`, `S-24`).
    pub fn new(
        id: impl Into<SharedString>,
        title: impl Into<SharedString>,
        primary_value: Option<Decimal>,
        currency: impl Into<SharedString>,
        quality: QualityStatus,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            primary_value,
            fallback_text: SharedString::from(I18nCatalog::tr("card.unavailable")),
            currency: currency.into(),
            quality,
            period_label: SharedString::from("当前范围"),
            note: SharedString::default(),
            is_signed_pnl: false,
            color_convention: MarketColorConvention::default(),
            drilldown_target: None,
        }
    }

    pub fn with_fallback_text(mut self, fallback_text: impl Into<SharedString>) -> Self {
        self.fallback_text = fallback_text.into();
        self
    }

    pub fn with_period(mut self, period_label: impl Into<SharedString>) -> Self {
        self.period_label = period_label.into();
        self
    }

    pub fn with_note(mut self, note: impl Into<SharedString>) -> Self {
        self.note = note.into();
        self
    }

    pub fn with_signed_pnl(mut self, is_signed_pnl: bool) -> Self {
        self.is_signed_pnl = is_signed_pnl;
        self
    }

    pub fn with_color_convention(mut self, convention: MarketColorConvention) -> Self {
        self.color_convention = convention;
        self
    }

    pub fn with_drilldown(mut self, target: impl Into<SharedString>) -> Self {
        self.drilldown_target = Some(target.into());
        self
    }

    /// Formats the primary numeric value in canonical Decimal text, or returns `fallback_text`
    /// when `primary_value` is `None`.
    ///
    /// **Critical boundary (`AC-03`, `DESIGN.md`)**: NEVER fabricates `"0"` when `primary_value` is `None`.
    pub fn formatted_value(&self) -> String {
        match self.primary_value {
            Some(val) => to_canonical_text(val),
            None => self.fallback_text.to_string(),
        }
    }

    /// Formats the complete primary display string including currency, sign symbol, and direction word.
    pub fn formatted_display_line(&self) -> String {
        if self.is_signed_pnl {
            self.color_convention
                .format_signed_change(self.primary_value, self.currency.as_ref())
                .text
        } else {
            match self.primary_value {
                Some(val) => {
                    let canonical = to_canonical_text(val);
                    let ccy = self.currency.trim();
                    if ccy.is_empty() {
                        canonical
                    } else {
                        format!("{canonical} {ccy}")
                    }
                }
                None => self.fallback_text.to_string(),
            }
        }
    }

    /// Renders a Longbridge-styled `Tag` pill for a [`QualityStatus`].
    pub fn render_quality_tag(quality: QualityStatus, locale: Locale) -> Tag {
        let label = I18nCatalog::quality_label(locale, quality);
        let tag = match quality {
            QualityStatus::Exact => Tag::success(),
            QualityStatus::CarriedForward => Tag::info(),
            QualityStatus::Partial | QualityStatus::Warning => Tag::warning(),
            QualityStatus::Unavailable => Tag::danger(),
        };
        tag.small().child(label)
    }

    /// Renders this card as a stateful GPUI `Div` with `.id(self.id.clone()).test_support()` (`M2-K01`, `M2-A-03`).
    pub fn render_card(&self, cx: &App) -> ObservedStatefulDiv {
        let quality_label = I18nCatalog::quality_label(Locale::ZhCn, self.quality);
        let value_text = self.formatted_value();
        let display_line = self.formatted_display_line();
        let a11y_summary = SharedString::from(format!(
            "{} | {} | {} | {}",
            self.title, display_line, quality_label, self.period_label
        ));

        let value_fg = if self.is_signed_pnl {
            let signed = self
                .color_convention
                .format_signed_change(self.primary_value, self.currency.as_ref());
            self.color_convention.colors(signed.direction, cx).0
        } else if self.primary_value.is_none() {
            cx.theme().muted_foreground
        } else {
            cx.theme().foreground
        };

        let drilldown_id = SharedString::from(format!("{}-drilldown", self.id));

        v_flex()
            .id(self.id.clone())
            .test_support()
            .aria_label(a11y_summary)
            .flex_1()
            .min_w(gpui_kit::px(190.))
            .border_1()
            .border_color(cx.theme().border)
            .rounded(cx.theme().radius_lg)
            .bg(cx.theme().background)
            .px_4()
            .py_3()
            .gap_1()
            .cursor_pointer()
            .hover(|card| card.border_color(cx.theme().muted_foreground.opacity(0.4)))
            .child(
                // Header row: muted title on the left, quality pill on the right.
                h_flex()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(self.title.clone())
                            .test_text(self.title.clone()),
                    )
                    .child(
                        div()
                            .child(Self::render_quality_tag(self.quality, Locale::ZhCn))
                            .test_text(quality_label),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .items_baseline()
                    .gap_1p5()
                    .child(
                        div()
                            .text_xl()
                            .font_semibold()
                            .text_color(value_fg)
                            .child(display_line.clone())
                            .test_text(display_line),
                    )
                    .when(!self.is_signed_pnl && self.primary_value.is_some(), |row| {
                        // Also record raw canonical value for exact headless assertions
                        row.child(div().size_0().test_text(value_text))
                    }),
            )
            .child(
                // Footer row: period, note and optional drilldown hint
                h_flex()
                    .justify_between()
                    .gap_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        h_flex()
                            .gap_1()
                            .min_w(gpui_kit::px(0.))
                            .child(
                                div()
                                    .child(self.period_label.clone())
                                    .test_text(self.period_label.clone()),
                            )
                            .when(!self.note.is_empty(), |row| row.child("·"))
                            .child(
                                div()
                                    .truncate()
                                    .child(self.note.clone())
                                    .test_text(self.note.clone()),
                            ),
                    )
                    .when_some(self.drilldown_target.clone(), |row, target| {
                        let hint_label = SharedString::from(format!(
                            "{} -> {}",
                            I18nCatalog::tr("card.drilldown_flows"),
                            target
                        ));
                        row.child(
                            h_flex()
                                .id(drilldown_id)
                                .test_support()
                                .aria_label(hint_label.clone())
                                .flex_none()
                                .gap_0p5()
                                .text_color(cx.theme().link)
                                .child(I18nCatalog::tr("card.drilldown_flows"))
                                .child(Icon::new(IconName::ChevronRight).xsmall())
                                .test_text(hint_label),
                        )
                    }),
            )
    }
}

/// Required honest system states covered by [`StatusBanner`] (`DESIGN.md`, `S-15`, `S-18`, `S-20`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkbenchSystemState {
    EmptyLibrary,
    Syncing {
        detail: String,
    },
    PartialValuation {
        missing: Vec<String>,
    },
    Offline {
        detail: String,
    },
    RateLimitedOrAuthFailed {
        provider: String,
        detail: String,
    },
    SaveFailed {
        detail: String,
    },
    Cancelled {
        stage: String,
    },
    StaleReport {
        report_id: String,
        saved_rev: i64,
        current_rev: i64,
    },
    RestoreFailed {
        detail: String,
    },
    LayoutFallback {
        reason: String,
    },
    ModelUnavailable {
        reason: String,
    },
}

/// Longbridge-styled alert banner rendering explicit cause and next-action button (`DESIGN.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusBanner {
    pub id: SharedString,
    pub state: WorkbenchSystemState,
    pub locale: Locale,
}

impl StatusBanner {
    pub fn new(id: impl Into<SharedString>, state: WorkbenchSystemState) -> Self {
        Self {
            id: id.into(),
            state,
            locale: Locale::ZhCn,
        }
    }

    pub fn with_locale(mut self, locale: Locale) -> Self {
        self.locale = locale;
        self
    }

    /// Returns the short title for the current system state.
    pub fn title(&self) -> &'static str {
        let l = self.locale;
        match &self.state {
            WorkbenchSystemState::EmptyLibrary => I18nCatalog::t(l, "state.empty_library.title"),
            WorkbenchSystemState::Syncing { .. } => I18nCatalog::t(l, "state.syncing.title"),
            WorkbenchSystemState::PartialValuation { .. } => {
                I18nCatalog::t(l, "state.partial_valuation.title")
            }
            WorkbenchSystemState::Offline { .. } => I18nCatalog::t(l, "state.offline.title"),
            WorkbenchSystemState::RateLimitedOrAuthFailed { .. } => {
                I18nCatalog::t(l, "state.auth_or_rate_limit.title")
            }
            WorkbenchSystemState::SaveFailed { .. } => I18nCatalog::t(l, "state.save_failed.title"),
            WorkbenchSystemState::Cancelled { .. } => I18nCatalog::t(l, "state.cancelled.title"),
            WorkbenchSystemState::StaleReport { .. } => {
                I18nCatalog::t(l, "state.stale_report.title")
            }
            WorkbenchSystemState::RestoreFailed { .. } => {
                I18nCatalog::t(l, "state.restore_failed.title")
            }
            WorkbenchSystemState::LayoutFallback { .. } => {
                I18nCatalog::t(l, "state.layout_fallback.title")
            }
            WorkbenchSystemState::ModelUnavailable { .. } => {
                I18nCatalog::t(l, "state.model_unavailable.title")
            }
        }
    }

    /// Returns the detailed honest reason message for the current system state.
    pub fn reason_message(&self) -> String {
        let l = self.locale;
        match &self.state {
            WorkbenchSystemState::EmptyLibrary => {
                I18nCatalog::t(l, "state.empty_library").to_string()
            }
            WorkbenchSystemState::Syncing { detail } => {
                format!("{} ({detail})", I18nCatalog::t(l, "state.syncing"))
            }
            WorkbenchSystemState::PartialValuation { missing } => {
                let joined = if missing.is_empty() {
                    "未纳入项待补齐".to_string()
                } else {
                    missing.join(", ")
                };
                format!(
                    "{} 缺失项: [{joined}]",
                    I18nCatalog::t(l, "state.partial_valuation")
                )
            }
            WorkbenchSystemState::Offline { detail } => {
                format!("{} ({detail})", I18nCatalog::t(l, "state.offline"))
            }
            WorkbenchSystemState::RateLimitedOrAuthFailed { provider, detail } => {
                format!(
                    "{} [{provider}: {detail}]",
                    I18nCatalog::t(l, "state.auth_or_rate_limit")
                )
            }
            WorkbenchSystemState::SaveFailed { detail } => {
                format!("{} ({detail})", I18nCatalog::t(l, "state.save_failed"))
            }
            WorkbenchSystemState::Cancelled { stage } => {
                format!("{} (阶段: {stage})", I18nCatalog::t(l, "state.cancelled"))
            }
            WorkbenchSystemState::StaleReport {
                report_id,
                saved_rev,
                current_rev,
            } => {
                format!(
                    "{} [报告 {report_id}: 生成时修订 r{saved_rev} -> 当前 r{current_rev}]",
                    I18nCatalog::t(l, "state.stale_report")
                )
            }
            WorkbenchSystemState::RestoreFailed { detail } => {
                format!("{} ({detail})", I18nCatalog::t(l, "state.restore_failed"))
            }
            WorkbenchSystemState::LayoutFallback { reason } => {
                format!("{} ({reason})", I18nCatalog::t(l, "state.layout_fallback"))
            }
            WorkbenchSystemState::ModelUnavailable { reason } => {
                format!(
                    "{} ({reason})",
                    I18nCatalog::t(l, "state.model_unavailable")
                )
            }
        }
    }

    /// Returns the localized next-action button label for the current system state.
    pub fn next_action_label(&self) -> &'static str {
        let l = self.locale;
        match &self.state {
            WorkbenchSystemState::EmptyLibrary => I18nCatalog::t(l, "action.open_wizard"),
            WorkbenchSystemState::Syncing { .. } => I18nCatalog::t(l, "action.cancel_sync"),
            WorkbenchSystemState::PartialValuation { .. } => {
                I18nCatalog::t(l, "action.inspect_gaps")
            }
            WorkbenchSystemState::Offline { .. } => I18nCatalog::t(l, "action.retry_sync"),
            WorkbenchSystemState::RateLimitedOrAuthFailed { .. } => {
                I18nCatalog::t(l, "action.configure_credentials")
            }
            WorkbenchSystemState::SaveFailed { .. } => I18nCatalog::t(l, "action.retry_save"),
            WorkbenchSystemState::Cancelled { .. } => I18nCatalog::t(l, "action.restart_task"),
            WorkbenchSystemState::StaleReport { .. } => {
                I18nCatalog::t(l, "action.recompute_report")
            }
            WorkbenchSystemState::RestoreFailed { .. } => {
                I18nCatalog::t(l, "action.restore_backup")
            }
            WorkbenchSystemState::LayoutFallback { .. } => I18nCatalog::t(l, "action.reset_layout"),
            WorkbenchSystemState::ModelUnavailable { .. } => {
                I18nCatalog::t(l, "action.configure_model")
            }
        }
    }

    /// Stable element ID for the banner's next-action button.
    pub fn action_button_id(&self) -> SharedString {
        SharedString::from(format!("{}-action", self.id))
    }

    fn icon_and_colors(&self, cx: &App) -> (IconName, Hsla, Hsla, Hsla) {
        let t = cx.theme();
        match &self.state {
            WorkbenchSystemState::EmptyLibrary | WorkbenchSystemState::Syncing { .. } => {
                (IconName::Info, t.info, t.background, t.info)
            }
            WorkbenchSystemState::PartialValuation { .. }
            | WorkbenchSystemState::Offline { .. }
            | WorkbenchSystemState::Cancelled { .. }
            | WorkbenchSystemState::StaleReport { .. }
            | WorkbenchSystemState::LayoutFallback { .. }
            | WorkbenchSystemState::ModelUnavailable { .. } => {
                (IconName::TriangleAlert, t.warning, t.background, t.warning)
            }
            WorkbenchSystemState::RateLimitedOrAuthFailed { .. }
            | WorkbenchSystemState::SaveFailed { .. }
            | WorkbenchSystemState::RestoreFailed { .. } => {
                (IconName::CircleX, t.danger, t.background, t.danger)
            }
        }
    }

    /// Renders the [`StatusBanner`] as an observed GPUI element.
    pub fn render_banner(&self, cx: &App) -> ObservedStatefulDiv {
        self.render_banner_with_action(cx, |_, _, _| {})
    }

    pub(crate) fn render_banner_with_action(
        &self,
        cx: &App,
        action: impl Fn(&gpui_kit::ClickEvent, &mut gpui_kit::Window, &mut App) + 'static,
    ) -> ObservedStatefulDiv {
        let (icon_name, fg_color, bg_color, border_color) = self.icon_and_colors(cx);
        let title_str = SharedString::from(self.title());
        let reason_str = SharedString::from(self.reason_message());
        let action_label = SharedString::from(self.next_action_label());
        let action_id = self.action_button_id();
        let a11y_text = SharedString::from(format!("{title_str}: {reason_str} [{action_label}]"));

        h_flex()
            .id(self.id.clone())
            .test_support()
            .aria_label(a11y_text)
            .w_full()
            .justify_between()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .border_1()
            .border_color(border_color)
            .rounded(cx.theme().radius)
            .bg(bg_color)
            .child(
                h_flex()
                    .gap_2p5()
                    .flex_1()
                    .child(Icon::new(icon_name).text_color(fg_color).small())
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_xs()
                                    .font_semibold()
                                    .text_color(fg_color)
                                    .child(title_str.clone())
                                    .test_text(title_str),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().foreground)
                                    .child(reason_str.clone())
                                    .test_text(reason_str),
                            ),
                    ),
            )
            .child(
                Button::new(action_id)
                    .secondary()
                    .xsmall()
                    .label(action_label.clone())
                    .on_click(action),
            )
    }
}

/// Custom GPUI K-Line chart renderer over a [`KlineWindowSliceDto`] (`S-16`, `M2-W05`, `M2-K02`).
///
/// Renders:
/// - Top header bar: instrument ID, venue vs trading-pair badge, timeframe, adjustment badge
///   (`raw` / `split_adjusted` / `total_return_adjusted`), timezone badge, incomplete-tail /
///   stale-data / session-gap warning badges, and pinned-vs-latest version drift badge.
/// - Tabular monospace crosshair OHLCV + SMA/EMA/MACD/RSI readout (never showing `0` for warmup `None`).
/// - Main candlestick + SMA/EMA overlay + explicit `"GAP"` columns (`has_gap_before`) + clickable
///   trade/note/plan marker badges (`chart-marker-{id}`) with `.test_support()` (`M2-A-11`).
/// - Volume sub-panel and MACD / RSI sub-panel aligned with the visible window slice.
#[derive(Debug, Clone, PartialEq)]
pub struct KlineChartView {
    pub id: SharedString,
    pub slice: KlineWindowSliceDto,
    pub color_convention: MarketColorConvention,
    pub crosshair_bar_index: Option<usize>,
    pub crosshair_price: Option<f64>,
    pub pinned_dataset_version: Option<i64>,
    pub latest_dataset_version: Option<i64>,
    pub masked_display_symbol: Option<String>,
    pub masked_bar_labels: Option<Vec<String>>,
}

impl KlineChartView {
    pub fn new(
        id: impl Into<SharedString>,
        slice: KlineWindowSliceDto,
        color_convention: MarketColorConvention,
    ) -> Self {
        Self {
            id: id.into(),
            slice,
            color_convention,
            crosshair_bar_index: None,
            crosshair_price: None,
            pinned_dataset_version: None,
            latest_dataset_version: None,
            masked_display_symbol: None,
            masked_bar_labels: None,
        }
    }

    pub fn with_masked_training_labels(
        mut self,
        display_symbol: impl Into<String>,
        bar_labels: Vec<String>,
    ) -> Self {
        self.masked_display_symbol = Some(display_symbol.into());
        self.masked_bar_labels = Some(bar_labels);
        self
    }

    pub fn with_crosshair(mut self, bar_index: Option<usize>) -> Self {
        self.crosshair_bar_index = bar_index;
        self
    }

    pub fn with_crosshair_price(mut self, price: Option<f64>) -> Self {
        self.crosshair_price = price.filter(|value| value.is_finite());
        self
    }

    pub fn with_dataset_versions(mut self, pinned: Option<i64>, latest: Option<i64>) -> Self {
        self.pinned_dataset_version = pinned;
        self.latest_dataset_version = latest;
        self
    }

    /// Returns the deterministic GPUI element ID for a [`ChartMarkerDto`] badge (`M2-A-11`).
    pub fn marker_element_id(marker_id: &str) -> SharedString {
        SharedString::from(format!("chart-marker-{marker_id}"))
    }

    /// Returns the deterministic GPUI element ID for a visual session gap spacer (`M2-A-10`).
    pub fn gap_element_id(&self, start_bar_index: usize) -> SharedString {
        SharedString::from(format!("{}-gap-{start_bar_index}", self.id))
    }

    /// Returns `true` when a saved note's pinned dataset version differs from the latest library version (`M2-A-11`).
    pub fn has_version_drift(&self) -> bool {
        match (self.pinned_dataset_version, self.latest_dataset_version) {
            (Some(pinned), Some(latest)) => pinned != latest,
            _ => false,
        }
    }

    /// Localized adjustment badge text (`raw` / `split_adjusted` / `total_return_adjusted`).
    pub fn adjustment_badge_text(&self) -> &'static str {
        match self.slice.adjustment.as_str() {
            "split_adjusted" => I18nCatalog::tr("kline.adj.split"),
            "total_return_adjusted" => I18nCatalog::tr("kline.adj.total_return"),
            _ => I18nCatalog::tr("kline.adj.raw"),
        }
    }

    /// Explicitly distinguishes trading venue vs instrument/pair (`M2-A-10`, `S-46`).
    pub fn venue_pair_badge_text(&self) -> String {
        if let Some(display_symbol) = &self.masked_display_symbol {
            return format!(
                "{} | 标的: {display_symbol}",
                delta_app::SIMULATED_TRAINING_BADGE
            );
        }
        format!(
            "场所: {} | 标的: {}",
            self.slice.venue, self.slice.instrument_id
        )
    }

    /// Resolves the local index into `self.slice.bars` for the active crosshair or last visible bar.
    pub fn active_local_bar_index(&self) -> Option<usize> {
        if self.slice.bars.is_empty() {
            return None;
        }
        if let Some(global_idx) = self.crosshair_bar_index {
            if global_idx >= self.slice.visible_start_index
                && global_idx < self.slice.visible_end_index
            {
                let local = global_idx - self.slice.visible_start_index;
                if local < self.slice.bars.len() {
                    return Some(local);
                }
            }
        }
        Some(self.slice.bars.len() - 1)
    }

    /// Tabular readout of OHLCV + SMA/EMA/MACD/RSI at the active crosshair bar (`S-16`, `AC-11`).
    /// Warm-up `None` values are explicitly formatted as `"— (预热期)"` and never `0`.
    pub fn crosshair_readout_text(&self) -> String {
        let Some(local_idx) = self.active_local_bar_index() else {
            return "OHLCV: — (无可见 K 线)".to_string();
        };
        let bar = &self.slice.bars[local_idx];
        let global_idx = self.slice.visible_start_index + local_idx;
        let bar_date_or_label = self
            .masked_bar_labels
            .as_ref()
            .and_then(|labels| {
                labels.get(global_idx).cloned().or_else(|| {
                    (labels.len() == self.slice.bars.len())
                        .then(|| labels.get(local_idx).cloned())
                        .flatten()
                })
            })
            .unwrap_or_else(|| {
                if self.masked_display_symbol.is_some() {
                    delta_app::bar_label_for_one_based(global_idx + 1)
                } else {
                    bar.session_date.to_string()
                }
            });
        let mut parts = vec![format!(
            "{bar_date_or_label} | O {} H {} L {} C {} V {}",
            to_canonical_text(bar.open),
            to_canonical_text(bar.high),
            to_canonical_text(bar.low),
            to_canonical_text(bar.close),
            to_canonical_text(bar.volume),
        )];

        for (period, series) in &self.slice.sma_series {
            let val_str = series
                .get(local_idx)
                .and_then(|pt| pt.value)
                .map(|v| format!("{v:.4}"))
                .unwrap_or_else(|| "— (预热期)".to_string());
            parts.push(format!("SMA({period}): {val_str}"));
        }

        for (period, series) in &self.slice.ema_series {
            let val_str = series
                .get(local_idx)
                .and_then(|pt| pt.value)
                .map(|v| format!("{v:.4}"))
                .unwrap_or_else(|| "— (预热期)".to_string());
            parts.push(format!("EMA({period}): {val_str}"));
        }

        if let Some(macd_pt) = self.slice.macd_series.get(local_idx) {
            let dif = macd_pt
                .macd
                .map(|v| format!("{v:.4}"))
                .unwrap_or_else(|| "— (预热期)".to_string());
            let dea = macd_pt
                .signal
                .map(|v| format!("{v:.4}"))
                .unwrap_or_else(|| "— (预热期)".to_string());
            let hist = macd_pt
                .histogram
                .map(|v| format!("{v:.4}"))
                .unwrap_or_else(|| "— (预热期)".to_string());
            parts.push(format!("MACD({dif}, {dea}, {hist})"));
        }

        if let Some(rsi_pt) = self.slice.rsi_series.get(local_idx) {
            let rsi = rsi_pt
                .value
                .map(|v| format!("{v:.2}"))
                .unwrap_or_else(|| "— (预热期)".to_string());
            parts.push(format!("RSI: {rsi}"));
        }

        parts.join("  ")
    }

    fn marker_prefix_and_color(&self, kind: MarkerKind, cx: &App) -> (&'static str, Hsla, Hsla) {
        match kind {
            MarkerKind::BuyFill => {
                let (fg, bg) = self.color_convention.colors(ChangeDirection::Up, cx);
                (I18nCatalog::tr("kline.marker.buy"), fg, bg)
            }
            MarkerKind::SellFill => {
                let (fg, bg) = self.color_convention.colors(ChangeDirection::Down, cx);
                (I18nCatalog::tr("kline.marker.sell"), fg, bg)
            }
            MarkerKind::Note => (
                I18nCatalog::tr("kline.marker.note"),
                cx.theme().info,
                cx.theme().secondary,
            ),
            MarkerKind::Plan => (
                I18nCatalog::tr("kline.marker.plan"),
                cx.theme().warning,
                cx.theme().secondary,
            ),
        }
    }

    fn render_marker_badge(&self, layout: &ChartMarkerLayout, cx: &App) -> ObservedStatefulDiv {
        let marker = &layout.marker;
        let (prefix, fg, bg) = self.marker_prefix_and_color(marker.kind, cx);
        let el_id = Self::marker_element_id(&marker.id);
        let marker_time_label = if self.masked_display_symbol.is_some() {
            delta_app::bar_label_for_one_based(marker.bar_index + 1)
        } else {
            marker.session_date.to_string()
        };
        let badge_text = SharedString::from(format!(
            "{} | {} @ {} ({marker_time_label})",
            prefix,
            marker.label,
            to_canonical_text(marker.price),
        ));

        h_flex()
            .id(el_id)
            .test_support()
            .aria_label(badge_text.clone())
            .gap_1()
            .px_2()
            .py_0p5()
            .border_1()
            .border_color(fg)
            .rounded(cx.theme().radius)
            .bg(bg)
            .text_xs()
            .font_medium()
            .text_color(fg)
            .child(div().child(badge_text.clone()).test_text(badge_text))
    }

    /// Renders the complete K-Line chart view (`S-16`, `M2-W05`, `M2-K02`).
    pub fn render_chart(&self, cx: &App) -> ObservedStatefulDiv {
        self.render_chart_with_bounds(Default::default(), cx)
    }

    pub(crate) fn render_chart_with_bounds(
        &self,
        bounds: crate::chart_canvas::PlotBounds,
        cx: &App,
    ) -> ObservedStatefulDiv {
        let venue_pair_text = SharedString::from(self.venue_pair_badge_text());
        let timeframe_text = SharedString::from(format!("周期: {}", self.slice.timeframe));
        let adj_text = SharedString::from(self.adjustment_badge_text());
        let tz_text = SharedString::from(format!("时区: {}", self.slice.timezone_label));
        let readout_text = SharedString::from(self.crosshair_readout_text());

        // Build clickable marker badges
        let marker_elements: Vec<AnyElement> = self
            .slice
            .markers
            .iter()
            .map(|m| self.render_marker_badge(m, cx).into_any_element())
            .collect();

        v_flex()
            .id(self.id.clone())
            .test_support()
            .aria_label(venue_pair_text.clone())
            .w_full()
            .bg(cx.theme().background)
            .gap_2()
            .child(
                // Top chart header bar: venue vs pair, timeframe, adjustment, timezone, and warning badges
                h_flex()
                    .flex_wrap()
                    .justify_between()
                    .gap_2()
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_1p5()
                            .child(
                                div()
                                    .child(
                                        Tag::secondary()
                                            .small()
                                            .outline()
                                            .child(venue_pair_text.clone()),
                                    )
                                    .test_text(venue_pair_text),
                            )
                            .child(
                                div()
                                    .child(
                                        Tag::secondary()
                                            .small()
                                            .outline()
                                            .child(timeframe_text.clone()),
                                    )
                                    .test_text(timeframe_text),
                            )
                            .child(
                                div()
                                    .child(Tag::info().small().outline().child(adj_text.clone()))
                                    .test_text(adj_text),
                            )
                            .child(
                                div()
                                    .child(
                                        Tag::secondary().small().outline().child(tz_text.clone()),
                                    )
                                    .test_text(tz_text),
                            ),
                    )
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_1p5()
                            .when(self.slice.has_incomplete_tail, |row| {
                                let label =
                                    SharedString::from(I18nCatalog::tr("kline.incomplete_tail"));
                                row.child(
                                    div()
                                        .child(Tag::warning().small().child(label.clone()))
                                        .test_text(label),
                                )
                            })
                            .when(self.slice.is_stale, |row| {
                                let label = SharedString::from(I18nCatalog::tr("kline.stale"));
                                row.child(
                                    div()
                                        .child(Tag::danger().small().child(label.clone()))
                                        .test_text(label),
                                )
                            })
                            .when(!self.slice.gap_indices.is_empty(), |row| {
                                let label = SharedString::from(format!(
                                    "缺口: {} 处 (GAP)",
                                    self.slice.gap_indices.len()
                                ));
                                row.child(
                                    div()
                                        .child(Tag::warning().small().child(label.clone()))
                                        .test_text(label),
                                )
                            })
                            .when(self.has_version_drift(), |row| {
                                let label = SharedString::from(format!(
                                    "{} (r{} vs r{})",
                                    I18nCatalog::tr("kline.version_drift"),
                                    self.pinned_dataset_version.unwrap_or_default(),
                                    self.latest_dataset_version.unwrap_or_default()
                                ));
                                row.child(
                                    div()
                                        .child(Tag::warning().small().child(label.clone()))
                                        .test_text(label),
                                )
                            }),
                    ),
            )
            .child(
                // Crosshair OHLCV + SMA/EMA/MACD/RSI readout bar in tabular monospace numerals
                div()
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().muted)
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_xs()
                    .text_color(cx.theme().foreground)
                    .child(readout_text.clone())
                    .test_text(readout_text),
            )
            .child(
                // Main candlestick + explicit GAP columns area
                v_flex()
                    .w_full()
                    .p_2()
                    .gap_2()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius)
                    .child(div().id("kline-canvas").test_support().w_full().child(
                        crate::chart_canvas::render_canvas(
                            &self.slice,
                            self.color_convention,
                            self.crosshair_bar_index,
                            self.crosshair_price,
                            bounds,
                            cx,
                        ),
                    ))
                    .children(self.slice.gap_indices.iter().map(|&index| {
                        div()
                            .id(self.gap_element_id(index))
                            .test_support()
                            .text_xs()
                            .text_color(cx.theme().warning)
                            .child(I18nCatalog::tr("kline.gap"))
                            .test_text(I18nCatalog::tr("kline.gap"))
                    }))
                    .when(!marker_elements.is_empty(), |area| {
                        area.child(Separator::horizontal())
                            .child(h_flex().flex_wrap().gap_1p5().children(marker_elements))
                    }),
            )
            .child(
                // MACD / RSI sub-panel
                h_flex()
                    .w_full()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius)
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!(
                        "成交量 · MACD · RSI | SMA {:?} · EMA {:?} | 可见窗口 [{}..{}]",
                        self.slice
                            .sma_series
                            .iter()
                            .map(|(p, _)| *p)
                            .collect::<Vec<_>>(),
                        self.slice
                            .ema_series
                            .iter()
                            .map(|(p, _)| *p)
                            .collect::<Vec<_>>(),
                        self.slice.visible_start_index,
                        self.slice.visible_end_index,
                    )),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone, Utc};
    use delta_app::{AdjustmentKind, BarRecord};
    use std::str::FromStr;

    #[test]
    fn test_s15_metric_card_never_fabricates_zero_for_none() {
        let missing_card = MetricCard::new(
            "card-partial-nav",
            I18nCatalog::tr("card.nav"),
            None,
            "USD",
            QualityStatus::Partial,
        );
        assert_eq!(missing_card.formatted_value(), "— (缺失/不可用)");
        assert_eq!(missing_card.formatted_display_line(), "— (缺失/不可用)");
        assert!(!missing_card.formatted_value().contains('0'));

        let missing_pnl = MetricCard::new(
            "card-missing-pnl",
            I18nCatalog::tr("card.total_pnl"),
            None,
            "USD",
            QualityStatus::Unavailable,
        )
        .with_signed_pnl(true);
        assert_eq!(missing_pnl.formatted_display_line(), "— (缺失/不可用)");
        assert!(!missing_pnl.formatted_display_line().contains('0'));

        // AC-02 canonical values: cash 1438, NAV 2068, total P&L +68
        let cash_card = MetricCard::new(
            "card-cash",
            I18nCatalog::tr("card.cash"),
            Some(Decimal::from_str("1438.00").unwrap()),
            "USD",
            QualityStatus::Exact,
        );
        assert_eq!(cash_card.formatted_value(), "1438");
        assert_eq!(cash_card.formatted_display_line(), "1438 USD");

        let nav_card = MetricCard::new(
            "card-nav",
            I18nCatalog::tr("card.nav"),
            Some(Decimal::from_str("2068").unwrap()),
            "USD",
            QualityStatus::Exact,
        );
        assert_eq!(nav_card.formatted_value(), "2068");

        let pnl_card = MetricCard::new(
            "card-total-pnl",
            I18nCatalog::tr("card.total_pnl"),
            Some(Decimal::from_str("68").unwrap()),
            "USD",
            QualityStatus::Exact,
        )
        .with_signed_pnl(true);
        assert_eq!(pnl_card.formatted_value(), "68");
        assert_eq!(pnl_card.formatted_display_line(), "+68 USD (▲ 涨)");
    }

    #[test]
    fn test_s15_status_banner_covers_all_required_states() {
        let states = vec![
            WorkbenchSystemState::EmptyLibrary,
            WorkbenchSystemState::Syncing {
                detail: "NASDAQ:AAPL".to_string(),
            },
            WorkbenchSystemState::PartialValuation {
                missing: vec!["FX:EUR/USD".to_string()],
            },
            WorkbenchSystemState::Offline {
                detail: "network unreachable".to_string(),
            },
            WorkbenchSystemState::RateLimitedOrAuthFailed {
                provider: "tiingo".to_string(),
                detail: "HTTP 429".to_string(),
            },
            WorkbenchSystemState::SaveFailed {
                detail: "disk full".to_string(),
            },
            WorkbenchSystemState::Cancelled {
                stage: "market_sync".to_string(),
            },
            WorkbenchSystemState::StaleReport {
                report_id: "RPT-1".to_string(),
                saved_rev: 2,
                current_rev: 3,
            },
            WorkbenchSystemState::RestoreFailed {
                detail: "checksum mismatch".to_string(),
            },
            WorkbenchSystemState::LayoutFallback {
                reason: "schema version mismatch".to_string(),
            },
            WorkbenchSystemState::ModelUnavailable {
                reason: "not configured".to_string(),
            },
        ];

        for (idx, state) in states.into_iter().enumerate() {
            let banner = StatusBanner::new(format!("banner-{idx}"), state);
            assert!(!banner.title().is_empty());
            assert!(!banner.reason_message().is_empty());
            assert!(!banner.next_action_label().is_empty());
        }
    }

    #[test]
    fn test_s16_kline_chart_view_warmup_gap_and_marker_ids() {
        let d1 = NaiveDate::from_ymd_opt(2026, 1, 2).unwrap();
        let d2 = NaiveDate::from_ymd_opt(2026, 1, 12).unwrap();
        let bars = vec![
            BarRecord {
                id: "B1".to_string(),
                instrument_id: "NASDAQ:AAPL".to_string(),
                timeframe: "1d".to_string(),
                session: "regular".to_string(),
                session_date: d1,
                open_at: Utc.from_utc_datetime(&d1.and_hms_opt(14, 30, 0).unwrap()),
                close_at: Utc.from_utc_datetime(&d1.and_hms_opt(21, 0, 0).unwrap()),
                open: Decimal::from(100),
                high: Decimal::from(105),
                low: Decimal::from(99),
                close: Decimal::from(104),
                volume: Decimal::from(1000),
                source: "test".to_string(),
                adjustment: AdjustmentKind::Raw,
                is_final: true,
                valid_from_version: 1,
                valid_to_version: None,
                raw_payload_hash: None,
            },
            BarRecord {
                id: "B2".to_string(),
                instrument_id: "NASDAQ:AAPL".to_string(),
                timeframe: "1d".to_string(),
                session: "regular".to_string(),
                session_date: d2,
                open_at: Utc.from_utc_datetime(&d2.and_hms_opt(14, 30, 0).unwrap()),
                close_at: Utc.from_utc_datetime(&d2.and_hms_opt(21, 0, 0).unwrap()),
                open: Decimal::from(104),
                high: Decimal::from(110),
                low: Decimal::from(103),
                close: Decimal::from(108),
                volume: Decimal::from(1200),
                source: "test".to_string(),
                adjustment: AdjustmentKind::Raw,
                is_final: true,
                valid_from_version: 1,
                valid_to_version: None,
                raw_payload_hash: None,
            },
        ];

        let markers = vec![ChartMarkerDto {
            id: "M-BUY-1".to_string(),
            kind: MarkerKind::BuyFill,
            session_date: d2,
            bar_index: 1,
            price: Decimal::from(105),
            label: "Buy 10".to_string(),
            target_id: "EV-BUY".to_string(),
            target_revision: 1,
        }];

        let vp = KlineViewport::new(2, 2);
        let slice = vp.build_window_slice(
            "NASDAQ:AAPL",
            "NASDAQ",
            "1d",
            AdjustmentKind::Raw,
            &bars,
            None,
            &markers,
            &[],
            64,
        );

        let chart = KlineChartView::new("kline-main", slice, MarketColorConvention::RedUpGreenDown)
            .with_dataset_versions(Some(1), Some(2));
        assert!(chart.has_version_drift());
        assert_eq!(
            chart.venue_pair_badge_text(),
            "场所: NASDAQ | 标的: NASDAQ:AAPL"
        );
        assert!(chart.crosshair_readout_text().contains("— (预热期)"));
        assert_eq!(
            KlineChartView::marker_element_id("M-BUY-1"),
            SharedString::from("chart-marker-M-BUY-1")
        );
    }

    #[test]
    fn test_m4_masked_bar_labels_with_nonzero_visible_start_index() {
        let base = NaiveDate::from_ymd_opt(2025, 1, 1).unwrap();
        let bars: Vec<BarRecord> = (0..220)
            .map(|idx| {
                let d = base.checked_add_days(chrono::Days::new(idx)).unwrap();
                BarRecord {
                    id: format!("B-{idx}"),
                    instrument_id: "HIDDEN:INSTRUMENT_A".to_string(),
                    timeframe: "1d".to_string(),
                    session: "24x7".to_string(),
                    session_date: d,
                    open_at: Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0).unwrap()),
                    close_at: Utc.from_utc_datetime(&d.and_hms_opt(23, 59, 59).unwrap()),
                    open: Decimal::from(100 + idx),
                    high: Decimal::from(105 + idx),
                    low: Decimal::from(98 + idx),
                    close: Decimal::from(102 + idx),
                    volume: Decimal::from(1000),
                    source: "masked".to_string(),
                    adjustment: AdjustmentKind::Raw,
                    is_final: true,
                    valid_from_version: 1,
                    valid_to_version: None,
                    raw_payload_hash: None,
                }
            })
            .collect();

        // Viewport showing the trailing 160 bars (`60..220`, so `visible_start_index == 60`).
        let vp = KlineViewport::new(220, 160);
        assert_eq!(vp.visible_start, 60);
        assert_eq!(vp.visible_end, 220);

        let slice = vp.build_window_slice(
            "HIDDEN:INSTRUMENT_A",
            "HIDDEN",
            "1d",
            AdjustmentKind::Raw,
            &bars,
            None,
            &[],
            &[],
            160,
        );
        assert_eq!(slice.visible_start_index, 60);
        assert_eq!(slice.bars.len(), 160);

        let global_labels: Vec<String> =
            (1..=220).map(delta_app::bar_label_for_one_based).collect();

        // Default crosshair (latest visible bar `local_idx = 159`, `global_idx = 219`) MUST read `"第 220 根"`!
        let chart_latest = KlineChartView::new(
            "trn-kline",
            slice.clone(),
            MarketColorConvention::RedUpGreenDown,
        )
        .with_masked_training_labels("隐藏标的 #1（加密现货）", global_labels.clone());
        let latest_readout = chart_latest.crosshair_readout_text();
        assert!(
            latest_readout.starts_with("第 220 根 | "),
            "unexpected readout: {latest_readout}"
        );
        assert!(!latest_readout.starts_with("第 160 根"));

        // Explicit crosshair at `global_idx = 60` (`local_idx = 0`) MUST read `"第 61 根"`!
        let chart_start =
            KlineChartView::new("trn-kline", slice, MarketColorConvention::RedUpGreenDown)
                .with_masked_training_labels("隐藏标的 #1（加密现货）", global_labels)
                .with_crosshair(Some(60));
        let start_readout = chart_start.crosshair_readout_text();
        assert!(
            start_readout.starts_with("第 61 根 | "),
            "unexpected readout: {start_readout}"
        );
    }
}

//! M5 chart-trading workbench interaction model (`S-54`).
//!
//! Everything here is pure and headless-testable: keys, tools, playback, draw-to-order drafts,
//! default proportions and drag-to-modify. The rule enforced by construction: **no single
//! click, drag, playback step or single key produces an order submission or a quick fill**.
//! Only `confirm_draft` (an explicit confirm click or `Ctrl+Enter`) and the quick-trade buttons
//! (mouse click handlers) create trading actions.

use delta_app::{
    OrderSide, SimEvent, SimOrderType, TimeInForce, TrainingDrawingDto, TrainingOrderRequestDto,
    TrainingStepReceiptDto,
};
use rust_decimal::prelude::*;

/// Continuous-play speeds in milliseconds per bar.
pub const PLAY_SPEEDS_MS: [u64; 4] = [1000, 600, 350, 150];
/// Default buy proportions of account equity, default 1/10.
pub const BUY_FRACTIONS: [(u32, &str); 6] = [
    (1, "全仓"),
    (2, "1/2"),
    (3, "1/3"),
    (4, "1/4"),
    (5, "1/5"),
    (10, "1/10"),
];
pub const SELL_FRACTIONS: [(u32, &str); 4] = [(1, "100%"), (2, "1/2"), (3, "1/3"), (4, "1/4")];
pub const MA_PERIODS: [usize; 4] = [5, 10, 20, 60];
pub const DRAWING_COLORS: [&str; 5] = ["#f0b90b", "#1e88e5", "#e53935", "#43a047", "#8e24aa"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkbenchTool {
    #[default]
    None,
    Ray,
    Horizontal,
    Segment,
    Measure,
    /// Placing a horizontal line opens an order draft (never submits by itself).
    DrawOrder,
}

impl WorkbenchTool {
    pub fn drawing_kind(self) -> Option<&'static str> {
        match self {
            Self::Ray => Some("ray"),
            Self::Horizontal | Self::DrawOrder => Some("horizontal"),
            Self::Segment => Some("segment"),
            _ => None,
        }
    }

    pub fn anchors_needed(self) -> usize {
        match self {
            Self::Horizontal | Self::DrawOrder => 1,
            Self::Ray | Self::Segment | Self::Measure => 2,
            Self::None => 0,
        }
    }
}

/// Legal order options for a line placed relative to the latest close (`S-54` 画线下单).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftOption {
    /// Below the close.
    BuyLimit,
    SellStop,
    /// Above the close.
    BuyStop,
    SellLimit,
}

impl DraftOption {
    pub fn label(self) -> &'static str {
        match self {
            Self::BuyLimit => "买入限价",
            Self::SellStop => "卖出止损",
            Self::BuyStop => "买入突破",
            Self::SellLimit => "卖出限价",
        }
    }

    pub fn request(self, price: Decimal, quantity: Decimal) -> TrainingOrderRequestDto {
        let (side, order_type) = match self {
            Self::BuyLimit => (OrderSide::Buy, SimOrderType::Limit { limit_price: price }),
            Self::SellLimit => (OrderSide::Sell, SimOrderType::Limit { limit_price: price }),
            Self::BuyStop => (
                OrderSide::Buy,
                SimOrderType::StopMarket { stop_price: price },
            ),
            Self::SellStop => (
                OrderSide::Sell,
                SimOrderType::StopMarket { stop_price: price },
            ),
        };
        TrainingOrderRequestDto::single(side, order_type, quantity, TimeInForce::Gtc, None)
    }
}

pub fn legal_draft_options(price: Decimal, last_close: Decimal) -> Vec<DraftOption> {
    if price < last_close {
        vec![DraftOption::BuyLimit, DraftOption::SellStop]
    } else if price > last_close {
        vec![DraftOption::BuyStop, DraftOption::SellLimit]
    } else {
        Vec::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderDraft {
    pub price: Decimal,
    pub options: Vec<DraftOption>,
    pub selected: Option<DraftOption>,
    pub quantity: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MeasureState {
    pub start: Option<(i64, Decimal)>,
    pub end: Option<(i64, Decimal)>,
}

impl MeasureState {
    /// (price diff, percent, bars) between the two points.
    pub fn result(&self) -> Option<(Decimal, Decimal, i64)> {
        let ((b0, p0), (b1, p1)) = (self.start?, self.end?);
        let diff = p1 - p0;
        let pct = if p0.is_zero() {
            Decimal::ZERO
        } else {
            (diff / p0 * Decimal::from(100)).round_dp(2)
        };
        Some((diff, pct, b1 - b0))
    }

    /// Text for the overlay; never contains a date (hidden mode safe by construction).
    pub fn label(&self) -> Option<String> {
        let (diff, pct, bars) = self.result()?;
        Some(format!(
            "{} ({}%) · {} 根",
            delta_app::to_canonical_text(diff),
            delta_app::to_canonical_text(pct),
            bars
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Playback {
    pub playing: bool,
    pub speed_index: usize,
    /// A step is being committed; ticks never queue another one.
    pub in_flight: bool,
    pub last_pause_reason: Option<String>,
}

impl Default for Playback {
    fn default() -> Self {
        Self {
            playing: false,
            speed_index: 1,
            in_flight: false,
            last_pause_reason: None,
        }
    }
}

impl Playback {
    pub fn interval_ms(&self) -> u64 {
        PLAY_SPEEDS_MS[self.speed_index.min(PLAY_SPEEDS_MS.len() - 1)]
    }

    /// One timer tick: `true` = start exactly one atomic advance now.
    pub fn tick(&mut self) -> bool {
        if !self.playing || self.in_flight {
            return false;
        }
        self.in_flight = true;
        true
    }

    /// After a step completes: auto-pause on fills, triggers, rejections, end of run or error.
    pub fn on_step_done(&mut self, receipt: Option<&TrainingStepReceiptDto>, error: bool) {
        self.in_flight = false;
        let reason = if error {
            Some("出错已暂停")
        } else if let Some(r) = receipt {
            if r.run.status != delta_app::SimRunStatus::Active {
                Some("训练结束")
            } else if r
                .events
                .iter()
                .any(|e| matches!(e, SimEvent::OrderFilled { .. }))
            {
                // Stop orders fill on trigger in this engine, so a trigger always shows as a fill.
                Some("有成交或触发，已自动暂停")
            } else if r
                .events
                .iter()
                .any(|e| matches!(e, SimEvent::OrderRejected { .. }))
            {
                Some("有拒单，已自动暂停")
            } else {
                None
            }
        } else {
            None
        };
        if let Some(reason) = reason {
            if self.playing {
                self.playing = false;
                self.last_pause_reason = Some(reason.to_string());
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkbenchState {
    pub tool: WorkbenchTool,
    pub playback: Playback,
    pub pending_anchors: Vec<(i64, Decimal)>,
    pub drawings: Vec<TrainingDrawingDto>,
    pub selected_drawing: Option<String>,
    pub measure: MeasureState,
    pub draft: Option<OrderDraft>,
    pub draw_order_enabled: bool,
    pub locked_orders: std::collections::BTreeSet<String>,
    pub buy_fraction: u32,
    pub sell_fraction: u32,
    pub ma_enabled: [bool; 4],
    pub show_volume: bool,
    pub show_drawings: bool,
    pub crosshair_enabled: bool,
    pub pending_confirm: Option<ConfirmKind>,
    pub drawing_color: usize,
    pub drawing_style: &'static str,
    /// Higher-timeframe view selected (`None` = the run's own timeframe).
    pub view_timeframe: Option<delta_app::Timeframe>,
    pub manual_price_scale: bool,
    /// An order line grabbed on mouse-down: (order id, original price at grab, live preview price).
    pub dragging_order: Option<(String, Decimal, Decimal)>,
}

impl Default for WorkbenchState {
    fn default() -> Self {
        Self {
            tool: WorkbenchTool::None,
            playback: Playback::default(),
            pending_anchors: Vec::new(),
            drawings: Vec::new(),
            selected_drawing: None,
            measure: MeasureState::default(),
            draft: None,
            draw_order_enabled: false,
            locked_orders: Default::default(),
            buy_fraction: 10,
            sell_fraction: 1,
            ma_enabled: [true, true, true, true],
            show_volume: true,
            show_drawings: true,
            crosshair_enabled: true,
            pending_confirm: None,
            drawing_color: 0,
            drawing_style: "solid",
            view_timeframe: None,
            manual_price_scale: false,
            dragging_order: None,
        }
    }
}

/// Destructive workbench actions that require a confirmation step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmKind {
    NewRun,
    Restart,
    ClearDrawings,
}

/// What a key press asks the workbench to do. There is deliberately no variant that submits an
/// order or a quick fill except `ConfirmDraft`, which only exists when a draft is open and the
/// user pressed `Ctrl+Enter`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkbenchCommand {
    Advance,
    TogglePlay,
    SelectTool(WorkbenchTool),
    DeleteSelectedDrawing(String),
    Undo,
    ConfirmDraft,
    Cancel,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct KeyMods {
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
    pub platform: bool,
}

impl KeyMods {
    fn none(self) -> bool {
        !self.control && !self.alt && !self.shift && !self.platform
    }
}

/// Map a key press to a workbench command. `input_focused` = a text input holds focus: then
/// `N`/`→`/`Space`/`Q`/`W`/`E`/`Delete` do nothing (the input gets the key).
pub fn command_for_key(
    state: &WorkbenchState,
    key: &str,
    mods: KeyMods,
    input_focused: bool,
) -> Option<WorkbenchCommand> {
    let k = key.to_ascii_lowercase();
    if k == "escape" && mods.none() {
        return Some(WorkbenchCommand::Cancel);
    }
    if mods.control && !mods.alt && !mods.shift && !mods.platform {
        return match k.as_str() {
            "z" if !input_focused => Some(WorkbenchCommand::Undo),
            "enter" if state.draft.as_ref().is_some_and(|d| d.selected.is_some()) => {
                Some(WorkbenchCommand::ConfirmDraft)
            }
            _ => None,
        };
    }
    if input_focused || !mods.none() {
        return None;
    }
    match k.as_str() {
        "n" | "right" => Some(WorkbenchCommand::Advance),
        "space" | " " => Some(WorkbenchCommand::TogglePlay),
        "q" => Some(WorkbenchCommand::SelectTool(WorkbenchTool::Ray)),
        "w" => Some(WorkbenchCommand::SelectTool(WorkbenchTool::Horizontal)),
        "e" => Some(WorkbenchCommand::SelectTool(WorkbenchTool::Segment)),
        "delete" | "backspace" => state
            .selected_drawing
            .clone()
            .map(WorkbenchCommand::DeleteSelectedDrawing),
        _ => None,
    }
}

/// Result of a chart click while a tool is active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChartClickOutcome {
    Nothing,
    ToggleCrosshair,
    /// A completed drawing to save (not an order).
    SaveDrawing(TrainingDrawingDto),
    MeasureUpdated,
    /// An order draft popped up; submission still needs an explicit confirm.
    DraftOpened,
}

impl WorkbenchState {
    pub fn select_tool(&mut self, tool: WorkbenchTool) {
        self.tool = if self.tool == tool {
            WorkbenchTool::None
        } else {
            tool
        };
        self.pending_anchors.clear();
        if tool == WorkbenchTool::Measure {
            self.measure = MeasureState::default();
        }
    }

    /// A click on the chart at (bar label index, price). Never trades.
    pub fn chart_click(
        &mut self,
        bar: i64,
        price: Decimal,
        last_close: Decimal,
        default_qty: Decimal,
    ) -> ChartClickOutcome {
        let tool = if self.draw_order_enabled && self.tool == WorkbenchTool::None {
            WorkbenchTool::DrawOrder
        } else {
            self.tool
        };
        match tool {
            WorkbenchTool::None => {
                self.crosshair_enabled = !self.crosshair_enabled;
                ChartClickOutcome::ToggleCrosshair
            }
            WorkbenchTool::Measure => {
                if self.measure.start.is_none() || self.measure.end.is_some() {
                    self.measure = MeasureState {
                        start: Some((bar, price)),
                        end: None,
                    };
                } else {
                    self.measure.end = Some((bar, price));
                }
                ChartClickOutcome::MeasureUpdated
            }
            WorkbenchTool::DrawOrder => {
                if price <= Decimal::ZERO {
                    return ChartClickOutcome::Nothing;
                }
                let options = legal_draft_options(price, last_close);
                if options.is_empty() {
                    return ChartClickOutcome::Nothing;
                }
                self.draft = Some(OrderDraft {
                    price,
                    selected: None,
                    options,
                    quantity: default_qty,
                });
                ChartClickOutcome::DraftOpened
            }
            drawing => {
                self.pending_anchors.push((bar, price));
                if self.pending_anchors.len() < drawing.anchors_needed() {
                    return ChartClickOutcome::Nothing;
                }
                let anchors = std::mem::take(&mut self.pending_anchors);
                ChartClickOutcome::SaveDrawing(TrainingDrawingDto {
                    drawing_id: String::new(),
                    kind: drawing.drawing_kind().unwrap_or("horizontal").to_string(),
                    anchors,
                    color: DRAWING_COLORS[self.drawing_color % DRAWING_COLORS.len()].to_string(),
                    line_style: self.drawing_style.to_string(),
                })
            }
        }
    }

    /// The draft's order request once the user picked an option and confirmed.
    pub fn confirm_draft(&mut self) -> Option<TrainingOrderRequestDto> {
        let draft = self.draft.take()?;
        let Some(option) = draft.selected else {
            self.draft = Some(draft);
            return None;
        };
        Some(option.request(draft.price, draft.quantity))
    }

    /// Releasing a dragged order line: a modify request unless the order is locked or unchanged.
    pub fn drag_release(
        &self,
        order_id: &str,
        old_price: Decimal,
        new_price: Decimal,
        tick: Decimal,
    ) -> Option<(String, Decimal)> {
        if self.locked_orders.contains(order_id) {
            return None;
        }
        let rounded = round_to_tick(new_price, tick);
        (rounded != old_price && rounded > Decimal::ZERO).then(|| (order_id.to_string(), rounded))
    }
}

pub fn round_to_tick(price: Decimal, tick: Decimal) -> Decimal {
    if tick <= Decimal::ZERO {
        return price;
    }
    ((price / tick).round() * tick).normalize()
}

/// Default buy quantity: `equity / fraction / price`, floored to `lot_size`.
pub fn buy_quantity(equity: Decimal, fraction: u32, price: Decimal, lot: Decimal) -> Decimal {
    if equity <= Decimal::ZERO || price <= Decimal::ZERO || fraction == 0 {
        return Decimal::ZERO;
    }
    floor_to_lot(equity / Decimal::from(fraction) / price, lot)
}

/// Default sell quantity: `position / fraction`, floored to `lot_size`.
pub fn sell_quantity(position: Decimal, fraction: u32, lot: Decimal) -> Decimal {
    if position <= Decimal::ZERO || fraction == 0 {
        return Decimal::ZERO;
    }
    floor_to_lot(position / Decimal::from(fraction), lot)
}

pub fn floor_to_lot(qty: Decimal, lot: Decimal) -> Decimal {
    if qty <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    if lot <= Decimal::ZERO {
        return qty;
    }
    ((qty / lot).floor() * lot).normalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        delta_app::parse_decimal_lossless(s).unwrap()
    }

    #[test]
    fn no_key_or_click_ever_trades() {
        let mut st = WorkbenchState::default();
        let none = KeyMods::default();
        for key in [
            "n", "right", "space", "q", "w", "e", "b", "s", "enter", "delete", "x", "c", "1",
        ] {
            let cmd = command_for_key(&st, key, none, false);
            assert!(
                !matches!(cmd, Some(WorkbenchCommand::ConfirmDraft)),
                "{key} must not submit"
            );
        }
        // Ctrl+Enter without an open, selected draft does nothing.
        let ctrl = KeyMods {
            control: true,
            ..Default::default()
        };
        assert_eq!(command_for_key(&st, "enter", ctrl, false), None);
        // Clicking the chart with draw-to-order only opens a draft.
        st.draw_order_enabled = true;
        assert_eq!(
            st.chart_click(10, d("90"), d("100"), d("1")),
            ChartClickOutcome::DraftOpened
        );
        assert_eq!(st.confirm_draft(), None, "no option picked yet");
        assert!(st.draft.is_some());
        assert_eq!(command_for_key(&st, "enter", ctrl, false), None);
        st.draft.as_mut().unwrap().selected = Some(DraftOption::BuyLimit);
        assert_eq!(
            command_for_key(&st, "enter", ctrl, false),
            Some(WorkbenchCommand::ConfirmDraft)
        );
        let req = st.confirm_draft().unwrap();
        assert_eq!(
            req.kind,
            delta_app::OrderSubmitKind::Single {
                side: OrderSide::Buy,
                order_type: SimOrderType::Limit {
                    limit_price: d("90")
                },
                attached_bracket: None
            }
        );
        // Playback ticks only ever advance.
        st.playback.playing = true;
        assert!(st.playback.tick());
        assert!(!st.playback.tick(), "no queued second step while in flight");
    }

    #[test]
    fn focused_input_swallows_workbench_keys() {
        let st = WorkbenchState::default();
        for key in ["n", "right", "space", "q", "w", "e", "delete"] {
            assert_eq!(
                command_for_key(&st, key, KeyMods::default(), true),
                None,
                "{key}"
            );
        }
        assert_eq!(
            command_for_key(&st, "n", KeyMods::default(), false),
            Some(WorkbenchCommand::Advance)
        );
        assert_eq!(
            command_for_key(&st, "space", KeyMods::default(), false),
            Some(WorkbenchCommand::TogglePlay)
        );
    }

    #[test]
    fn draft_options_follow_line_position() {
        assert_eq!(
            legal_draft_options(d("90"), d("100")),
            vec![DraftOption::BuyLimit, DraftOption::SellStop]
        );
        assert_eq!(
            legal_draft_options(d("110"), d("100")),
            vec![DraftOption::BuyStop, DraftOption::SellLimit]
        );
        assert!(legal_draft_options(d("100"), d("100")).is_empty());
    }

    #[test]
    fn proportions_floor_to_lot_and_drag_respects_lock() {
        // 1/10 of 100000 at 43210.5 = 0.231424..., lot 0.00001 -> 0.23142
        assert_eq!(
            buy_quantity(d("100000"), 10, d("43210.5"), d("0.00001")),
            d("0.23142")
        );
        assert_eq!(buy_quantity(d("1000"), 1, d("3"), d("1")), d("333"));
        assert_eq!(sell_quantity(d("0.23142"), 3, d("0.00001")), d("0.07714"));
        let mut st = WorkbenchState::default();
        assert_eq!(
            st.drag_release("O1", d("100"), d("101.237"), d("0.01")),
            Some(("O1".to_string(), d("101.24")))
        );
        assert_eq!(
            st.drag_release("O1", d("100"), d("100.001"), d("0.01")),
            None
        );
        st.locked_orders.insert("O1".into());
        assert_eq!(st.drag_release("O1", d("100"), d("105"), d("0.01")), None);
    }

    #[test]
    fn drawings_need_their_anchor_count_and_measure_has_no_dates() {
        let mut st = WorkbenchState::default();
        st.select_tool(WorkbenchTool::Segment);
        assert_eq!(
            st.chart_click(1, d("10"), d("12"), d("1")),
            ChartClickOutcome::Nothing
        );
        match st.chart_click(5, d("15"), d("12"), d("1")) {
            ChartClickOutcome::SaveDrawing(dr) => {
                assert_eq!(dr.kind, "segment");
                assert_eq!(dr.anchors, vec![(1, d("10")), (5, d("15"))]);
            }
            other => panic!("{other:?}"),
        }
        st.select_tool(WorkbenchTool::Measure);
        st.chart_click(3, d("100"), d("100"), d("1"));
        st.chart_click(13, d("110"), d("100"), d("1"));
        assert_eq!(st.measure.label().unwrap(), "10 (10%) · 10 根");
        // Tool toggles off on second selection.
        st.select_tool(WorkbenchTool::Measure);
        assert_eq!(st.tool, WorkbenchTool::None);
    }

    #[test]
    fn non_positive_quantity_and_order_price_inputs_are_guarded() {
        assert_eq!(buy_quantity(d("-100"), 1, d("10"), d("1")), Decimal::ZERO);
        assert_eq!(buy_quantity(d("100"), 1, d("-10"), d("1")), Decimal::ZERO);
        assert_eq!(sell_quantity(d("-5"), 1, d("1")), Decimal::ZERO);
        assert_eq!(floor_to_lot(d("-5"), d("1")), Decimal::ZERO);
        let mut st = WorkbenchState {
            draw_order_enabled: true,
            ..Default::default()
        };
        assert_eq!(
            st.chart_click(1, d("0"), d("100"), d("1")),
            ChartClickOutcome::Nothing
        );
        assert_eq!(
            st.chart_click(1, d("-5"), d("100"), d("1")),
            ChartClickOutcome::Nothing
        );
        assert!(st.draft.is_none());
    }
}

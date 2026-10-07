//! Chart canvas (POC-02): candlesticks + volume + MA20, wheel zoom, drag
//! pan, crosshair. Input comes from service data or explicit synthetic demo.

use std::{cell::Cell, rc::Rc};

use gpui_kit::gpui::{
    canvas, div, fill, hsla, point, px, size as gsize, Bounds, Entity, InteractiveElement,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, ScrollWheelEvent, Size,
    Styled,
};

#[derive(Debug, Clone, Copy)]
pub struct Candle {
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

/// Deterministic synthetic series (xorshift), so results are reproducible.
pub fn synthetic_candles(n: usize) -> Vec<Candle> {
    let mut out = Vec::with_capacity(n);
    let mut price = 100.0f64;
    let mut seed: u64 = 0x9E3779B97F4A7C15;
    for _ in 0..n {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let r = ((seed % 2000) as f64 - 1000.0) / 100_000.0;
        let open = price;
        let close = price * (1.0 + r + 0.0002);
        let high = open.max(close) * 1.004;
        let low = open.min(close) * 0.996;
        let volume = 100_000.0 + (seed % 50_000) as f64;
        out.push(Candle {
            open,
            high,
            low,
            close,
            volume,
        });
        price = close;
    }
    out
}

/// Simple moving average of closes over the whole series: `None` until
/// `period` closes exist (warmup), so the viewport never restarts warmup.
pub fn sma_close(candles: &[Candle], period: usize) -> Vec<Option<f64>> {
    let mut out = vec![None; candles.len()];
    if period == 0 {
        return out;
    }
    let mut run = 0.0;
    for (i, c) in candles.iter().enumerate() {
        run += c.close;
        if i >= period {
            run -= candles[i - period].close;
        }
        if i + 1 >= period {
            out[i] = Some(run / period as f64);
        }
    }
    out
}

pub const MA_PERIOD: usize = 20;

#[derive(Debug, Clone)]
pub struct ChartState {
    pub candles: Vec<Candle>,
    /// MA20 over the full series, indexed like `candles`.
    pub ma: Vec<Option<f64>>,
    pub offset: usize,
    /// Earlier bars are MA warmup only; never pan/zoom into this prefix.
    pub min_offset: usize,
    pub visible: usize,
    pub cursor: Option<(f32, f32)>,
    pub drag: Option<(f32, usize)>,
    /// Last hover info line shown above the chart.
    pub hover_text: String,
    pub dates: Vec<String>,
    pub markers: Vec<(usize, String)>,
}

impl ChartState {
    pub fn new(n: usize) -> Self {
        let candles = synthetic_candles(n);
        Self::from_candles(candles)
    }

    pub fn from_candles(candles: Vec<Candle>) -> Self {
        let n = candles.len();
        let ma = sma_close(&candles, MA_PERIOD);
        Self {
            candles,
            ma,
            offset: 0,
            min_offset: 0,
            visible: 1000.min(n),
            cursor: None,
            drag: None,
            hover_text: String::new(),
            dates: Vec::new(),
            markers: Vec::new(),
        }
    }

    pub fn zoom(&mut self, direction: f32) {
        let factor = if direction > 0. { 0.8 } else { 1.25 };
        self.zoom_by(factor);
    }
    pub fn constrain_view(&mut self) {
        self.visible = self
            .visible
            .min(self.candles.len().saturating_sub(self.min_offset));
        self.offset = self
            .offset
            .max(self.min_offset)
            .min(self.candles.len().saturating_sub(self.visible));
    }
    fn zoom_by(&mut self, factor: f32) {
        let (visible, offset) = zoom_window(
            self.candles.len().saturating_sub(self.min_offset),
            self.visible,
            self.offset.saturating_sub(self.min_offset),
            factor,
        );
        self.visible = visible;
        self.offset = offset + self.min_offset;
        self.constrain_view();
    }
}

fn plot_y(price: f64, min_p: f64, span: f64, plot_h: f32) -> f32 {
    let t = ((price - min_p) / span) as f32;
    plot_h * (1.0 - t.clamp(0.0, 1.0))
}

type ChartBounds = Rc<Cell<Option<Bounds<Pixels>>>>;

pub struct ChartEvidence(pub String);
impl gpui_kit::gpui::EventEmitter<ChartEvidence> for ChartState {}

pub fn interactive_chart(state: Entity<ChartState>) -> impl gpui_kit::gpui::IntoElement {
    let geometry = Rc::new(Cell::new(None));
    attach_interactions(
        chart_element(state.clone(), geometry.clone()),
        state,
        geometry,
    )
}

fn chart_element(
    state: Entity<ChartState>,
    geometry: ChartBounds,
) -> impl gpui_kit::gpui::IntoElement {
    let state_prepaint = state.clone();
    let state_paint = state.clone();
    canvas(
        // Prepainted view data: what the paint pass needs.
        move |bounds: Bounds<Pixels>, _window, _cx| {
            geometry.set(Some(bounds));
            let s = state_prepaint.read(_cx);
            (s.offset, s.visible, bounds.size)
        },
        move |bounds: Bounds<Pixels>, view: (usize, usize, Size<Pixels>), window, cx| {
            // R1-A-24 probe: CPU time of this paint pass, recorded on every exit.
            let _frame = crate::perf::FrameTimer::start();
            let s = state_paint.read(cx);
            let (offset, visible, _size) = view;
            let plot_w = bounds.size.width.to_f64() as f32;
            let plot_h = bounds.size.height.to_f64() as f32 * 0.78;
            if s.candles.is_empty() || offset >= s.candles.len() {
                return;
            }
            let end = (offset + visible).min(s.candles.len());
            let slice = &s.candles[offset..end];
            let vis = slice.len().max(1);
            let min_p = slice.iter().map(|c| c.low).fold(f64::INFINITY, f64::min);
            let max_p = slice
                .iter()
                .map(|c| c.high)
                .fold(f64::NEG_INFINITY, f64::max);
            let span = (max_p - min_p).max(0.0001);
            let max_vol = slice
                .iter()
                .map(|c| c.volume)
                .fold(0.0f64, f64::max)
                .max(0.0001);
            let w = plot_w / vis as f32;
            let left = bounds.left().to_f64() as f32;
            let top = bounds.top().to_f64() as f32;

            // Quiet grid shared by the workbench and full chart view.
            for line in 0..6 {
                window.paint_quad(fill(
                    Bounds::new(
                        point(px(left), px(top + plot_h * line as f32 / 5.)),
                        gsize(px(plot_w), px(1.)),
                    ),
                    hsla(220. / 360., 0.2, 0.93, 1.0),
                ));
            }

            // Crosshair.
            if let Some((x, y)) = s.cursor {
                window.paint_quad(fill(
                    Bounds::new(
                        point(px(left + x), px(top)),
                        gsize(px(1.), px(bounds.size.height.to_f64() as f32)),
                    ),
                    hsla(0., 0., 0.55, 0.55),
                ));
                window.paint_quad(fill(
                    Bounds::new(point(px(left), px(top + y)), gsize(px(plot_w), px(1.))),
                    hsla(0., 0., 0.55, 0.55),
                ));
            }

            for (i, c) in slice.iter().enumerate() {
                let x = left + i as f32 * w;
                let up = c.close >= c.open;
                let color = if up {
                    hsla(145. / 360., 0.65, 0.45, 1.0)
                } else {
                    hsla(28. / 360., 0.95, 0.55, 1.0)
                };
                let y_high = plot_y(c.high, min_p, span, plot_h);
                let y_low = plot_y(c.low, min_p, span, plot_h);
                window.paint_quad(fill(
                    Bounds::new(
                        point(px(x + w * 0.45), px(top + y_high)),
                        gsize(px(1.), px((y_low - y_high).max(1.))),
                    ),
                    color,
                ));
                let y_open = plot_y(c.open, min_p, span, plot_h);
                let y_close = plot_y(c.close, min_p, span, plot_h);
                let body_top = y_open.min(y_close);
                let body_h = (y_open - y_close).abs().max(1.0);
                window.paint_quad(fill(
                    Bounds::new(
                        point(px(x + w * 0.1), px(top + body_top)),
                        gsize(px((w * 0.8).max(1.)), px(body_h)),
                    ),
                    color,
                ));
                let total_h = bounds.size.height.to_f64() as f32;
                let vh = (c.volume / max_vol) as f32 * (total_h - plot_h) * 0.9;
                window.paint_quad(fill(
                    Bounds::new(
                        point(px(x + w * 0.1), px(top + total_h - vh)),
                        gsize(px((w * 0.8).max(1.)), px(vh)),
                    ),
                    color.opacity(0.45),
                ));
            }

            for (index, _) in &s.markers {
                if *index >= offset && *index < end {
                    let x = left + (*index - offset) as f32 * w + w / 2.;
                    window.paint_quad(fill(
                        Bounds::new(point(px(x - 3.), px(top + 8.)), gsize(px(6.), px(12.))),
                        hsla(30. / 360., 0.95, 0.5, 1.),
                    ));
                }
            }
            // MA20 polyline: segments join consecutive MA points; values
            // come from the full series, so panning keeps them stable.
            for i in 1..slice.len() {
                let (Some(ma_a), Some(ma_b)) = (s.ma[offset + i - 1], s.ma[offset + i]) else {
                    continue;
                };
                let xa = left + (i - 1) as f32 * w + w / 2.;
                let xb = left + i as f32 * w + w / 2.;
                let steps = (((xb - xa).abs() / 4.).ceil() as usize).max(1);
                for k in 0..steps {
                    let t0 = k as f32 / steps as f32;
                    let t1 = (k + 1) as f32 / steps as f32;
                    let px0 = xa + (xb - xa) * t0;
                    let px1 = xa + (xb - xa) * t1;
                    let py0 = plot_y(lerp(ma_a, ma_b, t0 as f64), min_p, span, plot_h);
                    let py1 = plot_y(lerp(ma_a, ma_b, t1 as f64), min_p, span, plot_h);
                    let top_line = py0.min(py1);
                    let h = (py1 - py0).abs().max(1.2);
                    window.paint_quad(fill(
                        Bounds::new(
                            point(px(px0), px(top + top_line)),
                            gsize(px((px1 - px0).abs().max(1.)), px(h)),
                        ),
                        hsla(30. / 360., 0.95, 0.55, 0.9),
                    ));
                }
            }
        },
    )
    .size_full()
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

/// Attach wheel/drag/crosshair interactions around the canvas element.
fn attach_interactions(
    el: impl gpui_kit::gpui::IntoElement,
    state: Entity<ChartState>,
    geometry: ChartBounds,
) -> impl gpui_kit::gpui::IntoElement {
    let state_move = state.clone();
    let state_down = state.clone();
    let state_up = state.clone();
    let state_wheel = state.clone();
    let state_up_out = state;
    let geometry_move = geometry.clone();
    let geometry_down = geometry;
    div()
        .size_full()
        .child(el)
        .on_mouse_move(move |ev: &MouseMoveEvent, window, cx| {
            let Some(bounds) = geometry_move.get() else {
                return;
            };
            let pos = (
                f32::from(ev.position.x - bounds.left()),
                f32::from(ev.position.y - bounds.top()),
            );
            let width = f32::from(bounds.size.width).max(1.);
            let _ = window;
            state_move.update(cx, |st, cx| {
                st.cursor = Some(pos);
                if let Some((start_x, start_offset)) = st.drag {
                    let dx = pos.0 - start_x;
                    st.offset = pan_offset(st.candles.len(), st.visible, start_offset, dx, width);
                    st.constrain_view();
                }
                if let Some((idx, c)) = st.hovered_info(pos, width) {
                    st.hover_text = format!(
                        "{} O {:+.2} H {:+.2} L {:+.2} C {:+.2}",
                        st.dates
                            .get(idx)
                            .cloned()
                            .unwrap_or_else(|| format!("#{idx}")),
                        c.open,
                        c.high,
                        c.low,
                        c.close
                    );
                }
                cx.notify();
            });
        })
        .on_mouse_down(
            gpui_kit::gpui::MouseButton::Left,
            move |ev: &MouseDownEvent, _window, cx| {
                let Some(bounds) = geometry_down.get() else {
                    return;
                };
                let pos = f32::from(ev.position.x - bounds.left());
                state_down.update(cx, |st, cx| {
                    let idx =
                        crosshair_index(st.offset, st.visible, pos, f32::from(bounds.size.width));
                    if f32::from(ev.position.y - bounds.top()) < 28. {
                        if let Some((_, id)) = st.markers.iter().find(|(i, _)| *i == idx) {
                            cx.emit(ChartEvidence(id.clone()));
                        }
                    }
                    st.drag = Some((pos, st.offset));
                    cx.notify();
                });
            },
        )
        .on_mouse_up(
            gpui_kit::gpui::MouseButton::Left,
            move |_ev: &MouseUpEvent, _window, cx| {
                state_up.update(cx, |st, cx| {
                    st.drag = None;
                    cx.notify();
                });
            },
        )
        .on_mouse_up_out(gpui_kit::gpui::MouseButton::Left, move |_, _, cx| {
            state_up_out.update(cx, |st, cx| {
                st.drag = None;
                cx.notify();
            });
        })
        .on_scroll_wheel(move |ev: &ScrollWheelEvent, _window, cx| {
            cx.stop_propagation();
            let delta = ev.delta.pixel_delta(px(20.)).y.to_f64() as f32;
            state_wheel.update(cx, |st, cx| {
                st.zoom_by(if delta > 0. { 0.85 } else { 1.18 });
                cx.notify();
            });
        })
}

fn pan_offset(len: usize, visible: usize, start_offset: usize, dx: f32, width: f32) -> usize {
    let shift = (dx * (visible as f32 / width.max(1.))) as isize;
    let new_offset = start_offset as isize - shift;
    new_offset.clamp(0, len.saturating_sub(visible) as isize) as usize
}

fn zoom_window(len: usize, visible: usize, offset: usize, factor: f32) -> (usize, usize) {
    let new_vis = ((visible as f32 * factor) as usize).clamp(50, len.max(50));
    let new_vis = new_vis.min(len.max(1));
    let center = offset + visible / 2;
    let new_offset = center
        .saturating_sub(new_vis / 2)
        .min(len.saturating_sub(new_vis));
    (new_vis, new_offset)
}

impl ChartState {
    fn hovered_info(&self, pos: (f32, f32), width: f32) -> Option<(usize, Candle)> {
        let idx = crosshair_index(self.offset, self.visible, pos.0, width);
        self.candles.get(idx).copied().map(|c| (idx, c))
    }
}

fn crosshair_index(offset: usize, visible: usize, x: f32, width: f32) -> usize {
    offset
        + (((x.max(0.) / width.max(1.)) * visible as f32) as usize).min(visible.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closes(values: &[f64]) -> Vec<Candle> {
        values
            .iter()
            .map(|&c| Candle {
                open: c,
                high: c,
                low: c,
                close: c,
                volume: 1.0,
            })
            .collect()
    }

    #[test]
    fn sma_close_uses_exactly_period_values_after_warmup() {
        let ma = sma_close(&closes(&[1.0, 2.0, 3.0, 4.0, 5.0]), 3);
        assert_eq!(ma, vec![None, None, Some(2.0), Some(3.0), Some(4.0)]);
    }

    #[test]
    fn r1_a_12_zoom_pan_and_crosshair_stay_inside_the_series() {
        let len = 1000;
        let (visible, offset) = zoom_window(len, 1000, 0, 0.85);
        assert!(visible < 1000);
        assert!(offset + visible <= len);
        let panned = pan_offset(len, visible, offset, -400., 1200.);
        assert!(panned + visible <= len);
        let idx = crosshair_index(panned, visible, 0., 1200.);
        assert_eq!(idx, panned);
    }

    #[test]
    fn chart_coordinates_follow_resized_panel_and_clamp_edges() {
        assert_eq!(crosshair_index(120, 132, 150., 300.), 186);
        assert_eq!(crosshair_index(120, 132, 300., 600.), 186);
        assert_eq!(crosshair_index(120, 132, -20., 300.), 120);
        assert_eq!(crosshair_index(120, 132, 500., 300.), 251);
        assert_eq!(pan_offset(252, 132, 120, 150., 300.), 54);
    }

    #[test]
    fn ma20_matches_naive_window_on_synthetic_series() {
        let candles = synthetic_candles(300);
        let ma = sma_close(&candles, MA_PERIOD);
        for i in 0..candles.len() {
            if i + 1 < MA_PERIOD {
                assert!(ma[i].is_none());
                continue;
            }
            let naive = candles[i + 1 - MA_PERIOD..=i]
                .iter()
                .map(|c| c.close)
                .sum::<f64>()
                / MA_PERIOD as f64;
            assert!((ma[i].unwrap() - naive).abs() < 1e-9, "index {i}");
        }
    }
}

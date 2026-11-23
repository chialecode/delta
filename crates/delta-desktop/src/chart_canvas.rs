//! Batch painting of the host-provided candles, indicators, and M5 workbench overlays.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use delta_app::KlineWindowSliceDto;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    canvas, fill, point, px, size, App, Bounds, Hsla, IntoElement, PathBuilder, Pixels, Point,
    Styled as _, Window,
};

use crate::i18n::{ChangeDirection, MarketColorConvention};
use crate::lock_or_recover;

/// Line stroke pattern for chart overlays (`DESIGN.md` §5: solid / dashed / dotted).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverlayLineStyle {
    #[default]
    Solid,
    Dashed,
    Dotted,
}

impl OverlayLineStyle {
    pub fn from_drawing_style(raw: &str) -> Self {
        match raw.trim() {
            "dashed" => Self::Dashed,
            "dotted" => Self::Dotted,
            _ => Self::Solid,
        }
    }
}

/// M5 workbench overlays painted on the main price panel (`S-54`). Bar positions are global
/// visible-bar indices (the same index space as `KlineWindowSliceDto::visible_start_index`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChartOverlay {
    pub hlines: Vec<OverlayHLine>,
    pub segments: Vec<OverlaySegment>,
    pub hide_volume: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OverlayHLine {
    pub price: f64,
    pub color: u32,
    pub style: OverlayLineStyle,
    pub thick: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OverlaySegment {
    pub from: (f64, f64),
    pub to: (f64, f64),
    /// Extend beyond `to` along the `(from -> to)` direction (a ray).
    pub ray: bool,
    pub color: u32,
    pub style: OverlayLineStyle,
    pub thick: bool,
}

/// `#rrggbb` → `0xrrggbb`; anything else falls back to amber.
pub fn parse_hex_color(raw: &str) -> u32 {
    raw.strip_prefix('#')
        .filter(|h| h.len() == 6)
        .and_then(|h| u32::from_str_radix(h, 16).ok())
        .unwrap_or(0xf0b90b)
}

pub(crate) type PlotBounds = Arc<Mutex<Option<Bounds<Pixels>>>>;
pub(crate) static PAINTED_CHART_FRAMES: AtomicUsize = AtomicUsize::new(0);

/// Returns finite `(min_low, max_high)` across the visible LOD candles.
pub(crate) fn slice_price_bounds(slice: &KlineWindowSliceDto) -> Option<(f64, f64)> {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for c in &slice.lod_candles {
        if c.low.is_finite() {
            min = min.min(c.low);
        }
        if c.high.is_finite() {
            max = max.max(c.high);
        }
    }
    (min.is_finite() && max.is_finite()).then_some((min, max))
}

/// Maps `value` to vertical plot ratio (`0.0` = top `max`, `1.0` = bottom `min`), unclamped.
/// Flat price windows (`max - min < 1e-9`) center at `0.5` (`UI-B16`).
pub(crate) fn price_ratio_unclamped(value: f64, min: f64, max: f64) -> f32 {
    let span = max - min;
    if span < 1e-9 {
        0.5
    } else {
        1. - ((value - min) / span) as f32
    }
}

/// Shared pointer-to-`(global_bar_index, price)` mapping for both Market and Training charts (`UI-Q04`).
pub(crate) fn pointer_bar_price(
    position: Point<Pixels>,
    bounds: Bounds<Pixels>,
    slice: &KlineWindowSliceDto,
) -> Option<(usize, f64)> {
    let (x, y) = pointer_ratio(position, bounds)?;
    if slice.bars.is_empty() {
        return None;
    }
    let index = slice.visible_start_index
        + ((x * slice.bars.len() as f32).floor() as usize).min(slice.bars.len() - 1);
    let (min, max) = slice_price_bounds(slice)?;
    let price = if (max - min) < 1e-9 {
        min
    } else {
        min + (max - min) * f64::from(1. - y)
    };
    Some((index, price))
}

/// Maps a visible price to screen Y pixel within `bounds`, returning `None` when the price is
/// non-finite or outside the currently painted `[min, max]` price range (`UI-B07`).
pub(crate) fn price_to_y(
    price: f64,
    bounds: Bounds<Pixels>,
    slice: &KlineWindowSliceDto,
) -> Option<f32> {
    if !price.is_finite() {
        return None;
    }
    let (min, max) = slice_price_bounds(slice)?;
    if (max - min) < 1e-9 {
        if (price - min).abs() > 1e-9 {
            return None;
        }
    } else if price < min || price > max {
        return None;
    }
    let ratio = price_ratio_unclamped(price, min, max).clamp(0., 1.);
    Some(f32::from(bounds.origin.y) + f32::from(bounds.size.height) * ratio)
}

/// Exact Liang-Barsky clip of a segment (`t in [0, 1]`) or ray (`t in [0, +inf)`) against a
/// rectangle `(left, top, right, bottom)`, preserving true slope and ray direction (`UI-B06`).
pub(crate) fn clip_line_to_rect(
    (x0, y0): (f32, f32),
    (x1, y1): (f32, f32),
    ray: bool,
    (left, top, right, bottom): (f32, f32, f32, f32),
) -> Option<((f32, f32), (f32, f32))> {
    if ![x0, y0, x1, y1, left, top, right, bottom]
        .iter()
        .all(|v| v.is_finite())
        || left >= right
        || top >= bottom
    {
        return None;
    }
    let dx = x1 - x0;
    let dy = y1 - y0;
    if ray && dx.abs() < 1e-6 && dy.abs() < 1e-6 {
        return None;
    }
    let mut t0 = 0.0_f32;
    let mut t1 = if ray { f32::INFINITY } else { 1.0_f32 };
    for (p, q) in [
        (-dx, x0 - left),
        (dx, right - x0),
        (-dy, y0 - top),
        (dy, bottom - y0),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
        } else {
            let t = q / p;
            if p < 0.0 {
                if t > t1 {
                    return None;
                }
                t0 = t0.max(t);
            } else {
                if t < t0 {
                    return None;
                }
                t1 = t1.min(t);
            }
        }
    }
    if !t0.is_finite() || !t1.is_finite() || t0 > t1 {
        return None;
    }
    Some(((x0 + t0 * dx, y0 + t0 * dy), (x0 + t1 * dx, y0 + t1 * dy)))
}

fn paint_path(path: PathBuilder, color: Hsla, window: &mut Window) {
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

fn paint_styled_segment(
    (x0, y0): (f32, f32),
    (x1, y1): (f32, f32),
    style: OverlayLineStyle,
    thick: bool,
    color: Hsla,
    window: &mut Window,
) {
    let width = if thick { px(2.5) } else { px(1.5) };
    let mut path = PathBuilder::stroke(width);
    match style {
        OverlayLineStyle::Solid => {
            path.move_to(point(px(x0), px(y0)));
            path.line_to(point(px(x1), px(y1)));
        }
        OverlayLineStyle::Dashed | OverlayLineStyle::Dotted => {
            let (dash, gap) = if style == OverlayLineStyle::Dashed {
                (6.0_f32, 4.0_f32)
            } else {
                (2.0_f32, 4.0_f32)
            };
            let dx = x1 - x0;
            let dy = y1 - y0;
            let len = dx.hypot(dy);
            if len < 1e-3 {
                return;
            }
            let (ux, uy) = (dx / len, dy / len);
            let mut pos = 0.0_f32;
            while pos < len {
                let end = (pos + dash).min(len);
                path.move_to(point(px(x0 + ux * pos), px(y0 + uy * pos)));
                path.line_to(point(px(x0 + ux * end), px(y0 + uy * end)));
                pos += dash + gap;
            }
        }
    }
    paint_path(path, color, window);
}

fn series_path(
    values: impl Iterator<Item = Option<f64>>,
    slice: &KlineWindowSliceDto,
    panel: Bounds<Pixels>,
    min: f64,
    max: f64,
    color: Hsla,
    window: &mut Window,
) {
    let mut path = PathBuilder::stroke(px(1.25));
    let mut connected = false;
    let count = slice.bars.len().max(1) as f32;
    for (index, value) in values.enumerate() {
        if slice
            .gap_indices
            .binary_search(&(slice.visible_start_index + index))
            .is_ok()
        {
            connected = false;
        }
        let Some(value) = value.filter(|value| value.is_finite()) else {
            connected = false;
            continue;
        };
        let ratio = price_ratio_unclamped(value, min, max).clamp(0., 1.);
        let p = point(
            panel.origin.x + panel.size.width * ((index as f32 + 0.5) / count),
            panel.origin.y + panel.size.height * ratio,
        );
        if connected {
            path.line_to(p);
        } else {
            path.move_to(p);
        }
        connected = true;
    }
    paint_path(path, color, window);
}

pub(crate) fn render_canvas(
    slice: Arc<KlineWindowSliceDto>,
    convention: MarketColorConvention,
    crosshair: Option<usize>,
    crosshair_price: Option<f64>,
    plot_bounds: PlotBounds,
    overlay: Option<ChartOverlay>,
    cx: &App,
) -> impl IntoElement {
    let overlay = overlay.unwrap_or_default();
    let colors = [
        convention.colors(ChangeDirection::Up, cx).0,
        convention.colors(ChangeDirection::Down, cx).0,
        convention.colors(ChangeDirection::Flat, cx).0,
    ];
    let grid = cx.theme().border;
    let warning = cx.theme().warning;
    let lines = [
        cx.theme().info,
        cx.theme().warning,
        cx.theme().success,
        cx.theme().primary,
    ];
    let foreground = cx.theme().foreground;
    canvas(
        move |bounds, _, _| {
            let main = Bounds::new(
                bounds.origin,
                size(bounds.size.width, bounds.size.height * 0.55),
            );
            *lock_or_recover(&plot_bounds) = Some(main);
            main
        },
        move |bounds, main, window, _| {
            if slice.lod_candles.is_empty()
                || bounds.size.width <= px(0.)
                || bounds.size.height <= px(0.)
            {
                return;
            }
            let Some((min, max)) = slice_price_bounds(&slice) else {
                return;
            };
            PAINTED_CHART_FRAMES.fetch_add(1, Ordering::Relaxed);
            let volume = Bounds::new(
                point(bounds.origin.x, bounds.origin.y + bounds.size.height * 0.58),
                size(bounds.size.width, bounds.size.height * 0.1),
            );
            let macd = Bounds::new(
                point(bounds.origin.x, bounds.origin.y + bounds.size.height * 0.71),
                size(bounds.size.width, bounds.size.height * 0.13),
            );
            let rsi = Bounds::new(
                point(bounds.origin.x, bounds.origin.y + bounds.size.height * 0.87),
                size(bounds.size.width, bounds.size.height * 0.13),
            );
            let max_volume = slice
                .lod_candles
                .iter()
                .map(|c| c.volume)
                .filter(|v| v.is_finite())
                .fold(1., f64::max);
            let count = slice.bars.len().max(1) as f32;
            let price_y = |value: f64| {
                main.origin.y
                    + main.size.height * price_ratio_unclamped(value, min, max).clamp(0., 1.)
            };
            let price_y_raw = |value: f64| {
                f32::from(main.origin.y)
                    + f32::from(main.size.height) * price_ratio_unclamped(value, min, max)
            };
            let grid_panels: &[Bounds<Pixels>] = if overlay.hide_volume {
                &[main, macd, rsi]
            } else {
                &[main, volume, macd, rsi]
            };
            for &panel in grid_panels {
                for row in 0..=4 {
                    window.paint_quad(fill(
                        Bounds::new(
                            point(
                                panel.origin.x,
                                panel.origin.y + panel.size.height * (row as f32 / 4.),
                            ),
                            size(panel.size.width, px(1.)),
                        ),
                        grid.opacity(0.6),
                    ));
                }
            }
            // GPUI batches solid quads in the scene. Avoid tessellating thousands of
            // disconnected closed subpaths every frame; paths are reserved for lines.
            for candle in &slice.lod_candles {
                let group = if candle.close > candle.open {
                    0
                } else if candle.close < candle.open {
                    1
                } else {
                    2
                };
                let color = colors[group];
                let local = candle
                    .start_bar_index
                    .saturating_sub(slice.visible_start_index) as f32;
                let bars = candle
                    .end_bar_index
                    .saturating_sub(candle.start_bar_index)
                    .max(1) as f32;
                let x = main.origin.x + main.size.width * ((local + bars / 2.) / count);
                let width = (main.size.width * (bars / count) * 0.7).max(px(1.));
                window.paint_quad(fill(
                    Bounds::new(
                        point(x, price_y(candle.high)),
                        size(
                            px(1.),
                            (price_y(candle.low) - price_y(candle.high)).max(px(1.)),
                        ),
                    ),
                    color,
                ));
                let top = price_y(candle.open.max(candle.close));
                let height = (price_y(candle.open.min(candle.close)) - top).max(px(1.));
                window.paint_quad(fill(
                    Bounds::new(point(x - width / 2., top), size(width, height)),
                    color,
                ));
                if overlay.hide_volume {
                    continue;
                }
                let height =
                    volume.size.height * ((candle.volume / max_volume).clamp(0., 1.) as f32);
                window.paint_quad(fill(
                    Bounds::new(
                        point(x - width / 2., volume.bottom() - height),
                        size(width, height),
                    ),
                    color.opacity(0.65),
                ));
            }
            for &global in &slice.gap_indices {
                let x = main.origin.x
                    + main.size.width
                        * (global.saturating_sub(slice.visible_start_index) as f32 / count);
                window.paint_quad(fill(
                    Bounds::new(
                        point(x - px(1.), main.origin.y),
                        size(px(2.), bounds.size.height),
                    ),
                    warning.opacity(0.4),
                ));
            }
            for (index, (_, series)) in slice.sma_series.iter().chain(&slice.ema_series).enumerate()
            {
                series_path(
                    series.iter().map(|point| point.value),
                    &slice,
                    main,
                    min,
                    max,
                    lines[index % lines.len()],
                    window,
                );
            }
            let macd_extent = slice
                .macd_series
                .iter()
                .flat_map(|point| [point.macd, point.signal, point.histogram])
                .flatten()
                .filter(|value| value.is_finite())
                .map(f64::abs)
                .fold(1e-9, f64::max);
            // Single-pass centered MACD histogram (`UI-B16`, `UI-Q01`).
            let macd_bar_w = (macd.size.width / count * 0.7).max(px(1.));
            for (index, pt) in slice.macd_series.iter().enumerate() {
                let Some(value) = pt.histogram.filter(|v| v.is_finite()) else {
                    continue;
                };
                let color = if value >= 0. { colors[0] } else { colors[1] };
                let cx_pos = macd.origin.x + macd.size.width * ((index as f32 + 0.5) / count);
                let y =
                    macd.origin.y + macd.size.height * (0.5 - (value / (2. * macd_extent)) as f32);
                window.paint_quad(fill(
                    Bounds::new(
                        point(cx_pos - macd_bar_w / 2., y.min(macd.center().y)),
                        size(macd_bar_w, (y - macd.center().y).abs().max(px(1.))),
                    ),
                    color.opacity(0.65),
                ));
            }
            series_path(
                slice.macd_series.iter().map(|point| point.macd),
                &slice,
                macd,
                -macd_extent,
                macd_extent,
                lines[0],
                window,
            );
            series_path(
                slice.macd_series.iter().map(|point| point.signal),
                &slice,
                macd,
                -macd_extent,
                macd_extent,
                lines[1],
                window,
            );
            series_path(
                slice.rsi_series.iter().map(|point| point.value),
                &slice,
                rsi,
                0.,
                100.,
                lines[2],
                window,
            );
            for marker in &slice.markers {
                let x = main.origin.x + main.size.width * marker.x_ratio;
                let y = main.origin.y + main.size.height * marker.y_ratio;
                let mut triangle = PathBuilder::fill();
                triangle.move_to(point(x, y - px(5.)));
                triangle.line_to(point(x - px(4.), y + px(4.)));
                triangle.line_to(point(x + px(4.), y + px(4.)));
                triangle.close();
                paint_path(triangle, lines[3], window);
            }
            let bar_x_raw = |global: f64| {
                f32::from(main.origin.x)
                    + f32::from(main.size.width)
                        * (((global - slice.visible_start_index as f64) as f32 + 0.5) / count)
            };
            for line in &overlay.hlines {
                if !line.price.is_finite() {
                    continue;
                }
                let in_range = if (max - min) < 1e-9 {
                    (line.price - min).abs() <= 1e-9
                } else {
                    line.price >= min && line.price <= max
                };
                if !in_range {
                    continue;
                }
                let y = price_y(line.price);
                let color: Hsla = gpui_kit::rgb(line.color).into();
                let thickness = if line.thick { px(2.) } else { px(1.) };
                match line.style {
                    OverlayLineStyle::Solid => {
                        window.paint_quad(fill(
                            Bounds::new(point(main.origin.x, y), size(main.size.width, thickness)),
                            color,
                        ));
                    }
                    OverlayLineStyle::Dashed | OverlayLineStyle::Dotted => {
                        let (dash, step) = if line.style == OverlayLineStyle::Dashed {
                            (px(6.), px(10.))
                        } else {
                            (px(2.), px(6.))
                        };
                        let mut x = main.origin.x;
                        while x < main.right() {
                            let w = dash.min(main.right() - x);
                            window.paint_quad(fill(
                                Bounds::new(point(x, y), size(w, thickness)),
                                color,
                            ));
                            x += step;
                        }
                    }
                }
            }
            let main_rect = (
                f32::from(main.origin.x),
                f32::from(main.origin.y),
                f32::from(main.right()),
                f32::from(main.bottom()),
            );
            for seg in &overlay.segments {
                if !seg.from.0.is_finite()
                    || !seg.from.1.is_finite()
                    || !seg.to.0.is_finite()
                    || !seg.to.1.is_finite()
                {
                    continue;
                }
                let p0 = (bar_x_raw(seg.from.0), price_y_raw(seg.from.1));
                let p1 = (bar_x_raw(seg.to.0), price_y_raw(seg.to.1));
                let Some((c0, c1)) = clip_line_to_rect(p0, p1, seg.ray, main_rect) else {
                    continue;
                };
                let color: Hsla = gpui_kit::rgb(seg.color).into();
                paint_styled_segment(c0, c1, seg.style, seg.thick, color, window);
            }
            if let Some(local) = crosshair
                .and_then(|global| global.checked_sub(slice.visible_start_index))
                .filter(|&index| index < slice.bars.len())
            {
                let x = main.origin.x + main.size.width * ((local as f32 + 0.5) / count);
                window.paint_quad(fill(
                    Bounds::new(point(x, main.origin.y), size(px(1.), bounds.size.height)),
                    foreground.opacity(0.65),
                ));
                let candle = slice.lod_candles.iter().find(|c| {
                    c.start_bar_index <= slice.visible_start_index + local
                        && c.end_bar_index > slice.visible_start_index + local
                });
                if let Some(price) = crosshair_price
                    .or_else(|| candle.map(|candle| candle.close))
                    .filter(|price| price.is_finite())
                {
                    window.paint_quad(fill(
                        Bounds::new(
                            point(main.origin.x, price_y(price)),
                            size(main.size.width, px(1.)),
                        ),
                        foreground.opacity(0.4),
                    ));
                }
            }
        },
    )
    .w_full()
    .h(px(400.))
}

pub(crate) fn pointer_ratio(position: Point<Pixels>, bounds: Bounds<Pixels>) -> Option<(f32, f32)> {
    if !bounds.contains(&position) || bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
        return None;
    }
    Some((
        (position.x - bounds.origin.x) / bounds.size.width,
        (position.y - bounds.origin.y) / bounds.size.height,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_ray_preserves_direction_and_slope_without_corner_bending() {
        let rect = (0., 0., 100., 100.);
        // Right-to-left ray from (80, 50) through (60, 40) must extend leftward to x=0, y=10 (NOT x=100!).
        let ((x0, y0), (x1, y1)) = clip_line_to_rect((80., 50.), (60., 40.), true, rect).unwrap();
        assert!((x0 - 80.).abs() < 1e-4);
        assert!((y0 - 50.).abs() < 1e-4);
        assert!((x1 - 0.).abs() < 1e-4);
        assert!((y1 - 10.).abs() < 1e-4);

        // Steep upward ray from (20, 80) through (40, 40) exits top edge y=0 at x=60 (NOT x=100, y=0!).
        let (_, (ux1, uy1)) = clip_line_to_rect((20., 80.), (40., 40.), true, rect).unwrap();
        assert!((ux1 - 60.).abs() < 1e-4);
        assert!((uy1 - 0.).abs() < 1e-4);

        // Segment completely off-screen left returns None.
        assert!(clip_line_to_rect((-50., 20.), (-10., 80.), false, rect).is_none());

        // Non-finite inputs return None.
        assert!(clip_line_to_rect((f32::NAN, 10.), (20., 30.), false, rect).is_none());
    }

    #[test]
    fn flat_price_centers_vertically() {
        assert!((price_ratio_unclamped(100.0, 100.0, 100.0) - 0.5).abs() < 1e-6);
        assert!((price_ratio_unclamped(110.0, 100.0, 120.0) - 0.5).abs() < 1e-6);
    }
}

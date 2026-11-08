//! Batch painting of the host-provided candles and indicator series.

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

pub(crate) type PlotBounds = Arc<Mutex<Option<Bounds<Pixels>>>>;
pub(crate) static PAINTED_CHART_FRAMES: AtomicUsize = AtomicUsize::new(0);

fn paint_path(path: PathBuilder, color: Hsla, window: &mut Window) {
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
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
    let span = (max - min).max(1e-9);
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
        let p = point(
            panel.origin.x + panel.size.width * ((index as f32 + 0.5) / count),
            panel.origin.y + panel.size.height * (1. - ((value - min) / span).clamp(0., 1.) as f32),
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
    slice: &KlineWindowSliceDto,
    convention: MarketColorConvention,
    crosshair: Option<usize>,
    crosshair_price: Option<f64>,
    plot_bounds: PlotBounds,
    cx: &App,
) -> impl IntoElement {
    let slice = slice.clone();
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
            *plot_bounds.lock().unwrap() = Some(main);
            main
        },
        move |bounds, main, window, _| {
            if slice.lod_candles.is_empty()
                || bounds.size.width <= px(0.)
                || bounds.size.height <= px(0.)
            {
                return;
            }
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
            let min = slice
                .lod_candles
                .iter()
                .map(|c| c.low)
                .fold(f64::INFINITY, f64::min);
            let max = slice
                .lod_candles
                .iter()
                .map(|c| c.high)
                .fold(f64::NEG_INFINITY, f64::max);
            let span = (max - min).max(1e-9);
            let max_volume = slice
                .lod_candles
                .iter()
                .map(|c| c.volume)
                .fold(1., f64::max);
            let count = slice.bars.len().max(1) as f32;
            let price_y = |value: f64| {
                main.origin.y
                    + main.size.height * (1. - ((value - min) / span).clamp(0., 1.) as f32)
            };
            for panel in [main, volume, macd, rsi] {
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
                let bars = (candle.end_bar_index - candle.start_bar_index).max(1) as f32;
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
            for (positive, color) in [(true, colors[0]), (false, colors[1])] {
                for (index, value) in
                    slice
                        .macd_series
                        .iter()
                        .enumerate()
                        .filter_map(|(index, point)| {
                            point
                                .histogram
                                .filter(|value| value.is_finite())
                                .map(|value| (index, value))
                        })
                {
                    if (value >= 0.) != positive {
                        continue;
                    }
                    let x = macd.origin.x + macd.size.width * (index as f32 / count);
                    let y = macd.origin.y
                        + macd.size.height * (0.5 - (value / (2. * macd_extent)) as f32);
                    window.paint_quad(fill(
                        Bounds::new(
                            point(x, y.min(macd.center().y)),
                            size(
                                (macd.size.width / count * 0.7).max(px(1.)),
                                (y - macd.center().y).abs().max(px(1.)),
                            ),
                        ),
                        color.opacity(0.65),
                    ));
                }
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

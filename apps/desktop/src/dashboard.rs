//! Reference-image composition, explicitly isolated as an offline visual sample.
//! Service-backed accounts/search remain on their existing pages. None of these
//! sample numbers can become a portfolio snapshot, a trade, or an AI result.
use super::*;
use gpui_kit::gpui::{canvas, fill, point, size, Bounds, FontWeight};

const SYMBOLS: [(&str, &str, &str, &str, u32); 8] = [
    ("₿", "BTC/USDT", "Bitcoin", "66,421.07", 0xff9600),
    ("◆", "ETH/USDT", "Ethereum", "3,248.62", 0x6276fc),
    ("≋", "SOL/USDT", "Solana", "162.34", 0x1b273d),
    ("◇", "BNB/USDT", "BNB", "592.31", 0xffa000),
    ("A", "AAPL", "Apple", "228.17", 0x647084),
    ("N", "NVDA", "NVIDIA", "121.48", 0x339c29),
    ("T", "TSLA", "Tesla", "242.76", 0xef3447),
    ("M", "MSFT", "Microsoft", "415.32", 0x168ade),
];
const CHANGES: [&str; 8] = [
    "+1.51%", "+1.23%", "+2.17%", "+0.84%", "+0.56%", "+1.32%", "−0.21%", "+0.67%",
];

fn coin(index: usize) -> impl IntoElement {
    let (glyph, _, _, _, color) = SYMBOLS[index];
    row()
        .justify_center()
        .size(px(23.))
        .flex_shrink_0()
        .rounded_full()
        .bg(rgb(color))
        .text_color(rgb(PANEL))
        .text_size(px(14.))
        .child(glyph)
}

fn heading(title: &str, right: impl IntoElement) -> impl IntoElement {
    row()
        .justify_between()
        .h(px(39.))
        .px_3()
        .child(label(title, 13., INK).font_weight(FontWeight::SEMIBOLD))
        .child(right)
}

fn tabs(names: &[&str], active: usize) -> impl IntoElement {
    row()
        .gap_4()
        .h(px(34.))
        .px_3()
        .border_b_1()
        .border_color(rgb(LINE))
        .children(names.iter().enumerate().map(|(i, name)| {
            label(*name, 11., if i == active { ORANGE } else { MUTED })
                .h_full()
                .pt_2()
                .when(i == active, |d| d.border_b_2().border_color(rgb(ORANGE)))
        }))
}

/// Small deterministic decorative line. It is deliberately not a performance
/// calculation and lives only in cards visibly marked as examples.
fn sparkline(height: f32, muted: bool) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let w = f32::from(bounds.size.width);
            let h = f32::from(bounds.size.height);
            let mut previous = None;
            for i in 0..100 {
                let t = i as f32 / 99.;
                let y = h * (0.88 - 0.69 * t - (t * 24.).sin() * 0.05 - (t * 81.).sin() * 0.025);
                let x = w * t;
                window.paint_quad(fill(
                    Bounds::new(
                        bounds.origin + point(px(x), px(y)),
                        size(px(w / 99. + 0.1), px(h - y)),
                    ),
                    rgb(if muted { 0xf4f5f7 } else { 0xfff6e9 }),
                ));
                if let Some((last_x, last_y)) = previous {
                    let steps = 5;
                    for step in 0..steps {
                        let f = step as f32 / steps as f32;
                        let sx = last_x + (x - last_x) * f;
                        let sy = last_y + (y - last_y) * f;
                        window.paint_quad(fill(
                            Bounds::new(
                                bounds.origin + point(px(sx), px(sy)),
                                size(px((x - last_x) / steps as f32 + 0.6), px(1.4)),
                            ),
                            rgb(if muted { MUTED } else { ORANGE }),
                        ));
                    }
                }
                previous = Some((x, y));
            }
        },
    )
    .w_full()
    .h(px(height))
}

fn metric(name: &str, value: &str, color: u32) -> impl IntoElement {
    column()
        .gap_1()
        .child(label(name, 9., MUTED))
        .child(label(value, 13., color))
}

impl Workspace {
    fn sample_market(&self, compact: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let title = if compact { "市场概览" } else { "自选" };
        card()
            .h_full()
            .child(heading(title, badge("样例")))
            .child(tabs(
                if compact {
                    &["加密", "美股", "外汇", "指数"]
                } else {
                    &["自选", "美股", "加密", "外汇"]
                },
                0,
            ))
            .child(
                row()
                    .justify_between()
                    .px_3()
                    .h(px(30.))
                    .child(label("名称 / 代码", 9., MUTED))
                    .child(label("价格       涨跌幅", 9., MUTED)),
            )
            .children(
                SYMBOLS
                    .iter()
                    .enumerate()
                    .map(|(i, (_, symbol, name, price, _))| {
                        row()
                            .id(SharedString::from(format!(
                                "sample-{}-{i}",
                                if compact { "market" } else { "watch" }
                            )))
                            .test_support()
                            .aria_label(format!("样例 {symbol}"))
                            .h(px(48.))
                            .px_3()
                            .gap_2()
                            .cursor_pointer()
                            .when(self.sample_selected == i, |d| d.bg(rgb(0xfffaf3)))
                            .hover(|d| d.bg(rgb(TINT)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.sample_selected = i;
                                this.sample_chart.update(cx, |chart, cx| {
                                    *chart = sample_chart(i);
                                    cx.notify();
                                });
                                cx.notify();
                            }))
                            .child(coin(i))
                            .child(
                                column()
                                    .flex_1()
                                    .gap_1()
                                    .child(label(*symbol, 11., INK))
                                    .child(label(*name, 9., MUTED)),
                            )
                            .child(
                                column()
                                    .items_end()
                                    .gap_1()
                                    .child(label(*price, 11., INK))
                                    .child(label(
                                        CHANGES[i],
                                        10.,
                                        if i == 6 { RED } else { GREEN },
                                    )),
                            )
                    }),
            )
    }

    fn sample_chart_card(&self, small: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let (_, symbol, name, price, _) = SYMBOLS[self.sample_selected];
        let entity = self.sample_chart.clone();
        let state = self.sample_chart.read(cx);
        let viewport = state.visible;
        let visible =
            &state.candles[state.offset..(state.offset + viewport).min(state.candles.len())];
        let low = visible.iter().map(|c| c.low).fold(f64::INFINITY, f64::min);
        let high = visible
            .iter()
            .map(|c| c.high)
            .fold(f64::NEG_INFINITY, f64::max);
        let chart = interactive_chart(entity);
        card()
            .h_full()
            .child(
                div()
                    .id(SharedString::from(format!("sample-chart-title-{small}")))
                    .test_support()
                    .aria_label(format!("{symbol} / {viewport} 根日线样例"))
                    .child(heading(
                        &format!("{symbol}{}", if small { " · 1D" } else { "" }),
                        label("日线样例", 10., MUTED),
                    )),
            )
            .when(!small, |d| {
                d.child(
                    row()
                        .gap_3()
                        .px_3()
                        .child(label(price, 23., INK).font_weight(FontWeight::SEMIBOLD))
                        .child(label(
                            CHANGES[self.sample_selected],
                            11.,
                            if self.sample_selected == 6 {
                                RED
                            } else {
                                GREEN
                            },
                        ))
                        .child(label(name, 10., MUTED)),
                )
            })
            .child(
                row()
                    .gap_3()
                    .px_3()
                    .h(px(31.))
                    .child(label("1D", 11., ORANGE))
                    .child(label("日线 / 未复权", 10., MUTED))
                    .child(label("MA 20", 10., ORANGE)),
            )
            .when(!small, |d| {
                d.child(
                    label("合成 OHLCV · 滚轮缩放 / 拖动平移 / 十字线", 10., MUTED)
                        .px_3()
                        .py_1(),
                )
            })
            .child(
                row()
                    .flex_1()
                    .min_h(px(100.))
                    .px_2()
                    .pb_2()
                    .child(div().flex_1().h_full().child(chart))
                    .child(
                        column().w(px(52.)).h_full().pl_2().child(
                            column()
                                .h(gpui_kit::gpui::relative(0.78))
                                .justify_between()
                                .children((0..6).map(|i| {
                                    label(
                                        format!("{:.2}", high - (high - low) * i as f64 / 5.),
                                        8.,
                                        MUTED,
                                    )
                                })),
                        ),
                    ),
            )
            .child(
                row()
                    .justify_between()
                    .h(px(35.))
                    .px_3()
                    .border_t_1()
                    .border_color(rgb(LINE))
                    .children(
                        [("1M", 22), ("3M", 66), ("6M", 132), ("1Y", 252)]
                            .into_iter()
                            .map(|(name, count)| {
                                label(name, 10., if viewport == count { ORANGE } else { MUTED })
                                    .id(SharedString::from(format!("range-{small}-{name}")))
                                    .test_support()
                                    .aria_label(name)
                                    .cursor_pointer()
                                    .px_2()
                                    .py_1()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.sample_chart.update(cx, |chart, cx| {
                                            chart.visible = count;
                                            chart.offset =
                                                chart.candles.len().saturating_sub(count);
                                            cx.notify();
                                        });
                                        cx.notify();
                                    }))
                            }),
                    ),
            )
    }

    fn sample_assets(&self) -> impl IntoElement {
        card()
            .h_full()
            .child(heading("个人资产", badge("布局样例")))
            .child(
                label("$ 152,436.32", 23., INK)
                    .px_3()
                    .font_weight(FontWeight::SEMIBOLD),
            )
            .child(
                label("+3,821.45 (+2.57%)  今日 · 样例", 10., GREEN)
                    .px_3()
                    .py_1(),
            )
            .child(div().px_3().py_2().child(sparkline(58., false)))
            .children(
                [
                    ("加密货币", "$ 68,215.32", "44.7%"),
                    ("美股", "$ 52,317.18", "34.3%"),
                    ("现金", "$ 12,480.62", "8.2%"),
                    ("其他", "$ 19,423.20", "12.8%"),
                ]
                .into_iter()
                .map(|(name, amount, share)| {
                    row()
                        .px_3()
                        .h(px(29.))
                        .gap_2()
                        .child(label("◇", 13., INK))
                        .child(label(name, 11., INK).flex_1())
                        .child(label(amount, 11., INK))
                        .child(label(share, 10., MUTED).w(px(36.)).text_right())
                }),
            )
            .child(
                label("资产来源 · 示例账户", 10., MUTED)
                    .px_3()
                    .pt_3()
                    .pb_1(),
            )
            .children(
                [
                    ("◈", "Binance", "$ 42,317.56"),
                    ("C", "Coinbase", "$ 18,562.34"),
                    ("◉", "IBKR", "$ 31,274.22"),
                    ("▣", "银行卡", "$ 12,480.62"),
                ]
                .into_iter()
                .map(|(icon, name, value)| {
                    row()
                        .px_3()
                        .h(px(28.))
                        .gap_2()
                        .child(label(icon, 17., ORANGE))
                        .child(label(name, 11., INK).flex_1())
                        .child(label(value, 11., INK))
                }),
            )
            .child(
                label("仅用于界面对照，不是账户估值", 9., MUTED)
                    .px_3()
                    .py_2(),
            )
    }

    fn sample_strategy(&self) -> impl IntoElement {
        column()
            .h_full()
            .gap_2()
            .child(
                card()
                    .flex_1()
                    .child(heading("策略回测", badge("效果预览")))
                    .child(tabs(&["我的策略", "模板策略"], 0))
                    .child(
                        column()
                            .px_3()
                            .py_3()
                            .gap_2()
                            .child(
                                label("◈  趋势跟随策略", 12., INK)
                                    .font_weight(FontWeight::SEMIBOLD),
                            )
                            .child(label("基于均线、RSI 与成交量的示例", 10., MUTED))
                            .child(label("BTC    ETH    1D    量化", 9., MUTED))
                            .child(
                                row()
                                    .justify_between()
                                    .pt_2()
                                    .child(metric("年化收益", "+42.36%", GREEN))
                                    .child(metric("最大回撤", "12.48%", INK))
                                    .child(metric("夏普比率", "2.31", INK)),
                            )
                            .child(sparkline(64., false))
                            .child(
                                row()
                                    .justify_between()
                                    .child(badge("回测功能待接入"))
                                    .child(label("示例曲线", 9., MUTED)),
                            ),
                    ),
            )
            .child(
                card()
                    .h(px(146.))
                    .child(heading("策略表现排行", label("样例", 9., MUTED)))
                    .child(
                        row()
                            .px_3()
                            .justify_between()
                            .child(label("策略名称", 9., MUTED))
                            .child(label("年化收益     最大回撤", 9., MUTED)),
                    )
                    .children(
                        [
                            ("趋势跟随策略", "+42.36%", "12.48%"),
                            ("网格交易策略", "+18.72%", "9.21%"),
                            ("多因子选股策略", "+27.31%", "15.62%"),
                        ]
                        .into_iter()
                        .map(|(name, gain, drawdown)| {
                            row()
                                .px_3()
                                .h(px(27.))
                                .gap_3()
                                .child(label(name, 10., INK).flex_1())
                                .child(label(gain, 10., GREEN))
                                .child(label(drawdown, 10., INK))
                        }),
                    ),
            )
    }

    fn sample_training(&self) -> impl IntoElement {
        column()
            .h_full()
            .gap_1()
            .child(
                card()
                    .flex_1()
                    .child(heading("训练中心", badge("效果预览")))
                    .child(tabs(&["K 线训练", "模拟交易", "题库"], 0))
                    .child(
                        column()
                            .p_3()
                            .gap_1()
                            .child(row().gap_1().child(coin(0)).child(label(
                                "BTC K 线训练",
                                12.,
                                INK,
                            )))
                            .child(label("基于历史日线，练习趋势与判断", 10., MUTED))
                            .child(
                                row()
                                    .justify_between()
                                    .pt_1()
                                    .child(label("训练进度  68%", 10., MUTED))
                                    .child(label("45 / 66 题", 10., INK)),
                            )
                            .child(
                                div()
                                    .h(px(5.))
                                    .w_full()
                                    .rounded_full()
                                    .bg(rgb(LINE))
                                    .child(div().h_full().w_2_3().rounded_full().bg(rgb(ORANGE))),
                            )
                            .child(
                                row()
                                    .justify_between()
                                    .py_1()
                                    .child(label("难度：中等", 10., GREEN))
                                    .child(badge("训练功能待接入")),
                            )
                            .child(label("最近训练 · 样例记录", 11., INK).pt_1())
                            .children(
                                [
                                    (0, "2024-08-12", "+2.3%"),
                                    (1, "2024-08-10", "+1.7%"),
                                    (2, "2024-08-08", "−1.2%"),
                                ]
                                .into_iter()
                                .map(|(index, date, value)| {
                                    row()
                                        .gap_1()
                                        .h(px(26.))
                                        .child(coin(index))
                                        .child(label(date, 9., MUTED).flex_1())
                                        .child(label(
                                            value,
                                            10.,
                                            if index == 2 { RED } else { GREEN },
                                        ))
                                }),
                            ),
                    ),
            )
            .child(
                card()
                    .h(px(146.))
                    .child(heading("交易笔记", label("样例", 9., MUTED)))
                    .children(
                        [
                            (0, "趋势突破，关注成交量"),
                            (1, "回踩均线，等待确认"),
                            (2, "量价配合良好，继续观察"),
                        ]
                        .into_iter()
                        .map(|(index, note)| {
                            row()
                                .px_3()
                                .gap_1()
                                .h(px(29.))
                                .child(coin(index))
                                .child(label(note, 10., INK))
                        }),
                    ),
            )
    }

    fn sample_assistant(&self) -> impl IntoElement {
        card().h_full().child(heading("◈  AI 助手", label("未连接模型", 10., MUTED)))
            .child(column().p_3().gap_1().flex_1()
                .child(row().justify_end().child(label("分析一下最近这笔 BTC 交易", 11., ORANGE).p_3().rounded(px(10.)).bg(rgb(TINT))))
                .child(column().p_3().gap_1().border_1().border_color(rgb(LINE)).rounded(px(8.))
                    .child(label("复盘示例 · 尚未生成分析", 12., INK).font_weight(FontWeight::SEMIBOLD))
                    .child(label("连接模型并选择账户范围后，可结合成交、行情与笔记复盘。", 11., INK))
                    .child(label("从技术面看", 11., INK).pt_2().font_weight(FontWeight::SEMIBOLD))
                    .child(label("• 趋势与均线的位置\n• 成交量是否支持突破\n• 入场和离场时的市场环境", 11., MUTED))
                    .child(label("从你的交易记录看", 11., INK).pt_2().font_weight(FontWeight::SEMIBOLD))
                    .child(label("• 回看交易理由与计划\n• 对照实际执行与费用\n• 打开原始成交和笔记证据", 11., MUTED))
                    .child(label("当前仅展示对话布局。不会发送账户数据，也不会生成投资建议。", 10., MUTED).pt_2())
                    .child(row().gap_1().pt_2().child(badge("成交证据")).child(badge("历史表现"))))
                .child(div().flex_1())
                .child(row().h(px(42.)).px_3().rounded(px(7.)).border_1().border_color(rgb(LINE))
                    .child(label("模型连接待接入", 11., MUTED).flex_1()).child(label("➤", 18., MUTED))))
    }

    pub(super) fn dashboard(&self, cx: &mut Context<Self>) -> impl IntoElement {
        column()
            .w_full()
            .min_w(px(1120.))
            .gap_2()
            .p_2()
            .child(
                row()
                    .h(px(468.))
                    .gap_2()
                    .items_stretch()
                    .child(
                        div()
                            .w(px(238.))
                            .flex_shrink_0()
                            .child(self.sample_market(false, cx)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(370.))
                            .child(self.sample_chart_card(false, cx)),
                    )
                    .child(
                        div()
                            .w(px(224.))
                            .flex_shrink_0()
                            .child(self.sample_market(true, cx)),
                    )
                    .child(
                        div()
                            .w(px(246.))
                            .flex_shrink_0()
                            .child(self.sample_assets()),
                    ),
            )
            .child(
                row()
                    .h(px(465.))
                    .gap_2()
                    .items_stretch()
                    .child(
                        column()
                            .flex_1()
                            .gap_2()
                            .child(div().h(px(377.)).child(self.sample_chart_card(true, cx)))
                            .child(
                                card()
                                    .h(px(80.))
                                    .child(heading("技术指标", label("MA20 已显示", 9., MUTED)))
                                    .child(row().px_3().gap_3().child(badge("MA")).child(label(
                                        "EMA   BOLL   MACD   RSI   KDJ",
                                        10.,
                                        MUTED,
                                    ))),
                            ),
                    )
                    .child(div().flex_1().child(self.sample_strategy()))
                    .child(div().flex_1().child(self.sample_training()))
                    .child(
                        div()
                            .w(px(310.))
                            .flex_shrink_0()
                            .child(self.sample_assistant()),
                    ),
            )
    }
}

pub(super) fn sample_chart(index: usize) -> ChartState {
    let prices = [
        66421.07, 3248.62, 162.34, 592.31, 228.17, 121.48, 242.76, 415.32,
    ];
    let mut chart = ChartState::new(252);
    for (i, candle) in chart.candles.iter_mut().enumerate() {
        let t = i as f64 / 251.;
        let trend = 0.79 + t * 0.22 + (t * 32.).sin() * 0.018 + (t * 68.).sin() * 0.007;
        let factor = prices[index] * trend / candle.close;
        candle.open *= factor;
        candle.high *= factor;
        candle.low *= factor;
        candle.close *= factor;
    }
    let scale = prices[index] / chart.candles.last().unwrap().close;
    for candle in &mut chart.candles {
        candle.open *= scale;
        candle.high *= scale;
        candle.low *= scale;
        candle.close *= scale;
    }
    chart.ma = crate::chart::sma_close(&chart.candles, crate::chart::MA_PERIOD);
    chart.visible = 132;
    chart.offset = 120;
    chart
}

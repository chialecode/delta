//! Workspace pages: nav, accounts table, instruments (search and watchlist),
//! chart, journal, logs, settings.

use std::collections::HashSet;
use std::sync::Arc;

use crate::chart::{interactive_chart, ChartState};
use crate::perf::InteractionTimer;
use crate::theme::*;
#[path = "business.rs"]
mod business;
#[path = "dashboard.rs"]
mod dashboard;
#[cfg(test)]
#[path = "workbench_tests.rs"]
mod workbench_tests;
use delta_infra::sqlite::app::{seed_synthetic_demo, InstrumentHit};
use delta_infra::sqlite::store::Library;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::gpui::prelude::FluentBuilder as _;
use gpui_kit::gpui::{
    div, px, rgb, App, AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, StatefulInteractiveElement, Styled, Window,
};
use gpui_kit::TestSupportExt as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Home,
    Accounts,
    Instruments,
    Chart,
    Journal,
    Import,
    Settings,
    Ai,
    Recovery,
}

/// Changing the account or period used for analysis opens a new AI session.
/// `AgentRuntime::run_turn` returns that session id. The desktop keeps the
/// previous transcript available to read and does not send it again.
///
/// The instrument search and the watchlist read and write the application
/// service (`Library`), the same one the AI tools use; this view holds no
/// instrument data of its own.
pub struct Workspace {
    page: Page,
    chart: Entity<ChartState>,
    business: business::Business,
    lines: Vec<AccountRow>,
    library: Option<Arc<Library>>,
    search: Entity<InputState>,
    hits: Vec<InstrumentHit>,
    watch: Vec<String>,
    status: String,
    sample_chart: Entity<ChartState>,
    sample_selected: usize,
}

impl Workspace {
    /// Production opens a persistent library asynchronously. Synthetic data is
    /// available only through the explicit demo action or --demo.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut workspace = Self::with_library(None, Vec::new(), window, cx);
        workspace.startup(window, cx);
        workspace
    }

    pub(crate) fn with_library(
        library: Option<Arc<Library>>,
        lines: Vec<AccountRow>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let chart = AppContext::new(cx, |_| ChartState::new(0));
        cx.subscribe_in(
            &chart,
            window,
            |this: &mut Self, _, event: &crate::chart::ChartEvidence, window, cx| {
                this.start_operation(
                    crate::tasks::Operation::Evidence(event.0.clone()),
                    window,
                    cx,
                );
            },
        )
        .detach();
        cx.observe(&chart, |_, _, cx| cx.notify()).detach();
        let search = AppContext::new(cx, |cx| {
            InputState::new(window, cx).placeholder("搜索代码或标的，如 AAPL、BTC")
        });
        cx.subscribe(&search, |this: &mut Self, state, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                let query = state.read(cx).value().to_string();
                let _timer = InteractionTimer::start();
                this.run_search(&query);
                cx.notify();
            }
        })
        .detach();
        let mut workspace = Self {
            page: Page::Home,
            chart,
            business: business::Business::new(window, cx),
            lines,
            library,
            search,
            hits: Vec::new(),
            watch: Vec::new(),
            status: String::new(),
            sample_chart: cx.new(|_| dashboard::sample_chart(0)),
            sample_selected: 0,
        };
        // `Some` is used only by the headless legacy harness. Production
        // starts empty and opens the library on the background executor.
        if let Some(library) = workspace.library.clone() {
            workspace.business.demo = true;
            match crate::tasks::snapshot(&library) {
                Ok(state) => workspace.apply_snapshot(state, true, window, cx),
                Err(e) => workspace.status = e.to_string(),
            }
        }
        workspace.run_search("");
        workspace
    }

    fn run_search(&mut self, query: &str) {
        let query = query.trim().to_lowercase();
        let Some(state) = &self.business.state else {
            self.hits.clear();
            return;
        };
        self.hits = state
            .instruments
            .iter()
            .filter(|hit| {
                [&hit.id, &hit.venue, &hit.base, &hit.quote]
                    .iter()
                    .any(|s| s.to_lowercase().contains(&query))
            })
            .cloned()
            .collect();
        self.status = if self.hits.is_empty() {
            "没有匹配的标的".into()
        } else {
            format!("{} 个标的，场所与交易对分别列出", self.hits.len())
        };
    }

    fn toggle_watch(&mut self, instrument_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let on = !self.watch.iter().any(|id| id == instrument_id);
        self.start_operation(
            crate::tasks::Operation::Watch(instrument_id.into(), on),
            window,
            cx,
        );
    }

    fn nav_item(
        &self,
        id: &'static str,
        label: &'static str,
        target: Page,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        gpui_kit::component::button::Button::new(id)
            .label(label)
            .w_full()
            .when(self.page == target, |button| {
                button.bg(rgb(TINT)).text_color(rgb(ORANGE))
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                this.page = target;
                cx.notify();
            }))
    }

    fn instruments_page(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let watched: HashSet<&str> = self.watch.iter().map(String::as_str).collect();
        let button = |id: SharedString, label: &'static str, on: bool| {
            div()
                .px_2()
                .py_1()
                .text_size(px(12.))
                .rounded(px(5.))
                .cursor_pointer()
                .when(on, |d| d.bg(rgb(TINT)).text_color(rgb(ORANGE)))
                .when(!on, |d| d.bg(rgb(BG)).text_color(rgb(INK)))
                .id(id)
                .test_support()
                .aria_label(label)
                .child(label)
        };
        let results = self.hits.iter().map(|hit| {
            let is_watched = watched.contains(hit.id.as_str());
            let target = hit.id.clone();
            let chart_target = hit.id.clone();
            let identity = format!(
                "{} · 场所 {} · 交易对 {}/{}",
                hit.id, hit.venue, hit.base, hit.quote
            );
            div()
                .flex()
                .gap_2()
                .items_center()
                .text_size(px(13.))
                .child(
                    div()
                        .w(px(460.))
                        .id(SharedString::from(format!("result-{}", hit.id)))
                        .test_support()
                        .aria_label(identity.clone())
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.select_instrument(chart_target.clone(), window, cx)
                        }))
                        .child(identity),
                )
                .child(
                    button(
                        SharedString::from(format!("watch-{}", hit.id)),
                        if is_watched {
                            "移出自选"
                        } else {
                            "加入自选"
                        },
                        is_watched,
                    )
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.toggle_watch(&target, window, cx);
                        cx.notify();
                    })),
                )
        });
        let watchlist = self.watch.iter().map(|id| {
            let target = id.clone();
            div()
                .flex()
                .gap_2()
                .items_center()
                .text_size(px(13.))
                .child(
                    div()
                        .w(px(460.))
                        .id(SharedString::from(format!("watchlist-{id}")))
                        .test_support()
                        .aria_label(id.clone())
                        .child(id.clone()),
                )
                .child(
                    button(
                        SharedString::from(format!("unwatch-{id}")),
                        "移出自选",
                        false,
                    )
                    .on_click(cx.listener(move |this, _ev, window, cx| {
                        this.toggle_watch(&target, window, cx);
                        cx.notify();
                    })),
                )
        });
        div()
            .flex()
            .flex_col()
            .p_4()
            .gap_2()
            .size_full()
            .child(div().text_size(px(18.)).child("标的搜索与自选"))
            .child(
                div()
                    .w(px(460.))
                    .child(Input::new(&self.search).id("instrument-search")),
            )
            .child(
                div()
                    .id("search-status")
                    .test_support()
                    .aria_label(self.status.clone())
                    .text_size(px(12.))
                    .text_color(rgb(MUTED))
                    .child(self.status.clone()),
            )
            .child(div().flex().flex_col().gap_1().children(results))
            .child(
                div()
                    .mt_2()
                    .text_size(px(15.))
                    .child(format!("自选（{}）", self.watch.len())),
            )
            .child(div().flex().flex_col().gap_1().children(watchlist))
            .child(
                div().text_size(px(12.)).text_color(rgb(MUTED)).child(
                    "点击标的打开对应场所的日线。自选保存在当前资料库；无文件数据时显示空态。",
                ),
            )
    }
}

type AccountRow = (String, String, String, String);

/// Seed the synthetic demo library and read the account rows from it. The file
/// sits in a per-process folder (one library per folder): the library stays open
/// while the window is, and a second instance must not remove it.
#[cfg(test)]
fn demo_library() -> (Option<Arc<Library>>, Vec<AccountRow>) {
    let dir = std::env::temp_dir()
        .join("delta-r1-desktop-demo")
        .join(std::process::id().to_string());
    if let Err(err) = std::fs::create_dir_all(&dir) {
        return (None, vec![service_error_row(err.to_string())]);
    }
    match seed_synthetic_demo(&dir.join("demo.sqlite")) {
        Ok(library) => {
            let lines = match library.account_lines() {
                Ok(lines) => lines
                    .into_iter()
                    .map(|line| (line.account, line.asset, line.quantity, line.cost))
                    .collect(),
                Err(err) => vec![service_error_row(err.to_string())],
            };
            (Some(Arc::new(library)), lines)
        }
        Err(err) => (None, vec![service_error_row(err.to_string())]),
    }
}

#[cfg(test)]
fn service_error_row(message: String) -> AccountRow {
    ("service".into(), message, String::new(), String::new())
}

/// The account rows exactly as the first screen gets them.
#[cfg(test)]
fn service_account_lines() -> Vec<AccountRow> {
    demo_library().1
}

impl Render for Workspace {
    fn render(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl gpui_kit::gpui::IntoElement {
        let page = self.page;

        let sidebar = column()
            .w(px(166.))
            .flex_shrink_0()
            .h_full()
            .px_2()
            .py_3()
            .border_r_1()
            .border_color(rgb(LINE))
            .gap_1()
            .child(self.nav_item("nav-home", "⌂    首页", Page::Home, cx))
            .child(self.nav_item("nav-instruments", "☆    市场 / 自选", Page::Instruments, cx))
            .child(self.nav_item("nav-chart", "〽    K 线", Page::Chart, cx))
            .child(label("♧    训练   · 待接入", 13., MUTED).px_3().py_2())
            .child(label("◇    策略   · 待接入", 13., MUTED).px_3().py_2())
            .child(self.nav_item("nav-accounts", "▣    资产", Page::Accounts, cx))
            .child(self.nav_item("nav-journal", "▤    交易笔记", Page::Journal, cx))
            .child(label("⊗    回测   · 待接入", 13., MUTED).px_3().py_2())
            .child(self.nav_item("nav-import", "⇩    CSV 导入", Page::Import, cx))
            .child(self.nav_item("nav-ai", "◈    AI 助手", Page::Ai, cx))
            .child(self.nav_item("nav-recovery", "↺    备份 / 恢复", Page::Recovery, cx))
            .child(self.nav_item("nav-settings", "⚙    资料库 / 设置", Page::Settings, cx))
            .child(div().flex_1())
            .child(label("我的资产", 11., ORANGE).px_3().py_2())
            .children(
                self.business
                    .state
                    .iter()
                    .flat_map(|s| s.accounts.iter().take(5))
                    .map(|a| label(&a.name, 11., INK).px_3().py_2()),
            )
            .child(
                label(
                    if self.business.demo {
                        "显式演示 · 临时库"
                    } else {
                        "本地持久资料库"
                    },
                    10.,
                    MUTED,
                )
                .px_3()
                .py_3(),
            );
        let body = match page {
            Page::Home if self.business.demo => self.dashboard(cx).into_any_element(),
            Page::Home => self.live_dashboard(cx).into_any_element(),
            Page::Accounts => self.accounts_business_page(cx).into_any_element(),
            Page::Instruments => self.instruments_page(cx).into_any_element(),
            Page::Chart => self.chart_business_page(cx).into_any_element(),
            Page::Journal => self.journal_page(cx).into_any_element(),
            Page::Import => self.import_page(cx).into_any_element(),
            Page::Settings => self.settings_page(cx).into_any_element(),
            Page::Ai => self.ai_page(cx).into_any_element(),
            Page::Recovery => self.recovery_page(cx).into_any_element(),
        };
        column()
            .size_full()
            .bg(rgb(BG))
            .on_key_down(
                cx.listener(|this, event: &gpui_kit::gpui::KeyDownEvent, _, cx| {
                    if event.keystroke.key == "escape" {
                        this.business.gate.cancel.cancel();
                        this.business.ai_cancel.cancel();
                        this.business.evidence = None;
                        this.status = "已请求取消；已提交的操作仍以回执为准".into();
                        cx.notify();
                    }
                }),
            )
            .font_family("Microsoft YaHei UI")
            .text_color(rgb(INK))
            .text_size(px(12.))
            .child(
                gpui_kit::component::TitleBar::new().child(
                    row()
                        .w_full()
                        .gap_4()
                        .px_2()
                        .child(
                            row()
                                .w(px(150.))
                                .gap_2()
                                .child(label("△", 31., ORANGE))
                                .child(
                                    label("DELTA", 22., INK)
                                        .font_weight(gpui_kit::gpui::FontWeight::BOLD),
                                ),
                        )
                        .child(div().flex_1())
                        .child(
                            label("⌕   搜索代码 / 资产", 11., MUTED)
                                .px_4()
                                .py_2()
                                .w(px(340.))
                                .bg(rgb(0xeff2f6))
                                .rounded_full()
                                .id("global-search")
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.page = Page::Instruments;
                                    this.search.update(cx, |input, cx| input.focus(window, cx));
                                    cx.notify();
                                })),
                        )
                        .child(div().flex_1())
                        .child(badge(if self.business.demo {
                            "显式演示"
                        } else {
                            "本地资料库"
                        }))
                        .child(label("◉   本地工作台", 11., INK)),
                ),
            )
            .child(
                row()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .child(sidebar)
                    .child(
                        div()
                            .id("workspace-scroll")
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_scroll()
                            .child(column().child(body).child(self.evidence_panel(cx))),
                    ),
            )
            .child(
                row()
                    .h(px(28.))
                    .px_4()
                    .gap_4()
                    .border_t_1()
                    .border_color(rgb(LINE))
                    .child(label("△ DELTA", 10., INK))
                    .child(label(self.status.clone(), 10., MUTED).flex_1())
                    .child(
                        gpui_kit::component::button::Button::new("cancel-task")
                            .label("取消任务 / Esc")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.business.gate.cancel.cancel();
                                this.business.ai_cancel.cancel();
                                this.status = "已请求取消；已提交的操作仍以回执为准".into();
                                cx.notify();
                            })),
                    )
                    .child(label(
                        if self.business.demo {
                            "首页为界面样例 · 行情非实时"
                        } else {
                            "本地资料库 · 金额来自应用服务"
                        },
                        10.,
                        MUTED,
                    )),
            )
    }
}

#[allow(unused)]
fn touch(_cx: &mut App) {}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::gpui::AnyWindowHandle;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::TestAppContext;

    #[test]
    fn r1_a_05_account_page_reads_service_lines() {
        let lines = super::service_account_lines();
        let usd = lines
            .iter()
            .find(|(account, asset, _, _)| account == "acc-us" && asset == "USD")
            .expect("usd row");
        assert_eq!(usd.2, "1438");
        assert_eq!(usd.3, "—");
        assert!(lines.iter().any(|(account, asset, qty, cost)| {
            account == "acc-wallet" && asset == "BTC" && qty == "0.5" && cost == "10060"
        }));
    }

    // ---- F-15 / R1-A-12: the search and watchlist screen, driven like a user ----
    //
    // The harness renders the real view in a headless window and dispatches real
    // clicks and typing. It does not draw pixels: screenshots wait for ACT-02.
    // Events a handler emits (the search box changing) are delivered when the
    // window update that caused them ends, so every step is its own update.

    struct Ui<'a> {
        cx: &'a mut TestAppContext,
        window: AnyWindowHandle,
    }

    impl<'a> Ui<'a> {
        fn open(cx: &'a mut TestAppContext, library: Arc<Library>) -> Self {
            cx.update(gpui_kit::init);
            let handle = cx.add_window(move |window, cx| {
                Workspace::with_library(Some(library), Vec::new(), window, cx)
            });
            let mut ui = Self {
                cx,
                window: handle.into(),
            };
            ui.step(|_, _| {});
            ui.click("nav-instruments");
            ui
        }

        /// One user action, then the follow-up events and a fresh frame.
        fn step(&mut self, action: impl FnOnce(&mut gpui_kit::Window, &mut App)) {
            self.cx
                .update_window(self.window, |_, window, cx| action(window, cx))
                .unwrap();
            self.cx.run_until_parked();
            self.cx
                .update_window(self.window, |_, window, cx| window.render_frame(cx))
                .unwrap();
        }

        fn click(&mut self, id: &str) {
            let id = SharedString::from(id.to_string());
            self.step(move |window, cx| window.click(id, cx));
        }

        /// Click the search box, replace its text, and let the search run.
        fn type_query(&mut self, text: &str) {
            let text = text.to_string();
            self.step(move |window, cx| {
                window.click("instrument-search", cx);
                window.press("secondary-a", cx);
                window.press("backspace", cx);
                if !text.is_empty() {
                    window.input(&text, cx);
                }
            });
        }

        fn label(&mut self, id: &str) -> Option<String> {
            let id = SharedString::from(id.to_string());
            self.cx
                .update_window(self.window, |_, window, _| {
                    window
                        .try_find(id)
                        .and_then(|e| e.label().map(str::to_owned))
                })
                .unwrap()
        }

        fn visible(&mut self, id: &str) -> bool {
            let id = SharedString::from(id.to_string());
            self.cx
                .update_window(self.window, |_, window, _| window.try_find(id).is_some())
                .unwrap()
        }
    }

    fn demo(dir: &std::path::Path) -> Arc<Library> {
        let library = seed_synthetic_demo(&dir.join("demo.sqlite")).unwrap();
        // The same pair on a second venue, plus symbols with wildcard characters.
        for asset in ["A_B", "AXB", "50%"] {
            library.ensure_asset(asset, "crypto").unwrap();
        }
        library
            .ensure_instrument("KRAKEN:BTCUSDT", "BTC", "USDT", "KRAKEN")
            .unwrap();
        library
            .ensure_instrument("TEST:A_BUSD", "A_B", "USD", "TEST")
            .unwrap();
        library
            .ensure_instrument("TEST:AXBUSD", "AXB", "USD", "TEST")
            .unwrap();
        library
            .ensure_instrument("TEST:HALF50%", "50%", "USD", "TEST")
            .unwrap();
        Arc::new(library)
    }

    #[gpui_kit::test]
    fn home_sample_selection_and_range_do_not_mutate_service_watchlist(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let library = demo(dir.path());
        let before = library.watchlist().unwrap();
        let mut ui = Ui::open(cx, library.clone());
        ui.click("nav-home");
        ui.click("sample-watch-1");
        assert_eq!(
            ui.label("sample-chart-title-false").as_deref(),
            Some("ETH/USDT / 132 根日线样例")
        );
        assert_eq!(
            ui.label("sample-chart-title-true").as_deref(),
            Some("ETH/USDT / 132 根日线样例")
        );
        ui.click("range-false-1M");
        assert_eq!(
            ui.label("sample-chart-title-true").as_deref(),
            Some("ETH/USDT / 22 根日线样例")
        );
        assert_eq!(library.watchlist().unwrap(), before);
        ui.click("nav-instruments");
        ui.type_query("AAPL");
        assert!(ui.visible("result-NASDAQ:AAPL"));
    }

    #[gpui_kit::test]
    fn r1_a_12_search_lists_venue_and_pair_and_filters_as_the_user_types(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut ui = Ui::open(cx, demo(dir.path()));
        // Before a query, every instrument is listed with its venue and pair.
        let binance = ui.label("result-BINANCE:BTCUSDT").expect("binance row");
        assert!(
            binance.contains("场所 BINANCE") && binance.contains("BTC/USDT"),
            "{binance}"
        );
        let kraken = ui.label("result-KRAKEN:BTCUSDT").expect("kraken row");
        assert!(
            kraken.contains("场所 KRAKEN") && kraken.contains("BTC/USDT"),
            "{kraken}"
        );
        assert!(ui.visible("result-NASDAQ:AAPL"));

        ui.type_query("btc");
        // The same pair on two venues stays two rows; other symbols are gone.
        assert!(ui.visible("result-BINANCE:BTCUSDT"));
        assert!(ui.visible("result-KRAKEN:BTCUSDT"));
        assert!(!ui.visible("result-NASDAQ:AAPL"));
        assert_eq!(
            ui.label("search-status").as_deref(),
            Some("2 个标的，场所与交易对分别列出")
        );

        ui.type_query("kraken");
        assert!(!ui.visible("result-BINANCE:BTCUSDT"));
        assert!(ui.visible("result-KRAKEN:BTCUSDT"));

        ui.type_query("no-such-symbol");
        assert!(!ui.visible("result-KRAKEN:BTCUSDT"));
        assert_eq!(ui.label("search-status").as_deref(), Some("没有匹配的标的"));

        // Clearing the box lists everything again.
        ui.type_query("");
        assert!(ui.visible("result-NASDAQ:AAPL"));
        assert!(ui.visible("result-BINANCE:BTCUSDT"));
    }

    /// `_` and `%` typed into the box are literal characters, not wildcards.
    #[gpui_kit::test]
    fn r1_a_12_wildcard_characters_typed_in_the_box_match_literally(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut ui = Ui::open(cx, demo(dir.path()));
        ui.type_query("A_B");
        assert!(ui.visible("result-TEST:A_BUSD"));
        assert!(
            !ui.visible("result-TEST:AXBUSD"),
            "an underscore must not match any character"
        );
        ui.type_query("_");
        assert!(ui.visible("result-TEST:A_BUSD"));
        assert!(!ui.visible("result-NASDAQ:AAPL"));
        ui.type_query("%");
        assert!(ui.visible("result-TEST:HALF50%"));
        assert!(
            !ui.visible("result-NASDAQ:AAPL"),
            "a percent sign must not match everything"
        );
    }

    #[gpui_kit::test]
    fn r1_a_12_watch_buttons_write_the_service_and_keep_venues_apart(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let library = demo(dir.path());
        let mut ui = Ui::open(cx, library.clone());
        ui.type_query("btc");
        assert_eq!(
            ui.label("watch-KRAKEN:BTCUSDT").as_deref(),
            Some("加入自选")
        );

        ui.click("watch-KRAKEN:BTCUSDT");
        assert_eq!(
            ui.label("watch-KRAKEN:BTCUSDT").as_deref(),
            Some("移出自选")
        );
        // The lookalike on the other venue is not watched.
        assert_eq!(
            ui.label("watch-BINANCE:BTCUSDT").as_deref(),
            Some("加入自选")
        );
        assert!(ui.visible("watchlist-KRAKEN:BTCUSDT"));
        assert!(!ui.visible("watchlist-BINANCE:BTCUSDT"));

        // The screen wrote the service, not a copy of its own.
        assert_eq!(
            library.watchlist().unwrap(),
            vec!["KRAKEN:BTCUSDT".to_string()]
        );

        ui.click("watch-BINANCE:BTCUSDT");
        assert!(ui.visible("watchlist-BINANCE:BTCUSDT"));
        assert_eq!(library.watchlist().unwrap().len(), 2);

        // Removing from the watchlist section updates the search rows too.
        ui.click("unwatch-KRAKEN:BTCUSDT");
        assert!(!ui.visible("watchlist-KRAKEN:BTCUSDT"));
        assert_eq!(
            ui.label("watch-KRAKEN:BTCUSDT").as_deref(),
            Some("加入自选")
        );
        assert_eq!(
            library.watchlist().unwrap(),
            vec!["BINANCE:BTCUSDT".to_string()]
        );

        // Toggling the result button again removes it.
        ui.click("watch-BINANCE:BTCUSDT");
        assert!(!ui.visible("watchlist-BINANCE:BTCUSDT"));
        assert!(library.watchlist().unwrap().is_empty());
    }

    /// A watch added through the service shows up when the screen is opened,
    /// so the screen reads the service state rather than remembering its own.
    #[gpui_kit::test]
    fn r1_a_12_watchlist_shows_what_the_service_already_holds(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let library = demo(dir.path());
        library.add_watch("NASDAQ:AAPL").unwrap();
        let mut ui = Ui::open(cx, library);
        assert!(ui.visible("watchlist-NASDAQ:AAPL"));
        assert_eq!(ui.label("watch-NASDAQ:AAPL").as_deref(), Some("移出自选"));
    }
}

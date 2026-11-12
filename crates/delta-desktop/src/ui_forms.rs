//! Form controls keep only input buffers; application operations run through the controller.

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use chrono::{DateTime, NaiveDate, Utc};
use delta_app::{
    parse_decimal_lossless, to_canonical_text, AccountBalanceSnapshotDto, AdjustmentKind,
    AutosaveStatus, ChartContextDto, ErrorEnvelope, ManualValuation, ModelConnection,
    ModelProtocol, NoteTemplateKind, StatementPreset, SyncRequest,
};
use gpui_kit::{
    component::{
        button::{Button, ButtonVariants as _},
        h_flex,
        input::{Input, InputEvent, InputState, Textarea},
        v_flex, ActiveTheme as _, Disableable as _, IconName, Selectable as _, Sizable as _,
    },
    div,
    prelude::FluentBuilder as _,
    px, App, AppContext as _, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Window,
};

use crate::{
    controller::UiAction,
    i18n::{I18nCatalog, MarketColorConvention},
    workbench::{CenterWorkspacePanel, RightAiReviewPanel},
};

pub(crate) struct FormFields(BTreeMap<&'static str, Entity<InputState>>);

impl FormFields {
    pub(crate) fn new<T: 'static>(
        specs: &[(&'static str, &str)],
        window: &mut Window,
        cx: &mut Context<T>,
    ) -> Self {
        Self(
            specs
                .iter()
                .map(|(key, value)| {
                    let input = cx.new(|cx| {
                        InputState::new(window, cx)
                            .placeholder(I18nCatalog::tr(key))
                            .default_value(value.to_string())
                            .masked(*key == "form.api_key")
                    });
                    (*key, input)
                })
                .collect(),
        )
    }

    pub(crate) fn get(&self, key: &str) -> &Entity<InputState> {
        &self.0[key]
    }
    pub(crate) fn value(&self, key: &str, cx: &App) -> String {
        self.get(key).read(cx).value().to_string()
    }
    pub(crate) fn set(
        &self,
        key: &str,
        value: impl Into<String>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let value = value.into();
        if self.value(key, cx) != value {
            self.get(key)
                .update(cx, |input, cx| input.set_value(value, window, cx));
        }
    }
    pub(crate) fn row(&self, keys: &[&'static str]) -> impl IntoElement {
        h_flex()
            .w_full()
            .gap_3()
            .flex_wrap()
            .children(keys.iter().map(|key| {
                v_flex()
                    .flex_1()
                    .min_w(px(140.))
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(crate::theme::Chrome::caption())
                            .child(I18nCatalog::tr(key)),
                    )
                    .child(
                        Input::new(self.get(key))
                            .small()
                            .accessibility_id(*key)
                            .aria_label(I18nCatalog::tr(key)),
                    )
            }))
    }

    /// Label-less fixed-width input for dense toolbars; the placeholder carries the label.
    pub(crate) fn compact(&self, key: &'static str, width: f32) -> impl IntoElement {
        div().w(px(width)).flex_none().child(
            Input::new(self.get(key))
                .small()
                .accessibility_id(key)
                .aria_label(I18nCatalog::tr(key)),
        )
    }

    pub(crate) fn is_any_focused(&self, keys: &[&str], window: &Window, cx: &App) -> bool {
        use gpui_kit::Focusable as _;
        keys.iter().any(|k| {
            self.0
                .get(k)
                .is_some_and(|input| input.read(cx).focus_handle(cx).is_focused(window))
        })
    }
}

pub(crate) const CENTER_FIELDS: &[(&str, &str)] = &[
    ("form.account_id", ""),
    ("form.account_name", ""),
    ("form.account_kind", "broker"),
    ("form.platform", "manual"),
    ("form.portfolio_id", ""),
    ("form.portfolio_name", ""),
    ("form.member_accounts", ""),
    ("form.member_portfolios", ""),
    ("form.import_account", ""),
    ("form.import_preset", "generic"),
    ("form.import_path", ""),
    ("form.sync_symbols", ""),
    ("form.sync_start", "2026-01-01"),
    ("form.sync_end", ""),
    ("form.asset_id", ""),
    ("form.value", ""),
    ("form.currency", "USD"),
    ("form.valuation_date", ""),
    ("form.valuation_method", "manual"),
    ("form.balance_account", ""),
    ("form.balance_asset", ""),
    ("form.balance_qty", ""),
    ("form.balance_time", ""),
    ("form.amend_qty", ""),
    ("form.amend_price", ""),
    ("form.amend_fee", ""),
    ("form.transaction_search", ""),
    ("form.instrument_search", ""),
    ("form.jump_date", ""),
    ("form.watchlist_name", ""),
    (
        "form.indicators",
        "{\"sma_periods\":[5,20],\"ema_periods\":[12,26],\"macd\":[12,26,9],\"rsi_period\":14}",
    ),
    ("form.note_title", "新建复盘笔记"),
    ("form.note_tags", ""),
    ("form.notes_search", ""),
    ("form.thesis", ""),
    ("form.invalidation", ""),
    ("form.planned_risk", ""),
    ("form.execution", ""),
    ("form.emotion", ""),
    ("form.conclusion", ""),
    ("form.fill_event", ""),
    ("form.fill_qty", ""),
    ("form.attachment_path", ""),
    ("form.report_currency", "USD"),
    ("form.timezone", "UTC"),
    ("form.model_protocol", "openai-responses"),
    ("form.base_url", "https://api.openai.com/v1"),
    ("form.model_id", ""),
    ("form.api_key", ""),
    ("form.context_window", "128000"),
    ("form.output_tokens", "4096"),
    ("form.run_timeout", "120"),
    ("form.backup_path", ""),
    ("form.export_path", ""),
    ("form.restore_backup", ""),
    ("form.restore_target", ""),
    ("form.delete_confirm", ""),
    ("form.trn_instrument", ""),
    ("form.trn_start_date", ""),
    ("form.trn_end_date", ""),
    ("form.trn_history_bars", "120"),
    ("form.trn_training_bars", "250"),
    ("form.trn_reveal_bars", "60"),
    ("form.trn_warmup_bars", "200"),
    ("form.trn_seed", ""),
    ("form.trn_initial_cash", "100000"),
    ("form.trn_fee_key", "custom_fee"),
    ("form.trn_fee_name", "自定义场景费率"),
    ("form.trn_fee_rate", "0.001"),
    ("form.trn_fee_min", "1"),
    ("form.trn_fee_slippage_bps", "5"),
    ("form.trn_order_qty", "10"),
    ("form.trn_order_limit", ""),
    ("form.trn_order_stop", ""),
    ("form.trn_order_tp", ""),
    ("form.trn_order_sl", ""),
    ("form.trn_note_rationale", ""),
    ("form.trn_note_emotion", "专注"),
    ("form.trn_self_eval_summary", ""),
    (
        "form.trn_ai_question",
        "请基于本次模拟训练记录客观复盘执行纪律与盈亏归因",
    ),
];

fn invalid(message: impl Into<String>) -> ErrorEnvelope {
    ErrorEnvelope::new("INVALID_ARGUMENT", message, false)
}
fn date(value: &str) -> Result<NaiveDate, ErrorEnvelope> {
    NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d")
        .map_err(|_| invalid("日期请填写 YYYY-MM-DD"))
}
fn names(value: &str) -> Vec<String> {
    value
        .split([',', '，', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

impl CenterWorkspacePanel {
    pub(crate) fn dispatch(
        &self,
        action: Result<UiAction, ErrorEnvelope>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            Ok(action) => self
                .controller
                .update(cx, |ctl, cx| ctl.dispatch(action, window, cx)),
            Err(error) => {
                self.state.lock().unwrap().ui_error = Some(error);
                cx.notify();
            }
        }
    }

    fn button(
        &self,
        id: &'static str,
        label: &'static str,
        callback: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> Button {
        Button::new(id)
            .small()
            .label(I18nCatalog::tr(label))
            .on_click(cx.listener(move |panel, _, window, cx| callback(panel, window, cx)))
    }

    pub(crate) fn sync_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.state.lock().unwrap();
        let editor = &state.note_editor;
        self.fields
            .set("form.note_title", &editor.title_buffer, window, cx);
        self.fields
            .set("form.note_tags", editor.tags_buffer.join(", "), window, cx);
        for (key, value) in [
            ("form.thesis", &editor.structured_buffer.thesis),
            ("form.invalidation", &editor.structured_buffer.invalidation),
            ("form.planned_risk", &editor.structured_buffer.planned_risk),
            ("form.execution", &editor.structured_buffer.actual_execution),
            ("form.emotion", &editor.structured_buffer.emotion),
            ("form.conclusion", &editor.structured_buffer.conclusion),
        ] {
            self.fields.set(key, value, window, cx);
        }
        if self.note_body.read(cx).value().as_str() != editor.body_buffer {
            self.note_body.update(cx, |input, cx| {
                input.set_value(editor.body_buffer.clone(), window, cx)
            });
        }
        let settings = (
            state.layout.report_currency.clone(),
            state.layout.display_timezone.clone(),
        );
        if self.applied_settings.as_ref() != Some(&settings) {
            self.fields
                .set("form.report_currency", &settings.0, window, cx);
            self.fields.set("form.timezone", &settings.1, window, cx);
            self.applied_settings = Some(settings);
        }
        if let Some(settings) = &state.model_connection_settings {
            if self.applied_model_settings.as_ref() != Some(settings) {
                let previous = self.applied_model_settings.as_ref().map(model_form_values);
                for (key, value) in model_form_values(settings) {
                    // First load restores persisted settings. Later updates only touch
                    // fields that still equal the last applied value, preserving drafts.
                    if previous.as_ref().is_none_or(|old| {
                        old.iter().any(|(old_key, old_value)| {
                            *old_key == key && self.fields.value(key, cx) == *old_value
                        })
                    }) {
                        self.fields.set(key, value, window, cx);
                    }
                }
                self.applied_model_settings = Some(settings.clone());
            }
        }
    }

    pub(crate) fn note_input_event(
        &mut self,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            InputEvent::Change => {
                let mut state = self.state.lock().unwrap();
                let mut editor = state.note_editor.clone();
                editor.title_buffer = self.fields.value("form.note_title", cx);
                editor.tags_buffer = names(&self.fields.value("form.note_tags", cx));
                editor.body_buffer = self.note_body.read(cx).value().to_string();
                editor.structured_buffer.thesis = self.fields.value("form.thesis", cx);
                editor.structured_buffer.invalidation = self.fields.value("form.invalidation", cx);
                editor.structured_buffer.planned_risk = self.fields.value("form.planned_risk", cx);
                editor.structured_buffer.actual_execution = self.fields.value("form.execution", cx);
                editor.structured_buffer.emotion = self.fields.value("form.emotion", cx);
                editor.structured_buffer.conclusion = self.fields.value("form.conclusion", cx);
                if editor == state.note_editor {
                    return;
                }
                editor.status = AutosaveStatus::Editing;
                state.note_editor = editor;
                drop(state);
                let timer = cx.background_executor().timer(Duration::from_secs(1));
                self.autosave_task = Some(cx.spawn_in(window, async move |panel, cx| {
                    timer.await;
                    let _ = panel.update_in(cx, |panel, window, cx| panel.save_note(window, cx));
                }));
                cx.notify();
            }
            InputEvent::Blur => self.save_note(window, cx),
            _ => {}
        }
    }

    pub(crate) fn save_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.state.lock().unwrap().note_editor.clone();
        if matches!(
            editor.status,
            AutosaveStatus::Editing | AutosaveStatus::SaveFailed { .. }
        ) {
            self.dispatch(Ok(UiAction::SaveNote(editor)), window, cx);
        }
    }

    pub(crate) fn render_data_center(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.lock().unwrap().clone();
        let preview = state.import_preview.clone();
        let held = state.held_import_rows.clone();
        v_flex()
            .id("data-center-form")
            .w_full()
            .gap_3()
            .child(section_title(I18nCatalog::tr("form.accounts"), cx))
            .children(state.accounts.iter().map(|a| {
                div().text_sm().child(format!(
                    "{} · {} · {} · {}",
                    a.id, a.name, a.account_type, a.status
                ))
            }))
            .child(self.fields.row(&[
                "form.account_id",
                "form.account_name",
                "form.account_kind",
                "form.platform",
            ]))
            .child(
                h_flex()
                    .gap_2()
                    .child(self.button(
                        "account-save",
                        "action.save_account",
                        |panel, window, cx| {
                            panel.dispatch(
                                Ok(UiAction::Account {
                                    id: panel.fields.value("form.account_id", cx),
                                    name: panel.fields.value("form.account_name", cx),
                                    kind: panel.fields.value("form.account_kind", cx),
                                    platform: panel.fields.value("form.platform", cx),
                                    status: "active".into(),
                                }),
                                window,
                                cx,
                            );
                        },
                        cx,
                    ))
                    .child(self.button(
                        "load-offline-example",
                        "action.load_example",
                        |panel, window, cx| {
                            panel.dispatch(Ok(UiAction::SeedExample), window, cx);
                        },
                        cx,
                    )),
            )
            .child(self.fields.row(&[
                "form.portfolio_id",
                "form.portfolio_name",
                "form.member_accounts",
                "form.member_portfolios",
            ]))
            .child(self.button(
                "portfolio-save",
                "action.save_portfolio",
                |panel, window, cx| {
                    panel.dispatch(
                        Ok(UiAction::Portfolio {
                            id: panel.fields.value("form.portfolio_id", cx),
                            name: panel.fields.value("form.portfolio_name", cx),
                            accounts: names(&panel.fields.value("form.member_accounts", cx)),
                            portfolios: names(&panel.fields.value("form.member_portfolios", cx)),
                        }),
                        window,
                        cx,
                    );
                },
                cx,
            ))
            .child(section_title(I18nCatalog::tr("form.import"), cx))
            .child(self.fields.row(&[
                "form.import_account",
                "form.import_preset",
                "form.import_path",
            ]))
            .child(
                h_flex()
                    .gap_2()
                    .child(self.button(
                        "import-preview",
                        "action.preview_import",
                        |panel, window, cx| panel.import_action(false, window, cx),
                        cx,
                    ))
                    .child(
                        self.button(
                            "import-commit",
                            "action.commit_import",
                            |panel, window, cx| panel.import_action(true, window, cx),
                            cx,
                        )
                        .disabled(preview.is_none()),
                    ),
            )
            .children(preview.into_iter().flat_map(|p| {
                p.rows.into_iter().map(|row| {
                    div()
                        .text_xs()
                        .p_2()
                        .border_1()
                        .border_color(cx.theme().border)
                        .child(format!(
                            "#{} · {} · {}\n{}\n{}",
                            row.row_no,
                            row.validation_state,
                            row.error_reason.unwrap_or_default(),
                            row.raw_payload,
                            row.normalized
                                .and_then(|v| serde_json::to_string(&v).ok())
                                .unwrap_or_default()
                        ))
                })
            }))
            .children(held.into_iter().map(|id| {
                let include_id = id.clone();
                let exclude_id = id.clone();
                h_flex()
                    .gap_2()
                    .child(div().text_xs().child(id.clone()))
                    .child(
                        Button::new(format!("held-include-{id}"))
                            .small()
                            .label(I18nCatalog::tr("action.include_held"))
                            .on_click(cx.listener(move |panel, _, window, cx| {
                                panel.dispatch(
                                    Ok(UiAction::ResolveHeld {
                                        row: include_id.clone(),
                                        include: true,
                                    }),
                                    window,
                                    cx,
                                )
                            })),
                    )
                    .child(
                        Button::new(format!("held-exclude-{id}"))
                            .small()
                            .label(I18nCatalog::tr("action.exclude_held"))
                            .on_click(cx.listener(move |panel, _, window, cx| {
                                panel.dispatch(
                                    Ok(UiAction::ResolveHeld {
                                        row: exclude_id.clone(),
                                        include: false,
                                    }),
                                    window,
                                    cx,
                                )
                            })),
                    )
            }))
            .child(section_title(I18nCatalog::tr("form.sync"), cx))
            .child(
                self.fields
                    .row(&["form.sync_symbols", "form.sync_start", "form.sync_end"]),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        self.button(
                            "market-sync",
                            "action.sync",
                            |panel, window, cx| {
                                let result = (|| {
                                    Ok(UiAction::Sync(SyncRequest {
                                        start: date(&panel.fields.value("form.sync_start", cx))?,
                                        end: date(&panel.fields.value("form.sync_end", cx))?,
                                        instruments: names(
                                            &panel.fields.value("form.sync_symbols", cx),
                                        ),
                                        fx_pairs: vec![],
                                        use_configured_credentials: true,
                                        base_urls: BTreeMap::new(),
                                    }))
                                })();
                                panel.dispatch(result, window, cx);
                            },
                            cx,
                        )
                        .disabled(state.active_sync_job.is_some()),
                    )
                    .child(self.button(
                        "sync-cancel",
                        "action.cancel_sync",
                        |panel, _, cx| panel.controller.update(cx, |ctl, cx| ctl.cancel_sync(cx)),
                        cx,
                    )),
            )
            .child(
                div().text_xs().child(
                    state
                        .data_quality
                        .map(|v| {
                            format!(
                                "数据版本 {} · {}\n{}\n{}",
                                v.value.dataset_version,
                                I18nCatalog::quality_label(crate::Locale::ZhCn, v.quality),
                                v.missing_inputs.join("\n"),
                                v.value
                                    .checkpoints
                                    .iter()
                                    .map(|cp| format!(
                                        "{} · {} · {}",
                                        cp.provider, cp.resource_key, cp.last_committed_at
                                    ))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            )
                        })
                        .unwrap_or_default(),
                ),
            )
    }

    fn import_action(&self, commit: bool, window: &mut Window, cx: &mut Context<Self>) {
        let result = (|| {
            let preset = StatementPreset::parse(&self.fields.value("form.import_preset", cx))
                .ok_or_else(|| invalid("请选择支持的账单格式"))?;
            let account = self.fields.value("form.import_account", cx);
            let path = self.fields.value("form.import_path", cx);
            if path.trim().is_empty() {
                return Err(invalid("请填写账单文件路径"));
            }
            let file = PathBuf::from(path);
            Ok(if commit {
                UiAction::ImportCommit {
                    account,
                    preset,
                    file,
                }
            } else {
                UiAction::ImportPreview {
                    account,
                    preset,
                    file,
                }
            })
        })();
        self.dispatch(result, window, cx);
    }

    pub(crate) fn render_journal(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.lock().unwrap().clone();
        let enabled = state.note_editor.allow_model_access;
        let save_status = match &state.note_editor.status {
            AutosaveStatus::Editing => I18nCatalog::tr("autosave.editing").to_string(),
            AutosaveStatus::Saving => I18nCatalog::tr("autosave.saving").to_string(),
            AutosaveStatus::Saved { saved_at, .. } => {
                format!("{} · {saved_at}", I18nCatalog::tr("autosave.saved"))
            }
            AutosaveStatus::SaveFailed { error, .. } => {
                format!("{} · {error}", I18nCatalog::tr("autosave.failed"))
            }
        };
        v_flex()
            .id("journal-editor-form")
            .w_full()
            .gap_2()
            .child(
                div()
                    .id("journal-autosave-status")
                    .text_xs()
                    .child(save_status),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(self.button(
                        "note-new-pre",
                        "action.note_pre",
                        |panel, window, cx| {
                            panel.dispatch(
                                Ok(UiAction::NewNote(NoteTemplateKind::PreTrade)),
                                window,
                                cx,
                            )
                        },
                        cx,
                    ))
                    .child(self.button(
                        "note-new-during",
                        "action.note_during",
                        |panel, window, cx| {
                            panel.dispatch(
                                Ok(UiAction::NewNote(NoteTemplateKind::DuringTrade)),
                                window,
                                cx,
                            )
                        },
                        cx,
                    ))
                    .child(self.button(
                        "note-new-post",
                        "action.note_post",
                        |panel, window, cx| {
                            panel.dispatch(
                                Ok(UiAction::NewNote(NoteTemplateKind::PostTrade)),
                                window,
                                cx,
                            )
                        },
                        cx,
                    ))
                    .child(self.button(
                        "note-save",
                        "action.retry_save",
                        |panel, window, cx| panel.save_note(window, cx),
                        cx,
                    ))
                    .child(self.button(
                        "note-model-access",
                        if enabled {
                            "action.disallow_model"
                        } else {
                            "action.allow_model"
                        },
                        |panel, window, cx| {
                            {
                                let mut state = panel.state.lock().unwrap();
                                state.note_editor.allow_model_access =
                                    !state.note_editor.allow_model_access;
                                state.note_editor.status = AutosaveStatus::Editing;
                            }
                            panel.save_note(window, cx);
                        },
                        cx,
                    )),
            )
            .child(self.fields.row(&["form.note_title", "form.note_tags"]))
            .child(
                Textarea::new(&self.note_body)
                    .h(px(190.))
                    .accessibility_id("note-body")
                    .aria_label(I18nCatalog::tr("form.note_body")),
            )
            .child(
                self.fields
                    .row(&["form.thesis", "form.invalidation", "form.planned_risk"]),
            )
            .child(
                self.fields
                    .row(&["form.execution", "form.emotion", "form.conclusion"]),
            )
            .child(
                div()
                    .text_sm()
                    .child(I18nCatalog::tr("form.markdown_preview")),
            )
            .child(gpui_kit::component::text::TextView::markdown(
                "note-preview",
                state.note_editor.body_buffer.clone(),
            ))
            .child(self.fields.row(&["form.fill_event", "form.fill_qty"]))
            .child(
                h_flex()
                    .gap_2()
                    .child(self.button(
                        "note-link-fill",
                        "action.link_fill",
                        |panel, window, cx| {
                            panel.dispatch(
                                Ok(UiAction::LinkFill {
                                    event: panel.fields.value("form.fill_event", cx),
                                    quantity: panel.fields.value("form.fill_qty", cx),
                                }),
                                window,
                                cx,
                            )
                        },
                        cx,
                    ))
                    .child(self.button(
                        "note-link-chart",
                        "action.link_chart",
                        |panel, window, cx| {
                            let result = panel.current_chart_context().map(UiAction::LinkChart);
                            panel.dispatch(result, window, cx);
                        },
                        cx,
                    )),
            )
            .child(self.fields.row(&["form.attachment_path"]))
            .child(self.button(
                "note-add-attachment",
                "action.attach",
                |panel, window, cx| {
                    panel.dispatch(
                        Ok(UiAction::Attachment(PathBuf::from(
                            panel.fields.value("form.attachment_path", cx),
                        ))),
                        window,
                        cx,
                    )
                },
                cx,
            ))
            .child(self.fields.row(&["form.notes_search"]))
            .child(self.button(
                "notes-search",
                "action.search",
                |panel, window, cx| {
                    panel.dispatch(
                        Ok(UiAction::SearchNotes(
                            panel.fields.value("form.notes_search", cx),
                        )),
                        window,
                        cx,
                    )
                },
                cx,
            ))
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().warning)
                    .child(state.notes_result_notice),
            )
            .children(state.notes_results.into_iter().map(|note| {
                let id = note.id.clone();
                let links = note.links.clone();
                v_flex()
                    .gap_1()
                    .child(
                        Button::new(format!("open-note-{}", note.id))
                            .small()
                            .ghost()
                            .label(note.title)
                            .on_click(cx.listener(move |panel, _, window, cx| {
                                panel.dispatch(Ok(UiAction::OpenNote(id.clone())), window, cx)
                            })),
                    )
                    .children(links.into_iter().map(|link| {
                        let target = link.target_id.clone();
                        Button::new(format!("note-link-{}", link.id))
                            .small()
                            .label(format!(
                                "{} · {}{}",
                                if link.target_type == "review_date" {
                                    I18nCatalog::tr("form.review_date")
                                } else {
                                    &link.target_type
                                },
                                link.target_id,
                                if link.is_target_amended {
                                    " · 已更正"
                                } else {
                                    ""
                                }
                            ))
                            .disabled(!matches!(
                                link.target_type.as_str(),
                                "chart_context" | "event"
                            ))
                            .on_click(cx.listener(move |panel, _, window, cx| {
                                let action = if link.target_type == "chart_context" {
                                    UiAction::ChartContext(target.clone())
                                } else {
                                    UiAction::SelectTransaction(target.clone())
                                };
                                panel.dispatch(Ok(action), window, cx);
                            }))
                    }))
            }))
    }

    fn current_chart_context(&self) -> Result<ChartContextDto, ErrorEnvelope> {
        let state = self.state.lock().unwrap();
        let slice = state
            .kline_slice
            .as_ref()
            .ok_or_else(|| invalid("请先打开有行情的图表"))?;
        let first = slice
            .bars
            .first()
            .ok_or_else(|| invalid("当前图表没有 K 线"))?;
        let last = slice
            .bars
            .last()
            .ok_or_else(|| invalid("当前图表没有 K 线"))?;
        Ok(ChartContextDto {
            id: format!("chart:{}", uuid::Uuid::new_v4()),
            instrument_id: state.active_instrument.clone(),
            venue: slice.venue.clone(),
            timeframe: state.active_timeframe.clone(),
            visible_start_date: first.session_date,
            visible_end_date: last.session_date,
            anchor_date: state
                .kline_viewport
                .crosshair_index
                .and_then(|index| index.checked_sub(slice.visible_start_index))
                .and_then(|index| slice.bars.get(index))
                .map(|b| b.session_date),
            adjustment: state.active_adjustment,
            indicators_json: serde_json::to_string(&state.kline_viewport.indicator_config)
                .map_err(|e| invalid(e.to_string()))?,
            dataset_version: state
                .pinned_chart_version
                .or(Some(state.latest_dataset_version)),
            latest_dataset_version: state.latest_dataset_version,
            has_newer_dataset_version: false,
        })
    }

    /// Left market column: search, results and watchlist groups (Longbridge watchlist layout).
    pub(crate) fn render_market_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.lock().unwrap().clone();
        let caption = crate::theme::Chrome::caption();
        let active = state.active_instrument.clone();
        let row = |id: SharedString,
                   symbol: String,
                   detail: String,
                   selected: bool,
                   cx: &mut Context<Self>| {
            h_flex()
                .id(id)
                .w_full()
                .px_3()
                .py_1p5()
                .gap_2()
                .items_center()
                .cursor_pointer()
                .border_b_1()
                .border_color(cx.theme().table_row_border)
                .when(selected, |row| row.bg(cx.theme().table_active))
                .hover(|row| row.bg(cx.theme().table_hover))
                .child(
                    v_flex()
                        .min_w(px(0.))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .child(symbol),
                        )
                        .child(div().text_xs().text_color(caption).truncate().child(detail)),
                )
        };
        let mut groups: Vec<(String, Vec<String>)> = state
            .watchlist_groups
            .iter()
            .map(|group| (group.name.clone(), group.instruments.clone()))
            .collect();
        if groups.is_empty() && !active.is_empty() {
            groups.push((
                I18nCatalog::tr("market.current").to_string(),
                vec![active.clone()],
            ));
        }
        v_flex()
            .id("market-controls")
            .w(px(230.))
            .flex_none()
            .h_full()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .px_3()
                    .h(px(crate::theme::PANEL_HEADER_HEIGHT))
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(I18nCatalog::tr("market.watchlist")),
                    )
                    .child(div().text_xs().text_color(caption).child(format!(
                        "{} {}",
                        groups.iter().map(|group| group.1.len()).sum::<usize>(),
                        I18nCatalog::tr("market.symbols")
                    ))),
            )
            .child(
                h_flex()
                    .px_3()
                    .pb_2()
                    .gap_1()
                    .child(
                        div().flex_1().min_w(px(0.)).child(
                            Input::new(self.fields.get("form.instrument_search"))
                                .small()
                                .accessibility_id("form.instrument_search")
                                .aria_label(I18nCatalog::tr("form.instrument_search")),
                        ),
                    )
                    .child(
                        Button::new("instrument-search")
                            .small()
                            .icon(IconName::Search)
                            .tooltip(I18nCatalog::tr("action.search"))
                            .on_click(cx.listener(|panel, _, window, cx| {
                                panel.dispatch(
                                    Ok(UiAction::SearchInstrument(
                                        panel.fields.value("form.instrument_search", cx),
                                    )),
                                    window,
                                    cx,
                                )
                            })),
                    ),
            )
            .when(!state.search_hits.is_empty(), |column| {
                column
                    .child(
                        div()
                            .px_3()
                            .py_1()
                            .text_xs()
                            .text_color(caption)
                            .bg(cx.theme().table_head)
                            .child(I18nCatalog::tr("market.search_results")),
                    )
                    .children(state.search_hits.iter().map(|hit| {
                        let id = hit.instrument_id.clone();
                        row(
                            SharedString::from(format!("instrument-hit-{id}")),
                            hit.symbol.clone(),
                            format!("{} · {}", hit.venue, id),
                            id == active,
                            cx,
                        )
                        .on_click(cx.listener(
                            move |panel, _, window, cx| {
                                panel.dispatch(
                                    Ok(UiAction::SelectInstrument(id.clone())),
                                    window,
                                    cx,
                                )
                            },
                        ))
                    }))
            })
            .children(
                groups
                    .into_iter()
                    .enumerate()
                    .map(|(group_index, (name, items))| {
                        v_flex()
                            .w_full()
                            .child(
                                h_flex()
                                    .px_3()
                                    .py_1()
                                    .gap_2()
                                    .bg(cx.theme().table_head)
                                    .text_xs()
                                    .text_color(caption)
                                    .child(name)
                                    .child(div().flex_1())
                                    .child(I18nCatalog::tr("market.name_code")),
                            )
                            .children(items.into_iter().enumerate().map(|(index, id)| {
                                let (venue, symbol) = id
                                    .split_once(':')
                                    .map(|(venue, symbol)| (venue.to_string(), symbol.to_string()))
                                    .unwrap_or_else(|| (String::new(), id.clone()));
                                let target = id.clone();
                                row(
                                    SharedString::from(format!(
                                        "watchlist-item-{group_index}-{index}"
                                    )),
                                    symbol,
                                    venue,
                                    id == active,
                                    cx,
                                )
                                .on_click(cx.listener(
                                    move |panel, _, window, cx| {
                                        panel.dispatch(
                                            Ok(UiAction::SelectInstrument(target.clone())),
                                            window,
                                            cx,
                                        )
                                    },
                                ))
                            }))
                    }),
            )
            .child(div().flex_1())
            .child(
                v_flex()
                    .p_3()
                    .gap_1p5()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(
                        Input::new(self.fields.get("form.watchlist_name"))
                            .small()
                            .accessibility_id("form.watchlist_name")
                            .aria_label(I18nCatalog::tr("form.watchlist_name")),
                    )
                    .child(
                        Button::new("watchlist-add")
                            .small()
                            .w_full()
                            .icon(IconName::Star)
                            .label(I18nCatalog::tr("action.add_watchlist"))
                            .on_click(cx.listener(|panel, _, window, cx| {
                                panel.dispatch(
                                    Ok(UiAction::Watchlist {
                                        name: panel.fields.value("form.watchlist_name", cx),
                                        instrument: panel
                                            .state
                                            .lock()
                                            .unwrap()
                                            .active_instrument
                                            .clone(),
                                    }),
                                    window,
                                    cx,
                                )
                            })),
                    ),
            )
    }

    /// Quote header above the chart: symbol, last close and host-computed change, OHLCV grid.
    pub(crate) fn render_quote_header(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.lock().unwrap().clone();
        let caption = crate::theme::Chrome::caption();
        let convention = state.layout.color_convention;
        let slice = state.kline_slice.as_ref();
        let (venue, symbol) = state
            .active_instrument
            .split_once(':')
            .map(|(venue, symbol)| (venue.to_string(), symbol.to_string()))
            .unwrap_or_else(|| (String::new(), state.active_instrument.clone()));
        let change = slice.and_then(|slice| slice.last_close_change());
        let signed = convention.format_signed_change(change.as_ref().and_then(|c| c.change), "");
        let (change_fg, _) = convention.colors(signed.direction, cx);
        let close_text = change
            .as_ref()
            .map(|c| to_canonical_text(c.close))
            .unwrap_or_else(|| "—".into());
        let pct_text = change
            .as_ref()
            .and_then(|c| c.change_pct)
            .map(|pct| {
                let text = to_canonical_text(pct);
                if pct.is_sign_positive() && !pct.is_zero() {
                    format!("+{text}%")
                } else {
                    format!("{text}%")
                }
            })
            .unwrap_or_default();
        let last_bar = slice.and_then(|slice| slice.bars.last());
        let stat = |label: &'static str, value: String| {
            h_flex()
                .w(px(132.))
                .justify_between()
                .gap_2()
                .child(div().text_xs().text_color(caption).child(label))
                .child(div().text_xs().child(value))
        };
        let value = |pick: fn(&delta_app::BarRecord) -> rust_decimal::Decimal| {
            last_bar
                .map(|bar| to_canonical_text(pick(bar)))
                .unwrap_or_else(|| "—".into())
        };
        h_flex()
            .id("market-quote-header")
            .w_full()
            .px_4()
            .py_2()
            .gap_6()
            .items_center()
            .flex_none()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_baseline()
                            .child(
                                div()
                                    .text_base()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(symbol),
                            )
                            .child(div().text_xs().text_color(caption).child(venue)),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .items_baseline()
                            .child(
                                div()
                                    .text_size(px(26.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(change_fg)
                                    .child(close_text),
                            )
                            .child(
                                v_flex()
                                    .text_xs()
                                    .text_color(change_fg)
                                    .child(signed.text)
                                    .child(pct_text),
                            ),
                    )
                    .child(
                        div().text_xs().text_color(caption).child(
                            change
                                .as_ref()
                                .map(|c| {
                                    format!(
                                        "{} {} · {}",
                                        I18nCatalog::tr("market.close_of"),
                                        c.session_date,
                                        I18nCatalog::tr("market.vs_previous")
                                    )
                                })
                                .unwrap_or_else(|| I18nCatalog::tr("market.no_bars").to_string()),
                        ),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w(px(0.))
                    .flex_wrap()
                    .gap_x_5()
                    .gap_y_1()
                    .child(stat(I18nCatalog::tr("market.open"), value(|bar| bar.open)))
                    .child(stat(I18nCatalog::tr("market.high"), value(|bar| bar.high)))
                    .child(stat(I18nCatalog::tr("market.low"), value(|bar| bar.low)))
                    .child(stat(
                        I18nCatalog::tr("market.volume"),
                        value(|bar| bar.volume),
                    ))
                    .child(stat(
                        I18nCatalog::tr("market.prev_close"),
                        change
                            .as_ref()
                            .and_then(|c| c.previous_close)
                            .map(to_canonical_text)
                            .unwrap_or_else(|| "—".into()),
                    ))
                    .child(stat(
                        I18nCatalog::tr("market.bars"),
                        slice
                            .map(|slice| slice.total_visible_bars.to_string())
                            .unwrap_or_else(|| "—".into()),
                    )),
            )
    }

    /// Chart toolbar under the quote header: jump-to-date, zoom/pan, adjustment and indicators.
    pub(crate) fn render_chart_toolbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let adjustment = self.state.lock().unwrap().active_adjustment;
        let tool = |id: &'static str, label: &'static str| {
            Button::new(id)
                .xsmall()
                .ghost()
                .label(I18nCatalog::tr(label))
        };
        h_flex()
            .id("market-chart-toolbar")
            .w_full()
            .px_3()
            .py_1()
            .gap_1()
            .flex_wrap()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().table_head)
            .child(
                Button::new("chart-period-1d")
                    .xsmall()
                    .ghost()
                    .selected(true)
                    .label(I18nCatalog::tr("market.daily")),
            )
            .child(div().w(px(1.)).h(px(14.)).mx_1().bg(cx.theme().border))
            .child(tool("chart-zoom-in", "action.zoom_in").on_click(
                cx.listener(|panel, _, window, cx| panel.change_viewport(0, 0.7, window, cx)),
            ))
            .child(tool("chart-zoom-out", "action.zoom_out").on_click(
                cx.listener(|panel, _, window, cx| panel.change_viewport(0, 1.4, window, cx)),
            ))
            .child(tool("chart-pan-left", "action.pan_left").on_click(
                cx.listener(|panel, _, window, cx| panel.change_viewport(-20, 1., window, cx)),
            ))
            .child(tool("chart-pan-right", "action.pan_right").on_click(
                cx.listener(|panel, _, window, cx| panel.change_viewport(20, 1., window, cx)),
            ))
            .child(
                Button::new("chart-adjustment")
                    .xsmall()
                    .ghost()
                    .selected(adjustment != AdjustmentKind::Raw)
                    .label(I18nCatalog::tr("action.adjustment"))
                    .on_click(cx.listener(|panel, _, window, cx| {
                        let adjustment = match panel.state.lock().unwrap().active_adjustment {
                            AdjustmentKind::Raw => AdjustmentKind::SplitAdjusted,
                            AdjustmentKind::SplitAdjusted => AdjustmentKind::TotalReturnAdjusted,
                            _ => AdjustmentKind::Raw,
                        };
                        panel.dispatch(Ok(UiAction::ChartAdjustment(adjustment)), window, cx);
                    })),
            )
            .child(div().flex_1())
            .child(self.fields.compact("form.jump_date", 110.))
            .child(tool("chart-jump", "action.jump_date").on_click(cx.listener(
                |panel, _, window, cx| {
                    panel.dispatch(
                        date(&panel.fields.value("form.jump_date", cx))
                            .map(UiAction::ChartJumpDate),
                        window,
                        cx,
                    );
                },
            )))
            .child(self.fields.compact("form.indicators", 170.))
            .child(
                tool("indicators-save", "action.apply_indicators").on_click(cx.listener(
                    |panel, _, window, cx| {
                        panel.dispatch(
                            Ok(UiAction::Indicators(
                                panel.fields.value("form.indicators", cx),
                            )),
                            window,
                            cx,
                        )
                    },
                )),
            )
    }

    /// Clickable trade/note markers under the chart.
    pub(crate) fn render_chart_markers(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.lock().unwrap().clone();
        h_flex()
            .px_3()
            .py_1()
            .gap_1()
            .flex_wrap()
            .items_center()
            .child(
                div()
                    .text_xs()
                    .text_color(crate::theme::Chrome::caption())
                    .child(I18nCatalog::tr("market.markers")),
            )
            .children(
                state
                    .kline_slice
                    .iter()
                    .flat_map(|slice| slice.markers.iter())
                    .map(|marker| {
                        let id = marker.marker.id.clone();
                        Button::new(format!("open-chart-marker-{id}"))
                            .xsmall()
                            .outline()
                            .label(marker.marker.label.clone())
                            .on_click(cx.listener(move |panel, _, window, cx| {
                                panel.dispatch(Ok(UiAction::SelectMarker(id.clone())), window, cx)
                            }))
                    }),
            )
    }

    pub(crate) fn change_viewport(
        &self,
        pan: i64,
        zoom: f64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut viewport = self.state.lock().unwrap().kline_viewport.clone();
        if pan != 0 {
            viewport.pan(pan as isize);
        }
        if zoom != 1. {
            viewport.zoom(zoom, 0.5);
        }
        self.state.lock().unwrap().kline_viewport = viewport.clone();
        self.dispatch(Ok(UiAction::ChartViewport(viewport)), window, cx);
    }

    pub(crate) fn render_asset_controls(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.lock().unwrap().clone();
        let selected = match &state.selected_marker_outcome {
            Some(crate::workbench::MarkerSelectionOutcome::TradeFill(row)) => Some(row.clone()),
            _ => None,
        };
        v_flex()
            .id("assets-controls")
            .w_full()
            .gap_2()
            .child(self.fields.row(&["form.transaction_search"]))
            .child(
                h_flex()
                    .gap_2()
                    .child(self.button(
                        "transaction-filter",
                        "action.search",
                        |panel, window, cx| {
                            let mut filter = panel.state.lock().unwrap().transaction_filter.clone();
                            let id = panel.fields.value("form.transaction_search", cx);
                            filter.instrument_id =
                                if id.trim().is_empty() { None } else { Some(id) };
                            filter.offset = 0;
                            panel.dispatch(Ok(UiAction::PageTransactions(filter)), window, cx);
                        },
                        cx,
                    ))
                    .child(self.button(
                        "transaction-prev",
                        "action.previous_page",
                        |panel, window, cx| panel.page_transactions(false, window, cx),
                        cx,
                    ))
                    .child(self.button(
                        "transaction-next",
                        "action.next_page",
                        |panel, window, cx| panel.page_transactions(true, window, cx),
                        cx,
                    )),
            )
            .child(
                div()
                    .text_sm()
                    .child(I18nCatalog::tr("form.manual_valuation")),
            )
            .child(self.fields.row(&[
                "form.account_id",
                "form.asset_id",
                "form.value",
                "form.currency",
            ]))
            .child(
                self.fields
                    .row(&["form.valuation_date", "form.valuation_method"]),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(self.button(
                        "manual-asset-save",
                        "action.save_asset",
                        |panel, window, cx| panel.manual_valuation(false, window, cx),
                        cx,
                    ))
                    .child(self.button(
                        "manual-liability-save",
                        "action.save_liability",
                        |panel, window, cx| panel.manual_valuation(true, window, cx),
                        cx,
                    )),
            )
            .children(state.manual_valuations.into_iter().map(|v| {
                div().text_xs().child(format!(
                    "{} · {} {} · {} · {}",
                    v.asset_or_liability_id,
                    to_canonical_text(v.value),
                    v.currency,
                    v.as_of,
                    v.method
                ))
            }))
            .child(
                div()
                    .text_sm()
                    .child(I18nCatalog::tr("form.balance_reconciliation")),
            )
            .child(self.fields.row(&[
                "form.balance_account",
                "form.balance_asset",
                "form.balance_qty",
                "form.balance_time",
            ]))
            .child(self.button(
                "balance-reconcile",
                "action.reconcile",
                |panel, window, cx| {
                    let result = (|| {
                        let time = DateTime::parse_from_rfc3339(
                            &panel.fields.value("form.balance_time", cx),
                        )
                        .map_err(|_| invalid("时间请使用含时区的 RFC3339 格式"))?
                        .with_timezone(&Utc);
                        let quantity =
                            parse_decimal_lossless(&panel.fields.value("form.balance_qty", cx))
                                .map_err(|e| invalid(e.to_string()))?;
                        Ok(UiAction::Reconcile {
                            account: panel.fields.value("form.balance_account", cx),
                            snapshots: vec![AccountBalanceSnapshotDto {
                                asset_id: panel.fields.value("form.balance_asset", cx),
                                quantity,
                                as_of: time,
                            }],
                        })
                    })();
                    panel.dispatch(result, window, cx);
                },
                cx,
            ))
            .children(selected.into_iter().map(|row| {
                let id = row.event_id.clone();
                v_flex()
                    .gap_2()
                    .child(div().text_xs().child(format!(
                        "{} · {} · {}\n{}",
                        row.event_id,
                        row.occurred_at,
                        row.account_id,
                        row.raw_payload.unwrap_or_default()
                    )))
                    .child(self.fields.row(&[
                        "form.amend_qty",
                        "form.amend_price",
                        "form.amend_fee",
                    ]))
                    .child(
                        Button::new("transaction-amend")
                            .small()
                            .label(I18nCatalog::tr("action.amend"))
                            .on_click(cx.listener(move |panel, _, window, cx| {
                                let result = (|| {
                                    let price = panel.fields.value("form.amend_price", cx);
                                    Ok(UiAction::Amend {
                                        event: id.clone(),
                                        quantity: parse_decimal_lossless(
                                            &panel.fields.value("form.amend_qty", cx),
                                        )
                                        .map_err(|e| invalid(e.to_string()))?,
                                        price: if price.trim().is_empty() {
                                            None
                                        } else {
                                            Some(
                                                parse_decimal_lossless(&price)
                                                    .map_err(|e| invalid(e.to_string()))?,
                                            )
                                        },
                                        fee: parse_decimal_lossless(
                                            &panel.fields.value("form.amend_fee", cx),
                                        )
                                        .map_err(|e| invalid(e.to_string()))?,
                                    })
                                })();
                                panel.dispatch(result, window, cx);
                            })),
                    )
            }))
    }

    fn page_transactions(&self, next: bool, window: &mut Window, cx: &mut Context<Self>) {
        let mut filter = self.state.lock().unwrap().transaction_filter.clone();
        filter.offset = if next {
            filter.offset.saturating_add(filter.limit)
        } else {
            filter.offset.saturating_sub(filter.limit)
        };
        self.dispatch(Ok(UiAction::PageTransactions(filter)), window, cx);
    }
    fn manual_valuation(&self, liability: bool, window: &mut Window, cx: &mut Context<Self>) {
        let result = (|| {
            let account = self.fields.value("form.account_id", cx);
            let asset = self.fields.value("form.asset_id", cx);
            Ok(UiAction::ManualValuation(ManualValuation {
                id: format!("manual:{account}:{asset}"),
                account_id: account,
                asset_or_liability_id: asset,
                is_liability: liability,
                value: parse_decimal_lossless(&self.fields.value("form.value", cx))
                    .map_err(|e| invalid(e.to_string()))?,
                currency: self.fields.value("form.currency", cx),
                as_of: date(&self.fields.value("form.valuation_date", cx))?,
                method: self.fields.value("form.valuation_method", cx),
            }))
        })();
        self.dispatch(result, window, cx);
    }

    pub(crate) fn render_settings(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.lock().unwrap().clone();
        let update = crate::lifecycle::update_state();
        let update_text = update
            .as_ref()
            .map(crate::lifecycle::describe_update)
            .unwrap_or_else(|| "此构建不提供在线更新（源码运行）".into());
        let update_ready = update
            .as_ref()
            .is_some_and(|u| matches!(u.status, crate::lifecycle::UpdateStatus::Ready { .. }));
        let update_busy = update.as_ref().is_some_and(|u| {
            matches!(u.status, crate::lifecycle::UpdateStatus::BusyConfirm { .. })
        });
        let update_available = update.as_ref().is_some_and(|u| {
            !matches!(u.status, crate::lifecycle::UpdateStatus::Unavailable { .. })
        });
        let auto_check = update.as_ref().is_some_and(|u| u.auto_check);
        let identity = delta_app::InstallIdentity::current();
        let about = format!(
            "{} {} · 通道 {} · 源码指纹 {} · {}: {}",
            identity
                .map(delta_app::InstallIdentity::display_name)
                .unwrap_or("DELTA (源码运行)"),
            delta_app::app_version(),
            identity
                .map(delta_app::InstallIdentity::name)
                .unwrap_or("source"),
            &delta_app::build_source_fingerprint()
                [..delta_app::build_source_fingerprint().len().min(12)],
            I18nCatalog::tr("form.data_directory"),
            state.data_dir.display()
        );
        let data_dir = state.data_dir.clone();
        let data_dir_toggle = state.data_dir.clone();
        v_flex().id("settings-form").w_full().gap_3()
            .child(section_title(I18nCatalog::tr("settings.about"), cx))
            .child(div().id("settings-about").text_xs().child(about))
            .child(section_title(I18nCatalog::tr("settings.update"), cx))
            .child(div().id("settings-update-status").text_xs().child(update_text))
            .when(update_available, |form| form.child(h_flex().gap_2()
                .child(Button::new("update-check").small().label(I18nCatalog::tr("action.check_update"))
                    .on_click(move |_, _, _| crate::lifecycle::start_check(data_dir.clone())))
                .child(Button::new("update-auto-check").small()
                    .label(I18nCatalog::tr(if auto_check { "action.auto_check_on" } else { "action.auto_check_off" }))
                    .on_click(move |_, _, _| crate::lifecycle::set_auto_check(data_dir_toggle.clone(), !auto_check)))
                .when(update_ready, |row| row.child(Button::new("update-apply").small().primary()
                    .label(I18nCatalog::tr("action.apply_update"))
                    .on_click(|_, _, _| crate::lifecycle::APPLY_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst))))
                .when(update_busy, |row| row.child(Button::new("update-apply-busy").small().warning()
                    .label(I18nCatalog::tr("action.apply_update_busy"))
                    .on_click(|_, _, _| {
                        crate::lifecycle::APPLY_CONFIRMED_BUSY.store(true, std::sync::atomic::Ordering::SeqCst);
                        crate::lifecycle::APPLY_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
                    })))))
            .child(self.fields.row(&["form.report_currency", "form.timezone"]))
            .child(h_flex().gap_2()
                .child(self.button("settings-save", "action.save_settings", |panel, window, cx| {
                    panel.dispatch(Ok(UiAction::Settings { currency: panel.fields.value("form.report_currency", cx), timezone: panel.fields.value("form.timezone", cx) }), window, cx);
                }, cx))
                .child(self.button("settings-colors", "action.change_colors", |panel, window, cx| {
                    { let mut state = panel.state.lock().unwrap(); state.layout.color_convention = match state.layout.color_convention {
                        MarketColorConvention::RedUpGreenDown => MarketColorConvention::GreenUpRedDown, _ => MarketColorConvention::RedUpGreenDown }; }
                    panel.dispatch(Ok(UiAction::SaveLayout), window, cx);
                }, cx)))
            .child(section_title(I18nCatalog::tr("form.model_connection"), cx))
            .child(self.fields.row(&["form.model_protocol", "form.base_url", "form.model_id"]))
            .child(self.fields.row(&["form.api_key", "form.context_window", "form.output_tokens", "form.run_timeout"]))
            .child(h_flex().gap_2()
                .child(self.button("model-configure", "action.save_connection", |panel, window, cx| {
                    let result = (|| {
                        let protocol = ModelProtocol::parse(&panel.fields.value("form.model_protocol", cx)).map_err(|e| invalid(e.safe_message))?;
                        let saved = panel.state.lock().unwrap().model_connection_settings.clone();
                        let id = saved.as_ref().map(|connection| connection.id.clone()).unwrap_or_else(|| panel.draft_model_connection_id.clone());
                        let mut connection = saved.unwrap_or_else(|| ModelConnection::loopback(id, protocol, "", ""));
                        connection.protocol = protocol;
                        connection.base_url = panel.fields.value("form.base_url", cx);
                        connection.model_id = panel.fields.value("form.model_id", cx);
                        connection.context_window = panel.fields.value("form.context_window", cx).parse().map_err(|_| invalid("上下文窗口请填写正整数"))?;
                        connection.budget.max_output_tokens = panel.fields.value("form.output_tokens", cx).parse().map_err(|_| invalid("输出预算请填写正整数"))?;
                        let seconds: u64 = panel.fields.value("form.run_timeout", cx).parse().map_err(|_| invalid("超时请填写正整数秒"))?;
                        if seconds == 0 || connection.budget.max_output_tokens == 0 {
                            return Err(invalid("超时和输出预算必须大于零"));
                        }
                        connection.budget.max_run_duration_ms = seconds.checked_mul(1000).ok_or_else(|| invalid("超时过大"))?;
                        connection.timeouts.total_ms = connection.budget.max_run_duration_ms;
                        let key = panel.fields.value("form.api_key", cx);
                        connection.validate_endpoint().map_err(|e| invalid(e.safe_message))?;
                        Ok(UiAction::ConfigureModel { connection, secret: if key.trim().is_empty() { None } else { Some(key) } })
                    })();
                    let valid = result.is_ok();
                    panel.dispatch(result, window, cx);
                    if valid { panel.fields.set("form.api_key", "", window, cx); }
                }, cx))
                .child(self.button("model-probe", "action.probe_model", |panel, window, cx| panel.dispatch(Ok(UiAction::ProbeModel), window, cx), cx)))
            .child(section_title(I18nCatalog::tr("form.library_operations"), cx))
            .child(self.fields.row(&["form.backup_path", "form.export_path"]))
            .child(h_flex().gap_2()
                .child(self.button("library-backup", "action.backup", |panel, window, cx| panel.dispatch(Ok(UiAction::Backup(PathBuf::from(panel.fields.value("form.backup_path", cx)))), window, cx), cx))
                .child(self.button("library-export", "action.export", |panel, window, cx| panel.dispatch(Ok(UiAction::Export(PathBuf::from(panel.fields.value("form.export_path", cx)))), window, cx), cx)))
            .child(self.fields.row(&["form.restore_backup", "form.restore_target"]))
            .child(self.button("library-restore", "action.restore", |panel, window, cx| panel.dispatch(Ok(UiAction::Restore {
                backup: PathBuf::from(panel.fields.value("form.restore_backup", cx)), target: PathBuf::from(panel.fields.value("form.restore_target", cx)) }), window, cx), cx))
            .child(self.button("library-delete-preview", "action.delete_preview", |panel, window, cx| panel.dispatch(Ok(UiAction::DeletePreview), window, cx), cx))
            .children(state.deletion_preview.into_iter().map(|preview| {
                v_flex().gap_2().child(div().text_sm().child(format!("{} · {} · {} 个账户 · {} 篇笔记 · {} 个附件 · {} 个会话 · {} 个文件 · {} 字节",
                    preview.library_id, preview.target_data_dir.display(), preview.accounts, preview.notes, preview.attachments, preview.ai_sessions, preview.file_count, preview.total_bytes)))
                    .child(self.fields.row(&["form.delete_confirm"]))
                    .child(Button::new("library-delete-confirm").small().label(I18nCatalog::tr("action.delete_confirm"))
                        .on_click(cx.listener(move |panel, _, window, cx| panel.dispatch(Ok(UiAction::DeleteLibrary { preview: preview.clone(), typed_id: panel.fields.value("form.delete_confirm", cx) }), window, cx))))
            }))
    }
}

fn model_form_values(connection: &ModelConnection) -> [(&'static str, String); 7] {
    [
        (
            "form.model_protocol",
            connection.protocol.as_str().to_string(),
        ),
        ("form.base_url", connection.base_url.clone()),
        ("form.model_id", connection.model_id.clone()),
        ("form.context_window", connection.context_window.to_string()),
        (
            "form.output_tokens",
            connection.budget.max_output_tokens.to_string(),
        ),
        (
            "form.run_timeout",
            (connection.budget.max_run_duration_ms / 1000).to_string(),
        ),
        // Secrets are never restored from the credential store into the form.
        ("form.api_key", String::new()),
    ]
}

impl RightAiReviewPanel {
    pub(crate) fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let question = self.question.read(cx).value().to_string();
        if question.trim().is_empty() {
            return;
        }
        let session = self.session_id.clone();
        self.controller.update(cx, |ctl, cx| {
            ctl.dispatch(UiAction::AiReview { session, question }, window, cx)
        });
    }
}

/// Section heading used across data center, journal and settings pages: a short dark bar and a
/// compact semibold label, matching the dense terminal layout.
pub(crate) fn section_title(label: &'static str, cx: &App) -> impl IntoElement {
    h_flex()
        .w_full()
        .pt_2()
        .pb_1()
        .gap_2()
        .items_center()
        .border_b_1()
        .border_color(cx.theme().border)
        .child(
            div()
                .w(px(3.))
                .h(px(13.))
                .rounded(px(1.5))
                .bg(cx.theme().foreground),
        )
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(label),
        )
}

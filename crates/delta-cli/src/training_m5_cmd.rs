//! M5 training commands (`S-55`): data preparation, practice accounts, modify / undo /
//! abandon / restart, drawings, history and higher-timeframe views. Same service as the desktop.

use super::*;
use delta_app::{
    ModifyTrainingOrderRequest, PracticeTransferKind, PrepareTrainingDataRequest, Timeframe,
    TrainingDrawingDto, DEFAULT_TRAINING_INSTRUMENT,
};
use std::collections::BTreeMap;

pub(super) const M5_USAGE: &str = "
  delta-cli training prepare-data [--instrument <id>]... [--timeframe 1d|4h|1h]... [--from <YYYY-MM-DD>] [--status] [--base-url <provider>=<url>]... [--data-dir <dir>]
  delta-cli training account show|history|analysis [--currency USD|USDT] [--data-dir <dir>]
  delta-cli training account transfer --currency USD|USDT (--deposit <dec> | --withdraw <dec>) [--data-dir <dir>]
  delta-cli training history [--data-dir <dir>]
  delta-cli training modify --run-id <id> --order-id <id> [--qty <dec>] [--limit-price <dec>] [--stop-price <dec>] [--data-dir <dir>]
  delta-cli training undo|abandon|restart --run-id <id> [--data-dir <dir>]
  delta-cli training view --run-id <id> --timeframe 4h|1d [--data-dir <dir>]
  delta-cli training drawing list --run-id <id> | add --run-id <id> --kind ray|horizontal|segment --anchor <bar>:<price>... [--color <c>] [--style solid|dashed|dotted] | delete --run-id <id> --drawing-id <id> [--data-dir <dir>]
  (create also accepts --timeframe 1d|4h|1h, --amount <dec> to fund from the practice account, --independent for independent capital;
   order accepts --fill-rule visible-close for a quick fill at the visible close)";

/// Parse `--flag value` / `--flag=value` / bare switches; repeated value flags accumulate.
fn parse_flags(
    action: &str,
    args: &[String],
    value_flags: &[&str],
    switches: &[&str],
) -> Result<BTreeMap<String, Vec<String>>, ErrorEnvelope> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut i = 0;
    while i < args.len() {
        let (flag, inline) = split_flag(&args[i]);
        if value_flags.contains(&flag) {
            let v = next_flag_value(action, flag, inline, args, &mut i)?;
            out.entry(flag.to_string()).or_default().push(v);
        } else if switches.contains(&flag) {
            reject_inline_bool_val(action, flag, inline)?;
            out.entry(flag.to_string()).or_default();
        } else {
            return Err(unknown_flag_err(action, &args[i]));
        }
        i += 1;
    }
    Ok(out)
}

fn one(flags: &BTreeMap<String, Vec<String>>, flag: &str) -> Option<String> {
    flags.get(flag).and_then(|v| v.last().cloned())
}

fn required(
    action: &str,
    flags: &BTreeMap<String, Vec<String>>,
    flag: &str,
) -> Result<String, ErrorEnvelope> {
    one(flags, flag)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ErrorEnvelope::new(
                "missing_arg",
                format!("'training {action}' requires {flag} <value>"),
                false,
            )
        })
}

pub(super) fn parse_timeframe_arg(raw: &str) -> Result<Timeframe, ErrorEnvelope> {
    Timeframe::parse(raw.trim()).ok_or_else(|| {
        ErrorEnvelope::new(
            "invalid_timeframe",
            format!("unknown timeframe '{raw}' (expected 1d, 4h or 1h)"),
            false,
        )
    })
}

fn currency(flags: &BTreeMap<String, Vec<String>>) -> String {
    one(flags, "--currency")
        .map(|c| c.trim().to_ascii_uppercase())
        .unwrap_or_else(|| "USDT".to_string())
}

pub(super) fn dispatch_m5(
    action: &str,
    svc: &mut AppService,
    args: &[String],
) -> Option<Result<String, ErrorEnvelope>> {
    let result = match action {
        "prepare-data" | "prepare_data" => cmd_prepare_data(svc, args),
        "account" => cmd_account(svc, args),
        "history" => ensure_no_extra_args("history", args)
            .and_then(|_| svc.training_history())
            .and_then(|v| to_sanitized_pretty_json(&v)),
        "modify" => cmd_modify(svc, args),
        "undo" => parse_only_run_id("undo", args)
            .and_then(|id| svc.undo_training_last(&id))
            .and_then(|v| to_sanitized_pretty_json(&v)),
        "abandon" => parse_only_run_id("abandon", args)
            .and_then(|id| svc.abandon_training_run(&id))
            .and_then(|v| to_sanitized_pretty_json(&v)),
        "restart" => parse_only_run_id("restart", args)
            .and_then(|id| svc.restart_training_run(&id))
            .and_then(|v| to_sanitized_pretty_json(&v)),
        "view" => cmd_view(svc, args),
        "drawing" => cmd_drawing(svc, args),
        _ => return None,
    };
    Some(result)
}

fn cmd_prepare_data(svc: &mut AppService, args: &[String]) -> Result<String, ErrorEnvelope> {
    let flags = parse_flags(
        "prepare-data",
        args,
        &["--instrument", "--timeframe", "--from", "--base-url"],
        &["--status"],
    )?;
    if flags.contains_key("--status") {
        let id =
            one(&flags, "--instrument").unwrap_or_else(|| DEFAULT_TRAINING_INSTRUMENT.to_string());
        return to_sanitized_pretty_json(&svc.training_data_status(&id, true)?);
    }
    let mut req = PrepareTrainingDataRequest::default();
    if let Some(ids) = flags.get("--instrument") {
        req.instruments = ids.iter().map(|s| s.trim().to_string()).collect();
    }
    if let Some(tfs) = flags.get("--timeframe") {
        req.timeframes = tfs
            .iter()
            .map(|t| parse_timeframe_arg(t))
            .collect::<Result<_, _>>()?;
    }
    if let Some(from) = one(&flags, "--from") {
        req.from = Some(parse_date_arg("--from", &from)?);
    }
    // Provider overrides (`binance=…`, `binance_archive=…`, `okx=…`) for recorded fixtures.
    for raw in flags.get("--base-url").cloned().unwrap_or_default() {
        let (key, url) = raw.split_once('=').ok_or_else(|| {
            ErrorEnvelope::new(
                "invalid_arg",
                format!("--base-url '{raw}' must be <provider>=<url>"),
                false,
            )
        })?;
        req.base_urls
            .insert(key.trim().to_string(), url.trim().to_string());
    }
    to_sanitized_pretty_json(&svc.prepare_training_data(&req)?)
}

fn cmd_account(svc: &mut AppService, args: &[String]) -> Result<String, ErrorEnvelope> {
    let (sub, rest) = match args.split_first() {
        Some((s, rest)) if !s.starts_with("--") => (s.as_str(), rest),
        _ => ("show", args),
    };
    match sub {
        "show" => {
            let flags = parse_flags("account show", rest, &["--currency"], &[])?;
            to_sanitized_pretty_json(&svc.show_practice_account(&currency(&flags))?)
        }
        "history" => {
            let flags = parse_flags("account history", rest, &["--currency"], &[])?;
            to_sanitized_pretty_json(&svc.practice_account_history(&currency(&flags))?)
        }
        "analysis" => {
            let flags = parse_flags("account analysis", rest, &["--currency"], &[])?;
            to_sanitized_pretty_json(&svc.practice_account_analysis(&currency(&flags))?)
        }
        "transfer" => {
            let flags = parse_flags(
                "account transfer",
                rest,
                &["--currency", "--deposit", "--withdraw"],
                &[],
            )?;
            let ccy = currency(&flags);
            let (kind, raw) =
                match (one(&flags, "--deposit"), one(&flags, "--withdraw")) {
                    (Some(d), None) => (PracticeTransferKind::Deposit, d),
                    (None, Some(w)) => (PracticeTransferKind::Withdrawal, w),
                    _ => return Err(ErrorEnvelope::new(
                        "invalid_arg",
                        "'training account transfer' needs exactly one of --deposit or --withdraw",
                        false,
                    )),
                };
            let amount = parse_dec_arg("--amount", &raw)?;
            to_sanitized_pretty_json(&svc.transfer_practice_funds(&ccy, kind, amount)?)
        }
        other => Err(ErrorEnvelope::new(
            "invalid_subcommand",
            format!("unknown 'training account' subcommand '{other}'"),
            false,
        )),
    }
}

fn cmd_modify(svc: &mut AppService, args: &[String]) -> Result<String, ErrorEnvelope> {
    let flags = parse_flags(
        "modify",
        args,
        &[
            "--run-id",
            "--order-id",
            "--qty",
            "--limit-price",
            "--stop-price",
        ],
        &[],
    )?;
    let run_id = required("modify", &flags, "--run-id")?;
    let dec_opt = |f: &str| -> Result<Option<Decimal>, ErrorEnvelope> {
        one(&flags, f).map(|v| parse_dec_arg(f, &v)).transpose()
    };
    let req = ModifyTrainingOrderRequest {
        order_id: required("modify", &flags, "--order-id")?,
        quantity: dec_opt("--qty")?,
        limit_price: dec_opt("--limit-price")?,
        stop_price: dec_opt("--stop-price")?,
    };
    to_sanitized_pretty_json(&svc.modify_training_order(&run_id, req)?)
}

fn cmd_view(svc: &mut AppService, args: &[String]) -> Result<String, ErrorEnvelope> {
    let flags = parse_flags("view", args, &["--run-id", "--timeframe"], &[])?;
    let run_id = required("view", &flags, "--run-id")?;
    let tf = parse_timeframe_arg(&required("view", &flags, "--timeframe")?)?;
    to_sanitized_pretty_json(&svc.get_training_aggregated_view(&run_id, tf)?)
}

fn cmd_drawing(svc: &mut AppService, args: &[String]) -> Result<String, ErrorEnvelope> {
    let (sub, rest) = match args.split_first() {
        Some((s, rest)) if !s.starts_with("--") => (s.as_str(), rest),
        _ => ("list", args),
    };
    match sub {
        "list" => {
            let flags = parse_flags("drawing list", rest, &["--run-id"], &[])?;
            let run_id = required("drawing list", &flags, "--run-id")?;
            to_sanitized_pretty_json(&svc.list_training_drawings(&run_id)?)
        }
        "add" => {
            let flags = parse_flags(
                "drawing add",
                rest,
                &[
                    "--run-id",
                    "--kind",
                    "--anchor",
                    "--color",
                    "--style",
                    "--drawing-id",
                ],
                &[],
            )?;
            let run_id = required("drawing add", &flags, "--run-id")?;
            let anchors = flags
                .get("--anchor")
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|a| {
                    let (bar, price) = a.split_once(':').ok_or_else(|| {
                        ErrorEnvelope::new(
                            "invalid_arg",
                            format!("anchor '{a}' must be <bar>:<price>"),
                            false,
                        )
                    })?;
                    let bar = bar.trim().parse::<i64>().map_err(|e| {
                        ErrorEnvelope::new("invalid_arg", format!("anchor bar '{bar}': {e}"), false)
                    })?;
                    Ok((bar, parse_dec_arg("--anchor", price)?))
                })
                .collect::<Result<Vec<_>, ErrorEnvelope>>()?;
            let dto = TrainingDrawingDto {
                drawing_id: one(&flags, "--drawing-id").unwrap_or_default(),
                kind: required("drawing add", &flags, "--kind")?,
                anchors,
                color: one(&flags, "--color").unwrap_or_else(|| "#f0b90b".to_string()),
                line_style: one(&flags, "--style").unwrap_or_else(|| "solid".to_string()),
            };
            to_sanitized_pretty_json(&svc.save_training_drawing(&run_id, dto)?)
        }
        "delete" => {
            let flags = parse_flags("drawing delete", rest, &["--run-id", "--drawing-id"], &[])?;
            let run_id = required("drawing delete", &flags, "--run-id")?;
            let id = required("drawing delete", &flags, "--drawing-id")?;
            svc.delete_training_drawing(&run_id, &id)?;
            to_sanitized_pretty_json(&serde_json::json!({ "deleted": id }))
        }
        other => Err(ErrorEnvelope::new(
            "invalid_subcommand",
            format!("unknown 'training drawing' subcommand '{other}'"),
            false,
        )),
    }
}

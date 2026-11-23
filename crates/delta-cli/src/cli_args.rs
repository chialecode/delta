//! Shared strict CLI argument parser and terminal-safe RFC 8259 JSON output helpers.

use chrono::NaiveDate;
use delta_app::{sanitize_terminal_output, BuildChannel, ErrorEnvelope};
use delta_core::parse_decimal_lossless;
use rust_decimal::Decimal;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;

/// Serialize `value` as pretty JSON while ensuring every ASCII/Unicode control character
/// (`\x07` BEL, `\x1b` ESC, `\x7f` DEL, C1 controls `U+0080..=U+009F`) is safely escaped as
/// valid RFC 8259 `\uXXXX` so output is both terminal-safe (`D-25` / `S-10`) and lossless JSON.
pub(crate) fn to_sanitized_pretty_json(value: &impl Serialize) -> Result<String, ErrorEnvelope> {
    let raw = serde_json::to_string_pretty(value)
        .map_err(|e| ErrorEnvelope::new("JSON_SERIALIZE_FAILED", e.to_string(), false))?;
    Ok(escape_json_control_chars(&raw))
}

fn escape_json_control_chars(raw: &str) -> String {
    let mut json_safe = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\n' | '\r' | '\t' => json_safe.push(ch),
            // Every Unicode `Cc` control char is U+0000..=U+001F or U+007F..=U+009F (<= 0xFFFF).
            c if c.is_control() => {
                json_safe.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => json_safe.push(c),
        }
    }
    sanitize_terminal_output(&json_safe)
}

/// Print `value` to stdout as terminal-safe, valid RFC 8259 pretty JSON.
pub(crate) fn print_json(value: &impl Serialize) {
    let text = to_sanitized_pretty_json(value).unwrap_or_else(|e| {
        let fallback = serde_json::json!({
            "code": e.code,
            "message": e.message,
        });
        escape_json_control_chars(&fallback.to_string())
    });
    println!("{text}");
}

/// Print an `ErrorEnvelope` to stderr as terminal-safe, valid RFC 8259 pretty JSON.
pub(crate) fn print_error(err: &ErrorEnvelope) {
    let text = to_sanitized_pretty_json(err).unwrap_or_else(|_| {
        let fallback = serde_json::json!({
            "code": err.code,
            "message": err.message,
        });
        escape_json_control_chars(&fallback.to_string())
    });
    eprintln!("{text}");
}

/// Global flags extracted strictly before subcommand dispatch (`--data-dir`, `--channel`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct GlobalFlags {
    pub data_dir: Option<PathBuf>,
    pub channel_override: Option<BuildChannel>,
    pub channel_raw: Option<String>,
}

/// Extract global `--data-dir` and `--channel` flags (supporting both `--flag value` and
/// `--flag=value`) while failing strictly on missing/empty values or unknown channel names.
/// When `allow_raw_channel` is true (e.g. `package-install` / `package-uninstall`), non-empty
/// raw channel values are preserved in `channel_raw` so the command can emit its own error code.
pub(crate) fn extract_global_flags(
    args: &[String],
    allow_raw_channel: bool,
) -> Result<(GlobalFlags, Vec<String>), ErrorEnvelope> {
    let mut globals = GlobalFlags::default();
    let mut rest = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let tok = args[i].as_str();
        if tok == "--data-dir" {
            let val = args.get(i + 1).ok_or_else(|| {
                ErrorEnvelope::new("missing_arg", "--data-dir requires a path value", false)
            })?;
            if val.trim().is_empty() {
                return Err(ErrorEnvelope::new(
                    "missing_arg",
                    "--data-dir requires a non-empty path value",
                    false,
                ));
            }
            globals.data_dir = Some(PathBuf::from(val));
            i += 2;
            continue;
        }
        if let Some(val) = tok.strip_prefix("--data-dir=") {
            if val.trim().is_empty() {
                return Err(ErrorEnvelope::new(
                    "missing_arg",
                    "--data-dir requires a non-empty path value",
                    false,
                ));
            }
            globals.data_dir = Some(PathBuf::from(val));
            i += 1;
            continue;
        }
        if tok == "--channel" {
            let val = args.get(i + 1).ok_or_else(|| {
                ErrorEnvelope::new("missing_arg", "--channel requires a channel name", false)
            })?;
            if val.trim().is_empty() {
                return Err(ErrorEnvelope::new(
                    "missing_arg",
                    "--channel requires a non-empty channel name",
                    false,
                ));
            }
            globals.channel_raw = Some(val.clone());
            match BuildChannel::parse(val) {
                Some(ch) => globals.channel_override = Some(ch),
                None if allow_raw_channel => {}
                None => {
                    return Err(ErrorEnvelope::new(
                        "invalid_channel",
                        format!("unsupported channel '{val}'"),
                        false,
                    ));
                }
            }
            i += 2;
            continue;
        }
        if let Some(val) = tok.strip_prefix("--channel=") {
            if val.trim().is_empty() {
                return Err(ErrorEnvelope::new(
                    "missing_arg",
                    "--channel requires a non-empty channel name",
                    false,
                ));
            }
            globals.channel_raw = Some(val.to_string());
            match BuildChannel::parse(val) {
                Some(ch) => globals.channel_override = Some(ch),
                None if allow_raw_channel => {}
                None => {
                    return Err(ErrorEnvelope::new(
                        "invalid_channel",
                        format!("unsupported channel '{val}'"),
                        false,
                    ));
                }
            }
            i += 1;
            continue;
        }
        rest.push(args[i].clone());
        i += 1;
    }
    Ok((globals, rest))
}

pub(crate) fn split_flag(raw: &str) -> (&str, Option<&str>) {
    if raw.starts_with("--") {
        if let Some((k, v)) = raw.split_once('=') {
            return (k, Some(v));
        }
    }
    (raw, None)
}

/// Declarative flag specification for a subcommand.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct FlagSpec<'a> {
    pub value_flags: &'a [&'a str],
    pub switches: &'a [&'a str],
    pub optional_value_flags: &'a [&'a str],
    /// `(alias, canonical)` pairs rewritten before matching against the slices above.
    pub aliases: &'a [(&'a str, &'a str)],
}

/// Parsed flags for a subcommand; values are keyed by canonical flag name.
#[derive(Debug, Default, Clone)]
pub(crate) struct ParsedFlags {
    values: BTreeMap<String, Vec<String>>,
    present: BTreeMap<String, bool>,
}

impl ParsedFlags {
    pub(crate) fn parse(
        action: &str,
        args: &[String],
        spec: FlagSpec<'_>,
    ) -> Result<Self, ErrorEnvelope> {
        let mut out = Self::default();
        let mut i = 0;
        while i < args.len() {
            let (raw_flag, inline) = split_flag(&args[i]);
            let flag = spec
                .aliases
                .iter()
                .find(|(alias, _)| *alias == raw_flag)
                .map_or(raw_flag, |(_, canonical)| *canonical);
            if spec.value_flags.contains(&flag) {
                let val = if let Some(v) = inline {
                    v.to_string()
                } else {
                    let v = args.get(i + 1).ok_or_else(|| {
                        ErrorEnvelope::new(
                            "missing_arg",
                            format!("flag '{raw_flag}' on '{action}' requires a value"),
                            false,
                        )
                    })?;
                    i += 1;
                    v.clone()
                };
                out.present.insert(flag.to_string(), true);
                out.values.entry(flag.to_string()).or_default().push(val);
            } else if spec.switches.contains(&flag) {
                if inline.is_some() {
                    return Err(ErrorEnvelope::new(
                        "invalid_arg",
                        format!("boolean flag '{raw_flag}' on '{action}' does not take '=value'"),
                        false,
                    ));
                }
                out.present.insert(flag.to_string(), true);
            } else if spec.optional_value_flags.contains(&flag) {
                out.present.insert(flag.to_string(), true);
                if let Some(v) = inline {
                    out.values
                        .entry(flag.to_string())
                        .or_default()
                        .push(v.to_string());
                } else if let Some(peek) = args.get(i + 1) {
                    if !peek.starts_with("--") {
                        i += 1;
                        out.values
                            .entry(flag.to_string())
                            .or_default()
                            .push(peek.clone());
                    }
                }
            } else {
                return Err(ErrorEnvelope::new(
                    "unknown_flag",
                    format!("unexpected argument '{}' for '{action}'", args[i]),
                    false,
                ));
            }
            i += 1;
        }
        Ok(out)
    }

    pub(crate) fn has(&self, flag: &str) -> bool {
        self.present.get(flag).copied().unwrap_or(false)
    }

    pub(crate) fn one(&self, flag: &str) -> Option<&str> {
        self.values
            .get(flag)
            .and_then(|v| v.last())
            .map(String::as_str)
    }

    pub(crate) fn all(&self, flag: &str) -> &[String] {
        self.values.get(flag).map_or(&[], Vec::as_slice)
    }

    pub(crate) fn required(&self, action: &str, flag: &str) -> Result<String, ErrorEnvelope> {
        self.one(flag)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ErrorEnvelope::new(
                    "missing_arg",
                    format!("'{action}' requires {flag} <value>"),
                    false,
                )
            })
    }

    pub(crate) fn opt_parsed<T: FromStr>(
        &self,
        flag: &str,
        err_code: &str,
    ) -> Result<Option<T>, ErrorEnvelope>
    where
        T::Err: std::fmt::Display,
    {
        self.one(flag)
            .map(|raw| {
                raw.trim().parse::<T>().map_err(|e| {
                    ErrorEnvelope::new(
                        err_code,
                        format!("invalid value for {flag} ('{raw}'): {e}"),
                        false,
                    )
                })
            })
            .transpose()
    }

    pub(crate) fn opt_decimal(&self, flag: &str) -> Result<Option<Decimal>, ErrorEnvelope> {
        self.one(flag)
            .map(|raw| parse_dec_arg(flag, raw))
            .transpose()
    }

    pub(crate) fn opt_date(&self, flag: &str) -> Result<Option<NaiveDate>, ErrorEnvelope> {
        self.one(flag)
            .map(|raw| parse_date_arg(flag, raw))
            .transpose()
    }
}

pub(crate) fn parse_dec_arg(flag: &str, raw: &str) -> Result<Decimal, ErrorEnvelope> {
    parse_decimal_lossless(raw.trim()).map_err(|e| {
        ErrorEnvelope::new(
            "invalid_decimal",
            format!("invalid decimal for {flag} ('{raw}'): {e}"),
            false,
        )
    })
}

pub(crate) fn parse_usize_arg(flag: &str, raw: &str) -> Result<usize, ErrorEnvelope> {
    raw.trim().parse::<usize>().map_err(|e| {
        ErrorEnvelope::new(
            "invalid_integer",
            format!("invalid non-negative integer for {flag} ('{raw}'): {e}"),
            false,
        )
    })
}

pub(crate) fn parse_date_arg(flag: &str, raw: &str) -> Result<NaiveDate, ErrorEnvelope> {
    NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d").map_err(|e| {
        ErrorEnvelope::new(
            "invalid_date",
            format!("invalid date for {flag} ('{raw}', expected YYYY-MM-DD): {e}"),
            false,
        )
    })
}

pub(crate) fn ensure_no_extra_args(action: &str, args: &[String]) -> Result<(), ErrorEnvelope> {
    if let Some(first) = args.first() {
        return Err(ErrorEnvelope::new(
            "unknown_flag",
            format!("unexpected argument '{first}' for '{action}'"),
            false,
        ));
    }
    Ok(())
}

/// True only when `--help` or `-h` appears as a standalone flag (not as the value of a preceding
/// `--flag <value>` token).
pub(crate) fn has_standalone_help_flag(args: &[String], value_flags: &[&str]) -> bool {
    let mut i = 0;
    while i < args.len() {
        let tok = args[i].as_str();
        if matches!(tok, "--help" | "-h") {
            return true;
        }
        if tok.starts_with("--") && !tok.contains('=') && value_flags.contains(&tok) {
            i += 2;
            continue;
        }
        i += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|v| (*v).to_string()).collect()
    }

    #[test]
    fn extract_global_flags_supports_equals_and_rejects_missing_value() {
        let (g, rest) =
            extract_global_flags(&s(&["delete", "--yes", "--data-dir=sandbox/dir"]), false)
                .unwrap();
        assert_eq!(g.data_dir, Some(PathBuf::from("sandbox/dir")));
        assert_eq!(rest, s(&["delete", "--yes"]));

        // Trailing `--data-dir` without a path MUST error (never silently fall back to default dir).
        let err = extract_global_flags(&s(&["delete", "--yes", "--data-dir"]), false).unwrap_err();
        assert_eq!(err.code, "missing_arg");

        let err = extract_global_flags(&s(&["delete", "--data-dir="]), false).unwrap_err();
        assert_eq!(err.code, "missing_arg");

        let err = extract_global_flags(&s(&["init", "--channel", "bogus"]), false).unwrap_err();
        assert_eq!(err.code, "invalid_channel");

        let err =
            extract_global_flags(&s(&["package-install", "--channel", ""]), true).unwrap_err();
        assert_eq!(err.code, "missing_arg");

        let err = extract_global_flags(&s(&["package-install", "--channel="]), true).unwrap_err();
        assert_eq!(err.code, "missing_arg");
    }

    #[test]
    fn parsed_flags_rejects_unknown_and_inline_bool_values() {
        let spec = FlagSpec {
            value_flags: &["--version"],
            switches: &["--yes"],
            optional_value_flags: &[],
            aliases: &[],
        };
        let err = ParsedFlags::parse("library delete", &s(&["--yes", "--datadir", "x"]), spec)
            .unwrap_err();
        assert_eq!(err.code, "unknown_flag");

        let err = ParsedFlags::parse("library delete", &s(&["--yes=true"]), spec).unwrap_err();
        assert_eq!(err.code, "invalid_arg");

        let parsed = ParsedFlags::parse("explain", &s(&["--version=bad"]), spec).unwrap();
        let err = parsed
            .opt_parsed::<i64>("--version", "invalid_integer")
            .unwrap_err();
        assert_eq!(err.code, "invalid_integer");
    }

    #[test]
    fn sanitized_json_preserves_rfc8259_for_del_and_c1_controls() {
        let payload = serde_json::json!({
            "msg": "bell\x07esc\x1b[31mdel\x7fcsi\u{009b}32mok"
        });
        let rendered = to_sanitized_pretty_json(&payload).unwrap();
        assert!(!rendered.contains('\x07'));
        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\x7f'));
        assert!(!rendered.contains('\u{009b}'));
        assert!(rendered.contains("\\u0007"));
        assert!(rendered.contains("\\u001b"));
        assert!(rendered.contains("\\u007f"));
        assert!(rendered.contains("\\u009b"));
        let roundtrip: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(roundtrip, payload);
    }

    #[test]
    fn standalone_help_flag_ignores_flag_values() {
        assert!(!has_standalone_help_flag(
            &s(&["--rationale", "-h", "--summary", "--help"]),
            &["--rationale", "--summary"]
        ));
        assert!(has_standalone_help_flag(
            &s(&["--rationale", "ok", "--help"]),
            &["--rationale"]
        ));
    }
}

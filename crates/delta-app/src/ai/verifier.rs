use std::collections::BTreeSet;

use delta_core::{parse_decimal_lossless, to_canonical_text};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::gateway::{looks_like_iso_date, AnalysisToolGateway};
use crate::host::{AnalysisScope, EvidenceRef};
use crate::service::AppService;

const INLINE_CITATION_PREFIXES: &[&str] = &[
    "RES:",
    "RES-",
    "REP:",
    "EV-",
    "LOT:",
    "REC:",
    "RECON:",
    "MV-",
    "NOTE-",
    "note:",
    "report:",
    "DSV-",
    "REPORT-",
    "SIM-ORD-",
    "SIM-FILL-",
    "SIM-GRP-",
    "TRN-NOTE-",
    "TRD-GRP-",
    "FPV:",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiReportVerificationStatus {
    Verified,
    NeedsReview,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiReviewReport {
    pub run_id: String,
    pub session_id: String,
    pub generation: u64,
    pub model_id: String,
    pub frozen_scope: AnalysisScope,
    pub summary: String,
    pub calculations: Vec<(String, String, String)>,
    pub verified_evidence: Vec<EvidenceRef>,
    pub blocked_citations: Vec<String>,
    pub limitations: Vec<String>,
    pub verification_status: AiReportVerificationStatus,
    pub unmatched_numbers: Vec<String>,
    pub newer_data_available_notice: Option<String>,
}

/// Pre-send scope preview displayed in the AI panel (`S-20`, DESIGN Rule 3).
/// Never exposes internal wire protocol identifiers (`openai-responses` / `openai-chat-completions`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreSendScopeSummary {
    pub model_display: String,
    pub accounts: Vec<String>,
    pub period_label: String,
    pub include_notes: bool,
    pub is_scope_pinned: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StructuredModelDraft {
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub calculations: Vec<(String, String, String)>,
    #[serde(default)]
    pub citations: Vec<String>,
    #[serde(default)]
    pub limitations: Vec<String>,
}

fn is_known_currency_prefix(prefix: &str) -> bool {
    let lower = prefix.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "us" | "hk"
            | "cn"
            | "usd"
            | "eur"
            | "gbp"
            | "hkd"
            | "cny"
            | "rmb"
            | "jpy"
            | "sgd"
            | "aud"
            | "cad"
            | "chf"
            | "usdt"
            | "usdc"
            | "btc"
            | "eth"
            | "bnb"
            | "sol"
    )
}

/// Extract standalone financial number tokens from natural language text while ignoring:
/// - ISO dates (`2026-01-15`)
/// - Identifiers where digits are attached to uppercase ID prefixes (`AC-02`, `ACC-1`, `EV-1`, `R1`, `v1.0`)
///
/// Handles magnitude suffixes (`千`/`万`/`亿`/`k`/`K`/`M`/`B`), full-width digits/signs,
/// currency-prefixed numbers (including lowercase/2-letter codes like `usd99999`, `US99999`),
/// and word-attached signed decimals/percentages (`loss-99.99`, `return-50%`) (`APP-B08`).
pub fn extract_financial_numbers_from_text(text: &str) -> Vec<String> {
    let normalized: String = text
        .chars()
        .map(|c| match c {
            '０'..='９' => char::from_u32(c as u32 - '０' as u32 + '0' as u32).unwrap(),
            '−' | '－' => '-',
            '＋' => '+',
            '．' => '.',
            '，' => ',',
            other => other,
        })
        .collect();
    let chars: Vec<char> = normalized.chars().collect();
    let mut result = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let start = index;
        let next_digit = if matches!(chars[index], '+' | '-') {
            let mut next = index + 1;
            while chars.get(next).is_some_and(|ch| ch.is_whitespace()) {
                next += 1;
            }
            chars
                .get(next)
                .is_some_and(char::is_ascii_digit)
                .then_some(next)
        } else {
            None
        };
        if !chars[index].is_ascii_digit() && next_digit.is_none() {
            index += 1;
            continue;
        }
        // Dates and identifiers are metadata; currency-adjacent values remain financial numbers.
        let tail: String = chars[index..].iter().take(10).collect();
        if looks_like_iso_date(&tail) {
            index += 10;
            continue;
        }
        let mut prefix_start = start;
        while prefix_start > 0
            && (chars[prefix_start - 1].is_ascii_alphanumeric()
                || matches!(chars[prefix_start - 1], '_' | ':' | '-'))
        {
            prefix_start -= 1;
        }
        let prefix: String = chars[prefix_start..start].iter().collect();
        let value_separator = start > 0 && matches!(chars[start - 1], ':' | '=');
        let sign_char = next_digit.map(|_| chars[start]);
        if let Some(next) = next_digit {
            index = next;
        }
        while index < chars.len()
            && (chars[index].is_ascii_digit()
                || matches!(chars[index], '.' | '_')
                || (chars[index] == ',' && chars.get(index + 1).is_some_and(char::is_ascii_digit)))
        {
            index += 1;
        }
        if matches!(chars.get(index), Some('e' | 'E')) {
            index += 1;
            if matches!(chars.get(index), Some('+' | '-')) {
                index += 1;
            }
            while chars.get(index).is_some_and(char::is_ascii_digit) {
                index += 1;
            }
        }
        let raw: String = chars[start..index]
            .iter()
            .filter(|c| **c != ',' && !c.is_whitespace())
            .collect();

        let next_after_suffix_is_letter =
            chars.get(index + 1).is_some_and(char::is_ascii_alphabetic);
        let scale_suffix: Option<(i64, char)> = match chars.get(index).copied() {
            Some('千') => Some((1_000, '千')),
            Some('万') => Some((10_000, '万')),
            Some('亿') => Some((100_000_000, '亿')),
            Some(c @ ('k' | 'K')) if !next_after_suffix_is_letter => Some((1_000, c)),
            Some(c @ 'M') if !next_after_suffix_is_letter => Some((1_000_000, c)),
            Some(c @ 'B') if !next_after_suffix_is_letter => Some((1_000_000_000, c)),
            _ => None,
        };
        let has_percent = matches!(chars.get(index), Some('%' | '％'));
        let has_decimal = raw.contains('.');

        // Determine whether this attached prefix is an identifier (`EV-1`, `ACC-1`, `AC-02`,
        // `R1`, `v1.0`, `T+1`) rather than a financial quantity (`USD68`, `usd99999`, `US99999`,
        // `loss-99.99`, `return-50%`, `68k`).
        let currency_like = ((3..=5).contains(&prefix.len())
            && prefix.chars().all(|ch| ch.is_ascii_uppercase()))
            || is_known_currency_prefix(&prefix);
        let is_version_prefix = prefix.eq_ignore_ascii_case("v") && sign_char.is_none();
        let is_hyphenated_upper_id = sign_char == Some('-')
            && !prefix.is_empty()
            && prefix.chars().all(|ch| {
                ch.is_ascii_uppercase() || ch.is_ascii_digit() || matches!(ch, '-' | '_')
            })
            && prefix.chars().any(|ch| ch.is_ascii_uppercase())
            && !has_decimal
            && !has_percent
            && scale_suffix.is_none();
        let is_settlement_day_id = sign_char == Some('+')
            && prefix.eq_ignore_ascii_case("t")
            && !has_decimal
            && !has_percent
            && scale_suffix.is_none();
        let metadata = if value_separator {
            false
        } else if is_version_prefix || is_hyphenated_upper_id || is_settlement_day_id {
            true
        } else if sign_char.is_some() {
            // Word-attached signed values like `loss-99.99`, `return-50%`, `pnl-68`, `profit+99`
            // are financial values, not identifiers.
            false
        } else if prefix.chars().any(|ch| ch.is_ascii_alphabetic()) {
            !currency_like && !has_decimal && !has_percent && scale_suffix.is_none()
        } else {
            false
        };

        if let Some((_, _)) = scale_suffix {
            if !metadata {
                index += 1;
            }
        }
        if metadata {
            continue;
        }
        if let Ok(mut decimal) =
            parse_decimal_lossless(raw.trim_start_matches('+').trim_end_matches('.'))
        {
            let multiplier = scale_suffix.map_or(1, |(m, _)| m);
            let Some(scaled) = decimal.checked_mul(rust_decimal::Decimal::from(multiplier)) else {
                if let Some((_, ch)) = scale_suffix {
                    result.push(format!("{raw}{ch}"));
                } else {
                    result.push(raw);
                }
                continue;
            };
            decimal = scaled;
            result.push(to_canonical_text(decimal));
        } else if !raw.is_empty() {
            result.push(raw);
        }
    }
    result
}

/// Host-side verification of model citations and financial numbers against tool receipts (`S-20`, `M2-A-14`).
pub fn verify_and_build_ai_report(
    svc: &AppService,
    gateway: &AnalysisToolGateway,
    session_id: &str,
    model_id: &str,
    raw_model_text: &str,
) -> AiReviewReport {
    let parsed = serde_json::from_str::<Value>(raw_model_text.trim())
        .ok()
        .filter(|value| {
            ["summary", "calculations", "citations", "limitations"]
                .iter()
                .all(|key| value.get(key).is_some())
        })
        .and_then(|value| serde_json::from_value::<StructuredModelDraft>(value).ok());
    let structured = parsed.is_some();
    let draft = parsed.unwrap_or_else(|| StructuredModelDraft {
        summary: raw_model_text.trim().to_string(),
        calculations: Vec::new(),
        citations: Vec::new(),
        limitations: Vec::new(),
    });

    let mut verified_evidence = Vec::new();
    let mut blocked_citations = Vec::new();
    let mut seen_verified = BTreeSet::new();

    for cited_id in &draft.citations {
        let cid = cited_id.trim();
        if cid.is_empty() {
            continue;
        }
        if let Some(ev_ref) = gateway.valid_evidence_map.get(cid) {
            if seen_verified.insert(ev_ref.id.clone()) {
                verified_evidence.push(ev_ref.clone());
            }
        } else {
            blocked_citations.push(cid.to_string());
        }
    }

    // Inline references also have to identify a receipt from this run. A forged reference
    // cannot become valid by omitting it from the structured citations array (`APP-B08`).
    for text in std::iter::once(draft.summary.as_str())
        .chain(draft.calculations.iter().map(|item| item.0.as_str()))
        .chain(draft.limitations.iter().map(String::as_str))
    {
        for token in text
            .split(|ch: char| !ch.is_ascii_alphanumeric() && !matches!(ch, ':' | '_' | '-' | '.'))
        {
            let token = token.trim_end_matches('.');
            if INLINE_CITATION_PREFIXES
                .iter()
                .any(|prefix| token.starts_with(prefix))
                && !gateway.valid_evidence_map.contains_key(token)
                && !blocked_citations.iter().any(|id| id == token)
            {
                blocked_citations.push(token.to_string());
            }
        }
    }

    // Number verification (`S-20`): every number in `calculations` and `summary` must match `gateway.authorized_numbers`
    let mut unmatched_numbers = Vec::new();
    let mut normalized_calculations = Vec::new();

    for (label, raw_val, ccy) in &draft.calculations {
        match parse_decimal_lossless(raw_val.trim()) {
            Ok(dec) => {
                let canon = to_canonical_text(dec);
                if !gateway.authorized_numbers.contains(&canon)
                    && !unmatched_numbers.contains(&canon)
                {
                    unmatched_numbers.push(canon.clone());
                }
                normalized_calculations.push((label.clone(), canon, ccy.clone()));
            }
            Err(_) => {
                unmatched_numbers.push(raw_val.clone());
                normalized_calculations.push((label.clone(), raw_val.clone(), ccy.clone()));
            }
        }
    }

    // Reference tokens (valid or blocked) are identifiers, not financial values; remove them
    // before number extraction so `RES:<hash>` digits are not misread and a `key:value`
    // financial number cannot hide behind an identifier-shaped prefix.
    let without_references = |text: &str| -> String {
        text.split_inclusive(|ch: char| {
            !ch.is_ascii_alphanumeric() && !matches!(ch, ':' | '_' | '-' | '.')
        })
        .map(|piece| {
            let token = piece
                .trim_end_matches(|ch: char| {
                    !ch.is_ascii_alphanumeric() && !matches!(ch, ':' | '_' | '-' | '.')
                })
                .trim_end_matches('.');
            if !token.is_empty()
                && (gateway.valid_evidence_map.contains_key(token)
                    || blocked_citations.iter().any(|id| id == token))
            {
                piece.replacen(token, " ", 1)
            } else {
                piece.to_string()
            }
        })
        .collect()
    };
    for num_str in extract_financial_numbers_from_text(&without_references(&draft.summary)) {
        if !gateway.authorized_numbers.contains(&num_str) && !unmatched_numbers.contains(&num_str) {
            unmatched_numbers.push(num_str);
        }
    }
    for (label, _, currency) in &draft.calculations {
        for number in extract_financial_numbers_from_text(&without_references(label)) {
            if !gateway.authorized_numbers.contains(&number) && !unmatched_numbers.contains(&number)
            {
                unmatched_numbers.push(number);
            }
        }
        let ccy_upper = currency.trim().to_ascii_uppercase();
        if !ccy_upper.is_empty()
            && !ccy_upper.eq_ignore_ascii_case(&gateway.frozen_scope.report_currency)
            && !gateway.authorized_currencies.contains(&ccy_upper)
        {
            unmatched_numbers.push(format!("currency:{currency}"));
        }
    }
    for limitation in &draft.limitations {
        for number in extract_financial_numbers_from_text(&without_references(limitation)) {
            if !gateway.authorized_numbers.contains(&number) && !unmatched_numbers.contains(&number)
            {
                unmatched_numbers.push(number);
            }
        }
    }

    let mut limitations = draft.limitations;
    if !structured {
        limitations.push("模型未返回完整的结构化报告，需人工复核".into());
    }
    if verified_evidence.is_empty() {
        limitations.push("模型没有明确引用本次工具回执中的有效证据，需人工复核".into());
    }
    if !blocked_citations.is_empty() {
        limitations.push(format!(
            "Host blocked {} unauthorized or non-existent citation(s): {}",
            blocked_citations.len(),
            blocked_citations.join(", ")
        ));
    }
    if !unmatched_numbers.is_empty() {
        limitations.push(format!(
            "Host flagged {} number(s) absent from deterministic tool receipts: {}",
            unmatched_numbers.len(),
            unmatched_numbers.join(", ")
        ));
    }

    let verification_status = if structured
        && blocked_citations.is_empty()
        && unmatched_numbers.is_empty()
        && !draft.summary.trim().is_empty()
        && !verified_evidence.is_empty()
    {
        AiReportVerificationStatus::Verified
    } else {
        AiReportVerificationStatus::NeedsReview
    };

    // Check if newer ledger_revision or dataset_version was committed while the run was executing
    let current_ledger_rev: i64 = svc
        .store()
        .conn()
        .query_row("SELECT ledger_revision FROM library LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap_or(gateway.frozen_scope.ledger_revision);
    let current_dsv: i64 = svc
        .store()
        .conn()
        .query_row(
            "SELECT COALESCE(MAX(version_num), 0) FROM dataset_publication",
            [],
            |r| r.get(0),
        )
        .unwrap_or(gateway.frozen_scope.dataset_version);

    let newer_data_available_notice = if current_ledger_rev > gateway.frozen_scope.ledger_revision
        || current_dsv > gateway.frozen_scope.dataset_version
    {
        Some(format!(
            "Newer data is available (current dataset v{current_dsv}, ledger r{current_ledger_rev}; report bound to frozen snapshot dataset v{}, ledger r{})",
            gateway.frozen_scope.dataset_version, gateway.frozen_scope.ledger_revision
        ))
    } else {
        None
    };

    AiReviewReport {
        run_id: gateway.run_id.clone(),
        session_id: session_id.to_string(),
        generation: gateway.generation,
        model_id: model_id.to_string(),
        frozen_scope: gateway.frozen_scope.clone(),
        summary: draft.summary,
        calculations: normalized_calculations,
        verified_evidence,
        blocked_citations,
        limitations,
        verification_status,
        unmatched_numbers,
        newer_data_available_notice,
    }
}

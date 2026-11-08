use rust_decimal::prelude::*;
use rust_decimal::Decimal;
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum DecimalError {
    #[error("invalid decimal literal '{0}'")]
    InvalidLiteral(String),
    #[error("decimal scale {actual} exceeds maximum supported scale {max} for '{raw}'")]
    PrecisionExceeded { raw: String, actual: u32, max: u32 },
    #[error("decimal value '{0}' out of supported range")]
    OutOfRange(String),
    #[error("floating point intermediary is forbidden for financial decimal parsing")]
    FloatForbidden,
}

/// Parse a decimal string losslessly, supporting standard and scientific notation without f64.
pub fn parse_decimal_lossless(raw: &str) -> Result<Decimal, DecimalError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(DecimalError::InvalidLiteral(raw.to_string()));
    }
    let parsed = if trimmed.contains(['e', 'E']) {
        Decimal::from_scientific(trimmed)
            .map_err(|_| DecimalError::InvalidLiteral(trimmed.to_string()))?
    } else {
        Decimal::from_str_exact(trimmed)
            .map_err(|_| DecimalError::InvalidLiteral(trimmed.to_string()))?
    };
    Ok(parsed.normalize())
}

/// Parse a decimal and enforce a maximum fractional scale without silent truncation.
pub fn parse_decimal_with_max_scale(raw: &str, max_scale: u32) -> Result<Decimal, DecimalError> {
    let dec = parse_decimal_lossless(raw)?;
    if dec.scale() > max_scale {
        return Err(DecimalError::PrecisionExceeded {
            raw: raw.to_string(),
            actual: dec.scale(),
            max: max_scale,
        });
    }
    Ok(dec)
}

/// Parse a JSON value (either a JSON string `"123.45"` or a raw JSON number token `123.45`)
/// directly from its source text without converting through `f64` (S-08).
pub fn parse_json_raw_decimal(raw_json_token: &str) -> Result<Decimal, DecimalError> {
    let trimmed = raw_json_token.trim();
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(trimmed);
    parse_decimal_lossless(unquoted)
}

/// Normalize a Decimal to its canonical SQLite TEXT representation.
pub fn to_canonical_text(value: Decimal) -> String {
    value.normalize().to_string()
}

/// Restore a Yahoo Finance Chart v8 split-adjusted price to raw unadjusted price (S-08):
/// multiply by cumulative split factor after the bar's session date, then round to 4 decimal
/// places (US equity tick/noise normalization) using MidpointNearestEven (or HalfUp).
pub fn restore_yahoo_raw_price(
    adjusted_price: Decimal,
    cumulative_split_factor: Decimal,
) -> Decimal {
    let raw = adjusted_price * cumulative_split_factor;
    raw.round_dp_with_strategy(4, RoundingStrategy::MidpointAwayFromZero)
        .normalize()
}

/// Restore a Yahoo Finance Chart v8 split-adjusted dividend amount to raw unadjusted dividend.
pub fn restore_yahoo_raw_dividend(
    adjusted_dividend: Decimal,
    cumulative_split_factor: Decimal,
) -> Decimal {
    let raw = adjusted_dividend * cumulative_split_factor;
    raw.round_dp_with_strategy(6, RoundingStrategy::MidpointAwayFromZero)
        .normalize()
}

/// Restore a Yahoo Finance Chart v8 split-adjusted volume to raw unadjusted volume.
pub fn restore_yahoo_raw_volume(
    adjusted_volume: Decimal,
    cumulative_split_factor: Decimal,
) -> Decimal {
    if cumulative_split_factor.is_zero() {
        return adjusted_volume;
    }
    (adjusted_volume / cumulative_split_factor)
        .round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero)
        .normalize()
}

/// Wrapper for deserializing JSON numbers or strings directly from raw source text into `Decimal`
/// without any `f64` conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct DirectDecimal(pub Decimal);

impl From<Decimal> for DirectDecimal {
    fn from(value: Decimal) -> Self {
        Self(value)
    }
}

impl From<DirectDecimal> for Decimal {
    fn from(value: DirectDecimal) -> Self {
        value.0
    }
}

impl Serialize for DirectDecimal {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&to_canonical_text(self.0))
    }
}

impl<'de> Deserialize<'de> for DirectDecimal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw: Box<serde_json::value::RawValue> = Box::deserialize(deserializer)?;
        let dec = parse_json_raw_decimal(raw.get()).map_err(de::Error::custom)?;
        Ok(DirectDecimal(dec))
    }
}

pub struct DirectDecimalVisitor;

impl<'de> Visitor<'de> for DirectDecimalVisitor {
    type Value = Decimal;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a decimal string or raw JSON number")
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        parse_decimal_lossless(v).map_err(E::custom)
    }
}

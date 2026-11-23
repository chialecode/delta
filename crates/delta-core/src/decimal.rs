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

/// Normalize a `Decimal` so `-0` and trailing fractional zeros never cause canonical divergence.
#[inline]
pub fn canon_decimal(value: Decimal) -> Decimal {
    if value.is_zero() {
        Decimal::ZERO
    } else {
        value.normalize()
    }
}

fn map_rust_decimal_error(raw: &str, err: rust_decimal::Error) -> DecimalError {
    match err {
        rust_decimal::Error::ExceedsMaximumPossibleValue
        | rust_decimal::Error::LessThanMinimumPossibleValue
        | rust_decimal::Error::Underflow => DecimalError::OutOfRange(raw.to_string()),
        rust_decimal::Error::ScaleExceedsMaximumPrecision(actual) => {
            let is_positive_sci_exp = raw
                .split_once(['e', 'E'])
                .is_some_and(|(_, exp)| !exp.starts_with('-'));
            if is_positive_sci_exp {
                DecimalError::OutOfRange(raw.to_string())
            } else {
                DecimalError::PrecisionExceeded {
                    raw: raw.to_string(),
                    actual,
                    max: 28,
                }
            }
        }
        rust_decimal::Error::ErrorString(msg) if msg.contains("overflow") => {
            DecimalError::OutOfRange(raw.to_string())
        }
        _ => DecimalError::InvalidLiteral(raw.to_string()),
    }
}

/// Parse a decimal string losslessly, supporting standard and scientific notation without f64.
pub fn parse_decimal_lossless(raw: &str) -> Result<Decimal, DecimalError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(DecimalError::InvalidLiteral(raw.to_string()));
    }
    let parsed = if trimmed.contains(['e', 'E']) {
        Decimal::from_scientific(trimmed).map_err(|e| map_rust_decimal_error(trimmed, e))?
    } else {
        Decimal::from_str_exact(trimmed).map_err(|e| map_rust_decimal_error(trimmed, e))?
    };
    Ok(canon_decimal(parsed))
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
    canon_decimal(value).to_string()
}

/// Restore a Yahoo Finance Chart v8 split-adjusted price to raw unadjusted price (S-08):
/// multiply by cumulative split factor after the bar's session date, then round to 4 decimal
/// places (US equity tick/noise normalization) using `MidpointAwayFromZero` (HalfUp).
pub fn restore_yahoo_raw_price(
    adjusted_price: Decimal,
    cumulative_split_factor: Decimal,
) -> Decimal {
    let raw = adjusted_price * cumulative_split_factor;
    canon_decimal(raw.round_dp_with_strategy(4, RoundingStrategy::MidpointAwayFromZero))
}

/// Restore a Yahoo Finance Chart v8 split-adjusted dividend amount to raw unadjusted dividend.
pub fn restore_yahoo_raw_dividend(
    adjusted_dividend: Decimal,
    cumulative_split_factor: Decimal,
) -> Decimal {
    let raw = adjusted_dividend * cumulative_split_factor;
    canon_decimal(raw.round_dp_with_strategy(6, RoundingStrategy::MidpointAwayFromZero))
}

/// Restore a Yahoo Finance Chart v8 split-adjusted volume to raw unadjusted volume.
pub fn restore_yahoo_raw_volume(
    adjusted_volume: Decimal,
    cumulative_split_factor: Decimal,
) -> Decimal {
    if cumulative_split_factor <= Decimal::ZERO {
        return canon_decimal(adjusted_volume);
    }
    canon_decimal(
        (adjusted_volume / cumulative_split_factor)
            .round_dp_with_strategy(0, RoundingStrategy::MidpointAwayFromZero),
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalizes_negative_zero_and_maps_out_of_range() {
        let neg_zero = parse_decimal_lossless("-0.00").unwrap();
        assert_eq!(to_canonical_text(neg_zero), "0");
        assert!(!neg_zero.is_sign_negative());
        assert_eq!(
            to_canonical_text(Decimal::new(0, 2) * Decimal::NEGATIVE_ONE),
            "0"
        );

        let err = parse_decimal_lossless("999999999999999999999999999999999").unwrap_err();
        assert!(matches!(err, DecimalError::OutOfRange(_)));
        assert!(matches!(
            parse_decimal_lossless("1e50").unwrap_err(),
            DecimalError::OutOfRange(_)
        ));
        assert!(matches!(
            parse_decimal_lossless("9e28").unwrap_err(),
            DecimalError::OutOfRange(_)
        ));
        assert!(matches!(
            parse_decimal_lossless("1e-29").unwrap_err(),
            DecimalError::PrecisionExceeded { actual: 29, .. }
        ));
        assert!(matches!(
            parse_decimal_lossless("0.00000000000000000000000000001").unwrap_err(),
            DecimalError::OutOfRange(_)
        ));
        assert!(matches!(
            parse_decimal_lossless("not-a-number").unwrap_err(),
            DecimalError::InvalidLiteral(_)
        ));
        assert_eq!(
            restore_yahoo_raw_volume(Decimal::from(100), Decimal::ZERO),
            Decimal::from(100)
        );
    }
}

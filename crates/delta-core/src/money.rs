//! Decimal-backed money and quantity values.
//!
//! Contract (data-model §1, financial-engine §1):
//! - serialized as decimal strings, never floats;
//! - checked arithmetic, out-of-range rejected;
//! - `None` (no value) is distinct from zero.

use crate::error::{DomainError, DomainResult};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fmt;

/// A currency or cash-like asset code: `USD`, `USDT`, `USDC` are distinct.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CurrencyCode(pub String);

impl CurrencyCode {
    pub fn new(code: impl Into<String>) -> Self {
        Self(code.into().to_ascii_uppercase())
    }
    pub fn usd() -> Self {
        Self("USD".into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CurrencyCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An amount of a specific currency. Serialized as `{"value": "...", "currency": "USD"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Amount {
    #[serde(with = "decimal_string")]
    pub value: Decimal,
    pub currency: CurrencyCode,
}

impl Amount {
    pub fn new(value: Decimal, currency: CurrencyCode) -> Self {
        Self { value, currency }
    }
    pub fn zero(currency: CurrencyCode) -> Self {
        Self {
            value: Decimal::ZERO,
            currency,
        }
    }
    pub fn add(&self, other: &Amount) -> DomainResult<Amount> {
        if self.currency != other.currency {
            return Err(DomainError::InvalidArgument(format!(
                "cannot add {} and {}",
                self.currency, other.currency
            )));
        }
        Ok(Amount {
            value: checked_add(self.value, other.value)?,
            currency: self.currency.clone(),
        })
    }
    pub fn neg(&self) -> Amount {
        Amount {
            value: -self.value,
            currency: self.currency.clone(),
        }
    }
}

/// A signed or unsigned quantity of one asset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Quantity(pub Decimal);

impl Quantity {
    pub fn new(value: Decimal) -> Self {
        Self(value)
    }
    pub fn zero() -> Self {
        Self(Decimal::ZERO)
    }
    pub fn is_positive(&self) -> bool {
        self.0 > Decimal::ZERO
    }
    pub fn is_zero(&self) -> bool {
        self.0 == Decimal::ZERO
    }
}

/// Checked decimal helpers: every ledger arithmetic path goes through these so
/// range violations surface as errors instead of panics or silent truncation.
pub(crate) fn checked_add(a: Decimal, b: Decimal) -> DomainResult<Decimal> {
    a.checked_add(b)
        .ok_or_else(|| DomainError::NumericOverflow(format!("{a} + {b}")))
}
pub(crate) fn checked_sub(a: Decimal, b: Decimal) -> DomainResult<Decimal> {
    a.checked_sub(b)
        .ok_or_else(|| DomainError::NumericOverflow(format!("{a} - {b}")))
}
pub(crate) fn checked_mul(a: Decimal, b: Decimal) -> DomainResult<Decimal> {
    a.checked_mul(b)
        .ok_or_else(|| DomainError::NumericOverflow(format!("{a} * {b}")))
}
pub(crate) fn checked_div(a: Decimal, b: Decimal) -> DomainResult<Decimal> {
    a.checked_div(b)
        .ok_or_else(|| DomainError::NumericOverflow(format!("{a} / {b}")))
}

/// Parse a decimal string, rejecting out-of-range input.
pub fn parse_decimal(s: &str) -> DomainResult<Decimal> {
    let t = s.trim();
    if t.is_empty() || t.len() > 64 {
        return Err(DomainError::NumericOverflow(format!(
            "decimal length rejected: {s:?}"
        )));
    }
    let digits = t.chars().filter(|c| c.is_ascii_digit()).count();
    if digits > 28 {
        return Err(DomainError::NumericOverflow(format!(
            "more than 28 significant digits: {s:?}"
        )));
    }
    let d: Decimal = t
        .parse()
        .map_err(|_| DomainError::NumericOverflow(format!("decimal out of range: {s:?}")))?;
    Ok(d)
}

/// Serde helper: Decimal as a decimal string.
pub mod decimal_string {
    use rust_decimal::Decimal;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &Decimal, s: S) -> Result<S::Ok, S::Error> {
        v.to_string().serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Decimal, D::Error> {
        let s = String::deserialize(d)?;
        s.parse::<Decimal>().map_err(serde::de::Error::custom)
    }
}

/// A directional rate: 1 unit of `base` equals `rate` units of `quote`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FxRate {
    pub base: CurrencyCode,
    pub quote: CurrencyCode,
    #[serde(with = "decimal_string")]
    pub rate: Decimal,
    /// Time the fact is effective at (`effective_at`).
    pub effective_at: chrono::DateTime<chrono::Utc>,
    /// Time the rate was observed/acquired (`observed_at`).
    pub observed_at: chrono::DateTime<chrono::Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn amount_serializes_as_decimal_string() {
        let a = Amount::new(dec!(600.6), CurrencyCode::usd());
        let json = serde_json::to_string(&a).unwrap();
        assert!(json.contains("\"600.6\""), "{json}");
        let back: Amount = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn overflow_is_rejected_not_silent() {
        assert!(checked_add(Decimal::MAX, Decimal::ONE).is_err());
        assert!(checked_mul(Decimal::MAX, Decimal::MAX).is_err());
    }

    #[test]
    fn currency_codes_are_normalized() {
        assert_eq!(CurrencyCode::new("usd"), CurrencyCode::new("USD"));
        assert_ne!(CurrencyCode::new("USDT"), CurrencyCode::new("USD"));
    }
}

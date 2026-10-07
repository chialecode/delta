//! Economic events: immutable business facts per account.
//!
//! Corrections use explicit reversal/replacement events (data-model §5):
//! the store resolves effective revisions; the ledger never mutates history.

use crate::ids::{AccountId, AssetId, EventId, InstrumentId, TransferGroupId};
use crate::money::{parse_decimal, CurrencyCode};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// A fee charged in a specific currency (quote currency, cash, or a third asset
/// such as paying exchange fees in BNB).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fee {
    pub asset: AssetId,
    #[serde(with = "crate::money::decimal_string")]
    pub amount: Decimal,
    /// Category, e.g. `commission`, `platform`, `withdrawal`.
    pub category: String,
}

/// Direction of a transfer for pairing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferKind {
    Out,
    In,
}

/// One business event on one account. `seq` breaks ties when events share
/// `occurred_at` (stable ordering: occurred_at, then seq).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EconomicEvent {
    pub id: EventId,
    pub account_id: AccountId,
    pub occurred_at: DateTime<Utc>,
    pub recorded_at: DateTime<Utc>,
    pub seq: i64,
    pub source_ref: Option<String>,
    /// Correction group id shared by an event and its corrections/replacements.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correction_group: Option<String>,
    /// Monotonic revision within a correction group (higher wins).
    #[serde(default)]
    pub revision: i64,
    /// If set, this event reverses the referenced event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reverses: Option<EventId>,
    pub payload: EventPayload,
}

impl EconomicEvent {
    pub fn sort_key(&self) -> (DateTime<Utc>, i64) {
        (self.occurred_at, self.seq)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventPayload {
    /// Opening balance/position. `cost: None` marks unknown cost (never zero).
    OpeningPosition {
        asset: AssetId,
        #[serde(with = "crate::money::decimal_string")]
        quantity: Decimal,
        #[serde(skip_serializing_if = "Option::is_none")]
        cost: Option<OpeningCost>,
    },
    Buy {
        instrument: InstrumentId,
        #[serde(with = "crate::money::decimal_string")]
        quantity: Decimal,
        #[serde(with = "crate::money::decimal_string")]
        price: Decimal,
        quote_currency: CurrencyCode,
        #[serde(default)]
        fees: Vec<Fee>,
    },
    Sell {
        instrument: InstrumentId,
        #[serde(with = "crate::money::decimal_string")]
        quantity: Decimal,
        #[serde(with = "crate::money::decimal_string")]
        price: Decimal,
        quote_currency: CurrencyCode,
        #[serde(default)]
        fees: Vec<Fee>,
    },
    CashDeposit {
        asset: AssetId,
        #[serde(with = "crate::money::decimal_string")]
        amount: Decimal,
    },
    CashWithdrawal {
        asset: AssetId,
        #[serde(with = "crate::money::decimal_string")]
        amount: Decimal,
    },
    /// Internal transfer principal. `fee` is charged separately so principal
    /// and fee are never inferred from a difference.
    Transfer {
        kind: TransferKind,
        group: TransferGroupId,
        counterparty: AccountId,
        asset: AssetId,
        #[serde(with = "crate::money::decimal_string")]
        principal: Decimal,
        #[serde(skip_serializing_if = "Option::is_none")]
        fee: Option<Fee>,
    },
    /// Standalone fee event (paid from the given asset).
    Fee {
        asset: AssetId,
        #[serde(with = "crate::money::decimal_string")]
        amount: Decimal,
        category: String,
    },
    Interest {
        asset: AssetId,
        #[serde(with = "crate::money::decimal_string")]
        amount: Decimal,
    },
    /// Cash dividend recorded on its pay date (S1 rule, financial-engine §3).
    DividendCash {
        cash_asset: AssetId,
        #[serde(with = "crate::money::decimal_string")]
        amount: Decimal,
        instrument: InstrumentId,
    },
    /// Stock split: `quantity *= ratio_num / ratio_den`, unit cost scales down,
    /// total cost unchanged.
    StockSplit {
        instrument: InstrumentId,
        ratio_num: i64,
        ratio_den: i64,
    },
}

/// Cost of an opening position: known cost in a currency, or unknown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpeningCost {
    Known {
        #[serde(with = "crate::money::decimal_string")]
        total: Decimal,
        currency: CurrencyCode,
    },
    Unknown,
}

/// Resolved correction view: which recorded events carry ledger effect.
///
/// Rules (data-model §5): within a correction group the highest
/// `(revision, seq)` member wins and earlier members are superseded; an
/// explicit `reverses` removes its target from the effective set. Reversal
/// marker events themselves carry no payload effect. Nothing is rewritten:
/// all recorded events remain for audit.
#[derive(Debug, Default, Clone)]
pub struct Corrections {
    /// Events whose payload no longer applies (superseded originals,
    /// reversal markers).
    pub excluded: std::collections::HashSet<EventId>,
    /// Targets removed by explicit reversals.
    pub removed: std::collections::HashSet<EventId>,
}

impl Corrections {
    /// Compute the effective correction view over recorded events.
    pub fn resolve(events: &[EconomicEvent]) -> Corrections {
        let mut view = Corrections::default();
        for e in events {
            if let Some(target) = &e.reverses {
                view.removed.insert(target.clone());
                view.excluded.insert(e.id.clone());
            }
        }
        let mut groups: std::collections::HashMap<&str, Vec<&EconomicEvent>> =
            std::collections::HashMap::new();
        for e in events {
            if let Some(g) = &e.correction_group {
                groups.entry(g.as_str()).or_default().push(e);
            }
        }
        for members in groups.values() {
            if members.len() < 2 {
                continue;
            }
            let mut sorted: Vec<&&EconomicEvent> = members.iter().collect();
            sorted.sort_by_key(|e| (e.revision, e.seq));
            for other in &sorted[..sorted.len() - 1] {
                view.excluded.insert(other.id.clone());
            }
        }
        view
    }

    /// The effective event ids after applying corrections.
    pub fn effective_ids(&self, all: &[EconomicEvent]) -> std::collections::HashSet<EventId> {
        all.iter()
            .map(|e| e.id.clone())
            .filter(|id| !self.removed.contains(id) && !self.excluded.contains(id))
            .collect()
    }

    /// Effective events, sorted by (occurred_at, seq).
    pub fn effective_events(&self, all: &[EconomicEvent]) -> Vec<EconomicEvent> {
        let ids = self.effective_ids(all);
        let mut out: Vec<EconomicEvent> = all
            .iter()
            .filter(|e| ids.contains(&e.id))
            .cloned()
            .collect();
        out.sort_by_key(|e| e.sort_key());
        out
    }
}

/// Parse helper used by import mapping: strict decimal strings.
pub fn parse_event_decimal(s: &str) -> crate::error::DomainResult<Decimal> {
    parse_decimal(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::TransferGroupId;
    use chrono::TimeZone;

    fn ts(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()
    }

    fn ev(id: &str, at: DateTime<Utc>, seq: i64, payload: EventPayload) -> EconomicEvent {
        EconomicEvent {
            id: EventId(id.into()),
            account_id: AccountId("a1".into()),
            occurred_at: at,
            recorded_at: at,
            seq,
            source_ref: None,
            correction_group: None,
            revision: 0,
            reverses: None,
            payload,
        }
    }

    #[test]
    fn corrections_replacement_wins_by_revision() {
        let original = EconomicEvent {
            correction_group: Some("g1".into()),
            revision: 0,
            ..ev(
                "e1",
                ts(2026, 1, 1),
                1,
                EventPayload::Fee {
                    asset: AssetId("USD".into()),
                    amount: Decimal::TWO,
                    category: "commission".into(),
                },
            )
        };
        let replacement = EconomicEvent {
            correction_group: Some("g1".into()),
            revision: 1,
            ..ev(
                "e2",
                ts(2026, 1, 2),
                2,
                EventPayload::Fee {
                    asset: AssetId("USD".into()),
                    amount: Decimal::ONE,
                    category: "commission".into(),
                },
            )
        };
        let view = Corrections::resolve(&[original.clone(), replacement.clone()]);
        let ids = view.effective_ids(&[original, replacement]);
        assert!(ids.contains(&EventId("e2".into())));
        assert!(!ids.contains(&EventId("e1".into())));
    }

    #[test]
    fn corrections_reversal_removes_target() {
        let original = ev(
            "e1",
            ts(2026, 1, 1),
            1,
            EventPayload::Fee {
                asset: AssetId("USD".into()),
                amount: Decimal::TWO,
                category: "commission".into(),
            },
        );
        let reversal = EconomicEvent {
            reverses: Some(EventId("e1".into())),
            ..ev(
                "e2",
                ts(2026, 1, 2),
                2,
                EventPayload::Fee {
                    asset: AssetId("USD".into()),
                    amount: Decimal::ZERO,
                    category: "reversal".into(),
                },
            )
        };
        let view = Corrections::resolve(&[original.clone(), reversal.clone()]);
        // The original is removed; the reversal marker itself carries no
        // payload effect (audit-only).
        assert!(view.removed.contains(&EventId("e1".into())));
        let ids = view.effective_ids(&[original, reversal]);
        assert!(ids.is_empty());
    }

    #[test]
    fn transfer_payload_roundtrips_with_principal_and_fee_separate() {
        let e = ev(
            "t1",
            ts(2026, 1, 1),
            1,
            EventPayload::Transfer {
                kind: TransferKind::Out,
                group: TransferGroupId("g".into()),
                counterparty: AccountId("a2".into()),
                asset: AssetId("USD".into()),
                principal: Decimal::new(998, 0),
                fee: Some(Fee {
                    asset: AssetId("USD".into()),
                    amount: Decimal::TWO,
                    category: "transfer".into(),
                }),
            },
        );
        let json = serde_json::to_string(&e).unwrap();
        let back: EconomicEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, e);
        assert!(json.contains("\"principal\":\"998\""));
    }
}

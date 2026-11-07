use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    Fiat,
    Stablecoin,
    Equity,
    Etf,
    Crypto,
    OtherAsset,
    Liability,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub id: String,
    pub kind: AssetKind,
    pub symbol: String,
    pub network: Option<String>,
    pub contract_address: Option<String>,
    pub precision: u32,
}

impl Asset {
    pub fn fiat(symbol: &str, precision: u32) -> Self {
        let sym = symbol.to_ascii_uppercase();
        Self {
            id: format!("ASSET:FIAT:{sym}"),
            kind: AssetKind::Fiat,
            symbol: sym,
            network: None,
            contract_address: None,
            precision,
        }
    }

    pub fn stablecoin(symbol: &str, precision: u32) -> Self {
        let sym = symbol.to_ascii_uppercase();
        Self {
            id: format!("ASSET:STABLE:{sym}"),
            kind: AssetKind::Stablecoin,
            symbol: sym,
            network: None,
            contract_address: None,
            precision,
        }
    }

    pub fn crypto(symbol: &str, precision: u32) -> Self {
        let sym = symbol.to_ascii_uppercase();
        Self {
            id: format!("ASSET:CRYPTO:{sym}"),
            kind: AssetKind::Crypto,
            symbol: sym,
            network: None,
            contract_address: None,
            precision,
        }
    }

    pub fn equity(venue: &str, symbol: &str, precision: u32) -> Self {
        let v = venue.to_ascii_uppercase();
        let sym = symbol.to_ascii_uppercase();
        Self {
            id: format!("ASSET:EQUITY:{v}:{sym}"),
            kind: AssetKind::Equity,
            symbol: sym,
            network: Some(v),
            contract_address: None,
            precision,
        }
    }

    pub fn etf(venue: &str, symbol: &str, precision: u32) -> Self {
        let v = venue.to_ascii_uppercase();
        let sym = symbol.to_ascii_uppercase();
        Self {
            id: format!("ASSET:ETF:{v}:{sym}"),
            kind: AssetKind::Etf,
            symbol: sym,
            network: Some(v),
            contract_address: None,
            precision,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstrumentKind {
    SpotEquity,
    SpotEtf,
    SpotCrypto,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instrument {
    pub id: String,
    pub kind: InstrumentKind,
    pub venue: String,
    pub symbol: String,
    pub base_asset_id: String,
    pub quote_asset_id: String,
    pub lot_size: Decimal,
    pub tick_size: Decimal,
    pub multiplier: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstrumentAlias {
    pub provider: String,
    pub external_symbol: String,
    pub instrument_id: String,
    pub valid_from: Option<NaiveDate>,
    pub valid_to: Option<NaiveDate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdjustmentKind {
    Raw,
    SplitAdjusted,
    TotalReturnAdjusted,
}

impl AdjustmentKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::SplitAdjusted => "split_adjusted",
            Self::TotalReturnAdjusted => "total_return_adjusted",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "raw" => Some(Self::Raw),
            "split_adjusted" => Some(Self::SplitAdjusted),
            "total_return_adjusted" => Some(Self::TotalReturnAdjusted),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TermsClass {
    PublicOpen,
    OfficialFreePersonal,
    UnofficialPersonal,
}

impl TermsClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::PublicOpen => "public_open",
            Self::OfficialFreePersonal => "official_free_personal",
            Self::UnofficialPersonal => "unofficial_personal",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityStatus {
    Exact,
    CarriedForward,
    Partial,
    Warning,
    Unavailable,
}

impl QualityStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::CarriedForward => "carried_forward",
            Self::Partial => "partial",
            Self::Warning => "warning",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalendarSessionKind {
    Regular,
    EarlyClose,
    Closed,
    Crypto24x7,
    CalendarUnknown,
}

impl CalendarSessionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Regular => "regular",
            Self::EarlyClose => "early_close",
            Self::Closed => "closed",
            Self::Crypto24x7 => "crypto_24_7",
            Self::CalendarUnknown => "calendar_unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarSession {
    pub venue: String,
    pub session_date: NaiveDate,
    pub timezone: String,
    pub open_at: Option<DateTime<Utc>>,
    pub close_at: Option<DateTime<Utc>>,
    pub kind: CalendarSessionKind,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BarRecord {
    pub id: String,
    pub instrument_id: String,
    pub timeframe: String,
    pub session: String,
    pub session_date: NaiveDate,
    pub open_at: DateTime<Utc>,
    pub close_at: DateTime<Utc>,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
    pub source: String,
    pub adjustment: AdjustmentKind,
    pub is_final: bool,
    pub valid_from_version: i64,
    pub valid_to_version: Option<i64>,
    pub raw_payload_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CorporateActionType {
    Split,
    CashDividend,
}

impl CorporateActionType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Split => "split",
            Self::CashDividend => "cash_dividend",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorporateActionRecord {
    pub id: String,
    pub instrument_id: String,
    pub action_type: CorporateActionType,
    pub ex_date: NaiveDate,
    pub pay_date: Option<NaiveDate>,
    pub ratio_or_amount: Decimal,
    pub currency: Option<String>,
    pub announced_at: Option<DateTime<Utc>>,
    pub source: String,
    pub factor_version: i64,
    pub raw_payload_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FxRateRecord {
    pub id: String,
    pub base_currency: String,
    pub quote_currency: String,
    pub rate: Decimal,
    pub effective_date: NaiveDate,
    pub effective_at: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
    pub source: String,
    pub quality: QualityStatus,
    pub carried_from_date: Option<NaiveDate>,
    pub dataset_version: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataQualityIssue {
    pub id: String,
    pub dataset_version: i64,
    pub instrument_id: String,
    pub session_date: NaiveDate,
    pub kind: String,
    pub sources: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageGap {
    pub id: String,
    pub dataset_version: i64,
    pub instrument_id: String,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub gap_kind: String,
    pub resolution: String,
}

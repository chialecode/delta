use delta_core::{Asset, Instrument, InstrumentAlias, InstrumentKind};
use rust_decimal::Decimal;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct InstrumentRegistry {
    pub assets: HashMap<String, Asset>,
    pub instruments: HashMap<String, Instrument>,
    pub aliases: Vec<InstrumentAlias>,
}

impl Default for InstrumentRegistry {
    fn default() -> Self {
        Self::new_seeded()
    }
}

impl InstrumentRegistry {
    pub fn new_seeded() -> Self {
        let mut reg = Self {
            assets: HashMap::new(),
            instruments: HashMap::new(),
            aliases: Vec::new(),
        };
        reg.seed_defaults();
        reg
    }

    fn seed_defaults(&mut self) {
        // Fiat currencies
        for (sym, prec) in [("USD", 2), ("HKD", 2), ("CNY", 2), ("EUR", 2)] {
            let asset = Asset::fiat(sym, prec);
            self.assets.insert(sym.to_string(), asset);
        }
        // Stablecoins (distinct from USD per FR-LED-02 / AC-06)
        for sym in ["USDT", "USDC"] {
            let asset = Asset::stablecoin(sym, 6);
            self.assets.insert(sym.to_string(), asset);
        }
        // Crypto base & fee assets
        for sym in ["BTC", "ETH", "BNB", "OKB", "SOL"] {
            let asset = Asset::crypto(sym, 8);
            self.assets.insert(sym.to_string(), asset);
        }

        // US Equities & ETFs
        let equities = [
            ("NASDAQ", "AAPL", InstrumentKind::SpotEquity, "105.AAPL"),
            ("NASDAQ", "NVDA", InstrumentKind::SpotEquity, "105.NVDA"),
            ("NASDAQ", "MSFT", InstrumentKind::SpotEquity, "105.MSFT"),
            ("NASDAQ", "TSLA", InstrumentKind::SpotEquity, "105.TSLA"),
            ("NYSE", "SPY", InstrumentKind::SpotEtf, "107.SPY"),
            ("NASDAQ", "QQQ", InstrumentKind::SpotEtf, "105.QQQ"),
        ];
        for (venue, sym, kind, em_secid) in equities {
            let canonical_id = format!("{venue}:{sym}");
            let asset = match kind {
                InstrumentKind::SpotEtf => Asset::etf(venue, sym, 4),
                _ => Asset::equity(venue, sym, 4),
            };
            self.assets.insert(canonical_id.clone(), asset);
            self.instruments.insert(
                canonical_id.clone(),
                Instrument {
                    id: canonical_id.clone(),
                    kind,
                    venue: venue.to_string(),
                    symbol: sym.to_string(),
                    base_asset_id: canonical_id.clone(),
                    quote_asset_id: "USD".to_string(),
                    lot_size: Decimal::ONE,
                    tick_size: Decimal::new(1, 4),
                    multiplier: Decimal::ONE,
                },
            );
            // Cross-source aliases: Futu US.AAPL, Longbridge AAPL.US, EastMoney 105.AAPL, Yahoo/Tiingo/IBKR/Tiger/Schwab AAPL
            let provider_aliases = [
                ("canonical", canonical_id.as_str()),
                ("futu", &format!("US.{sym}")),
                ("longbridge", &format!("{sym}.US")),
                ("eastmoney", em_secid),
                ("yahoo", sym),
                ("tiingo", sym),
                ("ibkr", sym),
                ("tiger", sym),
                ("schwab", sym),
                ("alpaca", sym),
            ];
            for (prov, ext) in provider_aliases {
                self.aliases.push(InstrumentAlias {
                    provider: prov.to_string(),
                    external_symbol: ext.to_string(),
                    instrument_id: canonical_id.clone(),
                    valid_from: None,
                    valid_to: None,
                });
            }
        }

        // Crypto spot pairs across user's primary venues: OKX & BINANCE
        let crypto_pairs = [
            ("OKX", "BTC", "USDT", "BTC-USDT"),
            ("BINANCE", "BTC", "USDT", "BTCUSDT"),
            ("OKX", "ETH", "USDT", "ETH-USDT"),
            ("BINANCE", "ETH", "USDT", "ETHUSDT"),
            ("OKX", "BNB", "USDT", "BNB-USDT"),
            ("BINANCE", "BNB", "USDT", "BNBUSDT"),
            ("OKX", "OKB", "USDT", "OKB-USDT"),
        ];
        for (venue, base, quote, native_sym) in crypto_pairs {
            let canonical_id = format!("{venue}:{base}/{quote}");
            self.instruments.insert(
                canonical_id.clone(),
                Instrument {
                    id: canonical_id.clone(),
                    kind: InstrumentKind::SpotCrypto,
                    venue: venue.to_string(),
                    symbol: format!("{base}/{quote}"),
                    base_asset_id: base.to_string(),
                    quote_asset_id: quote.to_string(),
                    lot_size: Decimal::new(1, 8),
                    tick_size: Decimal::new(1, 2),
                    multiplier: Decimal::ONE,
                },
            );
            let prov_lower = venue.to_ascii_lowercase();
            for (prov, ext) in [
                ("canonical", canonical_id.as_str()),
                (prov_lower.as_str(), native_sym),
                (prov_lower.as_str(), &format!("{base}/{quote}")),
                (prov_lower.as_str(), &format!("{base}-{quote}")),
                (prov_lower.as_str(), &format!("{base}{quote}")),
            ] {
                self.aliases.push(InstrumentAlias {
                    provider: prov.to_string(),
                    external_symbol: ext.to_string(),
                    instrument_id: canonical_id.clone(),
                    valid_from: None,
                    valid_to: None,
                });
            }
        }
    }

    /// Resolve a provider-specific symbol (e.g. `US.AAPL`, `105.AAPL`, `AAPL.US`, `BTC-USDT`, `BTCUSDT`)
    /// to its canonical `instrument_id` (`NASDAQ:AAPL`, `OKX:BTC/USDT`, `BINANCE:BTC/USDT`, etc.).
    pub fn resolve_symbol(&self, provider_hint: Option<&str>, raw_symbol: &str) -> Option<String> {
        let sym = raw_symbol.trim().to_ascii_uppercase();
        if self.instruments.contains_key(&sym) {
            return Some(sym);
        }
        if let Some(prov) = provider_hint {
            let p = prov.to_ascii_lowercase();
            for alias in &self.aliases {
                if alias.provider == p && alias.external_symbol.eq_ignore_ascii_case(&sym) {
                    return Some(alias.instrument_id.clone());
                }
            }
        }
        for alias in &self.aliases {
            if alias.external_symbol.eq_ignore_ascii_case(&sym) {
                return Some(alias.instrument_id.clone());
            }
        }
        None
    }

    /// Return the provider-specific symbol for a canonical instrument (e.g., `NASDAQ:AAPL` -> `105.AAPL` for `eastmoney`).
    pub fn external_symbol_for(&self, provider: &str, instrument_id: &str) -> Option<String> {
        let p = provider.to_ascii_lowercase();
        // SQLite returns aliases in a different order from the seed registry.
        // Select the wire format, rather than whichever display alias happens to be first.
        let mut aliases: Vec<_> = self
            .aliases
            .iter()
            .filter(|alias| {
                alias.provider == p && alias.instrument_id.eq_ignore_ascii_case(instrument_id)
            })
            .collect();
        aliases.sort_by_key(|alias| {
            let wire = match p.as_str() {
                "binance" => !alias.external_symbol.contains(['-', '/', ':']),
                "okx" => {
                    alias.external_symbol.contains('-')
                        && !alias.external_symbol.contains(['/', ':'])
                }
                _ => true,
            };
            (!wire, &alias.external_symbol)
        });
        aliases.first().map(|alias| alias.external_symbol.clone())
    }
}

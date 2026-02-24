use crate::types::{PriceLevel, Symbol};
use rust_decimal::Decimal;
use std::collections::BTreeMap;

/// In-memory L2 orderbook for a single symbol.
/// Bids: descending (highest first). Asks: ascending (lowest first).
pub struct LocalOrderbook {
    pub symbol: Symbol,
    pub bids: BTreeMap<OrdDecimal, Decimal>, // price → qty
    pub asks: BTreeMap<OrdDecimal, Decimal>,
    pub ts: u64,
}

impl LocalOrderbook {
    pub fn new(symbol: Symbol) -> Self {
        Self { symbol, bids: BTreeMap::new(), asks: BTreeMap::new(), ts: 0 }
    }

    /// Apply an incremental depth update. qty == 0 means remove the level.
    pub fn apply(&mut self, bids: &[[String; 2]], asks: &[[String; 2]], ts: u64) {
        for [price, qty] in bids {
            let p: Decimal = price.parse().unwrap_or(Decimal::ZERO);
            let q: Decimal = qty.parse().unwrap_or(Decimal::ZERO);
            if q.is_zero() {
                self.bids.remove(&OrdDecimal(p));
            } else {
                self.bids.insert(OrdDecimal(p), q);
            }
        }
        for [price, qty] in asks {
            let p: Decimal = price.parse().unwrap_or(Decimal::ZERO);
            let q: Decimal = qty.parse().unwrap_or(Decimal::ZERO);
            if q.is_zero() {
                self.asks.remove(&OrdDecimal(p));
            } else {
                self.asks.insert(OrdDecimal(p), q);
            }
        }
        self.ts = ts;
    }

    pub fn best_bid(&self) -> Option<PriceLevel> {
        self.bids.iter().next_back().map(|(p, q)| PriceLevel { price: p.0, quantity: *q })
    }

    pub fn best_ask(&self) -> Option<PriceLevel> {
        self.asks.iter().next().map(|(p, q)| PriceLevel { price: p.0, quantity: *q })
    }

    pub fn spread(&self) -> Option<Decimal> {
        Some(self.best_ask()?.price - self.best_bid()?.price)
    }
}

/// Newtype wrapper so Decimal is Ord (required for BTreeMap keys).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrdDecimal(pub Decimal);

impl PartialOrd for OrdDecimal {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrdDecimal {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

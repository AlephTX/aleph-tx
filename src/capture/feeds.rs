use super::LegConfig;
use crate::strategy::relative_value::{Book, Level, MarketTerms, Oracle, OracleMode};
use anyhow::{Context, Result, ensure};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

const LEVELS: usize = 20;
const MAX_LIGHTER_LEVELS: usize = 50_000;

pub(super) enum Change {
    Book,
    Gap,
    Clock(u64),
    Other,
}

#[derive(Clone, Copy)]
pub(super) struct ContextTick {
    pub oracle: Oracle,
    pub raw_rate: Option<Decimal>,
}

pub(super) struct LegFeed {
    market: String,
    config: LegConfig,
    depth: LighterDepth,
    book: Option<Book>,
    context: Option<ContextTick>,
}

impl LegFeed {
    pub fn new(terms: &MarketTerms, config: &LegConfig) -> Self {
        Self {
            market: terms.market.clone(),
            config: config.clone(),
            depth: LighterDepth::default(),
            book: None,
            context: None,
        }
    }

    pub fn accepts(&self, source: &str) -> bool {
        if self.config.lighter_market_id.is_some() {
            source == "lighter"
        } else {
            source == "hyperliquid"
                || (source == "hyperliquid_snapshot" && self.config.use_hyperliquid_snapshots)
        }
    }

    pub fn disconnect(&mut self, source: &str) {
        if !self.accepts(source) {
            return;
        }
        match source {
            "lighter" => {
                self.depth = LighterDepth::default();
                self.book = None;
                self.context = None;
            }
            "hyperliquid_snapshot" => self.book = None,
            _ => {
                self.context = None;
                if !self.config.use_hyperliquid_snapshots {
                    self.book = None;
                }
            }
        }
    }

    pub fn book(&self) -> Option<&Book> {
        self.book.as_ref()
    }

    pub fn context(&self) -> Option<ContextTick> {
        self.context
    }

    pub fn apply(&mut self, payload: &Value, received: u64, source: &str) -> Result<Change> {
        if let Some(id) = self.config.lighter_market_id {
            self.apply_lighter(payload, received, id)
        } else {
            self.apply_hyperliquid(payload, received, source == "hyperliquid_snapshot")
        }
    }

    fn apply_lighter(&mut self, payload: &Value, received: u64, market_id: u16) -> Result<Change> {
        let channel = payload["channel"].as_str().unwrap_or_default();
        if channel
            .split_once(':')
            .or_else(|| channel.split_once('/'))
            .and_then(|(_, id)| id.parse::<u16>().ok())
            != Some(market_id)
        {
            return Ok(Change::Other);
        }
        match payload["type"].as_str().unwrap_or_default() {
            "subscribed/order_book" | "update/order_book" => {
                let snapshot = payload["type"] == "subscribed/order_book";
                let update = LighterUpdate::deserialize(&payload["order_book"])?;
                let raw_time = update.last_updated_at / 1_000;
                ensure!(raw_time > 0, "missing Lighter book source time");
                let change = self.depth.apply(update, raw_time, snapshot)?;
                match change {
                    Change::Book => {
                        let Some(time) = self.source_time(raw_time, received) else {
                            self.book = None;
                            return Ok(Change::Clock(raw_time.saturating_sub(received)));
                        };
                        let [bids, asks] = self.depth.levels();
                        self.set_book(time, bids, asks);
                    }
                    Change::Gap => self.book = None,
                    _ => {}
                }
                Ok(change)
            }
            "subscribed/market_stats" | "update/market_stats" => {
                #[derive(Deserialize)]
                struct Stats {
                    market_id: u16,
                    symbol: String,
                    index_price: String,
                    current_funding_rate: Option<String>,
                }
                let value = Stats::deserialize(&payload["market_stats"])?;
                ensure!(
                    value.market_id == market_id && value.symbol == self.market,
                    "Lighter market ID does not match configured symbol"
                );
                let raw_time = payload["timestamp"]
                    .as_u64()
                    .context("missing Lighter context time")?;
                let Some(time) = self.source_time(raw_time, received) else {
                    self.context = None;
                    return Ok(Change::Clock(raw_time.saturating_sub(received)));
                };
                self.set_context(
                    time,
                    value.index_price.parse()?,
                    value
                        .current_funding_rate
                        .as_deref()
                        .map(str::parse)
                        .transpose()?,
                )?;
                Ok(Change::Other)
            }
            _ => Ok(Change::Other),
        }
    }

    fn apply_hyperliquid(
        &mut self,
        payload: &Value,
        received: u64,
        snapshot: bool,
    ) -> Result<Change> {
        let data = &payload["data"];
        if data["coin"].as_str() != Some(self.market.as_str()) {
            return Ok(Change::Other);
        }
        match payload["channel"].as_str().unwrap_or_default() {
            "l2Book" if snapshot == self.config.use_hyperliquid_snapshots => {
                let book = crate::exchanges::hyperliquid::L2Book::deserialize(data)?;
                let Some(time) = self.source_time(book.time, received) else {
                    self.book = None;
                    return Ok(Change::Clock(book.time.saturating_sub(received)));
                };
                if self
                    .book
                    .as_ref()
                    .is_some_and(|old| time <= old.timestamp_ms)
                {
                    return Ok(Change::Other);
                }
                let [bids, asks] = book.levels.map(|levels| {
                    levels
                        .into_iter()
                        .take(LEVELS)
                        .map(|l| parse_level(&l.px, &l.sz, false))
                        .collect::<Result<Vec<_>>>()
                });
                self.set_book(time, bids?, asks?);
                Ok(Change::Book)
            }
            "activeAssetCtx" if !snapshot => {
                let ctx = &data["ctx"];
                let price = ctx["oraclePx"]
                    .as_str()
                    .context("missing Hyperliquid oracle price")?
                    .parse()?;
                let raw_rate = ctx["funding"].as_str().map(str::parse).transpose()?;
                // The native active context carries no exchange publication time.
                let time = if self.config.accept_receipt_time_for_oracle {
                    received
                } else {
                    0
                };
                self.set_context(time, price, raw_rate)?;
                Ok(Change::Other)
            }
            _ => Ok(Change::Other),
        }
    }

    fn source_time(&self, raw: u64, received: u64) -> Option<u64> {
        let earliest = raw.checked_sub(self.config.source_clock_ahead_bound_ms)?;
        (earliest > 0 && earliest <= received).then_some(earliest)
    }

    fn set_context(
        &mut self,
        timestamp_ms: u64,
        price: Decimal,
        raw_rate: Option<Decimal>,
    ) -> Result<()> {
        ensure!(price > Decimal::ZERO, "invalid exchange oracle");
        if self
            .context
            .is_none_or(|old| timestamp_ms >= old.oracle.timestamp_ms)
        {
            let mode = if self.config.external_oracle && timestamp_ms > 0 {
                OracleMode::LiveExternal
            } else {
                OracleMode::Internal
            };
            self.context = Some(ContextTick {
                oracle: Oracle {
                    price,
                    timestamp_ms,
                    mode,
                },
                raw_rate,
            });
        }
        Ok(())
    }

    fn set_book(&mut self, timestamp_ms: u64, bids: Vec<Level>, asks: Vec<Level>) {
        let tradable = bids
            .first()
            .zip(asks.first())
            .is_some_and(|(b, a)| b.price < a.price)
            && bids.windows(2).all(|w| w[0].price > w[1].price)
            && asks.windows(2).all(|w| w[0].price < w[1].price);
        self.book = Some(Book {
            market: self.market.clone(),
            timestamp_ms,
            tradable,
            bids,
            asks,
            bounds: None,
            available_notional: Decimal::ZERO,
        });
    }
}

fn validate_time(timestamp: u64, received: u64) -> Result<()> {
    ensure!(
        timestamp > 0 && timestamp <= received,
        "missing or future source timestamp"
    );
    Ok(())
}

fn parse_level(price: &str, size: &str, allow_zero: bool) -> Result<Level> {
    let price: Decimal = price.parse()?;
    let contracts: Decimal = size.parse()?;
    let max = Decimal::from(1_000_000_000);
    ensure!(
        price > Decimal::ZERO
            && price <= max
            && contracts <= max
            && (contracts > Decimal::ZERO || (allow_zero && contracts == Decimal::ZERO)),
        "invalid public depth level"
    );
    Ok(Level { price, contracts })
}

#[derive(Deserialize)]
struct LighterLevel {
    price: String,
    size: String,
}

#[derive(Deserialize)]
struct LighterUpdate {
    nonce: u64,
    begin_nonce: u64,
    /// Matching engine publication in microseconds.
    last_updated_at: u64,
    bids: Vec<LighterLevel>,
    asks: Vec<LighterLevel>,
}

#[derive(Default)]
struct LighterDepth {
    nonce: Option<u64>,
    time: u64,
    sides: [BTreeMap<Decimal, Decimal>; 2],
}

impl LighterDepth {
    fn apply(&mut self, update: LighterUpdate, time: u64, snapshot: bool) -> Result<Change> {
        ensure!(update.nonce > 0, "invalid Lighter nonce");
        if !snapshot {
            if self.nonce.is_none() {
                return Ok(Change::Other);
            }
            if self.nonce.is_some_and(|nonce| update.nonce <= nonce) {
                return Ok(Change::Other);
            }
            if self.nonce != Some(update.begin_nonce) || time < self.time {
                *self = Self::default();
                return Ok(Change::Gap);
            }
        } else {
            *self = Self::default();
        }
        for (side, levels) in self.sides.iter_mut().zip([update.bids, update.asks]) {
            for raw in levels {
                let level = parse_level(&raw.price, &raw.size, true)?;
                if level.contracts.is_zero() {
                    side.remove(&level.price);
                } else {
                    side.insert(level.price, level.contracts);
                }
            }
            ensure!(
                side.len() <= MAX_LIGHTER_LEVELS,
                "Lighter reconstructed depth exceeds limit"
            );
        }
        self.nonce = Some(update.nonce);
        self.time = time;
        Ok(Change::Book)
    }

    fn levels(&self) -> [Vec<Level>; 2] {
        let to_level = |(&price, &contracts)| Level { price, contracts };
        [
            self.sides[0]
                .iter()
                .rev()
                .take(LEVELS)
                .map(to_level)
                .collect(),
            self.sides[1].iter().take(LEVELS).map(to_level).collect(),
        ]
    }
}

pub(super) fn coinbase(payload: &Value, product: &str, received: u64) -> Result<Option<Oracle>> {
    if payload["type"] != "ticker" || payload["product_id"].as_str() != Some(product) {
        return Ok(None);
    }
    let timestamp = payload["time"]
        .as_str()
        .context("missing Coinbase event time")?;
    let timestamp_ms =
        u64::try_from(chrono::DateTime::parse_from_rfc3339(timestamp)?.timestamp_millis())?;
    validate_time(timestamp_ms, received)?;
    let field = |name: &str| -> Result<Decimal> {
        Ok(payload[name]
            .as_str()
            .context("missing Coinbase BBO")?
            .parse()?)
    };
    let (bid, ask) = (field("best_bid")?, field("best_ask")?);
    ensure!(
        bid > Decimal::ZERO && ask >= bid && ask <= Decimal::from(1_000_000_000),
        "invalid Coinbase spot BBO"
    );
    Ok(Some(Oracle {
        price: (bid + ask) / Decimal::TWO,
        timestamp_ms,
        mode: OracleMode::LiveExternal,
    }))
}

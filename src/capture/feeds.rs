use super::{LegConfig, LighterClockAudit};
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
    ClockUnavailable,
    Other,
}

#[derive(Clone, Copy)]
pub(super) struct ContextTick {
    pub oracle: Oracle,
    pub raw_rate: Option<Decimal>,
    pub funding_price: Option<Decimal>,
    pub last_settled_ms: Option<u64>,
}

pub(super) struct LegFeed<'a> {
    venue: String,
    market: String,
    config: &'a LegConfig,
    depth: LighterDepth,
    book: Option<Book>,
    book_from_snapshot: bool,
    latest_full_book_ms: u64,
    context: Option<ContextTick>,
    lighter_clock: LighterClockAudit,
}

impl<'a> LegFeed<'a> {
    pub fn new(terms: &MarketTerms, config: &'a LegConfig) -> Self {
        Self {
            venue: terms.venue.clone(),
            market: terms.market.clone(),
            config,
            depth: LighterDepth::default(),
            book: None,
            book_from_snapshot: false,
            latest_full_book_ms: 0,
            context: None,
            lighter_clock: LighterClockAudit::default(),
        }
    }

    pub fn accepts(&self, source: &str) -> bool {
        match self.venue.as_str() {
            "lighter" => source == "lighter",
            "arcus" | "bulk" => {
                let rest_venue = source.strip_suffix("_snapshot");
                rest_venue.unwrap_or(source) == self.venue
                    && self
                        .config
                        .book_mode(&self.venue)
                        .includes(rest_venue.is_some())
            }
            _ => {
                source == "hyperliquid"
                    || (source == "hyperliquid_snapshot"
                        && self.config.book_mode(&self.venue).includes(true))
            }
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
                self.lighter_clock = LighterClockAudit::default();
            }
            "hyperliquid_snapshot" | "arcus_snapshot" | "bulk_snapshot" => {
                self.invalidate_full_book(true)
            }
            "arcus" | "bulk" => self.invalidate_full_book(false),
            _ => {
                self.context = None;
                self.invalidate_full_book(false);
            }
        }
    }

    fn invalidate_full_book(&mut self, snapshot: bool) {
        if self.book_from_snapshot == snapshot {
            self.book = None;
        }
    }

    pub fn book(&self) -> Option<&Book> {
        self.book.as_ref()
    }

    pub fn context(&self) -> Option<ContextTick> {
        self.context
    }

    pub fn clock_audit(&self) -> Option<LighterClockAudit> {
        self.config.lighter_clock.map(|_| self.lighter_clock)
    }

    pub fn apply(
        &mut self,
        payload: &Value,
        received: u64,
        request_sent: Option<u64>,
        source: &str,
    ) -> Result<Change> {
        match source {
            "lighter" => {
                let id = self
                    .config
                    .lighter_market_id
                    .context("missing Lighter ID")?;
                self.apply_lighter(payload, received, request_sent, id)
            }
            "arcus" | "arcus_snapshot" | "bulk" | "bulk_snapshot" => {
                self.apply_external_book(payload, received, source)
            }
            _ => self.apply_hyperliquid(payload, received, source == "hyperliquid_snapshot"),
        }
    }

    fn apply_lighter(
        &mut self,
        payload: &Value,
        received: u64,
        request_sent: Option<u64>,
        market_id: u16,
    ) -> Result<Change> {
        let channel = payload["channel"].as_str().unwrap_or_default();
        if channel
            .split_once(':')
            .or_else(|| channel.split_once('/'))
            .and_then(|(_, id)| id.parse::<u16>().ok())
            != Some(market_id)
        {
            return Ok(Change::Other);
        }
        let kind = payload["type"].as_str().unwrap_or_default();
        if let (Some(policy), Some(sent)) = (self.config.lighter_clock, request_sent)
            && matches!(kind, "subscribed/order_book" | "subscribed/market_stats")
        {
            let published = payload["timestamp"]
                .as_u64()
                .context("missing Lighter subscription time")?;
            self.lighter_clock
                .observe(policy, sent, received, published);
            if self.lighter_clock.conflicting {
                self.book = None;
                self.context = None;
            }
        }
        match kind {
            "subscribed/order_book" | "update/order_book" => {
                let snapshot = payload["type"] == "subscribed/order_book";
                let update = LighterUpdate::deserialize(&payload["order_book"])?;
                let raw_time = update.last_updated_at / 1_000;
                ensure!(raw_time > 0, "missing Lighter book source time");
                let checked_time = if self.config.lighter_clock.is_some() {
                    payload["timestamp"]
                        .as_u64()
                        .filter(|published| raw_time <= *published)
                        .context("Lighter book commit exceeds or lacks publication time")?
                } else {
                    raw_time
                };
                let change = self.depth.apply(update, raw_time, snapshot)?;
                match change {
                    Change::Book => {
                        let time = match self.lighter_time(raw_time, checked_time, received) {
                            Ok(time) => time,
                            Err(change) => {
                                self.book = None;
                                return Ok(change);
                            }
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
                    mark_price: Option<String>,
                    current_funding_rate: Option<String>,
                    funding_timestamp: Option<u64>,
                }
                let value = Stats::deserialize(&payload["market_stats"])?;
                ensure!(
                    value.market_id == market_id && value.symbol == self.market,
                    "Lighter market ID does not match configured symbol"
                );
                let raw_time = payload["timestamp"]
                    .as_u64()
                    .context("missing Lighter context time")?;
                let time = match self.lighter_time(raw_time, raw_time, received) {
                    Ok(time) => time,
                    Err(change) => {
                        self.context = None;
                        return Ok(change);
                    }
                };
                self.set_context(
                    time,
                    value.index_price.parse()?,
                    value
                        .current_funding_rate
                        .as_deref()
                        .map(str::parse)
                        .transpose()?,
                    value.mark_price.as_deref().map(str::parse).transpose()?,
                    value
                        .funding_timestamp
                        .filter(|time| *time > 0 && *time <= raw_time),
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
        let use_book = self.config.book_mode(&self.venue).includes(snapshot);
        match payload["channel"].as_str().unwrap_or_default() {
            "l2Book" if use_book => {
                let book = crate::exchanges::hyperliquid::L2Book::deserialize(data)?;
                let Some(time) = self.source_time(book.time, received) else {
                    self.invalidate_full_book(snapshot);
                    return Ok(Change::Clock(book.time.saturating_sub(received)));
                };
                // Both transports use the exchange clock. Retain the last
                // accepted publication time across transport failures.
                if time <= self.latest_full_book_ms {
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
                self.latest_full_book_ms = time;
                self.book_from_snapshot = snapshot;
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
                self.set_context(time, price, raw_rate, Some(price), None)?;
                Ok(Change::Other)
            }
            _ => Ok(Change::Other),
        }
    }

    fn apply_external_book(
        &mut self,
        payload: &Value,
        received: u64,
        source: &str,
    ) -> Result<Change> {
        let snapshot = source.ends_with("_snapshot");
        let data = match source {
            "arcus"
                if payload["channel"] == "l2Orderbook"
                    && matches!(
                        payload["type"].as_str(),
                        Some("subscribed" | "channel_data")
                    )
                    && payload["id"].as_str() == Some(self.market.as_str()) =>
            {
                &payload["contents"]
            }
            "bulk"
                if payload["type"] == "l2Snapshot"
                    && payload["data"]["book"]["symbol"].as_str() == Some(self.market.as_str()) =>
            {
                &payload["data"]["book"]
            }
            _ if snapshot => {
                let requested = payload["market"]
                    .as_str()
                    .context("missing requested market")?;
                if requested != self.market {
                    return Ok(Change::Other);
                }
                &payload["data"]
            }
            _ => return Ok(Change::Other),
        };
        let (raw_ms, bids, asks) = if self.venue == "arcus" {
            #[derive(Deserialize)]
            struct ArcusSnapshot {
                timestamp: u64,
                bids: Vec<[String; 2]>,
                asks: Vec<[String; 2]>,
            }
            let book = ArcusSnapshot::deserialize(data)?;
            let levels = |side: Vec<[String; 2]>| {
                side.into_iter()
                    .map(|[price, size]| parse_level(&price, &size, false))
                    .collect::<Result<Vec<_>>>()
            };
            (
                book.timestamp / 1_000,
                levels(book.bids)?,
                levels(book.asks)?,
            )
        } else {
            let book = crate::exchanges::bulk::Snapshot::deserialize(data)?;
            let time = book.timestamp_for(&self.market)?;
            let [bids, asks] = book.levels.map(|side| {
                side.into_iter()
                    .map(|level| Level {
                        price: level.px,
                        contracts: level.sz,
                    })
                    .collect::<Vec<_>>()
            });
            (time, bids, asks)
        };
        ensure!(
            bids.len() <= LEVELS
                && asks.len() <= LEVELS
                && bids.iter().chain(&asks).all(|level| {
                    level.price > Decimal::ZERO && level.contracts > Decimal::ZERO
                }),
            "invalid full book depth"
        );
        let Some(time) = self.source_time(raw_ms, received) else {
            self.invalidate_full_book(snapshot);
            return Ok(Change::Clock(raw_ms.saturating_sub(received)));
        };
        if time <= self.latest_full_book_ms {
            return Ok(Change::Other);
        }
        self.set_book(time, bids, asks);
        self.latest_full_book_ms = time;
        self.book_from_snapshot = snapshot;
        Ok(Change::Book)
    }

    fn source_time(&self, raw: u64, received: u64) -> Option<u64> {
        Self::adjusted_time(raw, received, self.config.source_clock_ahead_bound_ms)
    }

    fn lighter_time(
        &self,
        raw: u64,
        checked_time: u64,
        received: u64,
    ) -> std::result::Result<u64, Change> {
        let correction = match self.config.lighter_clock {
            Some(policy) => self
                .lighter_clock
                .correction(policy, received)
                .ok_or(Change::ClockUnavailable)?,
            None => self.config.source_clock_ahead_bound_ms,
        };
        Self::adjusted_time(checked_time, received, correction)
            .and_then(|_| raw.checked_sub(correction).filter(|time| *time > 0))
            .ok_or_else(|| Change::Clock(checked_time.saturating_sub(received)))
    }

    fn adjusted_time(raw: u64, received: u64, correction: u64) -> Option<u64> {
        let earliest = raw.checked_sub(correction)?;
        (earliest > 0 && earliest <= received).then_some(earliest)
    }

    fn set_context(
        &mut self,
        timestamp_ms: u64,
        price: Decimal,
        raw_rate: Option<Decimal>,
        funding_price: Option<Decimal>,
        last_settled_ms: Option<u64>,
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
                funding_price: funding_price.filter(|price| *price > Decimal::ZERO),
                last_settled_ms,
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

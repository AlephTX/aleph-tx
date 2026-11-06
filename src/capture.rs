//! Compile public market recordings into causal paper replay frames.

mod feeds;

use crate::{
    strategy::relative_value::{
        AssetClass, Book, EquitySession, Frame, FundingForecast, FundingPoint, FundingSettlement,
        MarketFrame, Oracle, OracleMode, PairConfig, RelativeValue,
    },
    venues::VenueRegistry,
};
use anyhow::{Context, Result, ensure};
use feeds::{ContextTick, LegFeed};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeMap,
    io::{BufRead, Read, Write},
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureConfig {
    pub pair: PairConfig,
    pub one_way_latency_ms: u64,
    pub trade_from_ms: Option<u64>,
    pub sample_ms: u64,
    #[serde(default)]
    pub paper_entries: bool,
    pub legs: [LegConfig; 2],
    pub reference: ReferenceFeed,
    #[serde(default)]
    pub equity_sessions: Vec<EquitySession>,
    #[serde(default)]
    pub settlements: [Vec<FundingSettlement>; 2],
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegConfig {
    pub lighter_market_id: Option<u16>,
    #[serde(with = "rust_decimal::serde::str")]
    pub available_notional: Decimal,
    /// As of the recorded session, reviewed as an external index.
    #[serde(default)]
    pub external_oracle: bool,
    /// The Hyperliquid active context has receipt time only.
    #[serde(default)]
    pub accept_receipt_time_for_oracle: bool,
    #[serde(default)]
    pub use_hyperliquid_snapshots: bool,
    /// An independently established upper bound on how far the source clock
    /// leads the recorder. Subtract it from every source publication time.
    #[serde(default)]
    pub source_clock_ahead_bound_ms: u64,
    pub funding: Option<FundingRule>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FundingRule {
    #[serde(with = "rust_decimal::serde::str")]
    pub raw_rate_scale: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub uncertainty_bps: Decimal,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReferenceFeed {
    Coinbase {
        product: String,
    },
    External {
        observations: Vec<ReferenceObservation>,
    },
    Missing,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceObservation {
    pub available_at_ms: u64,
    pub oracle: Oracle,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    source: String,
    event: String,
    received_at_ms: u64,
    available_at_ms: u64,
    payload: Option<serde_json::Value>,
}

#[derive(Debug, Default, Serialize)]
pub struct Coverage {
    pub first_frame_ms: Option<u64>,
    pub last_frame_ms: Option<u64>,
    pub records: usize,
    pub frames: usize,
    pub source_messages: BTreeMap<String, usize>,
    pub disconnects: BTreeMap<String, usize>,
    pub leg_book_updates: [usize; 2],
    pub rejected_publications: [usize; 2],
    pub clock_rejections: [usize; 2],
    pub max_rejected_clock_lead_ms: [u64; 2],
    pub lighter_nonce_gaps: [usize; 2],
    pub fresh_book_pair_frames: usize,
    pub synchronized_book_pair_frames: usize,
    pub fresh_reference_frames: usize,
    pub fresh_venue_oracle_frames: usize,
    pub equity_session_frames: usize,
    pub joined_input_frames: usize,
    pub settlement_records_written: [usize; 2],
    pub paper_entries: bool,
}

#[derive(Serialize)]
struct Header<'a> {
    pair: &'a PairConfig,
    one_way_latency_ms: u64,
    trade_from_ms: Option<u64>,
}

const MAX_LINE: u64 = 2 * 1024 * 1024;
const MAX_FRAMES: usize = 1_000_000;

fn read_record<T: DeserializeOwned>(
    reader: &mut impl BufRead,
    bytes: &mut Vec<u8>,
) -> Result<Option<T>> {
    bytes.clear();
    if reader.take(MAX_LINE + 1).read_until(b'\n', bytes)? == 0 {
        return Ok(None);
    }
    ensure!(bytes.len() as u64 <= MAX_LINE, "capture line too large");
    Ok(Some(serde_json::from_slice(bytes)?))
}

fn line(output: &mut impl Write, value: &impl Serialize) -> Result<()> {
    serde_json::to_writer(&mut *output, value)?;
    output.write_all(b"\n")?;
    Ok(())
}

fn missing_oracle() -> Oracle {
    Oracle {
        price: Decimal::ZERO,
        timestamp_ms: 0,
        mode: OracleMode::LastClose,
    }
}

fn fresh(timestamp: u64, now: u64, age: u64) -> bool {
    timestamp > 0 && now.checked_sub(timestamp).is_some_and(|delta| delta <= age)
}

impl CaptureConfig {
    fn validate(&self, registry: &VenueRegistry) -> Result<()> {
        RelativeValue::new(self.pair.clone(), registry)?;
        ensure!(
            (100..=60_000).contains(&self.sample_ms),
            "sample_ms must be 100..=60000"
        );
        ensure!(
            self.one_way_latency_ms > 0
                && self.one_way_latency_ms.saturating_mul(2) < self.pair.execution_window_ms,
            "execution window must fit sequential two-leg latency"
        );
        for (i, leg) in self.legs.iter().enumerate() {
            let venue = registry.get(&self.pair.legs[i].venue)?;
            ensure!(
                (venue.id == "lighter" && leg.lighter_market_id.is_some())
                    || (venue.hyperliquid_dex.is_some() && leg.lighter_market_id.is_none()),
                "capture needs a Lighter market ID or a Hyperliquid/HIP-3 coin"
            );
            ensure!(
                leg.source_clock_ahead_bound_ms <= 1_000
                    && (!leg.use_hyperliquid_snapshots || venue.hyperliquid_dex.is_some()),
                "invalid capture clock or snapshot policy"
            );
            ensure!(
                leg.available_notional >= Decimal::ZERO
                    && leg.available_notional <= Decimal::from(1_000_000_000)
                    && (!self.paper_entries || leg.available_notional > Decimal::ZERO),
                "invalid paper notional budget"
            );
            if let Some(rule) = leg.funding {
                ensure!(
                    rule.raw_rate_scale > Decimal::ZERO
                        && rule.raw_rate_scale <= Decimal::ONE
                        && (Decimal::ZERO..=Decimal::from(10_000)).contains(&rule.uncertainty_bps),
                    "invalid funding normalization"
                );
            }
            let payments = &self.settlements[i];
            let schedule = self.pair.legs[i].funding;
            ensure!(
                payments.windows(2).all(|w| w[0].at_ms < w[1].at_ms)
                    && payments.iter().all(|p| p.at_ms > 0
                        && p.at_ms % schedule.interval_ms == schedule.offset_ms
                        && p.rate.abs() <= Decimal::ONE
                        && p.oracle_price > Decimal::ZERO),
                "invalid or duplicate normalized funding settlement"
            );
        }
        ensure!(
            self.equity_sessions
                .windows(2)
                .all(|w| w[0].observed_at_ms < w[1].observed_at_ms)
                && self.equity_sessions.iter().all(|s| s.observed_at_ms > 0
                    && s.opens_at_ms < s.closes_at_ms
                    && s.clear_until_ms > s.opens_at_ms),
            "invalid session observations"
        );
        match &self.reference {
            ReferenceFeed::Coinbase { product } => {
                ensure!(
                    self.pair.asset_class == AssetClass::Crypto && !product.is_empty(),
                    "Coinbase reference requires a crypto pair and product ID"
                );
            }
            ReferenceFeed::External { observations } => ensure!(
                observations
                    .windows(2)
                    .all(|w| w[0].available_at_ms < w[1].available_at_ms)
                    && observations.iter().all(|o| o.oracle.timestamp_ms > 0
                        && o.oracle.timestamp_ms <= o.available_at_ms
                        && o.oracle.price > Decimal::ZERO),
                "invalid external reference observations"
            ),
            ReferenceFeed::Missing => {}
        }
        if self.paper_entries && self.pair.asset_class == AssetClass::Equity {
            ensure!(
                matches!(&self.reference, ReferenceFeed::External { observations } if !observations.is_empty())
                    && !self.equity_sessions.is_empty(),
                "equity paper entries require external reference and session data"
            );
        }
        Ok(())
    }
}

struct Compiler<'a> {
    config: &'a CaptureConfig,
    legs: [LegFeed; 2],
    coinbase: Option<Oracle>,
    last_frame_ms: Option<u64>,
    coverage: Coverage,
}

impl<'a> Compiler<'a> {
    fn new(config: &'a CaptureConfig) -> Self {
        Self {
            config,
            legs: std::array::from_fn(|i| LegFeed::new(&config.pair.legs[i], &config.legs[i])),
            coinbase: None,
            last_frame_ms: None,
            coverage: Coverage {
                paper_entries: config.paper_entries,
                ..Default::default()
            },
        }
    }

    fn apply(&mut self, row: Record) -> Result<()> {
        ensure!(
            matches!(
                row.source.as_str(),
                "lighter" | "hyperliquid" | "hyperliquid_snapshot" | "coinbase"
            ),
            "unsupported capture source"
        );
        match (row.event.as_str(), row.payload) {
            ("disconnect", None) => {
                *self
                    .coverage
                    .disconnects
                    .entry(row.source.clone())
                    .or_default() += 1;
                for leg in &mut self.legs {
                    leg.disconnect(&row.source);
                }
                if row.source == "coinbase" {
                    self.coinbase = None;
                }
            }
            ("message", Some(payload)) => {
                *self
                    .coverage
                    .source_messages
                    .entry(row.source.clone())
                    .or_default() += 1;
                if row.source == "coinbase" {
                    if let ReferenceFeed::Coinbase { product } = &self.config.reference
                        && let Ok(Some(quote)) =
                            feeds::coinbase(&payload, product, row.received_at_ms)
                        && self
                            .coinbase
                            .is_none_or(|prior| quote.timestamp_ms >= prior.timestamp_ms)
                    {
                        self.coinbase = Some(quote);
                    }
                } else {
                    for (i, leg) in self.legs.iter_mut().enumerate() {
                        if !leg.accepts(&row.source) {
                            continue;
                        }
                        match leg.apply(&payload, row.received_at_ms, &row.source) {
                            Ok(feeds::Change::Book) => self.coverage.leg_book_updates[i] += 1,
                            Ok(feeds::Change::Gap) => self.coverage.lighter_nonce_gaps[i] += 1,
                            Ok(feeds::Change::Clock(ahead)) => {
                                self.coverage.clock_rejections[i] += 1;
                                self.coverage.max_rejected_clock_lead_ms[i] =
                                    self.coverage.max_rejected_clock_lead_ms[i].max(ahead);
                            }
                            Ok(feeds::Change::Other) => {}
                            Err(_) => {
                                self.coverage.rejected_publications[i] += 1;
                                leg.disconnect(&row.source);
                            }
                        }
                    }
                }
            }
            _ => anyhow::bail!("invalid capture record"),
        }
        Ok(())
    }

    fn emit(&mut self, now: u64, output: &mut impl Write, final_frame: bool) -> Result<()> {
        if self
            .last_frame_ms
            .is_some_and(|last| now <= last || (!final_frame && now - last < self.config.sample_ms))
        {
            return Ok(());
        }
        ensure!(
            self.coverage.frames < MAX_FRAMES,
            "recording exceeds paper frame limit"
        );
        let reference = match &self.config.reference {
            ReferenceFeed::Coinbase { .. } => self.coinbase,
            ReferenceFeed::External { observations } => observations
                .get(..observations.partition_point(|o| o.available_at_ms <= now))
                .and_then(|slice| slice.last())
                .map(|o| o.oracle),
            ReferenceFeed::Missing => None,
        }
        .unwrap_or_else(missing_oracle);
        let equity_session = self
            .config
            .equity_sessions
            .get(
                ..self
                    .config
                    .equity_sessions
                    .partition_point(|s| s.observed_at_ms <= now),
            )
            .and_then(|slice| slice.last())
            .copied();
        let legs = std::array::from_fn(|i| self.market_frame(i, now));
        let frame = Frame {
            now_ms: now,
            reference,
            equity_session,
            legs,
        };
        self.measure(&frame);
        line(output, &frame)?;
        self.last_frame_ms = Some(now);
        Ok(())
    }

    fn market_frame(&self, i: usize, now: u64) -> MarketFrame {
        let leg = &self.legs[i];
        let mut book = leg.book().cloned().unwrap_or_else(|| Book {
            market: self.config.pair.legs[i].market.clone(),
            timestamp_ms: 0,
            tradable: false,
            bids: vec![],
            asks: vec![],
            bounds: None,
            available_notional: Decimal::ZERO,
        });
        if self.config.paper_entries {
            book.available_notional = self.config.legs[i].available_notional;
        }
        let payments = &self.config.settlements[i];
        let start = payments.partition_point(|p| p.at_ms <= self.last_frame_ms.unwrap_or(now));
        let end = payments.partition_point(|p| p.at_ms <= now);
        MarketFrame {
            book,
            oracle: leg.context().map_or_else(missing_oracle, |ctx| ctx.oracle),
            forecast: leg.context().and_then(|ctx| self.forecast(i, now, ctx)),
            settled: payments[start..end].to_vec(),
        }
    }

    fn forecast(&self, i: usize, now: u64, ctx: ContextTick) -> Option<FundingForecast> {
        let rule = self.config.legs[i].funding?;
        let schedule = self.config.pair.legs[i].funding;
        let observed = ctx.oracle.timestamp_ms;
        let remainder = observed % schedule.interval_ms;
        let delay = if remainder < schedule.offset_ms {
            schedule.offset_ms - remainder
        } else {
            schedule.interval_ms - (remainder - schedule.offset_ms)
        };
        let at_ms = observed.checked_add(delay)?;
        let rate = ctx.raw_rate?.checked_mul(rule.raw_rate_scale)?;
        if at_ms <= now
            || observed > now
            || rate.abs() > Decimal::ONE
            || ctx.oracle.mode != OracleMode::LiveExternal
        {
            return None;
        }
        // Only the next payment can be inferred from an observed context.
        Some(FundingForecast {
            observed_at_ms: observed,
            payments: vec![FundingPoint {
                at_ms,
                rate,
                uncertainty_bps: rule.uncertainty_bps,
                oracle_price: ctx.oracle.price,
            }],
        })
    }

    fn measure(&mut self, frame: &Frame) {
        let now = frame.now_ms;
        let pair = &self.config.pair;
        let both_books = frame.legs.iter().all(|leg| {
            leg.book.tradable && fresh(leg.book.timestamp_ms, now, pair.planner.max_quote_age_ms)
        });
        let synchronized = both_books
            && frame.legs[0]
                .book
                .timestamp_ms
                .abs_diff(frame.legs[1].book.timestamp_ms)
                <= pair.planner.max_quote_skew_ms;
        let is_reference = |oracle: Oracle| {
            oracle.mode == OracleMode::LiveExternal
                && fresh(oracle.timestamp_ms, now, pair.oracle_max_age_ms)
        };
        let reference = is_reference(frame.reference);
        let oracles = frame.legs.iter().all(|leg| is_reference(leg.oracle));
        let session = frame.equity_session.is_some_and(|s| {
            !s.halted && now >= s.opens_at_ms && now < s.closes_at_ms.min(s.clear_until_ms)
        });
        let counts = &mut self.coverage;
        counts.first_frame_ms.get_or_insert(now);
        counts.last_frame_ms = Some(now);
        counts.frames += 1;
        counts.fresh_book_pair_frames += usize::from(both_books);
        counts.synchronized_book_pair_frames += usize::from(synchronized);
        counts.fresh_reference_frames += usize::from(reference);
        counts.fresh_venue_oracle_frames += usize::from(oracles);
        counts.equity_session_frames += usize::from(session);
        counts.joined_input_frames += usize::from(
            synchronized
                && reference
                && oracles
                && (pair.asset_class == AssetClass::Crypto || session),
        );
        for (i, leg) in frame.legs.iter().enumerate() {
            counts.settlement_records_written[i] += leg.settled.len();
        }
    }
}

pub fn compile(
    config: &CaptureConfig,
    mut input: impl BufRead,
    mut output: impl Write,
    registry: &VenueRegistry,
) -> Result<Coverage> {
    config.validate(registry)?;
    let mut compiler = Compiler::new(config);
    line(
        &mut output,
        &Header {
            pair: &config.pair,
            one_way_latency_ms: config.one_way_latency_ms,
            trade_from_ms: config.trade_from_ms,
        },
    )?;
    let mut bytes = Vec::new();
    let mut group = 0;
    let mut ended = false;
    while let Some(row) = read_record::<Record>(&mut input, &mut bytes)
        .with_context(|| format!("capture line {}", compiler.coverage.records + 1))?
    {
        ensure!(!ended, "data after capture footer");
        let now = row.available_at_ms;
        ensure!(
            row.received_at_ms > 0 && now >= row.received_at_ms && now >= group,
            "invalid capture arrival order"
        );
        compiler.coverage.records += 1;
        if row.source == "collector" && row.event == "end" && row.payload.is_none() {
            compiler.emit(group, &mut output, true)?;
            ended = true;
            continue;
        }
        if group > 0 && now > group {
            compiler.emit(group, &mut output, false)?;
        }
        group = now;
        compiler.apply(row)?;
    }
    ensure!(
        ended && compiler.coverage.frames > 0,
        "capture incomplete or empty"
    );
    output.flush()?;
    Ok(compiler.coverage)
}

#[cfg(test)]
mod tests;

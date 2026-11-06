//! Cost-aware scanner over the existing seven-slot SHM transport.
//!
//! Execution remains separate. Each market needs explicit verified metadata,
//! trading capacity and costs through `configure_market`; `new()` alone cannot
//! turn a gross price discrepancy into an actionable signal. New venues use the
//! dynamic cross_venue planner without changing the stable SHM ABI.

use super::cross_venue::{PlanKind, PlannerConfig, TradePlan, VenueSnapshot, evaluate_pair};
use crate::shm_reader::ShmBboMessage;
use crate::strategy::Strategy;
use std::collections::HashMap;

pub use crate::shm_reader::NUM_EXCHANGES;

#[derive(Clone, Copy, Debug, Default)]
pub struct BboSnapshot {
    pub bid_price: f64,
    pub bid_size: f64,
    pub ask_price: f64,
    pub ask_size: f64,
    pub timestamp_ns: u64,
}

impl BboSnapshot {
    #[inline]
    pub fn is_valid(&self) -> bool {
        [self.bid_price, self.ask_price, self.bid_size, self.ask_size]
            .iter()
            .all(|value| value.is_finite() && *value > 0.0)
            && self.bid_price < self.ask_price
            && self.timestamp_ns > 0
    }

    #[inline]
    pub fn from_shm(msg: &ShmBboMessage) -> Self {
        Self {
            bid_price: msg.bid_price,
            bid_size: msg.bid_size,
            ask_price: msg.ask_price,
            ask_size: msg.ask_size,
            timestamp_ns: msg.timestamp_ns,
        }
    }
}

pub struct ArbitrageEngine {
    config: PlannerConfig,
    bbo_state: HashMap<u16, [ShmBboMessage; NUM_EXCHANGES]>,
    markets: HashMap<(u16, u8), VenueSnapshot>,
}

impl ArbitrageEngine {
    pub fn new(min_spread_bps: f64) -> Self {
        Self {
            config: PlannerConfig {
                min_net_edge_bps: min_spread_bps,
                ..Default::default()
            },
            bbo_state: HashMap::new(),
            markets: HashMap::new(),
        }
    }

    pub fn with_config(config: PlannerConfig) -> anyhow::Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            bbo_state: HashMap::new(),
            markets: HashMap::new(),
        })
    }

    /// The template carries verified economics/contract metadata; incoming SHM
    /// replaces only BBO fields. Funding and approved risk capacity must be
    /// refreshed by the caller; stale funding cannot produce a derivative plan.
    pub fn configure_market(
        &mut self,
        symbol_id: u16,
        exchange_id: u8,
        mut template: VenueSnapshot,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            exchange_id > 0 && (exchange_id as usize) < NUM_EXCHANGES,
            "exchange outside SHM transport"
        );
        anyhow::ensure!(
            !template.venue.trim().is_empty() && !template.liquidity_domain.trim().is_empty(),
            "market identity required"
        );
        if let Some(cache) = self.bbo_state.get(&symbol_id) {
            apply_bbo(&mut template, &cache[exchange_id as usize]);
        } else {
            apply_bbo(&mut template, &ShmBboMessage::default());
        }
        self.markets.insert((symbol_id, exchange_id), template);
        Ok(())
    }

    /// Copy a source only if it is not older than the last accepted publication.
    /// A zero/invalid update explicitly invalidates the cached book.
    pub fn update_at(
        &mut self,
        symbol_id: u16,
        exchange_id: u8,
        bbo: &ShmBboMessage,
        now_ms: u64,
    ) -> Option<TradePlan> {
        if exchange_id == 0 || exchange_id as usize >= NUM_EXCHANGES {
            return None;
        }
        let cache = self
            .bbo_state
            .entry(symbol_id)
            .or_insert_with(|| [ShmBboMessage::default(); NUM_EXCHANGES]);
        let index = exchange_id as usize;
        if bbo.timestamp_ns != 0 && bbo.timestamp_ns < cache[index].timestamp_ns {
            return None;
        }
        if !BboSnapshot::from_shm(bbo).is_valid() || bbo.timestamp_ns / 1_000_000 > now_ms {
            cache[index] = ShmBboMessage::default();
            if let Some(template) = self.markets.get_mut(&(symbol_id, exchange_id)) {
                apply_bbo(template, &cache[index]);
            }
            return None;
        }
        cache[index] = *bbo;
        if let Some(template) = self.markets.get_mut(&(symbol_id, exchange_id)) {
            apply_bbo(template, bbo);
        }
        self.best_plan(symbol_id, now_ms)
    }

    pub fn best_plan(&self, symbol_id: u16, now_ms: u64) -> Option<TradePlan> {
        self.bbo_state.get(&symbol_id)?;
        let mut best: Option<TradePlan> = None;
        for buy_id in 1..NUM_EXCHANGES as u8 {
            let Some(buy) = self.markets.get(&(symbol_id, buy_id)) else {
                continue;
            };
            for sell_id in 1..NUM_EXCHANGES as u8 {
                if buy_id == sell_id {
                    continue;
                }
                let Some(sell) = self.markets.get(&(symbol_id, sell_id)) else {
                    continue;
                };
                if let Ok(plan) =
                    evaluate_pair(buy, sell, &self.config, now_ms, PlanKind::SpreadCapture)
                    && best
                        .as_ref()
                        .is_none_or(|current| plan.net_edge_usd > current.net_edge_usd)
                {
                    best = Some(plan);
                }
            }
        }
        best
    }
}

fn apply_bbo(snapshot: &mut VenueSnapshot, bbo: &ShmBboMessage) {
    snapshot.bid = bbo.bid_price;
    snapshot.ask = bbo.ask_price;
    snapshot.bid_size = bbo.bid_size;
    snapshot.ask_size = bbo.ask_size;
    snapshot.timestamp_ms = bbo.timestamp_ns / 1_000_000;
}

impl Strategy for ArbitrageEngine {
    fn name(&self) -> &str {
        "Cost-aware Cross-Exchange Planner"
    }

    fn on_bbo_update(&mut self, symbol_id: u16, exchange_id: u8, bbo: &ShmBboMessage) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        if let Some(plan) = self.update_at(symbol_id, exchange_id, bbo, now) {
            tracing::info!(
                symbol_id,
                buy_venue = plan.buy.venue,
                sell_venue = plan.sell.venue,
                base_quantity = plan.base_quantity,
                net_edge_bps = plan.net_edge_bps,
                "Cross-venue candidate after costs and risk reserves; no orders submitted"
            );
        }
    }

    fn on_idle(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::cross_venue::tests::market;

    fn bbo(bid: f64, ask: f64, time: u64) -> ShmBboMessage {
        ShmBboMessage {
            bid_price: bid,
            ask_price: ask,
            bid_size: 10.0,
            ask_size: 10.0,
            timestamp_ns: time * 1_000_000,
            ..Default::default()
        }
    }

    #[test]
    fn scanner_includes_slots_five_and_six_and_rejects_stale_reordering() {
        let mut engine = ArbitrageEngine::new(5.0);
        engine
            .configure_market(1002, 5, market("backpack", 99.0, 100.0))
            .unwrap();
        engine
            .configure_market(1002, 6, market("binance", 101.0, 102.0))
            .unwrap();
        assert!(
            engine
                .update_at(1002, 5, &bbo(99.0, 100.0, 1_000_000), 1_000_000)
                .is_none()
        );
        assert!(
            engine
                .update_at(1002, 6, &bbo(101.0, 102.0, 1_000_000), 1_000_000)
                .is_some()
        );
        assert!(
            engine
                .update_at(1002, 6, &bbo(500.0, 501.0, 999_999), 1_000_000)
                .is_none()
        );
        assert!(engine.best_plan(1002, 1_002_000).is_none());
        assert!(
            engine
                .update_at(1002, 6, &ShmBboMessage::default(), 1_000_000)
                .is_none()
        );
        assert!(engine.best_plan(1002, 1_000_000).is_none());
    }

    #[test]
    fn no_actionable_signal_without_explicit_verified_costs_and_metadata() {
        let mut engine = ArbitrageEngine::new(1.0);
        engine.update_at(1002, 1, &bbo(99.0, 100.0, 1_000_000), 1_000_000);
        assert!(
            engine
                .update_at(1002, 2, &bbo(110.0, 111.0, 1_000_000), 1_000_000)
                .is_none()
        );
    }
}

//! Receipt-causal Core-only entry participation for offline research.
//! Quotes, cancels, fill accounting and all inventory limits use the existing
//! lifecycle. An unready signal may withdraw an entry, never a possible exit.
use super::*;

const BUCKET_MS: u64 = 100;
const BUCKETS: usize = 64;

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LocalReversalFilter {
    pub lookback_ms: u64,
    pub recovery_ms: u64,
    pub max_age_ms: u64,
    pub shock_bps: f64,
    pub recovery_bps: f64,
    pub flow_imbalance: f64,
    pub min_shock_notional: f64,
    #[serde(default)]
    pub entry_hold_ms: u64,
}

impl LocalReversalFilter {
    pub(super) fn validate(self) -> Result<()> {
        ensure!(
            self.lookback_ms.is_multiple_of(BUCKET_MS)
                && self.recovery_ms.is_multiple_of(BUCKET_MS)
                && (1_000..=5_000).contains(&self.lookback_ms)
                && (200..self.lookback_ms).contains(&self.recovery_ms)
                && (1..=1_000).contains(&self.max_age_ms)
                && self.shock_bps.is_finite()
                && self.shock_bps > 0.0
                && self.recovery_bps.is_finite()
                && self.recovery_bps > 0.0
                && self.flow_imbalance > 0.0
                && self.flow_imbalance < 1.0
                && self.min_shock_notional.is_finite()
                && self.min_shock_notional > 0.0
                && self.entry_hold_ms <= 1_000,
            "invalid local reversal policy"
        );
        Ok(())
    }
}

#[derive(Clone, Copy, Default)]
struct Bucket {
    slot: u64,
    book_at: u64,
    source_ms: u64,
    mid: f64,
    buy: f64,
    sell: f64,
}

pub(super) struct LocalSignal {
    buckets: [Bucket; BUCKETS],
    online_since: Option<u64>,
    last_trade_source: Option<u64>,
    pub rejected_trade_times: u64,
    policy: LocalReversalFilter,
}

impl LocalSignal {
    pub(super) fn new(policy: LocalReversalFilter) -> Self {
        Self {
            buckets: [Bucket::default(); BUCKETS],
            online_since: None,
            last_trade_source: None,
            rejected_trade_times: 0,
            policy,
        }
    }

    pub(super) fn feed(&mut self, at: u64, online: bool) {
        if !online || self.online_since.is_none() {
            self.buckets = [Bucket::default(); BUCKETS];
            self.last_trade_source = None;
            self.online_since = online.then_some(at);
        }
    }

    fn bucket(&mut self, at: u64) -> &mut Bucket {
        let slot = at / BUCKET_MS;
        let bucket = &mut self.buckets[(slot % BUCKETS as u64) as usize];
        if bucket.slot != slot {
            *bucket = Bucket {
                slot,
                ..Default::default()
            };
        }
        bucket
    }

    pub(super) fn book(&mut self, at: u64, book: &Book) {
        if self.online_since.is_none() || !book.fresh(at, self.policy.max_age_ms) {
            return;
        }
        if let Some((bid, ask)) = book.touch() {
            let bucket = self.bucket(at);
            bucket.book_at = at;
            bucket.source_ms = book.timestamp_ms;
            bucket.mid = (bid[0] + ask[0]) * 0.5;
        }
    }

    pub(super) fn trade(&mut self, at: u64, source: u64, notional: f64, buy: bool) {
        let Some(since) = self.online_since else {
            return;
        };
        if source < since || source > at || at - source > self.policy.max_age_ms {
            self.rejected_trade_times += 1;
            return;
        }
        self.last_trade_source = Some(self.last_trade_source.unwrap_or(0).max(source));
        let bucket = self.bucket(at);
        if buy {
            bucket.buy += notional;
        } else {
            bucket.sell += notional;
        }
    }

    // Use completed receipt buckets for flow. The current bucket is partial;
    // excluding it prevents a large arriving batch from defining both periods.
    fn flow(&self, from: u64, until: u64) -> Option<(f64, f64)> {
        let (mut buy, mut sell) = (0.0, 0.0);
        for bucket in &self.buckets {
            if from <= bucket.slot && bucket.slot < until {
                buy += bucket.buy;
                sell += bucket.sell;
            }
        }
        let total = buy + sell;
        (total.is_finite() && total > 0.0).then(|| ((buy - sell) / total, total))
    }

    fn book_before(&self, at: u64) -> Option<Bucket> {
        self.buckets
            .iter()
            .filter(|b| b.mid > 0.0 && b.book_at <= at && at - b.book_at <= 2 * BUCKET_MS)
            .max_by_key(|b| b.book_at)
            .copied()
    }

    pub(super) fn allowed(&self, at: u64) -> Option<[bool; 2]> {
        let policy = self.policy;
        if at
            < self
                .online_since?
                .checked_add(policy.lookback_ms + 2 * BUCKET_MS)?
            || at.checked_sub(self.last_trade_source?)? > policy.max_age_ms
        {
            return None;
        }
        let now = self.book_before(at)?;
        if at.checked_sub(now.source_ms)? > policy.max_age_ms {
            return None;
        }
        let pivot = self.book_before(at.checked_sub(policy.recovery_ms)?)?;
        let old = self.book_before(at.checked_sub(policy.lookback_ms)?)?;
        let slot = at / BUCKET_MS;
        let split = slot - policy.recovery_ms / BUCKET_MS;
        let (shock_flow, volume) = self.flow(slot - policy.lookback_ms / BUCKET_MS, split)?;
        let (recovery_flow, _) = self.flow(split, slot)?;
        let shock = (pivot.mid / old.mid - 1.0) * 10_000.0;
        let recovery = (now.mid / pivot.mid - 1.0) * 10_000.0;
        Some([
            volume >= policy.min_shock_notional
                && shock <= -policy.shock_bps
                && recovery >= policy.recovery_bps
                && shock_flow <= -policy.flow_imbalance
                && recovery_flow >= 0.0,
            volume >= policy.min_shock_notional
                && shock >= policy.shock_bps
                && recovery <= -policy.recovery_bps
                && shock_flow >= policy.flow_imbalance
                && recovery_flow <= 0.0,
        ])
    }
}

#[derive(Serialize)]
pub(in crate::strategy::inventory_neutral_mm) struct LocalParticipation {
    policy: LocalReversalFilter,
    pub decisions: u64,
    pub ready_decisions: u64,
    pub admitted_bid_targets: u64,
    pub admitted_ask_targets: u64,
    pub withdrawn_bid_targets: u64,
    pub withdrawn_ask_targets: u64,
    pub held_bid_targets: u64,
    pub held_ask_targets: u64,
    pub rejected_trade_times: u64,
    #[serde(skip)]
    entry_until_ms: [u64; 2],
    #[serde(skip)]
    signal: Arc<PlMutex<LocalSignal>>,
}

impl LocalParticipation {
    pub(super) fn new(policy: LocalReversalFilter, signal: Arc<PlMutex<LocalSignal>>) -> Self {
        Self {
            policy,
            signal,
            decisions: 0,
            ready_decisions: 0,
            admitted_bid_targets: 0,
            admitted_ask_targets: 0,
            withdrawn_bid_targets: 0,
            withdrawn_ask_targets: 0,
            held_bid_targets: 0,
            held_ask_targets: 0,
            rejected_trade_times: 0,
            entry_until_ms: [0; 2],
        }
    }

    fn with_hold(&mut self, at: u64, ready: Option<[bool; 2]>) -> [bool; 2] {
        let Some(allowed) = ready else {
            // A held permit cannot survive missing/freshness-invalid evidence.
            self.entry_until_ms = [0; 2];
            return [false; 2];
        };
        std::array::from_fn(|side| {
            if allowed[side] && at >= self.entry_until_ms[side] {
                self.entry_until_ms[side] = at.saturating_add(self.policy.entry_hold_ms);
            }
            allowed[side] || at < self.entry_until_ms[side]
        })
    }

    pub(in crate::strategy::inventory_neutral_mm) fn apply(
        &mut self,
        target: &mut QuoteTarget,
        positions: [f64; 2],
        config: &InventoryNeutralMMConfig,
    ) {
        self.decisions += 1;
        let at = now_ms();
        let signal = self.signal.lock();
        let ready = signal.allowed(at);
        self.rejected_trade_times = signal.rejected_trade_times;
        drop(signal);
        self.ready_decisions += u64::from(ready.is_some());
        let raw = ready.unwrap_or([false; 2]);
        let [buy, sell] = self.with_hold(at, ready);
        // Both account publications and confirmed fills must agree this side
        // cannot reduce inventory. Ambiguous evidence never suppresses an exit.
        if positions.iter().all(|&p| p >= 0.0)
            && target.bid_size >= components::min_quotable_size(config, target.bid_price)
        {
            if buy {
                self.admitted_bid_targets += 1;
                self.held_bid_targets += u64::from(!raw[0]);
            } else {
                self.withdrawn_bid_targets += 1;
                target.bid_size = 0.0;
            }
        }
        if positions.iter().all(|&p| p <= 0.0)
            && target.ask_size >= components::min_quotable_size(config, target.ask_price)
        {
            if sell {
                self.admitted_ask_targets += 1;
                self.held_ask_targets += u64::from(!raw[1]);
            } else {
                self.withdrawn_ask_targets += 1;
                target.ask_size = 0.0;
            }
        }
    }
}

#[cfg(test)]
mod tests;

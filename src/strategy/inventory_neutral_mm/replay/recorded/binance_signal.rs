//! Offline, receipt-causal risk filter. Binance never supplies quote prices.
use super::*;

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Policy {
    pub enabled: bool,
    pub lookback_ms: u64,
    pub max_age_ms: u64,
    pub shock_bps: f64,
}

impl Policy {
    pub(super) fn validate(self) -> Result<()> {
        ensure!(
            (100..=5_000).contains(&self.lookback_ms)
                && (1..=1_000).contains(&self.max_age_ms)
                && self.shock_bps.is_finite()
                && (0.01..=100.0).contains(&self.shock_bps),
            "invalid Binance risk filter"
        );
        Ok(())
    }
}

pub(super) struct Signal {
    policy: Policy,
    history: VecDeque<(u64, f64, u64)>,
    epoch: u64,
    update_id: u64,
    pub rejected_updates: u64,
    pub resets: u64,
}

impl Signal {
    pub(super) fn new(policy: Policy) -> Self {
        Self {
            policy,
            history: VecDeque::new(),
            epoch: 0,
            update_id: 0,
            rejected_updates: 0,
            resets: 0,
        }
    }

    pub(super) fn update(
        &mut self,
        at: u64,
        observed_ms: u64,
        epoch: u64,
        mid: Option<f64>,
        update_id: Option<u64>,
    ) -> Result<()> {
        ensure!(
            observed_ms <= at
                && epoch > 0
                && mid.is_none_or(valid_price)
                && mid.is_some() == update_id.is_some()
                && update_id != Some(0),
            "invalid Binance reference event"
        );
        if epoch < self.epoch
            || epoch == self.epoch && update_id.is_some_and(|id| id <= self.update_id)
        {
            self.rejected_updates += 1;
            self.history.clear();
            return Ok(());
        }
        if epoch != self.epoch || mid.is_none() {
            self.history.clear();
            if epoch != self.epoch {
                self.update_id = 0;
            }
            self.epoch = epoch;
            self.resets += 1;
        }
        if let (Some(mid), Some(id)) = (mid, update_id) {
            self.update_id = id;
            self.history.push_back((at, mid, observed_ms));
            let oldest = at.saturating_sub(self.policy.lookback_ms + self.policy.max_age_ms);
            while self
                .history
                .front()
                .is_some_and(|&(time, _, _)| time < oldest)
            {
                self.history.pop_front();
            }
            ensure!(
                self.history.len() <= 100_000,
                "Binance reference density exceeds bound"
            );
        }
        Ok(())
    }

    fn allowed(&self, at: u64) -> Option<[bool; 2]> {
        let cutoff = at.checked_sub(self.policy.lookback_ms)?;
        // The newest available sample at or BEFORE each lookup is the only
        // admissible one; later publications cannot repair missing history.
        let &(_, recent, recent_at) = self.history.iter().rev().find(|&&(t, _, _)| t <= at)?;
        let &(_, past, past_at) = self.history.iter().rev().find(|&&(t, _, _)| t <= cutoff)?;
        if at - recent_at > self.policy.max_age_ms || cutoff - past_at > self.policy.max_age_ms {
            return None;
        }
        let change = (recent / past - 1.0) * 10_000.0;
        Some([
            change > -self.policy.shock_bps,
            change < self.policy.shock_bps,
        ])
    }
}

#[derive(Serialize)]
pub(in crate::strategy::inventory_neutral_mm) struct Participation {
    policy: Policy,
    #[serde(skip)]
    signal: Arc<PlMutex<Signal>>,
    decisions: u64,
    fresh_reference_decisions: u64,
    withdrawn_bid_targets: u64,
    withdrawn_ask_targets: u64,
    rejected_updates: u64,
    resets: u64,
}

impl Participation {
    pub(super) fn new(policy: Policy, signal: Arc<PlMutex<Signal>>) -> Self {
        Self {
            policy,
            signal,
            decisions: 0,
            fresh_reference_decisions: 0,
            withdrawn_bid_targets: 0,
            withdrawn_ask_targets: 0,
            rejected_updates: 0,
            resets: 0,
        }
    }

    pub(in crate::strategy::inventory_neutral_mm) fn apply(
        &mut self,
        target: &mut QuoteTarget,
        positions: [f64; 2],
        config: &InventoryNeutralMMConfig,
    ) {
        self.decisions += 1;
        let signal = self.signal.lock();
        let ready = signal.allowed(now_ms());
        self.fresh_reference_decisions += u64::from(ready.is_some());
        self.rejected_updates = signal.rejected_updates;
        self.resets = signal.resets;
        if !self.policy.enabled {
            return;
        }
        let [buy, sell] = ready.unwrap_or([false; 2]);
        // Never suppress a potential reduction when account and fill evidence
        // disagree. Existing execution planning retains pending cancel exposure.
        if !buy
            && positions.iter().all(|&p| p >= 0.0)
            && target.bid_size >= components::min_quotable_size(config, target.bid_price)
        {
            target.bid_size = 0.0;
            self.withdrawn_bid_targets += 1;
        }
        if !sell
            && positions.iter().all(|&p| p <= 0.0)
            && target.ask_size >= components::min_quotable_size(config, target.ask_price)
        {
            target.ask_size = 0.0;
            self.withdrawn_ask_targets += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy() -> Policy {
        Policy {
            enabled: true,
            lookback_ms: 1_000,
            max_age_ms: 500,
            shock_bps: 1.0,
        }
    }

    #[test]
    fn causal_direction_staleness_and_reconnect() {
        let mut s = Signal::new(policy());
        s.update(1_000, 1_000, 1, Some(100.0), Some(10)).unwrap();
        s.update(2_000, 2_000, 1, Some(99.98), Some(99)).unwrap();
        assert_eq!(s.allowed(2_000), Some([false, true]));
        assert_eq!(s.allowed(1_999), None);
        assert_eq!(s.allowed(2_501), None);
        s.update(2_000, 2_000, 1, Some(100.02), Some(100)).unwrap();
        assert_eq!(s.allowed(2_000), Some([true, false]));
        s.update(2_001, 2_001, 2, Some(100.02), Some(1)).unwrap();
        assert_eq!(s.allowed(2_001), None);
        s.update(2_002, 2_002, 1, Some(100.0), Some(200)).unwrap();
        assert_eq!(s.rejected_updates, 1);
        assert_eq!(s.allowed(2_002), None);
    }

    #[tokio::test(start_paused = true)]
    async fn stale_filter_preserves_exits_and_record_only_is_inert() {
        let _clock = ReplayClock::open(12_300, false);
        let config = InventoryNeutralMMConfig::default();
        for enabled in [false, true] {
            let policy = Policy {
                enabled,
                ..policy()
            };
            let mut gate = Participation::new(policy, Arc::new(PlMutex::new(Signal::new(policy))));
            for (positions, expected) in [
                ([0.0, 0.0], [0.0, 0.0]),
                ([1.0, 1.0], [0.0, 1.0]),
                ([-1.0, -1.0], [1.0, 0.0]),
                ([-1.0, 1.0], [1.0, 1.0]),
            ] {
                let mut target = QuoteTarget {
                    bid_price: 100.0,
                    ask_price: 101.0,
                    bid_size: 1.0,
                    ask_size: 1.0,
                };
                gate.apply(&mut target, positions, &config);
                assert_eq!(
                    [target.bid_size, target.ask_size],
                    if enabled { expected } else { [1.0, 1.0] }
                );
                assert_eq!([target.bid_price, target.ask_price], [100.0, 101.0]);
            }
        }
    }
}

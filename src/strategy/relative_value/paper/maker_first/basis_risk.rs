//! Causal, receipt-time basis range used only as a turnover entry risk reserve.
//! It is a bounded research hypothesis, not a calibrated fill-loss forecast.
use super::*;

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BasisRiskReserve {
    pub lookback_ms: u64,
    pub sample_ms: u64,
    pub max_gap_ms: u64,
    #[serde(with = "rust_decimal::serde::str")]
    pub min_reserve_bps: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub range_multiplier: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_reserve_bps: Decimal,
}

impl BasisRiskReserve {
    pub(super) fn validate(self, config: &MakerFirstConfig) -> Result<()> {
        ensure!(
            config.pair.mode.incentive().is_some()
                && config.turnover_feedback.is_some()
                && (1_000..=60_000).contains(&self.lookback_ms)
                && (100..=1_000).contains(&self.sample_ms)
                && (self.sample_ms..=self.lookback_ms).contains(&self.max_gap_ms)
                && (Decimal::ZERO..=Decimal::from(10)).contains(&self.range_multiplier)
                && (config.latency_reserve_bps..=Decimal::from(100))
                    .contains(&self.min_reserve_bps)
                && (self.min_reserve_bps..=Decimal::from(100)).contains(&self.max_reserve_bps),
            "invalid turnover basis risk reserve"
        );
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct BasisRiskReport {
    pub policy: BasisRiskReserve,
    pub fixed_floor_bps: Decimal,
    pub ready_samples: usize,
    /// Historical counter: only resets which discarded a nonempty sample deque.
    pub resets: usize,
    /// Every invalidation call, including repeated invalid books and empty history.
    pub reset_events: usize,
    pub trade_feed_offline_events: usize,
    pub mean_ready_reserve_bps: Option<Decimal>,
    pub max_ready_reserve_bps: Option<Decimal>,
    pub scope: &'static str,
}

#[derive(Default)]
pub(super) struct BasisRiskState {
    samples: VecDeque<(u64, Decimal)>,
    last_identity: Option<[u64; 2]>,
    last_observed_ms: Option<u64>,
    reserve: Option<Decimal>,
    ready_samples: usize,
    resets: usize,
    reset_events: usize,
    trade_feed_offline_events: usize,
    sum: Decimal,
    peak: Option<Decimal>,
}

impl BasisRiskState {
    pub(super) fn reset(&mut self) {
        self.reset_events += 1;
        self.resets += usize::from(!self.samples.is_empty());
        self.samples.clear();
        self.last_identity = None;
        self.last_observed_ms = None;
        self.reserve = None;
    }

    pub(super) fn feed_offline(&mut self) {
        self.trade_feed_offline_events += 1;
        self.reset();
    }

    pub(super) fn observe(
        &mut self,
        policy: BasisRiskReserve,
        floor: Decimal,
        now: u64,
        observation: Option<(Decimal, [u64; 2])>,
    ) {
        let Some((basis, identity)) = observation else {
            self.reset();
            return;
        };
        if self
            .last_observed_ms
            .is_some_and(|at| now.saturating_sub(at) > policy.max_gap_ms)
        {
            self.reset();
        }
        // Repeated snapshots cannot manufacture a full observation window.
        if self.last_identity == Some(identity) {
            return;
        }
        self.last_identity = Some(identity);
        self.last_observed_ms = Some(now);
        if self
            .samples
            .back()
            .is_some_and(|(at, _)| now - at < policy.sample_ms)
        {
            return;
        }
        self.samples.push_back((now, basis));
        let cutoff = now.saturating_sub(policy.lookback_ms);
        // Keep one left-boundary observation, at most max_gap_ms before cutoff.
        while self.samples.get(1).is_some_and(|(at, _)| *at <= cutoff) {
            self.samples.pop_front();
        }
        if now - self.samples[0].0 < policy.lookback_ms {
            self.reserve = None;
            return;
        }
        let (low, high) = self
            .samples
            .iter()
            .fold((basis, basis), |(low, high), (_, v)| {
                (low.min(*v), high.max(*v))
            });
        let reserve = (floor.max(policy.min_reserve_bps) + policy.range_multiplier * (high - low))
            .min(policy.max_reserve_bps);
        self.reserve = Some(reserve);
        self.ready_samples += 1;
        self.sum += reserve;
        self.peak = Some(self.peak.map_or(reserve, |p| p.max(reserve)));
    }

    pub(super) fn value(&self, policy: BasisRiskReserve, now: u64) -> Option<Decimal> {
        self.last_observed_ms
            .filter(|at| now.saturating_sub(*at) <= policy.max_gap_ms)?;
        self.reserve
    }

    pub(super) fn report(self, policy: BasisRiskReserve, floor: Decimal) -> BasisRiskReport {
        BasisRiskReport {
            policy,
            fixed_floor_bps: floor,
            ready_samples: self.ready_samples,
            resets: self.resets,
            reset_events: self.reset_events,
            trade_feed_offline_events: self.trade_feed_offline_events,
            mean_ready_reserve_bps: (self.ready_samples > 0)
                .then(|| self.sum / Decimal::from(self.ready_samples)),
            max_ready_reserve_bps: self.peak,
            scope: "available_book_basis_range_entry_only_not_calibrated_execution_loss",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn d(v: &str) -> Decimal {
        v.parse().unwrap()
    }
    fn policy() -> BasisRiskReserve {
        BasisRiskReserve {
            lookback_ms: 1000,
            sample_ms: 100,
            max_gap_ms: 250,
            min_reserve_bps: d("0.5"),
            range_multiplier: d("0.5"),
            max_reserve_bps: d("2"),
        }
    }
    fn feed(s: &mut BasisRiskState, at: u64, basis: &str) {
        s.observe(policy(), d("0.5"), at, Some((d(basis), [at; 2])));
    }
    #[test]
    fn causal_range_has_warmup_floor_cap_and_expires() {
        let mut s = BasisRiskState::default();
        for at in (1000..2000).step_by(100) {
            feed(&mut s, at, "0");
        }
        assert_eq!(s.value(policy(), 1900), None);
        feed(&mut s, 2000, "0");
        assert_eq!(s.value(policy(), 2000), Some(d("0.5")));
        feed(&mut s, 2100, "2");
        assert_eq!(s.value(policy(), 2100), Some(d("1.5")));
        feed(&mut s, 2200, "100");
        assert_eq!(s.value(policy(), 2200), Some(d("2")));
        assert_eq!(s.value(policy(), 2451), None);
        for at in (2300..=3300).step_by(100) {
            feed(&mut s, at, "0");
        }
        assert_eq!(s.value(policy(), 3300), Some(d("0.5")));
    }
    #[test]
    fn gaps_invalid_books_and_duplicate_snapshots_cannot_warm_up() {
        let mut s = BasisRiskState::default();
        for at in (1000..=2100).step_by(100) {
            s.observe(policy(), d("0.5"), at, Some((d("0"), [1000; 2])));
        }
        assert_eq!(s.value(policy(), 2100), None);
        for at in (2200..=3300).step_by(100) {
            feed(&mut s, at, "0");
        }
        assert!(s.value(policy(), 3300).is_some());
        feed(&mut s, 3600, "0");
        assert_eq!(s.value(policy(), 3600), None);
        for at in (3700..=4700).step_by(100) {
            feed(&mut s, at, "0");
        }
        s.observe(policy(), d("0.5"), 4701, None);
        assert_eq!(s.value(policy(), 4701), None);
    }

    #[test]
    fn offline_events_are_counted_even_before_any_sample() {
        let mut s = BasisRiskState::default();
        s.feed_offline();
        s.feed_offline();
        let report = s.report(policy(), d("0.5"));
        assert_eq!(report.trade_feed_offline_events, 2);
        assert_eq!(report.reset_events, 2);
        assert_eq!(report.resets, 0);
    }
}

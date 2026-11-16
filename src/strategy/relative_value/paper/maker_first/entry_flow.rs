//! Receipt-causal hedge-leg lead-lag shock filter for maker entry quotes.
//! Steps aside on the vulnerable opening side during rapid liquid-venue moves
//! without ever blocking hedges, exits, or terminal drains.
use super::*;
use std::cell::Cell;

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EntryFlowFilter {
    pub lookback_ms: u64,
    pub sample_ms: u64,
    pub max_gap_ms: u64,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_adverse_move_bps: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_range_bps: Decimal,
    #[serde(default)]
    pub cancel_resting_entries: bool,
}

impl EntryFlowFilter {
    pub(super) fn validate(self) -> Result<()> {
        ensure!(
            (200..=60_000).contains(&self.lookback_ms)
                && (50..=1_000).contains(&self.sample_ms)
                && self.sample_ms <= self.lookback_ms
                && (self.sample_ms..=self.lookback_ms.max(2_000)).contains(&self.max_gap_ms)
                && self.max_adverse_move_bps > Decimal::ZERO
                && self.max_adverse_move_bps <= Decimal::from(500)
                && (self.max_adverse_move_bps..=Decimal::from(1_000)).contains(&self.max_range_bps),
            "invalid maker entry flow filter"
        );
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct EntryFlowReport {
    pub policy: EntryFlowFilter,
    pub ready_samples: usize,
    pub resets: usize,
    pub reset_events: usize,
    pub trade_feed_offline_events: usize,
    pub blocked_buy_entries: usize,
    pub blocked_sell_entries: usize,
    pub blocked_range_entries: usize,
    pub cancelled_resting_entries: usize,
    pub scope: &'static str,
}

#[derive(Default)]
pub(super) struct EntryFlowState {
    samples: VecDeque<(u64, Decimal)>,
    last_book_ts_ms: Option<u64>,
    last_observed_ms: Option<u64>,
    ready: Option<(Decimal, Decimal)>,
    ready_samples: usize,
    resets: usize,
    reset_events: usize,
    trade_feed_offline_events: usize,
    pub blocked_buy_entries: Cell<usize>,
    pub blocked_sell_entries: Cell<usize>,
    pub blocked_range_entries: Cell<usize>,
    pub cancelled_resting_entries: usize,
}

impl EntryFlowState {
    pub(super) fn reset(&mut self) {
        self.reset_events += 1;
        self.resets += usize::from(!self.samples.is_empty());
        self.samples.clear();
        self.last_book_ts_ms = None;
        self.last_observed_ms = None;
        self.ready = None;
    }

    pub(super) fn feed_offline(&mut self) {
        self.trade_feed_offline_events += 1;
        self.reset();
    }

    pub(super) fn exceeds_gap(&self, policy: EntryFlowFilter, now: u64) -> bool {
        self.last_observed_ms
            .is_some_and(|at| now.saturating_sub(at) > policy.max_gap_ms)
    }

    pub(super) fn observe(
        &mut self,
        policy: EntryFlowFilter,
        now: u64,
        observation: Option<(Decimal, u64)>,
    ) {
        let Some((mid, book_ts_ms)) = observation else {
            self.reset();
            return;
        };
        if mid <= Decimal::ZERO {
            self.reset();
            return;
        }
        if self
            .last_observed_ms
            .is_some_and(|at| now.saturating_sub(at) > policy.max_gap_ms)
        {
            self.reset();
        }
        // Duplicate hedge snapshots cannot manufacture a warmed-up window.
        if self.last_book_ts_ms == Some(book_ts_ms) {
            return;
        }
        self.last_book_ts_ms = Some(book_ts_ms);
        self.last_observed_ms = Some(now);
        if self
            .samples
            .back()
            .is_some_and(|(at, _)| now.saturating_sub(*at) < policy.sample_ms)
        {
            // Still refresh the latest window metric against the newest mid if warmed up.
            if let Some(last) = self.samples.back_mut() {
                last.1 = mid;
            }
        } else {
            self.samples.push_back((now, mid));
        }
        let cutoff = now.saturating_sub(policy.lookback_ms);
        while self.samples.get(1).is_some_and(|(at, _)| *at <= cutoff) {
            self.samples.pop_front();
        }
        if now.saturating_sub(self.samples[0].0) < policy.lookback_ms {
            self.ready = None;
            return;
        }
        let anchor = self.samples[0].1;
        let (low, high) = self
            .samples
            .iter()
            .fold((mid, mid), |(low, high), (_, px)| {
                (low.min(*px), high.max(*px))
            });
        let move_bps = (mid - anchor) / anchor * BPS;
        let range_bps = (high - low) / mid * BPS;
        self.ready = Some((move_bps, range_bps));
        self.ready_samples += 1;
    }

    pub(super) fn metrics(&self, policy: EntryFlowFilter, now: u64) -> Option<(Decimal, Decimal)> {
        self.last_observed_ms
            .filter(|at| now.saturating_sub(*at) <= policy.max_gap_ms)?;
        self.ready
    }

    pub(super) fn check_entry(
        &self,
        policy: EntryFlowFilter,
        side: HedgeSide,
        now: u64,
    ) -> std::result::Result<(), &'static str> {
        let Some((move_bps, range_bps)) = self.metrics(policy, now) else {
            return Err("entry_flow_warmup_or_gap");
        };
        if range_bps > policy.max_range_bps {
            self.blocked_range_entries
                .set(self.blocked_range_entries.get() + 1);
            return Err("entry_flow_range");
        }
        match side {
            HedgeSide::Buy if move_bps <= -policy.max_adverse_move_bps => {
                self.blocked_buy_entries
                    .set(self.blocked_buy_entries.get() + 1);
                Err("entry_flow_adverse_move")
            }
            HedgeSide::Sell if move_bps >= policy.max_adverse_move_bps => {
                self.blocked_sell_entries
                    .set(self.blocked_sell_entries.get() + 1);
                Err("entry_flow_adverse_move")
            }
            _ => Ok(()),
        }
    }

    pub(super) fn permits_resting(
        &self,
        policy: EntryFlowFilter,
        side: HedgeSide,
        now: u64,
    ) -> bool {
        let Some((move_bps, range_bps)) = self.metrics(policy, now) else {
            return false;
        };
        if range_bps > policy.max_range_bps {
            return false;
        }
        match side {
            HedgeSide::Buy => move_bps > -policy.max_adverse_move_bps,
            HedgeSide::Sell => move_bps < policy.max_adverse_move_bps,
        }
    }

    pub(super) fn report(self, policy: EntryFlowFilter) -> EntryFlowReport {
        EntryFlowReport {
            policy,
            ready_samples: self.ready_samples,
            resets: self.resets,
            reset_events: self.reset_events,
            trade_feed_offline_events: self.trade_feed_offline_events,
            blocked_buy_entries: self.blocked_buy_entries.get(),
            blocked_sell_entries: self.blocked_sell_entries.get(),
            blocked_range_entries: self.blocked_range_entries.get(),
            cancelled_resting_entries: self.cancelled_resting_entries,
            scope: "causal_hedge_leg_momentum_and_range_entry_gate",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(v: &str) -> Decimal {
        v.parse().unwrap()
    }

    fn policy() -> EntryFlowFilter {
        EntryFlowFilter {
            lookback_ms: 1_000,
            sample_ms: 100,
            max_gap_ms: 300,
            max_adverse_move_bps: d("3.0"),
            max_range_bps: d("8.0"),
            cancel_resting_entries: true,
        }
    }

    #[test]
    fn blocks_only_vulnerable_side_on_directional_impulse_and_both_on_range_spike() {
        let mut s = EntryFlowState::default();
        let p = policy();
        for at in (1_000..2_000).step_by(100) {
            s.observe(p, at, Some((d("2000.0"), at)));
        }
        assert_eq!(
            s.check_entry(p, HedgeSide::Buy, 1_900),
            Err("entry_flow_warmup_or_gap")
        );
        s.observe(p, 2_000, Some((d("2000.0"), 2_000)));
        assert_eq!(s.check_entry(p, HedgeSide::Buy, 2_000), Ok(()));
        assert_eq!(s.check_entry(p, HedgeSide::Sell, 2_000), Ok(()));

        // +4 bps up-shock on hedge leg blocks Sell entry, keeps Buy entry open.
        s.observe(p, 2_100, Some((d("2000.8"), 2_100)));
        assert_eq!(
            s.check_entry(p, HedgeSide::Sell, 2_100),
            Err("entry_flow_adverse_move")
        );
        assert_eq!(s.check_entry(p, HedgeSide::Buy, 2_100), Ok(()));

        // Violent whip (+4 bps to -6 bps = 10 bps range) blocks both sides.
        s.observe(p, 2_200, Some((d("1998.8"), 2_200)));
        assert_eq!(
            s.check_entry(p, HedgeSide::Buy, 2_200),
            Err("entry_flow_range")
        );
        assert_eq!(
            s.check_entry(p, HedgeSide::Sell, 2_200),
            Err("entry_flow_range")
        );
    }
}

//! Receipt-causal cross-venue equilibrium basis tracker for `PairHedge` mode.
//! Models conservative mean reversion toward an EWMA equilibrium basis rather
//! than evaluating opening and closing legs on the identical dislocated book.
use super::*;

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MakerBasisReversion {
    pub warmup_ms: u64,
    pub half_life_ms: u64,
    pub max_gap_ms: u64,
    #[serde(with = "rust_decimal::serde::str")]
    pub min_dislocation_bps: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub cancel_dislocation_bps: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub reversion_fraction: Decimal,
    /// Evaluate `min_dislocation_bps` and `cancel_dislocation_bps` at the
    /// candidate/resting quote price vs hedge mid rather than maker mid only.
    #[serde(default)]
    pub use_quote_level_dislocation: bool,
    /// Optional structural-break upper cap on dislocation (`Leung-Li` bounded entry zone).
    #[serde(
        default,
        with = "rust_decimal::serde::str_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_dislocation_bps: Option<Decimal>,
    /// Optional signal persistence window (ms) requiring sustained dislocation before entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_persist_ms: Option<u64>,
}

impl MakerBasisReversion {
    pub(super) fn validate(self, config: &MakerFirstConfig) -> Result<()> {
        ensure!(
            config.pair.mode.is_pair_hedge()
                && config.hedged_exit.is_some()
                && config.passive_offset.is_none()
                && (5_000..=1_800_000).contains(&self.warmup_ms)
                && (5_000..=86_400_000).contains(&self.half_life_ms)
                && (100..=60_000).contains(&self.max_gap_ms)
                && self.max_gap_ms <= self.warmup_ms
                && self.min_dislocation_bps > Decimal::ZERO
                && self.min_dislocation_bps <= Decimal::from(500)
                && (-Decimal::from(500)..=self.min_dislocation_bps)
                    .contains(&self.cancel_dislocation_bps)
                && self
                    .max_dislocation_bps
                    .is_none_or(|max| max > self.min_dislocation_bps && max <= Decimal::from(500))
                && self.min_persist_ms.is_none_or(|ms| ms <= 60_000)
                && (!self.use_quote_level_dislocation
                    || self.min_persist_ms.is_none_or(|ms| ms == 0)
                    || config.opening_max_levels == 1)
                && (Decimal::ZERO..=Decimal::ONE).contains(&self.reversion_fraction),
            "invalid maker basis reversion policy"
        );
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct BasisReversionReport {
    pub policy: MakerBasisReversion,
    pub ready_samples: usize,
    pub resets: usize,
    pub reset_events: usize,
    pub trade_feed_offline_events: usize,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub final_mean_basis_bps: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub max_abs_dislocation_bps: Option<Decimal>,
    pub scope: &'static str,
}

#[derive(Default)]
pub(super) struct BasisReversionState {
    ready_since_ms: Option<u64>,
    last_observed_ms: Option<u64>,
    last_identity: Option<[u64; 2]>,
    mean_basis_bps: Option<Decimal>,
    current_basis_bps: Option<Decimal>,
    persist_since_ms: [Option<u64>; 2],
    ready_samples: usize,
    resets: usize,
    reset_events: usize,
    trade_feed_offline_events: usize,
    max_abs_dislocation_bps: Option<Decimal>,
}

impl BasisReversionState {
    pub(super) fn reset(&mut self) {
        self.reset_events += 1;
        self.resets += usize::from(self.ready_since_ms.is_some());
        self.ready_since_ms = None;
        self.last_observed_ms = None;
        self.last_identity = None;
        self.mean_basis_bps = None;
        self.current_basis_bps = None;
        self.persist_since_ms = [None; 2];
    }

    pub(super) fn feed_offline(&mut self) {
        self.trade_feed_offline_events += 1;
        self.reset();
    }

    pub(super) fn exceeds_gap(&self, policy: MakerBasisReversion, now: u64) -> bool {
        self.last_observed_ms
            .is_some_and(|at| now.saturating_sub(at) > policy.max_gap_ms)
    }

    pub(super) fn observe(
        &mut self,
        policy: MakerBasisReversion,
        now: u64,
        observation: Option<(Decimal, [u64; 2])>,
    ) {
        self.observe_with_quotes(policy, now, observation, None);
    }

    pub(super) fn observe_with_quotes(
        &mut self,
        policy: MakerBasisReversion,
        now: u64,
        observation: Option<(Decimal, [u64; 2])>,
        quote_basis_bps: Option<[Decimal; 2]>,
    ) {
        let Some((basis_bps, identity)) = observation else {
            self.reset();
            return;
        };
        if self
            .last_observed_ms
            .is_some_and(|at| now.saturating_sub(at) > policy.max_gap_ms)
        {
            self.reset();
        }
        // Repeated book snapshots cannot advance warmup or bias the EWMA mean.
        if self.last_identity == Some(identity) {
            return;
        }
        let mean = match (self.mean_basis_bps, self.last_observed_ms) {
            (Some(prev_mean), Some(prev_ms)) => {
                let dt_ms = now.saturating_sub(prev_ms) as f64;
                let half_life = policy.half_life_ms as f64;
                let weight =
                    (1.0 - (-std::f64::consts::LN_2 * dt_ms / half_life).exp()).clamp(0.0, 1.0);
                let alpha = Decimal::from_f64_retain(weight)
                    .unwrap_or(Decimal::ZERO)
                    .clamp(Decimal::ZERO, Decimal::ONE);
                prev_mean + alpha * (basis_bps - prev_mean)
            }
            _ => {
                self.ready_since_ms = Some(now);
                basis_bps
            }
        };
        self.last_identity = Some(identity);
        self.last_observed_ms = Some(now);
        self.mean_basis_bps = Some(mean);
        self.current_basis_bps = Some(basis_bps);
        if self
            .ready_since_ms
            .is_some_and(|since| now.saturating_sub(since) >= policy.warmup_ms)
        {
            self.ready_samples += 1;
            let abs_dev = (basis_bps - mean).abs();
            self.max_abs_dislocation_bps = Some(
                self.max_abs_dislocation_bps
                    .map_or(abs_dev, |prev| prev.max(abs_dev)),
            );
            let persist_dislocs = if policy.use_quote_level_dislocation {
                quote_basis_bps.map(|[buy_q_bps, sell_q_bps]| [mean - buy_q_bps, sell_q_bps - mean])
            } else {
                Some([mean - basis_bps, basis_bps - mean])
            };
            if let Some(sides) = persist_dislocs {
                for (idx, side_disloc) in sides.into_iter().enumerate() {
                    if side_disloc >= policy.min_dislocation_bps {
                        self.persist_since_ms[idx].get_or_insert(now);
                    } else {
                        self.persist_since_ms[idx] = None;
                    }
                }
            } else {
                self.persist_since_ms = [None; 2];
            }
        } else {
            self.persist_since_ms = [None; 2];
        }
    }

    pub(super) fn mid_dislocation_bps(
        &self,
        policy: MakerBasisReversion,
        side: HedgeSide,
        now: u64,
    ) -> Option<Decimal> {
        let last = self.last_observed_ms?;
        let since = self.ready_since_ms?;
        if now.saturating_sub(last) > policy.max_gap_ms
            || now.saturating_sub(since) < policy.warmup_ms
        {
            return None;
        }
        let mean = self.mean_basis_bps?;
        let current = self.current_basis_bps?;
        Some(match side {
            HedgeSide::Buy => mean - current,
            HedgeSide::Sell => current - mean,
        })
    }

    fn signed_quote_dislocation(
        &self,
        policy: MakerBasisReversion,
        side: HedgeSide,
        quote_basis_bps: Option<Decimal>,
        now: u64,
    ) -> Option<Decimal> {
        let last = self.last_observed_ms?;
        let since = self.ready_since_ms?;
        if now.saturating_sub(last) > policy.max_gap_ms
            || now.saturating_sub(since) < policy.warmup_ms
        {
            return None;
        }
        let mean = self.mean_basis_bps?;
        let basis = if policy.use_quote_level_dislocation {
            quote_basis_bps?
        } else {
            self.current_basis_bps?
        };
        Some(match side {
            HedgeSide::Buy => mean - basis,
            HedgeSide::Sell => basis - mean,
        })
    }

    pub(super) fn favorable_dislocation_bps(
        &self,
        policy: MakerBasisReversion,
        side: HedgeSide,
        now: u64,
    ) -> std::result::Result<Decimal, &'static str> {
        self.favorable_quote_dislocation_bps(policy, side, None, now)
    }

    pub(super) fn favorable_quote_dislocation_bps(
        &self,
        policy: MakerBasisReversion,
        side: HedgeSide,
        quote_basis_bps: Option<Decimal>,
        now: u64,
    ) -> std::result::Result<Decimal, &'static str> {
        let Some(dislocation) = self.signed_quote_dislocation(policy, side, quote_basis_bps, now)
        else {
            return Err("basis_reversion_warmup_or_gap");
        };
        if dislocation < policy.min_dislocation_bps {
            return Err("basis_dislocation");
        }
        if policy
            .max_dislocation_bps
            .is_some_and(|max| dislocation > max)
        {
            return Err("basis_dislocation_above_max");
        }
        if let Some(req_ms) = policy.min_persist_ms
            && req_ms > 0
        {
            let side_idx = match side {
                HedgeSide::Buy => 0,
                HedgeSide::Sell => 1,
            };
            let persisted = self.persist_since_ms[side_idx]
                .is_some_and(|since| now.saturating_sub(since) >= req_ms);
            if !persisted {
                return Err("basis_dislocation_persist");
            }
        }
        Ok(dislocation)
    }

    pub(super) fn resting_quote_valid(
        &self,
        policy: MakerBasisReversion,
        side: HedgeSide,
        quote_basis_bps: Option<Decimal>,
        now: u64,
    ) -> bool {
        self.signed_quote_dislocation(policy, side, quote_basis_bps, now)
            .is_some_and(|dislocation| dislocation >= policy.cancel_dislocation_bps)
    }

    #[cfg(test)]
    pub(super) fn resting_entry_valid(
        &self,
        policy: MakerBasisReversion,
        side: HedgeSide,
        now: u64,
    ) -> bool {
        self.resting_quote_valid(policy, side, None, now)
    }

    pub(super) fn report(self, policy: MakerBasisReversion) -> BasisReversionReport {
        BasisReversionReport {
            policy,
            ready_samples: self.ready_samples,
            resets: self.resets,
            reset_events: self.reset_events,
            trade_feed_offline_events: self.trade_feed_offline_events,
            final_mean_basis_bps: self.mean_basis_bps.map(|v| v.round_dp(6)),
            max_abs_dislocation_bps: self.max_abs_dislocation_bps.map(|v| v.round_dp(6)),
            scope: "causal_synchronized_ewma_basis_reversion_hypothesis",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(v: &str) -> Decimal {
        v.parse().unwrap()
    }

    fn policy() -> MakerBasisReversion {
        MakerBasisReversion {
            warmup_ms: 5_000,
            half_life_ms: 10_000,
            max_gap_ms: 1_000,
            min_dislocation_bps: d("3.0"),
            cancel_dislocation_bps: d("0.5"),
            reversion_fraction: d("0.6"),
            use_quote_level_dislocation: false,
            max_dislocation_bps: None,
            min_persist_ms: None,
        }
    }

    #[test]
    fn requires_warmup_rejects_duplicate_identities_and_detects_directional_dislocation() {
        let mut s = BasisReversionState::default();
        let p = policy();
        for at in (1_000..=6_500).step_by(500) {
            s.observe(p, at, Some((d("1.0"), [1_000; 2])));
        }
        assert_eq!(
            s.favorable_dislocation_bps(p, HedgeSide::Buy, 6_500),
            Err("basis_reversion_warmup_or_gap")
        );

        let mut s = BasisReversionState::default();
        for at in (1_000..=6_000).step_by(500) {
            s.observe(p, at, Some((d("1.0"), [at; 2])));
        }
        assert_eq!(
            s.favorable_dislocation_bps(p, HedgeSide::Buy, 6_000),
            Err("basis_dislocation")
        );
        s.observe(p, 6_100, Some((d("-4.0"), [6_100; 2])));
        let buy_disloc = s
            .favorable_dislocation_bps(p, HedgeSide::Buy, 6_100)
            .expect("cheap maker admits buy");
        assert!(buy_disloc > d("4.5"));
        assert_eq!(
            s.favorable_dislocation_bps(p, HedgeSide::Sell, 6_100),
            Err("basis_dislocation")
        );
        assert!(s.resting_entry_valid(p, HedgeSide::Buy, 6_100));

        // Gap beyond max_gap_ms invalidates warmup.
        assert_eq!(
            s.favorable_dislocation_bps(p, HedgeSide::Buy, 7_200),
            Err("basis_reversion_warmup_or_gap")
        );
        assert!(!s.exceeds_gap(p, 7_100));
        assert!(s.exceeds_gap(p, 7_101));
    }

    #[test]
    fn supports_long_slow_equilibrium_leung_li_cap_and_persistence_dip_reset() {
        let legacy_json = r#"{
            "warmup_ms": 5000,
            "half_life_ms": 14400000,
            "max_gap_ms": 1000,
            "min_dislocation_bps": "3.0",
            "cancel_dislocation_bps": "0.5",
            "reversion_fraction": "0.6"
        }"#;
        let legacy: MakerBasisReversion = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(legacy.half_life_ms, 14_400_000);
        assert_eq!(legacy.max_dislocation_bps, None);
        assert_eq!(legacy.min_persist_ms, None);

        let guarded = MakerBasisReversion {
            half_life_ms: 14_400_000, // 4h slow equilibrium
            max_dislocation_bps: Some(d("25.0")),
            min_persist_ms: Some(300),
            ..legacy
        };
        let mut s = BasisReversionState::default();
        for at in (1_000..=6_000).step_by(500) {
            s.observe(guarded, at, Some((d("0.0"), [at; 2])));
        }

        // First sample exceeding min_dislocation starts persistence clock, but < 300ms fails.
        s.observe(guarded, 6_100, Some((d("-5.0"), [6_100; 2])));
        assert_eq!(
            s.favorable_dislocation_bps(guarded, HedgeSide::Buy, 6_100),
            Err("basis_dislocation_persist")
        );
        // Dip below threshold clears persistence clock.
        s.observe(guarded, 6_250, Some((d("-1.0"), [6_250; 2])));
        s.observe(guarded, 6_450, Some((d("-5.0"), [6_450; 2])));
        assert_eq!(
            s.favorable_dislocation_bps(guarded, HedgeSide::Buy, 6_450),
            Err("basis_dislocation_persist")
        );
        // Sustained >= 300ms after re-arming passes.
        s.observe(guarded, 6_750, Some((d("-5.0"), [6_750; 2])));
        assert!(
            s.favorable_dislocation_bps(guarded, HedgeSide::Buy, 6_750)
                .is_ok()
        );
        // Structural break beyond max_dislocation_bps fails closed (`Leung-Li` bounded entry).
        s.observe(guarded, 6_900, Some((d("-40.0"), [6_900; 2])));
        assert_eq!(
            s.favorable_dislocation_bps(guarded, HedgeSide::Buy, 6_900),
            Err("basis_dislocation_above_max")
        );
    }

    #[test]
    fn quote_level_persistence_uses_quote_series_not_mid_series() {
        let q_policy = MakerBasisReversion {
            use_quote_level_dislocation: true,
            min_persist_ms: Some(200),
            ..policy()
        };
        let mut s = BasisReversionState::default();
        for at in (1_000..=6_000).step_by(500) {
            s.observe_with_quotes(
                q_policy,
                at,
                Some((d("0.0"), [at; 2])),
                Some([d("0.0"), d("0.0")]),
            );
        }

        // Mid basis is -5 bps, but candidate buy quote basis is only -1 bps (< 3 bps): must NOT arm persistence!
        s.observe_with_quotes(
            q_policy,
            6_100,
            Some((d("-5.0"), [6_100; 2])),
            Some([d("-1.0"), d("-1.0")]),
        );
        s.observe_with_quotes(
            q_policy,
            6_350,
            Some((d("-5.0"), [6_350; 2])),
            Some([d("-5.0"), d("-1.0")]),
        );
        assert_eq!(
            s.favorable_quote_dislocation_bps(q_policy, HedgeSide::Buy, Some(d("-5.0")), 6_350),
            Err("basis_dislocation_persist")
        );

        // Once quote dislocation sustains >= 200ms (even if mid is neutral 0 bps), persistence passes.
        s.observe_with_quotes(
            q_policy,
            6_600,
            Some((d("0.0"), [6_600; 2])),
            Some([d("-5.0"), d("0.0")]),
        );
        assert!(
            s.favorable_quote_dislocation_bps(q_policy, HedgeSide::Buy, Some(d("-5.0")), 6_600)
                .is_ok()
        );
    }
}

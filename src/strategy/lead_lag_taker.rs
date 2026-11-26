//! Track 01 (`01_quant_profit`) venue-decoupled cross-venue lead-lag taker state machine.
//!
//! A fast reference book (e.g. a large CEX for crypto, or a fast WS venue for RWA perps)
//! often leads a slower or speed-bumped target venue. With
//!
//! ```text
//! x_t   = ln(ref_mid_t) - ln(tgt_mid_t)
//! dev_t = (x_t - EMA_tau(x)_{t-})  * 1e4      [bps, strictly causal: EMA excludes x_t]
//! ```
//!
//! a positive `dev` means the reference is rich versus its recent relation to the target,
//! so the target is expected to rise (enter `Long`), and vice versa. Positions never
//! overlap, are closed after `hold_ms` measured from the venue-delayed fill time, and new
//! entries are blocked on stale books, warm-up, or structural-break-sized deviations.
//!
//! This module only *decides*; it never submits orders and has zero exchange-adapter
//! coupling. Execution adapters must still reserve exposure before dispatch, apply tick /
//! lot rounding and reduce-only direction checks at the venue boundary (`AGENTS.md`).
//! The research replay that motivates the defaults is `scripts/research_lead_lag_quant.py`.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// Direction of a lead-lag position on the target venue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum LeadLagSide {
    Long,
    Short,
}

impl LeadLagSide {
    pub fn sign(self) -> f64 {
        match self {
            Self::Long => 1.0,
            Self::Short => -1.0,
        }
    }
}

/// Reason an observation produced no new entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum LeadLagBlock {
    InvalidInput,
    WarmingUp,
    StaleReference,
    StaleTarget,
    StructuralBreak,
}

/// Exit execution mode after entering a lead-lag position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LeadLagExitMode {
    /// Hold for `hold_ms` then cross the spread (`Exit`) — preserves historical default.
    #[default]
    TakerOnly,
    /// Immediately rest a passive `reduce_only` exit (`QuotePassiveExit`) after `min_passive_ms`
    /// and fall back to a taker `Exit` at `exit_due_ms` if still unfilled.
    PassiveTouchThenTaker { min_passive_ms: u64 },
}

/// Decision emitted for one synchronized observation.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub enum LeadLagAction {
    /// No signal, or already positioned before passive exit begins (or holding TakerOnly).
    Idle,
    /// Cross the target book in `side` direction (taker).
    Enter { side: LeadLagSide, dev_bps: f64 },
    /// Post or maintain a passive `reduce_only` exit order opposite `side`.
    QuotePassiveExit { side: LeadLagSide },
    /// Close the open position (taker) because its hold timer expired.
    Exit { side: LeadLagSide },
    /// Entry suppressed; an open position (if any) is kept until books are fresh again.
    Blocked(LeadLagBlock),
}

/// Configuration for [`LeadLagTaker`].
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LeadLagTakerConfig {
    /// Absolute deviation required to enter, in bps (`> 0`).
    pub theta_bps: f64,
    /// EMA time constant of the reference/target log-ratio, in ms (`> 0`).
    pub tau_ms: u64,
    /// Holding time after the (delayed) entry fill, in ms (`> 0`).
    pub hold_ms: u64,
    /// Expected taker execution delay on the target venue (speed bump + RTT), in ms.
    pub taker_delay_ms: u64,
    /// Maximum reference book age at decision time, in ms (`> 0`).
    pub max_ref_age_ms: u64,
    /// Maximum target book age at decision time, in ms (`> 0`).
    pub max_tgt_age_ms: u64,
    /// Deviations at or above this cap are treated as bad data / regime breaks (`> theta`).
    pub max_dev_bps: f64,
    /// Observation time required after the first sample before entries are allowed, in ms.
    pub warmup_ms: u64,
    /// Gap after which the EMA is re-seeded instead of decayed, in ms (`> 0`).
    pub max_gap_ms: u64,
    /// Exit execution mode after entry (`TakerOnly` by default).
    pub exit_mode: LeadLagExitMode,
    /// Optional multiplier on causal target 1s log-return sigma (`bps/sqrt(s)`):
    /// `effective_theta_bps = max(theta_bps, vol_theta_mult * sigma_1s_bps)` when `> 0.0`.
    pub vol_theta_mult: f64,
    /// EWMA time constant in ms for the causal target 1s log-return variance (`> 0`).
    pub vol_ewma_tau_ms: u64,
    /// When `true`, [`LeadLagTaker::observe_with_confirm`] fails closed on entries:
    /// entry requires a fresh, warmed confirmation leader observation that agrees on direction.
    /// Missing or stale confirmation observations suppress new entries.
    pub require_dual_leader_confirm: bool,
    /// Optional fraction in `(0.0, 1.0]` of the entry lead gap that, once closed by the target
    /// after `entry_filled_est_ms`, triggers an early profit-protecting `Exit`.
    pub exit_on_catchup_frac: Option<f64>,
}

impl Default for LeadLagTakerConfig {
    fn default() -> Self {
        // W19-frozen `lighter_rh ETH <- binance` arm: the only arm whose print-verified
        // walk-forward stayed positive (17/21 windows, t = 3.4; see
        // docs/TRACK01_TRACK02_LEAD_LAG_AND_MAKER_HEDGE_2026_11_26.md). Research
        // hypothesis only: live use stays behind HOLD and execution-boundary checks.
        Self {
            theta_bps: 6.0,
            tau_ms: 120_000,
            hold_ms: 10_000,
            taker_delay_ms: 230,
            max_ref_age_ms: 5_000,
            max_tgt_age_ms: 5_000,
            max_dev_bps: 60.0,
            warmup_ms: 60_000,
            max_gap_ms: 30_000,
            exit_mode: LeadLagExitMode::TakerOnly,
            vol_theta_mult: 0.0,
            vol_ewma_tau_ms: 15_000,
            require_dual_leader_confirm: false,
            exit_on_catchup_frac: None,
        }
    }
}

impl LeadLagTakerConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.theta_bps.is_finite() && self.theta_bps > 0.0,
            "theta_bps must be finite and > 0"
        );
        ensure!(
            self.max_dev_bps.is_finite() && self.max_dev_bps > self.theta_bps,
            "max_dev_bps must be finite and > theta_bps"
        );
        ensure!(self.tau_ms > 0, "tau_ms must be > 0");
        ensure!(self.hold_ms > 0, "hold_ms must be > 0");
        ensure!(
            self.max_ref_age_ms > 0 && self.max_tgt_age_ms > 0,
            "book age limits must be > 0"
        );
        ensure!(self.max_gap_ms > 0, "max_gap_ms must be > 0");
        ensure!(
            self.vol_theta_mult.is_finite() && self.vol_theta_mult >= 0.0,
            "vol_theta_mult must be finite and >= 0"
        );
        ensure!(self.vol_ewma_tau_ms > 0, "vol_ewma_tau_ms must be > 0");
        if let Some(frac) = self.exit_on_catchup_frac {
            ensure!(
                frac.is_finite() && frac > 0.0 && frac <= 1.0,
                "exit_on_catchup_frac must be finite and in (0.0, 1.0]"
            );
        }
        if let LeadLagExitMode::PassiveTouchThenTaker { min_passive_ms } = self.exit_mode {
            ensure!(
                min_passive_ms <= self.hold_ms,
                "min_passive_ms must be <= hold_ms"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct OpenPosition {
    side: LeadLagSide,
    entry_filled_est_ms: u64,
    exit_due_ms: u64,
    entry_tgt_mid: f64,
    entry_gap_bps: f64,
    exit_triggered: bool,
}

/// Causal lead-lag taker decision engine (single target market).
#[derive(Debug, Clone)]
pub struct LeadLagTaker {
    cfg: LeadLagTakerConfig,
    ema: Option<f64>,
    first_obs_ms: Option<u64>,
    last_obs_ms: Option<u64>,
    confirm_ema: Option<f64>,
    confirm_last_obs_ms: Option<u64>,
    tgt_vol_last: Option<(u64, f64)>,
    tgt_var_bps2_per_s: f64,
    position: Option<OpenPosition>,
    last_dev_bps: Option<f64>,
}

impl LeadLagTaker {
    pub fn new(cfg: LeadLagTakerConfig) -> Result<Self> {
        cfg.validate()?;
        Ok(Self {
            cfg,
            ema: None,
            first_obs_ms: None,
            last_obs_ms: None,
            confirm_ema: None,
            confirm_last_obs_ms: None,
            tgt_vol_last: None,
            tgt_var_bps2_per_s: 0.0,
            position: None,
            last_dev_bps: None,
        })
    }

    pub fn config(&self) -> &LeadLagTakerConfig {
        &self.cfg
    }

    pub fn position(&self) -> Option<LeadLagSide> {
        self.position.map(|p| p.side)
    }

    pub fn last_dev_bps(&self) -> Option<f64> {
        self.last_dev_bps
    }

    /// Current causal EWMA 1s log-return volatility of the target mid, in bps/sqrt(s).
    pub fn sigma_1s_bps(&self) -> f64 {
        self.tgt_var_bps2_per_s.max(0.0).sqrt()
    }

    /// Current effective entry threshold in bps (`max(theta_bps, vol_theta_mult * sigma_1s_bps)`).
    pub fn effective_theta_bps(&self) -> f64 {
        if self.cfg.vol_theta_mult > 0.0 {
            self.cfg
                .theta_bps
                .max(self.cfg.vol_theta_mult * self.sigma_1s_bps())
        } else {
            self.cfg.theta_bps
        }
    }

    /// Forget an open position after the execution layer reports it flat (e.g. after a
    /// rejected entry). Never call this on a timeout alone.
    pub fn clear_position(&mut self) {
        self.position = None;
    }

    /// Close the open position cleanly when the execution layer confirms the `reduce_only`
    /// passive exit order filled before `exit_due_ms`.
    pub fn notify_passive_exit_filled(&mut self) {
        self.position = None;
    }

    /// Confirm an authoritative exit fill so risk is released only once execution reports flat.
    ///
    /// Timeout or catch-up `Exit` actions retain `self.position` until execution confirms the
    /// position is flat (`AGENTS.md`: never release risk on a timeout alone).
    pub fn notify_exit_filled(&mut self) {
        self.position = None;
    }

    /// Process one synchronized observation. `*_recv_ms` are local receipt times of the
    /// books used (never venue source times), `now_ms` is the local decision clock.
    pub fn observe(
        &mut self,
        now_ms: u64,
        ref_mid: f64,
        ref_recv_ms: u64,
        tgt_mid: f64,
        tgt_recv_ms: u64,
    ) -> LeadLagAction {
        self.observe_with_confirm(now_ms, (ref_mid, ref_recv_ms), None, (tgt_mid, tgt_recv_ms))
    }

    /// Process one synchronized observation with an optional secondary confirmation leader
    /// `(confirm_ref_mid, confirm_ref_recv_ms)`.
    ///
    /// When `require_dual_leader_confirm` is enabled and `confirm_ref_obs` is provided and fresh,
    /// both primary and confirm leader impulses vs target must agree on direction before opening.
    pub fn observe_with_confirm(
        &mut self,
        now_ms: u64,
        ref_obs: (f64, u64),
        confirm_ref_obs: Option<(f64, u64)>,
        tgt_obs: (f64, u64),
    ) -> LeadLagAction {
        let (ref_mid, ref_recv_ms) = ref_obs;
        let (tgt_mid, tgt_recv_ms) = tgt_obs;
        if !(ref_mid.is_finite() && ref_mid > 0.0 && tgt_mid.is_finite() && tgt_mid > 0.0)
            || ref_recv_ms > now_ms
            || tgt_recv_ms > now_ms
        {
            return LeadLagAction::Blocked(LeadLagBlock::InvalidInput);
        }
        if let Some((c_mid, c_recv_ms)) = confirm_ref_obs
            && (!(c_mid.is_finite() && c_mid > 0.0) || c_recv_ms > now_ms)
        {
            return LeadLagAction::Blocked(LeadLagBlock::InvalidInput);
        }
        if self.last_obs_ms.is_some_and(|last| now_ms < last)
            || self.tgt_vol_last.is_some_and(|(last, _)| now_ms < last)
            || self.confirm_last_obs_ms.is_some_and(|last| now_ms < last)
        {
            return LeadLagAction::Blocked(LeadLagBlock::InvalidInput);
        }

        let ref_stale = now_ms.saturating_sub(ref_recv_ms) > self.cfg.max_ref_age_ms;
        let tgt_stale = now_ms.saturating_sub(tgt_recv_ms) > self.cfg.max_tgt_age_ms;
        let confirm_fresh_obs = confirm_ref_obs
            .filter(|&(_, c_recv_ms)| now_ms.saturating_sub(c_recv_ms) <= self.cfg.max_ref_age_ms);

        // Track causal EWMA 1s log-return variance on fresh target observations only.
        if !tgt_stale {
            if let Some((prev_ms, prev_mid)) = self.tgt_vol_last {
                let dt_ms = now_ms - prev_ms;
                if dt_ms > self.cfg.max_gap_ms {
                    self.tgt_var_bps2_per_s = 0.0;
                } else if dt_ms > 0 && prev_mid > 0.0 {
                    let dt_s = dt_ms as f64 / 1000.0;
                    let ret_bps = (tgt_mid / prev_mid).ln() * 1e4;
                    let inst_var = (ret_bps * ret_bps) / dt_s;
                    if inst_var.is_finite() {
                        let alpha = 1.0 - (-(dt_ms as f64) / self.cfg.vol_ewma_tau_ms as f64).exp();
                        self.tgt_var_bps2_per_s =
                            (1.0 - alpha) * self.tgt_var_bps2_per_s + alpha * inst_var;
                    }
                }
            }
            self.tgt_vol_last = Some((now_ms, tgt_mid));
        }

        // Only fold fresh paired observations into the EMA and observation timestamps,
        // preventing stale feeds from poisoning the EMA or masking `max_gap_ms` re-seeding.
        let dev_bps = if !ref_stale && !tgt_stale {
            let x = ref_mid.ln() - tgt_mid.ln();
            // Strictly causal deviation against the prior EMA, then update.
            let gap = self.last_obs_ms.map(|l| now_ms - l);
            let prior = match (self.ema, gap) {
                (Some(e), Some(g)) if g <= self.cfg.max_gap_ms => Some(e),
                _ => None,
            };
            let dev_bps = prior.map(|e| (x - e) * 1e4);
            self.ema = Some(match (prior, gap) {
                (Some(e), Some(g)) => {
                    let alpha = 1.0 - (-(g as f64) / self.cfg.tau_ms as f64).exp();
                    e + alpha * (x - e)
                }
                _ => {
                    // (Re)seed after a gap: restart warm-up so no entry uses a cold EMA.
                    self.first_obs_ms = Some(now_ms);
                    x
                }
            });
            if self.first_obs_ms.is_none() {
                self.first_obs_ms = Some(now_ms);
            }
            self.last_obs_ms = Some(now_ms);
            self.last_dev_bps = dev_bps;
            dev_bps
        } else {
            None
        };

        // Update optional secondary leader EMA strictly causally when both confirm and tgt are fresh.
        let confirm_dev_bps = if !tgt_stale {
            if let Some((c_mid, _)) = confirm_fresh_obs {
                let x_c = c_mid.ln() - tgt_mid.ln();
                let gap_c = self.confirm_last_obs_ms.map(|l| now_ms - l);
                let prior_c = match (self.confirm_ema, gap_c) {
                    (Some(e), Some(g)) if g <= self.cfg.max_gap_ms => Some(e),
                    _ => None,
                };
                let c_dev = prior_c.map(|e| (x_c - e) * 1e4);
                self.confirm_ema = Some(match (prior_c, gap_c) {
                    (Some(e), Some(g)) => {
                        let alpha = 1.0 - (-(g as f64) / self.cfg.tau_ms as f64).exp();
                        e + alpha * (x_c - e)
                    }
                    _ => x_c,
                });
                self.confirm_last_obs_ms = Some(now_ms);
                c_dev
            } else {
                None
            }
        } else {
            None
        };

        if let Some(mut pos) = self.position {
            if pos.exit_triggered || now_ms >= pos.exit_due_ms {
                if tgt_stale {
                    return LeadLagAction::Blocked(LeadLagBlock::StaleTarget);
                }
                pos.exit_triggered = true;
                self.position = Some(pos);
                // Retain `self.position` until execution confirms flat via `notify_exit_filled()`.
                return LeadLagAction::Exit { side: pos.side };
            }
            if let Some(frac) = self.cfg.exit_on_catchup_frac
                && !tgt_stale
                && now_ms >= pos.entry_filled_est_ms
                && pos.entry_tgt_mid > 0.0
                && pos.entry_gap_bps > 0.0
            {
                let catchup_bps = pos.side.sign() * (tgt_mid / pos.entry_tgt_mid).ln() * 1e4;
                if catchup_bps.is_finite() && catchup_bps >= frac * pos.entry_gap_bps {
                    pos.exit_triggered = true;
                    self.position = Some(pos);
                    return LeadLagAction::Exit { side: pos.side };
                }
            }
            if let LeadLagExitMode::PassiveTouchThenTaker { min_passive_ms } = self.cfg.exit_mode
                && now_ms >= pos.entry_filled_est_ms.saturating_add(min_passive_ms)
            {
                if tgt_stale {
                    return LeadLagAction::Blocked(LeadLagBlock::StaleTarget);
                }
                return LeadLagAction::QuotePassiveExit { side: pos.side };
            }
            return LeadLagAction::Idle;
        }
        if ref_stale {
            return LeadLagAction::Blocked(LeadLagBlock::StaleReference);
        }
        if tgt_stale {
            return LeadLagAction::Blocked(LeadLagBlock::StaleTarget);
        }
        let Some(dev) = dev_bps else {
            return LeadLagAction::Blocked(LeadLagBlock::WarmingUp);
        };
        let warm_since = self.first_obs_ms.unwrap_or(now_ms);
        if now_ms.saturating_sub(warm_since) < self.cfg.warmup_ms {
            return LeadLagAction::Blocked(LeadLagBlock::WarmingUp);
        }
        if dev.abs() >= self.cfg.max_dev_bps {
            return LeadLagAction::Blocked(LeadLagBlock::StructuralBreak);
        }
        if dev.abs() < self.effective_theta_bps() {
            return LeadLagAction::Idle;
        }
        let side = if dev > 0.0 {
            LeadLagSide::Long
        } else {
            LeadLagSide::Short
        };
        if self.cfg.require_dual_leader_confirm {
            match (confirm_fresh_obs, confirm_dev_bps) {
                (Some(_), Some(c_dev))
                    if c_dev.abs() < self.cfg.max_dev_bps && side.sign() * c_dev > 0.0 => {}
                _ => return LeadLagAction::Idle,
            }
        }
        let entry_filled_est_ms = now_ms.saturating_add(self.cfg.taker_delay_ms);
        self.position = Some(OpenPosition {
            side,
            entry_filled_est_ms,
            exit_due_ms: entry_filled_est_ms.saturating_add(self.cfg.hold_ms),
            entry_tgt_mid: tgt_mid,
            entry_gap_bps: dev.abs(),
            exit_triggered: false,
        });
        LeadLagAction::Enter { side, dev_bps: dev }
    }
}

/// Net per-trade edge of a lead-lag round trip from touch prices, in bps.
pub fn round_trip_net_bps(
    side: LeadLagSide,
    entry_px: f64,
    exit_px: f64,
    taker_fee_bps_per_side: f64,
) -> Option<f64> {
    if !(entry_px.is_finite() && entry_px > 0.0 && exit_px.is_finite() && exit_px > 0.0)
        || !taker_fee_bps_per_side.is_finite()
    {
        return None;
    }
    Some(side.sign() * (exit_px / entry_px - 1.0) * 1e4 - 2.0 * taker_fee_bps_per_side)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> LeadLagTakerConfig {
        LeadLagTakerConfig {
            theta_bps: 3.0,
            tau_ms: 2_000,
            hold_ms: 1_000,
            taker_delay_ms: 300,
            warmup_ms: 1_000,
            ..LeadLagTakerConfig::default()
        }
    }

    fn warm(engine: &mut LeadLagTaker, start: u64, steps: u64) -> u64 {
        let mut t = start;
        for _ in 0..steps {
            assert!(!matches!(
                engine.observe(t, 100.0, t, 100.0, t),
                LeadLagAction::Enter { .. }
            ));
            t += 100;
        }
        t
    }

    #[test]
    fn enters_long_when_reference_leads_up_and_exits_after_delay_plus_hold() {
        let mut e = LeadLagTaker::new(cfg()).unwrap();
        let t = warm(&mut e, 1_000, 20);
        // Reference jumps +5 bp, target unchanged -> dev ~ +5 bp >= 3 -> Long.
        let a = e.observe(t, 100.05, t, 100.0, t);
        let LeadLagAction::Enter { side, dev_bps } = a else {
            panic!("expected entry, got {a:?}")
        };
        assert_eq!(side, LeadLagSide::Long);
        assert!((dev_bps - 5.0).abs() < 0.05, "dev {dev_bps}");
        // Positioned: no new entry before delay + hold (1_300 ms).
        assert_eq!(
            e.observe(t + 1_200, 100.1, t + 1_200, 100.0, t + 1_200),
            LeadLagAction::Idle
        );
        assert_eq!(
            e.observe(t + 1_300, 100.1, t + 1_300, 100.05, t + 1_300),
            LeadLagAction::Exit {
                side: LeadLagSide::Long
            }
        );
        assert_eq!(e.position(), Some(LeadLagSide::Long));
        e.notify_exit_filled();
        assert_eq!(e.position(), None);
    }

    #[test]
    fn short_on_negative_deviation_and_blocks_on_stale_or_breaks() {
        let mut e = LeadLagTaker::new(cfg()).unwrap();
        let t = warm(&mut e, 10_000, 20);
        // Stale reference blocks entries even with a large signal.
        assert_eq!(
            e.observe(t, 99.95, t - 6_000, 100.0, t),
            LeadLagAction::Blocked(LeadLagBlock::StaleReference)
        );
        let mut e = LeadLagTaker::new(cfg()).unwrap();
        let t = warm(&mut e, 1_000, 20);
        // 100 bp jump exceeds max_dev_bps (60) -> structural break, not a trade.
        assert_eq!(
            e.observe(t, 101.0, t, 100.0, t),
            LeadLagAction::Blocked(LeadLagBlock::StructuralBreak)
        );
        let mut e = LeadLagTaker::new(cfg()).unwrap();
        let t = warm(&mut e, 1_000, 20);
        assert!(matches!(
            e.observe(t, 99.95, t, 100.0, t),
            LeadLagAction::Enter {
                side: LeadLagSide::Short,
                ..
            }
        ));
    }

    #[test]
    fn deviation_is_strictly_causal_and_gap_reseeds_warmup() {
        let mut e = LeadLagTaker::new(cfg()).unwrap();
        // First observation can never produce a deviation (no prior EMA).
        assert_eq!(
            e.observe(1_000, 100.0, 1_000, 100.0, 1_000),
            LeadLagAction::Blocked(LeadLagBlock::WarmingUp)
        );
        let t = warm(&mut e, 1_100, 20);
        // A gap beyond max_gap_ms re-seeds the EMA and restarts warm-up.
        let later = t + 40_000;
        assert_eq!(
            e.observe(later, 100.2, later, 100.0, later),
            LeadLagAction::Blocked(LeadLagBlock::WarmingUp)
        );
        assert_eq!(
            e.observe(later + 100, 100.25, later + 100, 100.0, later + 100),
            LeadLagAction::Blocked(LeadLagBlock::WarmingUp)
        );
        // Clock regression and future receipts are rejected.
        assert_eq!(
            e.observe(later, 100.0, later, 100.0, later),
            LeadLagAction::Blocked(LeadLagBlock::InvalidInput)
        );
        assert_eq!(
            e.observe(later + 200, 100.0, later + 300, 100.0, later + 200),
            LeadLagAction::Blocked(LeadLagBlock::InvalidInput)
        );
    }

    #[test]
    fn round_trip_net_charges_both_taker_fees_and_validates_config() {
        let net = round_trip_net_bps(LeadLagSide::Long, 100.0, 100.05, 0.0).unwrap();
        assert!((net - 5.0).abs() < 1e-9);
        let net = round_trip_net_bps(LeadLagSide::Short, 100.0, 100.05, 2.3).unwrap();
        assert!((net + 9.6).abs() < 1e-9);
        assert!(round_trip_net_bps(LeadLagSide::Long, 0.0, 1.0, 0.0).is_none());
        let bad = LeadLagTakerConfig {
            max_dev_bps: 2.0,
            ..cfg()
        };
        assert!(LeadLagTaker::new(bad).is_err());
        let bad_passive = LeadLagTakerConfig {
            exit_mode: LeadLagExitMode::PassiveTouchThenTaker {
                min_passive_ms: 2_000,
            },
            ..cfg()
        };
        assert!(LeadLagTaker::new(bad_passive).is_err());
    }

    #[test]
    fn passive_exit_mode_quotes_passive_then_falls_back_or_closes_early() {
        let mut e = LeadLagTaker::new(LeadLagTakerConfig {
            exit_mode: LeadLagExitMode::PassiveTouchThenTaker {
                min_passive_ms: 100,
            },
            ..cfg()
        })
        .unwrap();
        let t = warm(&mut e, 1_000, 20);
        assert!(matches!(
            e.observe(t, 100.05, t, 100.0, t),
            LeadLagAction::Enter {
                side: LeadLagSide::Long,
                ..
            }
        ));
        // Before taker_delay_ms + min_passive_ms (300 + 100 = 400 ms) -> Idle.
        assert_eq!(
            e.observe(t + 350, 100.05, t + 350, 100.0, t + 350),
            LeadLagAction::Idle
        );
        // After 400 ms and before exit_due_ms (1_300 ms) -> QuotePassiveExit.
        assert_eq!(
            e.observe(t + 500, 100.05, t + 500, 100.02, t + 500),
            LeadLagAction::QuotePassiveExit {
                side: LeadLagSide::Long
            }
        );
        // At timeout -> falls back to taker Exit, retaining position until confirmed flat.
        assert_eq!(
            e.observe(t + 1_300, 100.05, t + 1_300, 100.02, t + 1_300),
            LeadLagAction::Exit {
                side: LeadLagSide::Long
            }
        );
        assert_eq!(e.position(), Some(LeadLagSide::Long));
        e.notify_exit_filled();
        assert_eq!(e.position(), None);

        // Early passive fill confirmation clears position before timeout.
        let t2 = warm(&mut e, t + 2_000, 20);
        assert!(matches!(
            e.observe(t2, 100.08, t2, 100.0, t2),
            LeadLagAction::Enter { .. }
        ));
        assert_eq!(
            e.observe(t2 + 500, 100.08, t2 + 500, 100.01, t2 + 500),
            LeadLagAction::QuotePassiveExit {
                side: LeadLagSide::Long
            }
        );
        e.notify_passive_exit_filled();
        assert_eq!(e.position(), None);
        assert_eq!(
            e.observe(t2 + 600, 100.01, t2 + 600, 100.01, t2 + 600),
            LeadLagAction::Idle
        );
    }

    #[test]
    fn clock_regression_and_future_receipt_do_not_corrupt_ema_or_passive_exit_timers() {
        let mut e = LeadLagTaker::new(LeadLagTakerConfig {
            exit_mode: LeadLagExitMode::PassiveTouchThenTaker {
                min_passive_ms: 100,
            },
            ..cfg()
        })
        .unwrap();
        let t = warm(&mut e, 1_000, 20);
        assert!(matches!(
            e.observe(t, 100.05, t, 100.0, t),
            LeadLagAction::Enter {
                side: LeadLagSide::Long,
                ..
            }
        ));
        let dev_before = e.last_dev_bps();

        // Advance into QuotePassiveExit window (taker_delay 300 + min_passive 100 = 400 ms).
        assert_eq!(
            e.observe(t + 500, 100.05, t + 500, 100.02, t + 500),
            LeadLagAction::QuotePassiveExit {
                side: LeadLagSide::Long
            }
        );
        let dev_at_passive = e.last_dev_bps();

        // Clock regression (now_ms < last_obs_ms) and future target/ref receipt timestamps
        // must reject cleanly without corrupting EMA, position, or exit_due_ms.
        assert_eq!(
            e.observe(t + 450, 101.0, t + 450, 100.0, t + 450),
            LeadLagAction::Blocked(LeadLagBlock::InvalidInput)
        );
        assert_eq!(
            e.observe(t + 600, 101.0, t + 600, 100.0, t + 700),
            LeadLagAction::Blocked(LeadLagBlock::InvalidInput)
        );
        assert_eq!(
            e.observe(t + 600, 101.0, t + 700, 100.0, t + 600),
            LeadLagAction::Blocked(LeadLagBlock::InvalidInput)
        );
        assert_eq!(e.position(), Some(LeadLagSide::Long));
        assert_eq!(e.last_dev_bps(), dev_at_passive);
        assert!(dev_before.is_some());

        // Passive exit window and final taker exit still fire on the original schedule.
        assert_eq!(
            e.observe(t + 1_200, 100.05, t + 1_200, 100.02, t + 1_200),
            LeadLagAction::QuotePassiveExit {
                side: LeadLagSide::Long
            }
        );
        assert_eq!(
            e.observe(t + 1_300, 100.05, t + 1_300, 100.02, t + 1_300),
            LeadLagAction::Exit {
                side: LeadLagSide::Long
            }
        );
        assert_eq!(e.position(), Some(LeadLagSide::Long));
        e.notify_exit_filled();
        assert_eq!(e.position(), None);
    }

    #[test]
    fn timeout_exit_retains_position_until_confirmed_flat() {
        let mut e = LeadLagTaker::new(cfg()).unwrap();
        let t = warm(&mut e, 1_000, 20);
        assert!(matches!(
            e.observe(t, 100.05, t, 100.0, t),
            LeadLagAction::Enter {
                side: LeadLagSide::Long,
                ..
            }
        ));

        // At exit_due_ms (1_300 ms): emits Exit but retains position until execution confirms flat.
        assert_eq!(
            e.observe(t + 1_300, 100.05, t + 1_300, 100.0, t + 1_300),
            LeadLagAction::Exit {
                side: LeadLagSide::Long
            }
        );
        assert_eq!(e.position(), Some(LeadLagSide::Long));

        // Subsequent ticks re-emit Exit (never Enter) even with strong new signals.
        assert_eq!(
            e.observe(t + 1_400, 99.80, t + 1_400, 100.0, t + 1_400),
            LeadLagAction::Exit {
                side: LeadLagSide::Long
            }
        );
        assert_eq!(
            e.observe(t + 1_500, 100.20, t + 1_500, 100.0, t + 1_500),
            LeadLagAction::Exit {
                side: LeadLagSide::Long
            }
        );
        assert_eq!(e.position(), Some(LeadLagSide::Long));

        // Once execution confirms flat, position clears and normal quoting/entry resumes.
        e.notify_exit_filled();
        assert_eq!(e.position(), None);
        assert_eq!(
            e.observe(t + 1_600, 100.0, t + 1_600, 100.0, t + 1_600),
            LeadLagAction::Idle
        );
    }

    #[test]
    fn stale_feed_does_not_pollute_ema_and_still_allows_open_position_exit() {
        let c = cfg();
        let mut e = LeadLagTaker::new(c).unwrap();
        let t = warm(&mut e, 10_000, 20);
        let last_warm = t - 100;

        // While flat, stale reference or stale target with extreme prices must not poison EMA.
        for dt in [6_000, 8_000, 10_000] {
            let t_s = last_warm + dt;
            assert_eq!(
                e.observe(t_s, 105.0, last_warm, 100.0, t_s),
                LeadLagAction::Blocked(LeadLagBlock::StaleReference)
            );
            assert_eq!(
                e.observe(t_s, 100.0, t_s, 95.0, last_warm),
                LeadLagAction::Blocked(LeadLagBlock::StaleTarget)
            );
        }

        // Fresh observation within max_gap_ms (30_000 ms) sees unpoisoned EMA (~ 0 bps dev).
        let t_rec = last_warm + 12_000;
        assert_eq!(
            e.observe(t_rec, 100.0, t_rec, 100.0, t_rec),
            LeadLagAction::Idle
        );
        assert!(
            e.last_dev_bps().unwrap().abs() < 1e-9,
            "EMA polluted by stale ticks: {:?}",
            e.last_dev_bps()
        );

        // Enter Long, then let the reference feed go stale past exit_due_ms.
        let t_enter = t_rec + 100;
        assert!(matches!(
            e.observe(t_enter, 100.05, t_enter, 100.0, t_enter),
            LeadLagAction::Enter {
                side: LeadLagSide::Long,
                ..
            }
        ));

        // Reference stale at t_exit (age 6_000 > max_ref_age_ms 5_000), target fresh:
        // open position must still be allowed to unwind via Exit, without polluting EMA.
        let t_exit = t_enter + 6_000;
        assert_eq!(
            e.observe(t_exit, 120.0, t_enter, 100.0, t_exit),
            LeadLagAction::Exit {
                side: LeadLagSide::Long
            }
        );
        // If target is stale at exit time, Exit is blocked by StaleTarget.
        assert_eq!(
            e.observe(t_exit + 100, 100.0, t_exit + 100, 100.0, t_enter),
            LeadLagAction::Blocked(LeadLagBlock::StaleTarget)
        );

        e.notify_exit_filled();
        assert_eq!(e.position(), None);

        // Long stale outage (> max_gap_ms = 30_000) does not advance last_obs_ms, so recovery re-warms.
        let t_long_stale = t_enter + c.max_gap_ms + 5_000;
        assert_eq!(
            e.observe(t_long_stale, 105.0, t_enter, 100.0, t_long_stale),
            LeadLagAction::Blocked(LeadLagBlock::StaleReference)
        );
        assert_eq!(
            e.observe(
                t_long_stale + 100,
                101.0,
                t_long_stale + 100,
                100.0,
                t_long_stale + 100
            ),
            LeadLagAction::Blocked(LeadLagBlock::WarmingUp)
        );
    }

    #[test]
    fn vol_adaptive_theta_raises_threshold_and_ignores_stale_or_regressed_ticks() {
        let c = LeadLagTakerConfig {
            theta_bps: 3.0,
            vol_theta_mult: 1.5,
            vol_ewma_tau_ms: 2_000,
            ..cfg()
        };
        let mut e = LeadLagTaker::new(c).unwrap();
        // Warm with volatile target oscillations (+/- 10 bps per 500 ms -> inst sigma ~ 14.1 bps/sqrt(s)).
        let mut t = 1_000u64;
        for i in 0..10u64 {
            let tgt = if i % 2 == 0 { 100.0 } else { 100.10 };
            let _ = e.observe(t, tgt, t, tgt, t);
            t += 500;
        }
        let sigma = e.sigma_1s_bps();
        let eff = e.effective_theta_bps();
        assert!(sigma > 5.0, "expected elevated sigma, got {sigma}");
        assert!(eff > 7.5, "expected effective_theta > 7.5, got {eff}");

        let t_last = t - 500;
        // Stale target tick with extreme price (150.0) and clock regression must not pollute sigma.
        assert_eq!(
            e.observe(t, 100.0, t, 150.0, t - 6_000),
            LeadLagAction::Blocked(LeadLagBlock::StaleTarget)
        );
        assert_eq!(
            e.observe(t_last - 100, 100.0, t_last - 100, 150.0, t_last - 100),
            LeadLagAction::Blocked(LeadLagBlock::InvalidInput)
        );
        assert!((e.sigma_1s_bps() - sigma).abs() < 1e-12);

        // Settle one fresh flat tick at 100.0; +5 bp ref impulse exceeds base 3.0 bp but < eff_theta.
        assert_eq!(e.observe(t, 100.0, t, 100.0, t), LeadLagAction::Idle);
        let eff_now = e.effective_theta_bps();
        assert!(eff_now > 5.0);
        assert_eq!(
            e.observe(t + 100, 100.05, t + 100, 100.0, t + 100),
            LeadLagAction::Idle
        );
        // A strong +20 bp ref impulse above effective_theta_bps still enters.
        assert!(matches!(
            e.observe(t + 200, 100.20, t + 200, 100.0, t + 200),
            LeadLagAction::Enter {
                side: LeadLagSide::Long,
                ..
            }
        ));
    }

    #[test]
    fn dual_leader_confirm_requires_directional_agreement_when_fresh() {
        let c = LeadLagTakerConfig {
            require_dual_leader_confirm: true,
            ..cfg()
        };
        let mut e = LeadLagTaker::new(c).unwrap();
        let mut t = 10_000u64;
        for _ in 0..20 {
            assert!(!matches!(
                e.observe_with_confirm(t, (100.0, t), Some((100.0, t)), (100.0, t)),
                LeadLagAction::Enter { .. }
            ));
            t += 100;
        }

        // Confirm leader disagrees (moves -3 bp while primary moves +5 bp) -> suppressed (Idle).
        assert_eq!(
            e.observe_with_confirm(t, (100.05, t), Some((99.97, t)), (100.0, t)),
            LeadLagAction::Idle
        );
        assert_eq!(e.position(), None);

        // Confirm leader invalid input (NaN or future recv) -> Blocked(InvalidInput), no state change.
        assert_eq!(
            e.observe_with_confirm(
                t + 100,
                (100.05, t + 100),
                Some((f64::NAN, t + 100)),
                (100.0, t + 100)
            ),
            LeadLagAction::Blocked(LeadLagBlock::InvalidInput)
        );
        assert_eq!(
            e.observe_with_confirm(
                t + 100,
                (100.05, t + 100),
                Some((100.05, t + 200)),
                (100.0, t + 100)
            ),
            LeadLagAction::Blocked(LeadLagBlock::InvalidInput)
        );

        // Missing confirm fails closed even when primary moves +5 bp -> Idle.
        t += 100;
        assert_eq!(
            e.observe_with_confirm(t, (100.05, t), None, (100.0, t)),
            LeadLagAction::Idle
        );
        assert_eq!(e.position(), None);

        // Stale confirm fails closed on primary +5 bp move AND does not pollute confirm_ema.
        t += 100;
        assert_eq!(
            e.observe_with_confirm(t, (100.05, t), Some((120.0, t - 6_000)), (100.0, t)),
            LeadLagAction::Idle
        );
        assert_eq!(e.position(), None);

        // Both leaders agree upward (+5 bp primary, +2 bp confirm) -> enters Long.
        t += 100;
        assert!(matches!(
            e.observe_with_confirm(t, (100.05, t), Some((100.02, t)), (100.0, t)),
            LeadLagAction::Enter {
                side: LeadLagSide::Long,
                ..
            }
        ));
        e.clear_position();

        // Short agreement works symmetrically.
        t = warm(&mut e, t + 200, 20);
        // Re-warm confirm leader cleanly at 100.0 after warm() passed None.
        for _ in 0..20 {
            let _ = e.observe_with_confirm(t, (100.0, t), Some((100.0, t)), (100.0, t));
            t += 100;
        }
        assert!(matches!(
            e.observe_with_confirm(t, (99.94, t), Some((99.98, t)), (100.0, t)),
            LeadLagAction::Enter {
                side: LeadLagSide::Short,
                ..
            }
        ));
    }

    #[test]
    fn catchup_exit_triggers_early_on_target_convergence_and_retains_position_until_confirmed() {
        let c = LeadLagTakerConfig {
            exit_on_catchup_frac: Some(0.70),
            ..cfg()
        };
        let mut e = LeadLagTaker::new(c).unwrap();
        let t = warm(&mut e, 10_000, 20);
        // Primary jumps +10 bp -> Enter Long (taker_delay=300, hold=1000 -> exit_due = t + 1300).
        let LeadLagAction::Enter { side, dev_bps } = e.observe(t, 100.10, t, 100.0, t) else {
            panic!("expected Enter");
        };
        assert_eq!(side, LeadLagSide::Long);
        assert!((dev_bps - 10.0).abs() < 0.05);

        // Before entry_filled_est_ms (t + 300): even if target jumps +8 bp, wait until fill lands.
        assert_eq!(
            e.observe(t + 200, 100.10, t + 200, 100.08, t + 200),
            LeadLagAction::Idle
        );

        // After entry_filled_est_ms, +5 bp catch-up (< 70% of 10 bp) stays Idle.
        assert_eq!(
            e.observe(t + 400, 100.10, t + 400, 100.05, t + 400),
            LeadLagAction::Idle
        );

        // Target reaches +7.5 bp (>= 70% of 10 bp) -> triggers early Exit before t + 1300!
        assert_eq!(
            e.observe(t + 500, 100.10, t + 500, 100.075, t + 500),
            LeadLagAction::Exit {
                side: LeadLagSide::Long
            }
        );
        assert_eq!(e.position(), Some(LeadLagSide::Long));

        // If target is stale on next tick, exit is Blocked(StaleTarget).
        assert_eq!(
            e.observe(t + 550, 100.10, t + 550, 100.00, t - 6_000),
            LeadLagAction::Blocked(LeadLagBlock::StaleTarget)
        );

        // Even if target dips back before confirm, exit_triggered persists until notify_exit_filled().
        assert_eq!(
            e.observe(t + 600, 100.10, t + 600, 100.00, t + 600),
            LeadLagAction::Exit {
                side: LeadLagSide::Long
            }
        );
        e.notify_exit_filled();
        assert_eq!(e.position(), None);

        // Short catch-up exit works symmetrically.
        let t2 = warm(&mut e, t + 1_000, 20);
        assert!(matches!(
            e.observe(t2, 99.90, t2, 100.0, t2),
            LeadLagAction::Enter {
                side: LeadLagSide::Short,
                ..
            }
        ));
        assert_eq!(
            e.observe(t2 + 400, 99.90, t2 + 400, 99.925, t2 + 400),
            LeadLagAction::Exit {
                side: LeadLagSide::Short
            }
        );
        e.clear_position();
        assert_eq!(e.position(), None);
    }

    #[test]
    fn validates_new_config_bounds_and_preserves_legacy_serde_defaults() {
        let legacy_json = r#"{"theta_bps":6.0,"tau_ms":120000,"hold_ms":10000,"taker_delay_ms":230,"max_ref_age_ms":5000,"max_tgt_age_ms":5000,"max_dev_bps":60.0,"warmup_ms":60000,"max_gap_ms":30000}"#;
        let parsed: LeadLagTakerConfig = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(parsed, LeadLagTakerConfig::default());

        for bad in [
            LeadLagTakerConfig {
                vol_theta_mult: -0.1,
                ..cfg()
            },
            LeadLagTakerConfig {
                vol_theta_mult: f64::NAN,
                ..cfg()
            },
            LeadLagTakerConfig {
                vol_ewma_tau_ms: 0,
                ..cfg()
            },
            LeadLagTakerConfig {
                exit_on_catchup_frac: Some(0.0),
                ..cfg()
            },
            LeadLagTakerConfig {
                exit_on_catchup_frac: Some(1.1),
                ..cfg()
            },
            LeadLagTakerConfig {
                exit_on_catchup_frac: Some(f64::INFINITY),
                ..cfg()
            },
        ] {
            assert!(LeadLagTaker::new(bad).is_err(), "expected err for {bad:?}");
        }
    }
}

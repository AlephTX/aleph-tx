//! Track 02 (`02_hedge_arbitrage`) venue-decoupled maker-first basis quoter.
//!
//! Rests passive quotes on a *maker* venue whose prices are derived from the *hedge*
//! venue touch, so that a fill can be immediately hedged with a taker order while the
//! entered basis beats a causal fair basis by an inventory-skewed edge:
//!
//! ```text
//! b      = ln(maker_mid) - ln(hedge_mid)
//! fair   = EMA_tau(b)  (strictly prior observations)
//! shift  = exp(lead_beta * (b - fair))        // hedge venue expected to follow the maker
//! bid   <= hedge_bid * shift * exp(fair - e_bid),  e_bid = e0 + skew * q
//! ask   >= hedge_ask * shift * exp(fair + e_ask),  e_ask = e0 - skew * q
//! ```
//!
//! Quotes join (or optionally improve by one tick) the maker best price, never cross it,
//! and are withdrawn on stale books, warm-up or at the inventory cap. `lead_beta`
//! counters the dominant loss seen in `scripts/replay_maker_hedge_basis.py`: a maker
//! fill on the fast venue is followed by the slow hedge venue moving against the hedge
//! during its speed bump (mean hedge slip about -2 to -4 bp on Arcus -> Lighter RWA).
//!
//! Pure decision logic with zero exchange-adapter imports. Execution adapters must
//! reserve worst-case exposure for both legs before dispatch and treat the hedge leg's
//! ambiguity per the live-hedge journal rules (`AGENTS.md`).

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// Top-of-book snapshot with local receipt time.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct Touch {
    pub bid: f64,
    pub ask: f64,
    pub recv_ms: u64,
}

impl Touch {
    fn valid(&self) -> bool {
        self.bid.is_finite() && self.ask.is_finite() && self.bid > 0.0 && self.ask > self.bid
    }

    pub fn mid(&self) -> f64 {
        0.5 * (self.bid + self.ask)
    }
}

/// Why no quotes were produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum QuoteBlock {
    InvalidBook,
    StaleMaker,
    StaleHedge,
    WarmingUp,
}

/// Passive quotes for the maker venue (`None` = do not rest that side).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct MakerHedgeQuotes {
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    /// Causal fair basis used for this decision, bps.
    pub fair_basis_bps: f64,
    /// Current basis minus fair, bps.
    pub dislocation_bps: f64,
}

/// Side of a maker fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum FillSide {
    /// Our maker bid was hit: we are long the maker leg -> sell the hedge leg.
    MakerBought,
    /// Our maker ask was lifted: we are short the maker leg -> buy the hedge leg.
    MakerSold,
}

/// External reference cancel-shield configuration (exploits zero-speed-bump maker cancels).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RefCancelShieldConfig {
    /// Enable external-reference adverse-selection quote withdrawal (default `false`).
    pub enabled: bool,
    /// Withdraw the exposed side if external reference moved adversely by `>= adverse_move_bps`.
    pub adverse_move_bps: f64,
    /// Trailing lookback window for external reference return, ms (`> 0`).
    pub lookback_ms: u64,
    /// Allow inventory-reducing side even when the external shield would block it (`false` = fail-closed).
    pub allow_reduce_only_through_shield: bool,
    /// Withdraw both sides when the shield is enabled but reference return is unready, stale, or future-dated
    /// (`false` = fail-open for backwards compatibility; still respects `allow_reduce_only_through_shield`).
    pub fail_closed_when_unready: bool,
}

impl Default for RefCancelShieldConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            adverse_move_bps: 2.5,
            lookback_ms: 200,
            allow_reduce_only_through_shield: false,
            fail_closed_when_unready: false,
        }
    }
}

/// Decoupled economic viability model for a shielded maker-hedge pair.
///
/// Accounts for maker/hedge fees, cross-stablecoin settlement drag, expected hedge
/// execution slippage, and risk-domain separation with zero exchange-adapter coupling.
#[derive(Debug, Clone, Copy, PartialEq, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShieldedPairEconomics {
    /// Maker-leg fee in bps (positive = cost, negative = rebate).
    pub maker_fee_bps: f64,
    /// Hedge-leg taker fee in bps (e.g. `2.25` for Arcus RWA level_0).
    pub hedge_taker_fee_bps: f64,
    /// Additional basis/conversion drag in bps applied when `cross_stablecoin` is `true`.
    pub cross_stablecoin_drag_bps: f64,
    /// Expected adverse hedge execution slippage in bps.
    pub expected_hedge_slip_bps: f64,
    /// Whether the maker and hedge legs settle in different stablecoins (e.g. USDG vs USDC).
    pub cross_stablecoin: bool,
    /// Whether both legs share the same counterparty/liquidation risk domain.
    pub same_risk_domain: bool,
}

impl ShieldedPairEconomics {
    /// Compute expected net edge in bps after fees, expected hedge slippage, and
    /// cross-stablecoin drag (when `cross_stablecoin` is `true`).
    pub fn net_expected_edge_bps(&self, expected_gross_edge_bps: f64) -> f64 {
        let stablecoin_drag = if self.cross_stablecoin {
            self.cross_stablecoin_drag_bps
        } else {
            0.0
        };
        if !expected_gross_edge_bps.is_finite()
            || !self.maker_fee_bps.is_finite()
            || !self.hedge_taker_fee_bps.is_finite()
            || !stablecoin_drag.is_finite()
            || !self.expected_hedge_slip_bps.is_finite()
        {
            return f64::NAN;
        }
        expected_gross_edge_bps
            - self.maker_fee_bps
            - self.hedge_taker_fee_bps
            - stablecoin_drag
            - self.expected_hedge_slip_bps
    }

    /// Return `true` only when the legs are in distinct risk domains (`!same_risk_domain`)
    /// and finite net expected edge is at least `min_net_edge_bps`.
    pub fn is_economically_viable(
        &self,
        expected_gross_edge_bps: f64,
        min_net_edge_bps: f64,
    ) -> bool {
        if self.same_risk_domain || !min_net_edge_bps.is_finite() {
            return false;
        }
        let net = self.net_expected_edge_bps(expected_gross_edge_bps);
        net.is_finite() && net >= min_net_edge_bps
    }
}

/// Configuration for [`MakerHedgeQuoter`].
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MakerHedgeQuoterConfig {
    /// Base required edge versus fair basis, bps (`>= 0`).
    pub e0_bps: f64,
    /// Additional edge per clip of inventory on the increasing side, bps (`>= 0`).
    pub skew_bps_per_clip: f64,
    /// Maximum absolute inventory in clips (`> 0`).
    pub q_max_clips: f64,
    /// Fair-basis EMA time constant, ms (`> 0`).
    pub tau_ms: u64,
    /// Fraction of the current dislocation the hedge venue is expected to follow (`0..=1.5`).
    pub lead_beta: f64,
    /// Improve the maker best price by one tick when the spread allows it.
    pub improve_one_tick: bool,
    /// Maker venue tick size (`> 0`).
    pub tick_size: f64,
    /// Maximum book age for either venue, ms (`> 0`).
    pub max_book_age_ms: u64,
    /// Observation time before quoting, ms.
    pub warmup_ms: u64,
    /// Gap after which the fair EMA is re-seeded and warm-up restarts, ms (`> 0`).
    pub max_gap_ms: u64,
    /// External reference cancel shield configuration (opt-in, disabled by default).
    pub ref_shield: RefCancelShieldConfig,
}

impl Default for MakerHedgeQuoterConfig {
    fn default() -> Self {
        Self {
            e0_bps: 5.0,
            skew_bps_per_clip: 0.5,
            q_max_clips: 4.0,
            tau_ms: 3_600_000,
            lead_beta: 0.0,
            improve_one_tick: false,
            tick_size: 0.01,
            max_book_age_ms: 5_000,
            warmup_ms: 900_000,
            max_gap_ms: 60_000,
            ref_shield: RefCancelShieldConfig::default(),
        }
    }
}

impl MakerHedgeQuoterConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.e0_bps.is_finite() && self.e0_bps >= 0.0,
            "e0_bps must be finite and >= 0"
        );
        ensure!(
            self.skew_bps_per_clip.is_finite() && self.skew_bps_per_clip >= 0.0,
            "skew_bps_per_clip must be finite and >= 0"
        );
        ensure!(
            self.q_max_clips.is_finite() && self.q_max_clips > 0.0,
            "q_max_clips must be finite and > 0"
        );
        ensure!(
            self.lead_beta.is_finite() && (0.0..=1.5).contains(&self.lead_beta),
            "lead_beta must be in [0, 1.5]"
        );
        ensure!(
            self.tick_size.is_finite() && self.tick_size > 0.0,
            "tick_size must be finite and > 0"
        );
        ensure!(self.tau_ms > 0, "tau_ms must be > 0");
        ensure!(self.max_book_age_ms > 0, "max_book_age_ms must be > 0");
        ensure!(self.max_gap_ms > 0, "max_gap_ms must be > 0");
        ensure!(
            self.ref_shield.adverse_move_bps.is_finite() && self.ref_shield.adverse_move_bps > 0.0,
            "ref_shield.adverse_move_bps must be finite and > 0"
        );
        ensure!(
            self.ref_shield.lookback_ms > 0,
            "ref_shield.lookback_ms must be > 0"
        );
        Ok(())
    }
}

const REF_RING_CAP: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct RefPoint {
    ts_ms: u64,
    mid: f64,
}

/// Causal fair-basis tracker plus inventory-skewed maker quote derivation.
#[derive(Debug, Clone)]
pub struct MakerHedgeQuoter {
    cfg: MakerHedgeQuoterConfig,
    fair: Option<f64>,
    first_ms: Option<u64>,
    last_ms: Option<u64>,
    ref_ring: [RefPoint; REF_RING_CAP],
    ref_head: usize,
    ref_len: usize,
}

const TICK_EPS: f64 = 1e-9;

impl MakerHedgeQuoter {
    pub fn new(cfg: MakerHedgeQuoterConfig) -> Result<Self> {
        cfg.validate()?;
        Ok(Self {
            cfg,
            fair: None,
            first_ms: None,
            last_ms: None,
            ref_ring: [RefPoint::default(); REF_RING_CAP],
            ref_head: 0,
            ref_len: 0,
        })
    }

    pub fn config(&self) -> &MakerHedgeQuoterConfig {
        &self.cfg
    }

    /// Record an external fast-reference mid price (zero heap allocations) for pre-fill
    /// cancel-shield protection on subsequent [`Self::step`] calls.
    pub fn observe_reference(&mut self, now_ms: u64, ref_mid: f64) {
        if !(ref_mid.is_finite() && ref_mid > 0.0) {
            return;
        }
        if self.ref_len > 0 {
            let last_idx = (self.ref_head + REF_RING_CAP - 1) % REF_RING_CAP;
            let last_ts = self.ref_ring[last_idx].ts_ms;
            if now_ms < last_ts {
                return;
            }
            if now_ms.saturating_sub(last_ts) > self.cfg.max_gap_ms {
                self.ref_head = 0;
                self.ref_len = 0;
            }
        }
        self.ref_ring[self.ref_head] = RefPoint {
            ts_ms: now_ms,
            mid: ref_mid,
        };
        self.ref_head = (self.ref_head + 1) % REF_RING_CAP;
        self.ref_len = (self.ref_len + 1).min(REF_RING_CAP);
    }

    fn reference_return_bps(&self, now_ms: u64) -> Option<f64> {
        if !self.cfg.ref_shield.enabled || self.ref_len < 2 {
            return None;
        }
        let newest_idx = (self.ref_head + REF_RING_CAP - 1) % REF_RING_CAP;
        let newest = self.ref_ring[newest_idx];
        // Reject future reference timestamps instead of saturating age to 0.
        if newest.ts_ms > now_ms || now_ms - newest.ts_ms > self.cfg.max_book_age_ms {
            return None;
        }
        let cutoff = now_ms.saturating_sub(self.cfg.ref_shield.lookback_ms);
        // Find the sample at or immediately before cutoff, else the oldest in window.
        let mut base = None;
        for offset in 0..self.ref_len {
            let idx = (self.ref_head + REF_RING_CAP - 1 - offset) % REF_RING_CAP;
            let pt = self.ref_ring[idx];
            if pt.ts_ms > now_ms || now_ms - pt.ts_ms > self.cfg.max_book_age_ms {
                break;
            }
            base = Some(pt);
            if pt.ts_ms <= cutoff {
                break;
            }
        }
        let base = base?;
        if newest.ts_ms <= base.ts_ms || !base.mid.is_finite() || base.mid <= 0.0 {
            return None;
        }
        Some((newest.mid / base.mid - 1.0) * 1e4)
    }

    /// Quote using the fair basis from strictly prior observations, then fold this
    /// observation into the fair basis. `inventory_clips` is signed (`+` = long maker leg).
    pub fn step(
        &mut self,
        now_ms: u64,
        maker: Touch,
        hedge: Touch,
        inventory_clips: f64,
    ) -> std::result::Result<MakerHedgeQuotes, QuoteBlock> {
        if !maker.valid() || !hedge.valid() || !inventory_clips.is_finite() {
            return Err(QuoteBlock::InvalidBook);
        }
        if maker.recv_ms > now_ms || hedge.recv_ms > now_ms {
            return Err(QuoteBlock::InvalidBook);
        }
        if let Some(last) = self.last_ms
            && now_ms < last
        {
            return Err(QuoteBlock::InvalidBook);
        }
        let b = maker.mid().ln() - hedge.mid().ln();
        let gap = self.last_ms.map(|l| now_ms - l);
        let prior = match (self.fair, gap) {
            (Some(f), Some(g)) if g <= self.cfg.max_gap_ms => Some(f),
            _ => None,
        };
        self.fair = Some(match (prior, gap) {
            (Some(f), Some(g)) => {
                let alpha = 1.0 - (-(g as f64) / self.cfg.tau_ms as f64).exp();
                f + alpha * (b - f)
            }
            _ => {
                self.first_ms = Some(now_ms);
                b
            }
        });
        self.last_ms = Some(now_ms);

        if now_ms - maker.recv_ms > self.cfg.max_book_age_ms {
            return Err(QuoteBlock::StaleMaker);
        }
        if now_ms - hedge.recv_ms > self.cfg.max_book_age_ms {
            return Err(QuoteBlock::StaleHedge);
        }
        let Some(fair) = prior else {
            return Err(QuoteBlock::WarmingUp);
        };
        if now_ms - self.first_ms.unwrap_or(now_ms) < self.cfg.warmup_ms {
            return Err(QuoteBlock::WarmingUp);
        }
        Ok(self.derive_quotes(now_ms, maker, hedge, b, fair, inventory_clips))
    }

    fn derive_quotes(
        &self,
        now_ms: u64,
        maker: Touch,
        hedge: Touch,
        b: f64,
        fair: f64,
        q: f64,
    ) -> MakerHedgeQuotes {
        let c = &self.cfg;
        let tick = c.tick_size;
        let shift = (c.lead_beta * (b - fair)).exp();
        let (hb, ha) = (hedge.bid * shift, hedge.ask * shift);
        let can_improve = c.improve_one_tick && maker.ask - maker.bid > 1.5 * tick;
        let (raw_block_bid, raw_block_ask) = if c.ref_shield.enabled {
            match self.reference_return_bps(now_ms) {
                Some(r) => (
                    r <= -c.ref_shield.adverse_move_bps,
                    r >= c.ref_shield.adverse_move_bps,
                ),
                None if c.ref_shield.fail_closed_when_unready => (true, true),
                None => (false, false),
            }
        } else {
            (false, false)
        };
        let block_bid =
            raw_block_bid && !(c.ref_shield.allow_reduce_only_through_shield && q < 0.0);
        let block_ask =
            raw_block_ask && !(c.ref_shield.allow_reduce_only_through_shield && q > 0.0);
        let mut bid = None;
        if q < c.q_max_clips && !block_bid {
            let e_bid = c.e0_bps + c.skew_bps_per_clip * q;
            let cap = hb * (fair - e_bid * 1e-4).exp();
            let join = if can_improve {
                maker.bid + tick
            } else {
                maker.bid
            };
            let px = join.min((cap / tick + TICK_EPS).floor() * tick);
            if px > 0.0 && px < maker.ask {
                bid = Some(px);
            }
        }
        let mut ask = None;
        if q > -c.q_max_clips && !block_ask {
            let e_ask = c.e0_bps - c.skew_bps_per_clip * q;
            let floor = ha * (fair + e_ask * 1e-4).exp();
            let join = if can_improve {
                maker.ask - tick
            } else {
                maker.ask
            };
            let px = join.max((floor / tick - TICK_EPS).ceil() * tick);
            if px > maker.bid {
                ask = Some(px);
            }
        }
        MakerHedgeQuotes {
            bid,
            ask,
            fair_basis_bps: fair * 1e4,
            dislocation_bps: (b - fair) * 1e4,
        }
    }
}

/// Entered basis edge versus fair for a completed maker fill + hedge, in bps.
/// Positive means the pair was entered on the favourable side of fair.
pub fn entered_edge_bps(
    side: FillSide,
    maker_px: f64,
    hedge_px: f64,
    fair_basis_bps: f64,
) -> Option<f64> {
    if !(maker_px.is_finite() && maker_px > 0.0 && hedge_px.is_finite() && hedge_px > 0.0)
        || !fair_basis_bps.is_finite()
    {
        return None;
    }
    let entered = (maker_px.ln() - hedge_px.ln()) * 1e4;
    Some(match side {
        FillSide::MakerBought => fair_basis_bps - entered,
        FillSide::MakerSold => entered - fair_basis_bps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> MakerHedgeQuoterConfig {
        MakerHedgeQuoterConfig {
            e0_bps: 5.0,
            skew_bps_per_clip: 0.5,
            q_max_clips: 4.0,
            tau_ms: 60_000,
            lead_beta: 0.0,
            improve_one_tick: false,
            tick_size: 0.01,
            max_book_age_ms: 5_000,
            warmup_ms: 1_000,
            max_gap_ms: 60_000,
            ref_shield: RefCancelShieldConfig::default(),
        }
    }

    fn t(bid: f64, ask: f64, ms: u64) -> Touch {
        Touch {
            bid,
            ask,
            recv_ms: ms,
        }
    }

    fn px(v: Option<f64>, want: f64) {
        let got = v.unwrap_or_else(|| panic!("expected a quote near {want}"));
        assert!((got - want).abs() < 1e-6, "got {got}, want {want}");
    }

    fn warm(q: &mut MakerHedgeQuoter, mk: (f64, f64), hd: (f64, f64)) -> u64 {
        let mut now = 10_000;
        for _ in 0..20 {
            let _ = q.step(now, t(mk.0, mk.1, now), t(hd.0, hd.1, now), 0.0);
            now += 100;
        }
        now
    }

    #[test]
    fn quotes_are_capped_by_hedge_touch_minus_edge_and_never_cross() {
        let mut q = MakerHedgeQuoter::new(cfg()).unwrap();
        // Maker and hedge identical -> fair basis 0.
        let now = warm(&mut q, (99.99, 100.01), (99.99, 100.01));
        let out = q
            .step(now, t(99.99, 100.01, now), t(99.99, 100.01, now), 0.0)
            .unwrap();
        assert!(out.fair_basis_bps.abs() < 1e-9);
        // bid cap = 99.99 * exp(-5bp) = 99.940 -> floor tick 99.94 (below maker best).
        px(out.bid, 99.94);
        // ask floor = 100.01 * exp(+5bp) = 100.06002 -> ceil tick 100.07.
        px(out.ask, 100.07);
        // With a cheap hedge-relative edge requirement of 0 we join the maker touch.
        let mut q0 = MakerHedgeQuoter::new(MakerHedgeQuoterConfig {
            e0_bps: 0.0,
            ..cfg()
        })
        .unwrap();
        let now = warm(&mut q0, (99.99, 100.01), (99.99, 100.01));
        let out = q0
            .step(now, t(99.99, 100.01, now), t(99.99, 100.01, now), 0.0)
            .unwrap();
        px(out.bid, 99.99);
        px(out.ask, 100.01);
    }

    #[test]
    fn inventory_skew_and_cap_withdraw_the_increasing_side() {
        let mut q = MakerHedgeQuoter::new(cfg()).unwrap();
        let now = warm(&mut q, (99.99, 100.01), (99.99, 100.01));
        let long = q
            .step(now, t(99.99, 100.01, now), t(99.99, 100.01, now), 2.0)
            .unwrap();
        // e_bid = 6 bp (deeper bid), e_ask = 4 bp (tighter ask) when long 2 clips.
        px(long.bid, 99.93);
        px(long.ask, 100.06);
        let capped = q
            .step(
                now + 100,
                t(99.99, 100.01, now + 100),
                t(99.99, 100.01, now + 100),
                4.0,
            )
            .unwrap();
        assert_eq!(capped.bid, None);
        assert!(capped.ask.is_some());
    }

    #[test]
    fn lead_beta_moves_quotes_with_the_expected_hedge_catch_up() {
        let mut base = MakerHedgeQuoter::new(MakerHedgeQuoterConfig {
            e0_bps: 1.0,
            ..cfg()
        })
        .unwrap();
        let mut lead = MakerHedgeQuoter::new(MakerHedgeQuoterConfig {
            e0_bps: 1.0,
            lead_beta: 1.0,
            ..cfg()
        })
        .unwrap();
        let now = warm(&mut base, (99.99, 100.01), (99.99, 100.01));
        let _ = warm(&mut lead, (99.99, 100.01), (99.99, 100.01));
        // Maker venue drops 10 bp, hedge has not moved yet.
        let mk = t(99.89, 99.91, now);
        let hd = t(99.99, 100.01, now);
        let b0 = base.step(now, mk, hd, 0.0).unwrap();
        let b1 = lead.step(now, mk, hd, 0.0).unwrap();
        assert!(b0.dislocation_bps < -9.0);
        // Without lead adjustment the stale hedge touch lets us join the maker bid
        // (the toxic fill); with beta = 1 the bid cap follows the expected hedge drop.
        px(b0.bid, 99.89);
        px(b1.bid, 99.88);
    }

    #[test]
    fn blocks_on_stale_books_warmup_and_invalid_input() {
        let mut q = MakerHedgeQuoter::new(cfg()).unwrap();
        assert_eq!(
            q.step(
                10_000,
                t(99.99, 100.01, 10_000),
                t(99.99, 100.01, 10_000),
                0.0
            ),
            Err(QuoteBlock::WarmingUp)
        );
        let now = warm(&mut q, (99.99, 100.01), (99.99, 100.01));
        assert_eq!(
            q.step(
                now,
                t(99.99, 100.01, now - 6_000),
                t(99.99, 100.01, now),
                0.0
            ),
            Err(QuoteBlock::StaleMaker)
        );
        assert_eq!(
            q.step(
                now + 100,
                t(99.99, 100.01, now + 100),
                t(99.99, 100.01, now - 6_000),
                0.0
            ),
            Err(QuoteBlock::StaleHedge)
        );
        assert_eq!(
            q.step(
                now + 200,
                t(100.01, 99.99, now + 200),
                t(99.99, 100.01, now + 200),
                0.0
            ),
            Err(QuoteBlock::InvalidBook)
        );
        assert!(
            MakerHedgeQuoter::new(MakerHedgeQuoterConfig {
                lead_beta: 2.0,
                ..cfg()
            })
            .is_err()
        );
    }

    #[test]
    fn entered_edge_sign_convention() {
        // Bought maker at 99.94 vs hedge sold at 99.99 with fair 0: ~ +5 bp favourable.
        let e = entered_edge_bps(FillSide::MakerBought, 99.94, 99.99, 0.0).unwrap();
        assert!((e - 5.0).abs() < 0.05, "{e}");
        let e = entered_edge_bps(FillSide::MakerSold, 100.06, 100.01, 0.0).unwrap();
        assert!((e - 5.0).abs() < 0.05, "{e}");
        assert!(entered_edge_bps(FillSide::MakerSold, 0.0, 1.0, 0.0).is_none());
    }

    #[test]
    fn ref_shield_withdraws_only_toxic_side_and_resets_after_stale_or_recovery() {
        let mut q = MakerHedgeQuoter::new(MakerHedgeQuoterConfig {
            e0_bps: 1.0,
            ref_shield: RefCancelShieldConfig {
                enabled: true,
                adverse_move_bps: 2.5,
                lookback_ms: 200,
                allow_reduce_only_through_shield: false,
                fail_closed_when_unready: false,
            },
            ..cfg()
        })
        .unwrap();
        let now = warm(&mut q, (99.99, 100.01), (99.99, 100.01));
        q.observe_reference(now - 150, 100.0);
        // External ref dumps -5 bps in 150 ms -> bid is toxic and withdrawn, ask stays live.
        q.observe_reference(now, 99.95);
        let out = q
            .step(now, t(99.99, 100.01, now), t(99.99, 100.01, now), 0.0)
            .unwrap();
        assert_eq!(out.bid, None);
        assert!(out.ask.is_some());

        // External ref rallies +5 bps over the lookback -> ask is toxic and withdrawn, bid live.
        let t2 = now + 500;
        q.observe_reference(t2 - 150, 99.95);
        q.observe_reference(t2, 100.00);
        let out = q
            .step(t2, t(99.99, 100.01, t2), t(99.99, 100.01, t2), 0.0)
            .unwrap();
        assert!(out.bid.is_some());
        assert_eq!(out.ask, None);

        // Once calm (no lookback move >= 2.5 bps) or after a gap, both sides resume.
        let t3 = t2 + 1_000;
        q.observe_reference(t3 - 150, 100.00);
        q.observe_reference(t3, 100.00);
        let out = q
            .step(t3, t(99.99, 100.01, t3), t(99.99, 100.01, t3), 0.0)
            .unwrap();
        assert!(out.bid.is_some() && out.ask.is_some());

        assert!(
            MakerHedgeQuoter::new(MakerHedgeQuoterConfig {
                ref_shield: RefCancelShieldConfig {
                    enabled: true,
                    adverse_move_bps: 0.0,
                    lookback_ms: 200,
                    allow_reduce_only_through_shield: false,
                    fail_closed_when_unready: false,
                },
                ..cfg()
            })
            .is_err()
        );
    }

    #[test]
    fn ref_shield_fail_closed_when_unready_blocks_until_two_fresh_valid_samples() {
        let shield_cfg = |allow_reduce_only: bool| MakerHedgeQuoterConfig {
            e0_bps: 1.0,
            ref_shield: RefCancelShieldConfig {
                enabled: true,
                adverse_move_bps: 2.5,
                lookback_ms: 200,
                allow_reduce_only_through_shield: allow_reduce_only,
                fail_closed_when_unready: true,
            },
            ..cfg()
        };

        // 0 samples (unwarmed shield) -> fail-closed on both sides.
        let mut q = MakerHedgeQuoter::new(shield_cfg(false)).unwrap();
        let now = warm(&mut q, (99.99, 100.01), (99.99, 100.01));
        let out = q
            .step(now, t(99.99, 100.01, now), t(99.99, 100.01, now), 0.0)
            .unwrap();
        assert_eq!(out.bid, None);
        assert_eq!(out.ask, None);

        // 1 sample -> still unready -> fail-closed.
        q.observe_reference(now, 100.0);
        let t1 = now + 50;
        let out = q
            .step(t1, t(99.99, 100.01, t1), t(99.99, 100.01, t1), 0.0)
            .unwrap();
        assert_eq!(out.bid, None);
        assert_eq!(out.ask, None);

        // 2 fresh valid samples -> recovery, both sides quote.
        q.observe_reference(t1, 100.0);
        let t2 = t1 + 50;
        let out = q
            .step(t2, t(99.99, 100.01, t2), t(99.99, 100.01, t2), 0.0)
            .unwrap();
        assert!(out.bid.is_some() && out.ask.is_some());

        // Stale reference (> max_book_age_ms = 5_000) -> fail-closed.
        let t_stale = t2 + 5_500;
        let out = q
            .step(
                t_stale,
                t(99.99, 100.01, t_stale),
                t(99.99, 100.01, t_stale),
                0.0,
            )
            .unwrap();
        assert_eq!(out.bid, None);
        assert_eq!(out.ask, None);

        // Future reference timestamp (ref_ts_ms > now_ms) -> rejected, fail-closed.
        let t_future = t_stale + 1_000;
        q.observe_reference(t_future + 500, 100.0);
        let out = q
            .step(
                t_future,
                t(99.99, 100.01, t_future),
                t(99.99, 100.01, t_future),
                0.0,
            )
            .unwrap();
        assert_eq!(out.bid, None);
        assert_eq!(out.ask, None);

        // Recovery once clock reaches and advances past the future sample with a second fresh one.
        let t_rec = t_future + 600;
        q.observe_reference(t_rec, 100.0);
        let out = q
            .step(t_rec, t(99.99, 100.01, t_rec), t(99.99, 100.01, t_rec), 0.0)
            .unwrap();
        assert!(out.bid.is_some() && out.ask.is_some());

        // Reduce-only escape when holding long or short inventory while unready.
        let mut q_ro = MakerHedgeQuoter::new(shield_cfg(true)).unwrap();
        let t_ro = warm(&mut q_ro, (99.99, 100.01), (99.99, 100.01));
        let long_out = q_ro
            .step(t_ro, t(99.99, 100.01, t_ro), t(99.99, 100.01, t_ro), 1.0)
            .unwrap();
        assert_eq!(long_out.bid, None);
        assert!(long_out.ask.is_some());

        let short_out = q_ro
            .step(
                t_ro + 50,
                t(99.99, 100.01, t_ro + 50),
                t(99.99, 100.01, t_ro + 50),
                -1.0,
            )
            .unwrap();
        assert!(short_out.bid.is_some());
        assert_eq!(short_out.ask, None);
    }

    #[test]
    fn ref_shield_fail_open_when_unready_preserves_legacy_behavior() {
        let mut q = MakerHedgeQuoter::new(MakerHedgeQuoterConfig {
            e0_bps: 1.0,
            ref_shield: RefCancelShieldConfig {
                enabled: true,
                adverse_move_bps: 2.5,
                lookback_ms: 200,
                allow_reduce_only_through_shield: false,
                fail_closed_when_unready: false,
            },
            ..cfg()
        })
        .unwrap();
        let now = warm(&mut q, (99.99, 100.01), (99.99, 100.01));
        // 0 samples: fail-open quotes both sides.
        let out = q
            .step(now, t(99.99, 100.01, now), t(99.99, 100.01, now), 0.0)
            .unwrap();
        assert!(out.bid.is_some() && out.ask.is_some());

        // 1 sample + future sample: still fail-open when fail_closed_when_unready = false,
        // while still rejecting the future reference in reference_return_bps.
        q.observe_reference(now + 500, 99.50);
        assert!(q.reference_return_bps(now + 100).is_none());
        let out = q
            .step(
                now + 100,
                t(99.99, 100.01, now + 100),
                t(99.99, 100.01, now + 100),
                0.0,
            )
            .unwrap();
        assert!(out.bid.is_some() && out.ask.is_some());
    }

    #[test]
    fn shielded_pair_economics_accounts_for_fees_slippage_stablecoin_drag_and_domain() {
        let same_coin = ShieldedPairEconomics {
            maker_fee_bps: 0.0,
            hedge_taker_fee_bps: 1.15,
            cross_stablecoin_drag_bps: 2.0,
            expected_hedge_slip_bps: 1.5,
            cross_stablecoin: false,
            same_risk_domain: false,
        };
        let net_same = same_coin.net_expected_edge_bps(4.0);
        assert!((net_same - 1.35).abs() < 1e-9, "got {net_same}");
        assert!(same_coin.is_economically_viable(4.0, 1.0));

        // Enabling cross_stablecoin charges cross_stablecoin_drag_bps and flips edge negative.
        let cross_coin = ShieldedPairEconomics {
            cross_stablecoin: true,
            ..same_coin
        };
        let net_cross = cross_coin.net_expected_edge_bps(4.0);
        assert!((net_cross + 0.65).abs() < 1e-9, "got {net_cross}");
        assert!(!cross_coin.is_economically_viable(4.0, 0.0));

        // Arcus level_0 2.25 bp hedge taker fee stress.
        let arcus_l0 = ShieldedPairEconomics {
            maker_fee_bps: 0.0,
            hedge_taker_fee_bps: 2.25,
            cross_stablecoin_drag_bps: 0.0,
            expected_hedge_slip_bps: 1.5,
            cross_stablecoin: false,
            same_risk_domain: false,
        };
        assert!((arcus_l0.net_expected_edge_bps(4.0) - 0.25).abs() < 1e-9);
        assert!(arcus_l0.is_economically_viable(4.0, 0.0));
        assert!(!arcus_l0.is_economically_viable(4.0, 0.5));
        assert!((arcus_l0.net_expected_edge_bps(3.0) + 0.75).abs() < 1e-9);
        assert!(!arcus_l0.is_economically_viable(3.0, 0.0));

        // Same risk domain is never economically viable as a cross-venue hedge pair.
        let same_domain = ShieldedPairEconomics {
            same_risk_domain: true,
            ..same_coin
        };
        assert!((same_domain.net_expected_edge_bps(10.0) - 7.35).abs() < 1e-9);
        assert!(!same_domain.is_economically_viable(10.0, 0.0));

        // Non-finite inputs fail closed.
        assert!(same_coin.net_expected_edge_bps(f64::NAN).is_nan());
        assert!(!same_coin.is_economically_viable(f64::INFINITY, 0.0));
        assert!(!same_coin.is_economically_viable(10.0, f64::NAN));
    }
}

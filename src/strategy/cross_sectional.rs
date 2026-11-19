//! Venue-agnostic cross-sectional relative-strength signal & touch-pegged
//! `PostOnly`-first execution planner (`S4-3`).
//!
//! Designed directly from `basis_hours_v2` empirical findings: low-turnover
//! dollar-neutral relative-strength rotation executed passively on 0-bps-fee
//! `PostOnly` venues (`0 ms` create delay on Lighter Standard), with causal
//! receipt-time anchors, rank hysteresis, adverse-funding veto, and bounded
//! `Ioc` fallback or clean residual expiry.

use crate::exchange::{OrderParams, OrderType, Side};
use anyhow::ensure;
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const RING_SLOTS: usize = 96;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CrossSectionalDirection {
    #[default]
    Momentum,
    Reversal,
    /// Long most negative hourly funding / short most positive hourly funding (`S8-4`).
    FundingCarry,
}

fn default_max_funding_staleness_ms() -> u64 {
    7_200_000
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CrossSectionalConfig {
    /// Unique SHM symbol IDs in the tradeable asset universe (e.g. `[BTC, ETH, SOL, XRP]`).
    pub symbols: Vec<u16>,
    /// Ranking direction (`momentum` buys winners/shorts losers; `reversal` buys losers/shorts winners, `S7-1`;
    /// `funding_carry` longs most negative funding/shorts most positive funding, `S8-4`).
    #[serde(default)]
    pub direction: CrossSectionalDirection,
    /// Lookback horizon (ms) for cross-sectional return computation (e.g. `86_400_000` for 24h).
    pub lookback_ms: u64,
    /// Maximum age (ms) allowed for every universe asset's latest BBO before failing closed.
    pub max_quote_staleness_ms: u64,
    /// Maximum age (ms) allowed for causal funding observations when `direction == FundingCarry` (default 2h).
    #[serde(default = "default_max_funding_staleness_ms")]
    pub max_funding_staleness_ms: u64,
    /// Minimum spacing (ms) between portfolio target re-evaluations.
    pub rebalance_interval_ms: u64,
    /// Number of long legs paired 1-for-1 against equal-notional short legs (`1..=symbols.len()/2`).
    pub legs_per_side: usize,
    /// Optional minimum count of symbols with fresh BBOs and causal lookback anchors (`S6-4`).
    /// `None` requires all `symbols.len()` assets to be ready; `Some(min_n)` (`2 * legs_per_side <= min_n <= symbols.len()`)
    /// allows ranking clean subsets when missing/stale assets are currently flat (`|usd| <= 1.0`).
    #[serde(default)]
    pub min_active_symbols: Option<usize>,
    /// Target USD notional per active leg.
    pub leg_notional_usd: f64,
    /// Optional Moreira-Muir target 1-rebalance-horizon dollar PnL std dev per long/short pair (`S7-1`).
    #[serde(default)]
    pub vol_target_pnl_usd: Option<f64>,
    /// Optional minimum per-leg USD notional below which a volatility-scaled pair sits out at `0.0` (`S7-1`).
    #[serde(default)]
    pub min_leg_notional_usd: Option<f64>,
    /// Minimum return dispersion (`best_ret - worst_ret`, bps) required to enter positions.
    #[serde(default)]
    pub min_spread_return_bps: f64,
    /// Advantage (bps) an incumbent held leg enjoys before being rotated out.
    #[serde(default)]
    pub rank_hysteresis_bps: f64,
    /// Optional cap (bps/hr) on adverse hourly funding paid by an active leg (`None` disables).
    #[serde(default)]
    pub max_adverse_funding_bps: Option<f64>,
    /// Maximum chase duration (ms) working pegged `PostOnly` touch orders per rebalance leg.
    pub maker_chase_window_ms: u64,
    /// Optional slippage bound (bps) for `Ioc` completion after `maker_chase_window_ms`;
    /// `None` cancels unfilled residuals without crossing the spread.
    #[serde(default)]
    pub ioc_fallback_max_slippage_bps: Option<f64>,
}

impl CrossSectionalConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.symbols.len() >= 2,
            "cross_sectional: symbols requires at least 2 assets"
        );
        let mut seen = HashSet::with_capacity(self.symbols.len());
        for &sym in &self.symbols {
            ensure!(sym > 0, "cross_sectional: symbol_id must be positive");
            ensure!(
                seen.insert(sym),
                "cross_sectional: duplicate symbol_id {sym}"
            );
        }
        ensure!(
            self.lookback_ms >= 60_000
                && self.max_quote_staleness_ms >= 100
                && self.max_funding_staleness_ms >= 1_000
                && self.rebalance_interval_ms >= 1_000,
            "cross_sectional: invalid lookback, staleness, or rebalance interval"
        );
        ensure!(
            self.legs_per_side >= 1 && self.legs_per_side <= self.symbols.len() / 2,
            "cross_sectional: legs_per_side must lie in 1..={}",
            self.symbols.len() / 2
        );
        if let Some(min_n) = self.min_active_symbols {
            ensure!(
                min_n >= 2 * self.legs_per_side && min_n <= self.symbols.len(),
                "cross_sectional: min_active_symbols must lie in {}..={}",
                2 * self.legs_per_side,
                self.symbols.len()
            );
        }
        ensure!(
            self.leg_notional_usd.is_finite()
                && self.leg_notional_usd > 0.0
                && self.min_spread_return_bps.is_finite()
                && self.min_spread_return_bps >= 0.0
                && self.rank_hysteresis_bps.is_finite()
                && self.rank_hysteresis_bps >= 0.0,
            "cross_sectional: invalid notional or dispersion bounds"
        );
        if let Some(vol_target) = self.vol_target_pnl_usd {
            ensure!(
                vol_target.is_finite() && vol_target > 0.0 && vol_target <= 10_000.0,
                "cross_sectional: vol_target_pnl_usd must be in (0, 10000]"
            );
        }
        if let Some(min_leg) = self.min_leg_notional_usd {
            ensure!(
                self.vol_target_pnl_usd.is_some()
                    && min_leg.is_finite()
                    && min_leg > 0.0
                    && min_leg <= self.leg_notional_usd,
                "cross_sectional: min_leg_notional_usd requires vol_target_pnl_usd and must be in (0, leg_notional_usd]"
            );
        }
        if let Some(max_fund) = self.max_adverse_funding_bps {
            ensure!(
                max_fund.is_finite() && max_fund > 0.0 && max_fund <= 500.0,
                "cross_sectional: max_adverse_funding_bps must be in (0, 500]"
            );
        }
        if let Some(slip) = self.ioc_fallback_max_slippage_bps {
            ensure!(
                slip.is_finite() && (0.0..=100.0).contains(&slip),
                "cross_sectional: ioc_fallback_max_slippage_bps must be in [0, 100]"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TargetLeg {
    pub symbol_id: u16,
    pub target_notional_usd: f64,
    pub return_bps: f64,
    pub funding_bps: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LegAction {
    Hold,
    PostOnlyPeg { params: OrderParams },
    ProtectedIoc { params: OrderParams },
    ExpireUnfilled,
}

#[derive(Debug, Clone, Copy, Default)]
struct BookSlot {
    slot: u64,
    receipt_ms: u64,
    source_ms: u64,
    mid: f64,
}

#[derive(Debug, Clone)]
struct AssetHistory {
    symbol_id: u16,
    bucket_ms: u64,
    slots: [BookSlot; RING_SLOTS],
    latest_receipt_ms: u64,
    latest_source_ms: u64,
    latest_mid: f64,
    funding_receipt_ms: u64,
    funding_source_ms: u64,
    latest_funding_bps: Option<f64>,
}

impl AssetHistory {
    fn new(symbol_id: u16, lookback_ms: u64) -> Self {
        let bucket_ms = (lookback_ms / (RING_SLOTS as u64 - 16)).max(1_000);
        Self {
            symbol_id,
            bucket_ms,
            slots: [BookSlot::default(); RING_SLOTS],
            latest_receipt_ms: 0,
            latest_source_ms: 0,
            latest_mid: 0.0,
            funding_receipt_ms: 0,
            funding_source_ms: 0,
            latest_funding_bps: None,
        }
    }

    fn observe_book(&mut self, receipt_ms: u64, source_ms: u64, bid_px: f64, ask_px: f64) -> bool {
        if receipt_ms == 0
            || source_ms == 0
            || source_ms > receipt_ms
            || receipt_ms < self.latest_receipt_ms
            || source_ms < self.latest_source_ms
            || !bid_px.is_finite()
            || !ask_px.is_finite()
            || bid_px <= 0.0
            || ask_px <= bid_px
        {
            return false;
        }
        let mid = bid_px + 0.5 * (ask_px - bid_px);
        let slot_idx = receipt_ms / self.bucket_ms;
        let cell = &mut self.slots[(slot_idx % RING_SLOTS as u64) as usize];
        *cell = BookSlot {
            slot: slot_idx,
            receipt_ms,
            source_ms,
            mid,
        };
        self.latest_receipt_ms = receipt_ms;
        self.latest_source_ms = source_ms;
        self.latest_mid = mid;
        true
    }

    fn observe_funding(&mut self, receipt_ms: u64, source_ms: u64, rate_bps: f64) -> bool {
        if receipt_ms == 0
            || source_ms == 0
            || source_ms > receipt_ms
            || receipt_ms < self.funding_receipt_ms
            || source_ms < self.funding_source_ms
            || !rate_bps.is_finite()
        {
            return false;
        }
        self.funding_receipt_ms = receipt_ms;
        self.funding_source_ms = source_ms;
        self.latest_funding_bps = Some(rate_bps);
        true
    }

    fn has_fresh_book(&self, now_ms: u64, max_age_ms: u64) -> bool {
        // observe_book enforces source <= receipt; the source clock bounds both ages.
        self.latest_source_ms > 0
            && self.latest_receipt_ms <= now_ms
            && now_ms.saturating_sub(self.latest_source_ms) <= max_age_ms
    }

    fn fresh_funding_bps(&self, now_ms: u64, max_funding_age_ms: u64) -> Option<f64> {
        let rate = self.latest_funding_bps?;
        (self.funding_source_ms > 0
            && self.funding_receipt_ms <= now_ms
            && now_ms.saturating_sub(self.funding_source_ms) <= max_funding_age_ms)
            .then_some(rate)
    }

    fn causal_return_bps(
        &self,
        now_ms: u64,
        lookback_ms: u64,
        max_staleness_ms: u64,
    ) -> Option<f64> {
        let target_anchor_ms = now_ms.checked_sub(lookback_ms)?;
        let oldest_acceptable_ms = target_anchor_ms.saturating_sub(2 * self.bucket_ms);
        let anchor = self
            .slots
            .iter()
            .filter(|s| {
                s.mid > 0.0
                    && s.receipt_ms > 0
                    && s.receipt_ms <= target_anchor_ms
                    // The ring approximates the lookback with an earlier bucket.
                    // Its quote must have been fresh when that bucket was sampled.
                    && s.source_ms >= oldest_acceptable_ms
                    && s.receipt_ms - s.source_ms <= max_staleness_ms
                    && s.slot == s.receipt_ms / self.bucket_ms
            })
            .max_by_key(|s| s.receipt_ms)?;
        let ret = (self.latest_mid / anchor.mid - 1.0) * 10_000.0;
        ret.is_finite().then_some(ret)
    }
}

pub struct CrossSectionalEngine {
    config: CrossSectionalConfig,
    assets: Vec<AssetHistory>,
    last_rebalance_ms: Option<u64>,
    last_evaluation_ms: Option<u64>,
    last_targets: Vec<TargetLeg>,
}

impl CrossSectionalEngine {
    pub fn new(config: CrossSectionalConfig) -> anyhow::Result<Self> {
        config.validate()?;
        let assets = config
            .symbols
            .iter()
            .map(|&sym| AssetHistory::new(sym, config.lookback_ms))
            .collect();
        let last_targets = Vec::with_capacity(config.symbols.len());
        Ok(Self {
            config,
            assets,
            last_rebalance_ms: None,
            last_evaluation_ms: None,
            last_targets,
        })
    }

    pub fn observe_book(
        &mut self,
        symbol_id: u16,
        receipt_ms: u64,
        source_ms: u64,
        bid_px: f64,
        ask_px: f64,
    ) -> bool {
        let Some(asset) = self.assets.iter_mut().find(|a| a.symbol_id == symbol_id) else {
            return false;
        };
        asset.observe_book(receipt_ms, source_ms, bid_px, ask_px)
    }

    pub fn observe_funding(
        &mut self,
        symbol_id: u16,
        receipt_ms: u64,
        source_ms: u64,
        rate_bps: f64,
    ) -> bool {
        let Some(asset) = self.assets.iter_mut().find(|a| a.symbol_id == symbol_id) else {
            return false;
        };
        asset.observe_funding(receipt_ms, source_ms, rate_bps)
    }

    fn causal_pair_sigma_rebalance(
        &self,
        long_sym: u16,
        short_sym: u16,
        now_ms: u64,
    ) -> Option<f64> {
        let long_asset = self.assets.iter().find(|a| a.symbol_id == long_sym)?;
        let short_asset = self.assets.iter().find(|a| a.symbol_id == short_sym)?;
        let bucket_ms = long_asset.bucket_ms;
        let oldest_ms = now_ms.saturating_sub(self.config.lookback_ms);
        let valid_slots = |asset: &AssetHistory| -> Vec<(u64, f64)> {
            let mut out: Vec<(u64, f64)> = asset
                .slots
                .iter()
                .filter(|s| {
                    s.mid > 0.0
                        && s.receipt_ms > 0
                        && s.receipt_ms >= oldest_ms
                        && s.receipt_ms <= now_ms
                        && s.receipt_ms - s.source_ms <= self.config.max_quote_staleness_ms
                        && s.slot == s.receipt_ms / asset.bucket_ms
                })
                .map(|s| (s.slot, s.mid))
                .collect();
            out.sort_unstable_by_key(|(slot, _)| *slot);
            out.dedup_by_key(|(slot, _)| *slot);
            out
        };
        let long_slots = valid_slots(long_asset);
        let short_slots = valid_slots(short_asset);
        let mut common = Vec::with_capacity(long_slots.len().min(short_slots.len()));
        let (mut i, mut j) = (0usize, 0usize);
        while i < long_slots.len() && j < short_slots.len() {
            match long_slots[i].0.cmp(&short_slots[j].0) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    common.push((long_slots[i].0, long_slots[i].1, short_slots[j].1));
                    i += 1;
                    j += 1;
                }
            }
        }
        let mut sum_sq_per_ms = 0.0;
        let mut steps = 0usize;
        for w in common.windows(2) {
            let (prev_slot, prev_long, prev_short) = w[0];
            let (curr_slot, curr_long, curr_short) = w[1];
            if curr_slot <= prev_slot {
                continue;
            }
            let dt_ms = ((curr_slot - prev_slot) * bucket_ms) as f64;
            let r_pair = (curr_long / prev_long - 1.0) - (curr_short / prev_short - 1.0);
            if dt_ms > 0.0 && r_pair.is_finite() {
                sum_sq_per_ms += (r_pair * r_pair) / dt_ms;
                steps += 1;
            }
        }
        if steps < 4 {
            return None;
        }
        let var_per_ms = sum_sq_per_ms / (steps as f64);
        let sigma = (var_per_ms * (self.config.rebalance_interval_ms as f64)).sqrt();
        sigma.is_finite().then_some(sigma)
    }

    fn asset_score(&self, asset: &AssetHistory, now_ms: u64) -> Option<(f64, f64, Option<f64>)> {
        if !asset.has_fresh_book(now_ms, self.config.max_quote_staleness_ms) {
            return None;
        }
        match self.config.direction {
            CrossSectionalDirection::Momentum => {
                let raw_ret = asset.causal_return_bps(
                    now_ms,
                    self.config.lookback_ms,
                    self.config.max_quote_staleness_ms,
                )?;
                let funding = asset
                    .latest_funding_bps
                    .filter(|_| asset.funding_receipt_ms <= now_ms);
                Some((raw_ret, raw_ret, funding))
            }
            CrossSectionalDirection::Reversal => {
                let raw_ret = asset.causal_return_bps(
                    now_ms,
                    self.config.lookback_ms,
                    self.config.max_quote_staleness_ms,
                )?;
                let funding = asset
                    .latest_funding_bps
                    .filter(|_| asset.funding_receipt_ms <= now_ms);
                Some((-raw_ret, raw_ret, funding))
            }
            CrossSectionalDirection::FundingCarry => {
                let funding =
                    asset.fresh_funding_bps(now_ms, self.config.max_funding_staleness_ms)?;
                let raw_ret = asset
                    .causal_return_bps(
                        now_ms,
                        self.config.lookback_ms,
                        self.config.max_quote_staleness_ms,
                    )
                    .unwrap_or(0.0);
                // Positive score favors longs (most negative funding) and shorts (most positive funding).
                Some((-funding, raw_ret, Some(funding)))
            }
        }
    }

    /// Market-making one-sided entry veto filter (`[block_bid_entry, block_ask_entry]`, `S8-4`):
    /// uses causal cross-sectional ranking across ready universe assets without standalone directional churn.
    /// Extreme top-tail assets veto inventory-adding asks (`[false, true]`); extreme bottom-tail assets veto
    /// inventory-adding bids (`[true, false]`). Interior or uncertain states fail safe (`[false, false]`).
    pub fn mm_blocked_entry_sides(
        &self,
        symbol_id: u16,
        now_ms: u64,
        tail_count: usize,
    ) -> [bool; 2] {
        if tail_count == 0 || self.last_evaluation_ms.is_some_and(|last| now_ms < last) {
            return [false, false];
        }
        let min_required = self.config.min_active_symbols.unwrap_or(self.assets.len());
        let mut ranked: Vec<(u16, f64)> = self
            .assets
            .iter()
            .filter_map(|asset| {
                self.asset_score(asset, now_ms)
                    .map(|(score, _, _)| (asset.symbol_id, score))
            })
            .collect();
        if ranked.len() < min_required || 2 * tail_count > ranked.len() {
            return [false, false];
        }
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let spread = ranked.first().map_or(0.0, |x| x.1) - ranked.last().map_or(0.0, |x| x.1);
        if spread + 1e-9 < self.config.min_spread_return_bps {
            return [false, false];
        }
        let Some(idx) = ranked.iter().position(|(sym, _)| *sym == symbol_id) else {
            return [false, false];
        };
        if idx < tail_count {
            [false, true]
        } else if idx >= ranked.len() - tail_count {
            [true, false]
        } else {
            [false, false]
        }
    }

    /// Evaluates causal cross-sectional momentum/reversal/carry targets across the configured symbols.
    /// When `min_active_symbols` is `None`, fails closed (`None`) unless all assets have
    /// fresh BBOs and valid lookback/funding anchors. When `Some(min_n)` is set, allows ranking
    /// clean subsets of at least `min_n` ready symbols while still failing closed if any
    /// currently held leg (`|usd| > 1.0`) has a stale book or missing/stale anchor.
    pub fn compute_targets(
        &mut self,
        now_ms: u64,
        current_positions_usd: &[(u16, f64)],
    ) -> Option<&[TargetLeg]> {
        if self.last_evaluation_ms.is_some_and(|last| now_ms < last) {
            return None;
        }
        self.last_evaluation_ms = Some(now_ms);
        let held_sign = |sym: u16| -> i8 {
            current_positions_usd
                .iter()
                .find(|(s, _)| *s == sym)
                .map_or(0, |(_, usd)| {
                    if *usd > 1.0 {
                        1
                    } else if *usd < -1.0 {
                        -1
                    } else {
                        0
                    }
                })
        };
        let min_required = self.config.min_active_symbols.unwrap_or(self.assets.len());
        if self
            .last_rebalance_ms
            .is_some_and(|prev| now_ms - prev < self.config.rebalance_interval_ms)
            && !self.last_targets.is_empty()
        {
            // A cached entry can still open a flat leg. Its current book and lookback/funding
            // anchor must qualify on this decision clock, along with the universe quorum.
            let mut ready_count = 0;
            let mut active_targets_ready = true;
            for asset in &self.assets {
                if self.asset_score(asset, now_ms).is_some() {
                    ready_count += 1;
                } else {
                    if held_sign(asset.symbol_id) != 0 {
                        return None;
                    }
                    if self.last_targets.iter().any(|leg| {
                        leg.symbol_id == asset.symbol_id && leg.target_notional_usd != 0.0
                    }) {
                        active_targets_ready = false;
                    }
                }
            }
            if ready_count < min_required {
                return None;
            }
            if active_targets_ready {
                return Some(&self.last_targets);
            }
        }

        let mut ranked: Vec<(u16, f64, f64, Option<f64>)> = Vec::with_capacity(self.assets.len());
        for asset in &self.assets {
            let Some((score, raw_ret, funding)) = self.asset_score(asset, now_ms) else {
                if held_sign(asset.symbol_id) != 0 {
                    return None;
                }
                continue;
            };
            ranked.push((asset.symbol_id, score, raw_ret, funding));
        }
        if ranked.len() < min_required {
            return None;
        }

        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        self.last_targets.clear();
        let spread = ranked.first()?.1 - ranked.last()?.1;
        if spread + 1e-9 < self.config.min_spread_return_bps {
            for (sym, _, raw_ret, funding) in ranked {
                self.last_targets.push(TargetLeg {
                    symbol_id: sym,
                    target_notional_usd: 0.0,
                    return_bps: raw_ret,
                    funding_bps: funding,
                });
            }
            self.last_rebalance_ms = Some(now_ms);
            return Some(&self.last_targets);
        }

        let k = self.config.legs_per_side;
        let long_cutoff = ranked[k - 1].1 - self.config.rank_hysteresis_bps;
        let short_cutoff = ranked[ranked.len() - k].1 + self.config.rank_hysteresis_bps;
        let funding_ok = |funding: Option<f64>, is_long: bool| -> bool {
            let Some(cap) = self.config.max_adverse_funding_bps else {
                return true;
            };
            let Some(rate) = funding else {
                return true;
            };
            if is_long { rate <= cap } else { -rate <= cap }
        };

        // Preserve qualifying incumbents within hysteresis before promoting challengers.
        let mut long_candidates: Vec<u16> = Vec::with_capacity(k);
        for &(sym, score, _, funding) in &ranked {
            if long_candidates.len() == k {
                break;
            }
            if held_sign(sym) > 0 && score >= long_cutoff - 1e-9 && funding_ok(funding, true) {
                long_candidates.push(sym);
            }
        }
        for (idx, &(sym, _, _, funding)) in ranked.iter().enumerate() {
            if long_candidates.len() == k {
                break;
            }
            if idx < ranked.len() - k
                && !long_candidates.contains(&sym)
                && funding_ok(funding, true)
            {
                long_candidates.push(sym);
            }
        }

        let mut short_candidates: Vec<u16> = Vec::with_capacity(k);
        for &(sym, score, _, funding) in ranked.iter().rev() {
            if short_candidates.len() == k {
                break;
            }
            if !long_candidates.contains(&sym)
                && held_sign(sym) < 0
                && score <= short_cutoff + 1e-9
                && funding_ok(funding, false)
            {
                short_candidates.push(sym);
            }
        }
        for (rev_idx, &(sym, _, _, funding)) in ranked.iter().rev().enumerate() {
            if short_candidates.len() == k {
                break;
            }
            if rev_idx < ranked.len() - k
                && !long_candidates.contains(&sym)
                && !short_candidates.contains(&sym)
                && funding_ok(funding, false)
            {
                short_candidates.push(sym);
            }
        }

        let paired = long_candidates.len().min(short_candidates.len());
        long_candidates.truncate(paired);
        short_candidates.truncate(paired);

        let mut pair_notionals = Vec::with_capacity(paired);
        if let Some(target_usd) = self.config.vol_target_pnl_usd {
            for p in 0..paired {
                let long_sym = long_candidates[p];
                let short_sym = short_candidates[p];
                let Some(sigma) = self.causal_pair_sigma_rebalance(long_sym, short_sym, now_ms)
                else {
                    if held_sign(long_sym) != 0 || held_sign(short_sym) != 0 {
                        return None;
                    }
                    pair_notionals.push(0.0);
                    continue;
                };
                let raw_usd = if sigma > 1e-9 {
                    (target_usd / sigma).min(self.config.leg_notional_usd)
                } else {
                    self.config.leg_notional_usd
                };
                let pair_usd = if self
                    .config
                    .min_leg_notional_usd
                    .is_some_and(|min_u| raw_usd + 1e-9 < min_u)
                {
                    0.0
                } else {
                    raw_usd
                };
                pair_notionals.push(pair_usd);
            }
        } else {
            pair_notionals.resize(paired, self.config.leg_notional_usd);
        }

        for (sym, _, raw_ret, funding) in ranked {
            let target_notional_usd =
                if let Some(idx) = long_candidates.iter().position(|&s| s == sym) {
                    pair_notionals[idx]
                } else if let Some(idx) = short_candidates.iter().position(|&s| s == sym) {
                    -pair_notionals[idx]
                } else {
                    0.0
                };
            self.last_targets.push(TargetLeg {
                symbol_id: sym,
                target_notional_usd,
                return_bps: raw_ret,
                funding_bps: funding,
            });
        }
        self.last_rebalance_ms = Some(now_ms);
        Some(&self.last_targets)
    }
}

/// Generates one venue-agnostic `PostOnlyPeg` or bounded `ProtectedIoc` action
/// from a target position and confirmed venue inventory (`S4-3`).
#[allow(clippy::too_many_arguments)]
pub fn plan_leg_order(
    target_base: f64,
    confirmed_position_base: f64,
    bid_touch: f64,
    ask_touch: f64,
    tick_size: f64,
    step_size: f64,
    min_notional_usd: f64,
    elapsed_chase_ms: u64,
    config: &CrossSectionalConfig,
) -> LegAction {
    if ![
        target_base,
        confirmed_position_base,
        bid_touch,
        ask_touch,
        tick_size,
        step_size,
        min_notional_usd,
    ]
    .iter()
    .all(|v| v.is_finite())
        || bid_touch <= 0.0
        || ask_touch <= bid_touch
        || tick_size <= 0.0
        || step_size <= 0.0
    {
        return LegAction::Hold;
    }

    let delta = target_base - confirmed_position_base;
    let raw_lots = (delta.abs() / step_size + 1e-9).floor() * step_size;
    if raw_lots < step_size - 1e-9 {
        return LegAction::Hold;
    }

    let side = if delta > 0.0 { Side::Buy } else { Side::Sell };
    // Never mark `reduce_only` if the order flips confirmed sign; cap reducing leg size
    // cleanly at confirmed exposure when reducing toward flat.
    let same_direction_as_close = (confirmed_position_base > step_size * 0.5 && side == Side::Sell)
        || (confirmed_position_base < -step_size * 0.5 && side == Side::Buy);
    let reduce_only = same_direction_as_close && raw_lots <= confirmed_position_base.abs() + 1e-9;

    let floor_tick = |px: f64| (((px / tick_size) + 1e-9).floor()) * tick_size;
    let ceil_tick = |px: f64| (((px / tick_size) - 1e-9).ceil()) * tick_size;

    if elapsed_chase_ms < config.maker_chase_window_ms {
        let peg_px = match side {
            Side::Buy => floor_tick(bid_touch).min(floor_tick(ask_touch - tick_size)),
            Side::Sell => ceil_tick(ask_touch).max(ceil_tick(bid_touch + tick_size)),
        };
        if peg_px <= 0.0 || (!reduce_only && raw_lots * peg_px < min_notional_usd) {
            return LegAction::Hold;
        }
        return LegAction::PostOnlyPeg {
            params: OrderParams {
                side,
                size: raw_lots,
                price: peg_px,
                order_type: OrderType::PostOnly,
                reduce_only,
            },
        };
    }

    let Some(slip_bps) = config.ioc_fallback_max_slippage_bps else {
        return LegAction::ExpireUnfilled;
    };
    let touch = if side == Side::Buy {
        ask_touch
    } else {
        bid_touch
    };
    let Some(ioc_px) = protected_ioc_price(side, touch, tick_size, slip_bps) else {
        return LegAction::ExpireUnfilled;
    };
    if ioc_px <= 0.0 || (!reduce_only && raw_lots * ioc_px < min_notional_usd) {
        return LegAction::Hold;
    }
    LegAction::ProtectedIoc {
        params: OrderParams {
            side,
            size: raw_lots,
            price: ioc_px,
            order_type: OrderType::Ioc,
            reduce_only,
        },
    }
}

fn protected_ioc_price(side: Side, touch: f64, tick: f64, slip_bps: f64) -> Option<f64> {
    let touch = Decimal::try_from(touch).ok()?;
    let tick = Decimal::try_from(tick).ok()?;
    let slip = Decimal::try_from(slip_bps)
        .ok()?
        .checked_div(Decimal::from(10_000))?;
    let offset = touch.checked_mul(slip)?;
    // Round toward the touch so the worst executable tick stays inside the price limit.
    let bound = match side {
        Side::Buy => touch.checked_add(offset)?,
        Side::Sell => touch.checked_sub(offset)?,
    };
    let remainder = bound.checked_rem(tick)?;
    let price = match side {
        Side::Buy => bound.checked_sub(remainder)?,
        Side::Sell if !remainder.is_zero() => bound.checked_add(tick.checked_sub(remainder)?)?,
        Side::Sell => bound,
    };
    if price <= Decimal::ZERO
        || (side == Side::Buy && price < touch)
        || (side == Side::Sell && price > touch)
    {
        return None;
    }
    price.to_f64()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> CrossSectionalConfig {
        CrossSectionalConfig {
            symbols: vec![1001, 1002, 1003, 1004],
            direction: CrossSectionalDirection::Momentum,
            lookback_ms: 86_400_000,
            max_quote_staleness_ms: 5_000,
            max_funding_staleness_ms: default_max_funding_staleness_ms(),
            rebalance_interval_ms: 3_600_000,
            legs_per_side: 1,
            min_active_symbols: None,
            leg_notional_usd: 50.0,
            vol_target_pnl_usd: None,
            min_leg_notional_usd: None,
            min_spread_return_bps: 20.0,
            rank_hysteresis_bps: 15.0,
            max_adverse_funding_bps: Some(5.0),
            maker_chase_window_ms: 30_000,
            ioc_fallback_max_slippage_bps: Some(3.0),
        }
    }

    #[test]
    fn delayed_source_at_either_endpoint_blocks_ranking() {
        let cfg = cfg();
        let anchor = 1_700_000_000_000;
        let now = anchor + cfg.lookback_ms;
        let expiry = cfg.max_quote_staleness_ms + 1;
        for (historical_lag, current_lag) in [(expiry, 0), (0, cfg.lookback_ms)] {
            let mut engine = CrossSectionalEngine::new(cfg.clone()).unwrap();
            for &symbol in &cfg.symbols {
                let history_source = if symbol == 1001 {
                    anchor - historical_lag
                } else {
                    anchor
                };
                let current_source = if symbol == 1001 {
                    now - current_lag
                } else {
                    now
                };
                assert!(engine.observe_book(symbol, anchor, history_source, 100.0, 101.0));
                assert!(engine.observe_book(symbol, now, current_source, 104.0, 105.0));
            }
            assert!(engine.compute_targets(now, &[]).is_none());
        }
    }

    #[test]
    fn cached_targets_require_fresh_quotes_and_monotonic_decision_time() {
        let cfg = cfg();
        let mut engine = CrossSectionalEngine::new(cfg.clone()).unwrap();
        let anchor = 1_700_000_000_000;
        let now = anchor + cfg.lookback_ms;
        let limit = cfg.max_quote_staleness_ms;
        for &symbol in &cfg.symbols {
            assert!(engine.observe_book(symbol, anchor, anchor - limit, 100.0, 101.0));
            assert!(engine.observe_book(symbol, now, now, 110.0, 111.0));
        }
        let targets = engine.compute_targets(now, &[]).unwrap().to_vec();
        assert_eq!(engine.compute_targets(now + limit, &[]).unwrap(), targets);
        assert!(engine.compute_targets(now + limit - 1, &[]).is_none());
        let next = now + limit + 1;
        for &symbol in &cfg.symbols[1..] {
            assert!(engine.observe_book(symbol, next, next, 80.0, 81.0));
        }
        assert!(engine.compute_targets(next, &[]).is_none());
        assert!(engine.observe_book(1001, next, now, 110.0, 111.0));
        assert!(engine.compute_targets(next, &[]).is_none());
        assert!(engine.observe_book(1001, next + 1, next + 1, 109.0, 110.0));
        assert!(engine.compute_targets(next, &[]).is_none());
        assert_eq!(engine.compute_targets(next + 1, &[]).unwrap(), targets);
    }

    #[test]
    fn engine_validates_causality_fails_closed_when_incomplete_and_ranks_neutral_pairs() {
        let mut engine = CrossSectionalEngine::new(cfg()).unwrap();
        let t0 = 1_700_000_000_000_u64;
        let t1 = t0 + 86_400_000;

        // Future source time rejected (`source_ms > receipt_ms`).
        assert!(!engine.observe_book(1001, t0, t0 + 1, 100.0, 100.2));
        for &(sym, px0) in &[(1001, 100.0), (1002, 100.0), (1003, 100.0)] {
            assert!(engine.observe_book(sym, t0, t0, px0, px0 + 0.1));
        }
        // Missing 1004 history -> fails closed.
        for &(sym, px1) in &[(1001, 105.0), (1002, 101.0), (1003, 99.0), (1004, 96.0)] {
            assert!(engine.observe_book(sym, t1, t1, px1, px1 + 0.1));
        }
        assert!(engine.compute_targets(t1, &[]).is_none());

        // Chronological stream with 1h anchor snapshots so `t1`, `t2`, `t3` stay strictly monotonic.
        let mut engine = CrossSectionalEngine::new(cfg()).unwrap();
        let h = 3_600_000_u64;
        for step in [0, h, 2 * h] {
            for &sym in &[1001, 1002, 1003, 1004] {
                assert!(engine.observe_book(sym, t0 + step, t0 + step, 100.0, 100.1));
            }
        }
        for &(sym, px1) in &[(1001, 105.0), (1002, 101.0), (1003, 99.0), (1004, 96.0)] {
            assert!(engine.observe_book(sym, t1, t1, px1, px1 + 0.1));
        }
        let targets = engine.compute_targets(t1, &[]).unwrap();
        assert_eq!(
            targets
                .iter()
                .find(|t| t.symbol_id == 1001)
                .unwrap()
                .target_notional_usd,
            50.0
        );
        assert_eq!(
            targets
                .iter()
                .find(|t| t.symbol_id == 1004)
                .unwrap()
                .target_notional_usd,
            -50.0
        );
        assert_eq!(
            targets
                .iter()
                .find(|t| t.symbol_id == 1002)
                .unwrap()
                .target_notional_usd,
            0.0
        );

        // Next rebalance 1h later: 1002 leads (+510 bps) vs incumbent 1001 (+500 bps),
        // within 15 bps rank hysteresis -> holds 1001 long without churn.
        let t2 = t1 + h;
        for &(sym, px2) in &[(1001, 105.0), (1002, 105.1), (1003, 99.0), (1004, 96.0)] {
            assert!(engine.observe_book(sym, t2, t2, px2, px2 + 0.1));
        }
        let targets2 = engine
            .compute_targets(t2, &[(1001, 50.0), (1004, -50.0)])
            .unwrap();
        assert_eq!(
            targets2
                .iter()
                .find(|t| t.symbol_id == 1001)
                .unwrap()
                .target_notional_usd,
            50.0
        );

        // Adverse funding veto (+10 bps/hr > +5 bps cap) vetoes 1001 long and rotates to 1002.
        let t3 = t2 + h;
        assert!(engine.observe_funding(1001, t3, t3, 10.0));
        for &(sym, px3) in &[(1001, 105.0), (1002, 105.1), (1003, 99.0), (1004, 96.0)] {
            assert!(engine.observe_book(sym, t3, t3, px3, px3 + 0.1));
        }
        let targets3 = engine
            .compute_targets(t3, &[(1001, 50.0), (1004, -50.0)])
            .unwrap();
        assert_eq!(
            targets3
                .iter()
                .find(|t| t.symbol_id == 1001)
                .unwrap()
                .target_notional_usd,
            0.0
        );
        assert_eq!(
            targets3
                .iter()
                .find(|t| t.symbol_id == 1002)
                .unwrap()
                .target_notional_usd,
            50.0
        );
    }

    #[test]
    fn pegged_leg_planner_emits_post_only_then_ioc_or_expiry_and_reduce_only() {
        let mut c = cfg();
        // Opening buy during maker window uses PostOnly touch peg, reduce_only = false.
        let open_act = plan_leg_order(0.05, 0.0, 1000.0, 1000.5, 0.1, 0.01, 10.0, 5_000, &c);
        assert_eq!(
            open_act,
            LegAction::PostOnlyPeg {
                params: OrderParams {
                    side: Side::Buy,
                    size: 0.05,
                    price: 1000.0,
                    order_type: OrderType::PostOnly,
                    reduce_only: false,
                }
            }
        );
        // Reducing sell sets reduce_only = true automatically.
        let close_act = plan_leg_order(0.0, 0.05, 1000.0, 1000.5, 0.1, 0.01, 10.0, 0, &c);
        assert_eq!(
            close_act,
            LegAction::PostOnlyPeg {
                params: OrderParams {
                    side: Side::Sell,
                    size: 0.05,
                    price: 1000.5,
                    order_type: OrderType::PostOnly,
                    reduce_only: true,
                }
            }
        );
        // After chase window expires with 3 bps IOC slippage cap, emits bounded IOC.
        let ioc_act = plan_leg_order(0.05, 0.0, 1000.0, 1000.5, 0.1, 0.01, 10.0, 30_000, &c);
        match ioc_act {
            LegAction::ProtectedIoc { params } => {
                assert_eq!(params.side, Side::Buy);
                assert!((params.size - 0.05).abs() < 1e-9);
                assert!((params.price - 1000.8).abs() < 1e-9);
                assert_eq!(params.order_type, OrderType::Ioc);
                assert!(!params.reduce_only);
            }
            other => panic!("expected ProtectedIoc, got {other:?}"),
        }
        // Without IOC fallback, cancels residual without crossing spread.
        c.ioc_fallback_max_slippage_bps = None;
        assert_eq!(
            plan_leg_order(0.05, 0.0, 1000.0, 1000.5, 0.1, 0.01, 10.0, 30_000, &c),
            LegAction::ExpireUnfilled
        );
    }

    #[test]
    fn ioc_slippage_cap_holds_on_coarse_and_fractional_ticks() {
        for (side, touch, tick, slip, price) in [
            (Side::Buy, 101.0, 1.0, 3.0, Some(101.0)),
            (Side::Sell, 100.0, 1.0, 3.0, Some(100.0)),
            (Side::Sell, 1000.5, 0.1, 3.0, Some(1000.2)),
            (Side::Buy, 0.3, 0.1, 0.0, Some(0.3)),
            (Side::Sell, 0.3, 0.1, 0.0, Some(0.3)),
            (Side::Buy, 101.5, 1.0, 0.0, None),
            (Side::Sell, 100.5, 1.0, 0.0, None),
        ] {
            assert_eq!(protected_ioc_price(side, touch, tick, slip), price);
        }
    }

    #[test]
    fn cached_targets_require_ready_universe_and_active_leg_anchors() {
        let start = 1_700_000_000_000;
        let horizon = 60_000;
        let later = 4_000;
        for (minimum, renewed, expected_long) in [
            (None, 3, None),
            (Some(3), 2, None),
            (Some(3), 3, Some(1002)),
        ] {
            let mut c = cfg();
            c.lookback_ms = horizon;
            c.min_active_symbols = minimum;
            let mut engine = CrossSectionalEngine::new(c).unwrap();
            for (index, symbol) in (1001..=1004).enumerate() {
                assert!(engine.observe_book(symbol, start, start, 100.0, 100.1));
                if index >= 4 - renewed {
                    let ts = start + later;
                    assert!(engine.observe_book(symbol, ts, ts, 100.0, 100.1));
                }
                let now = start + horizon;
                let px = 104.0 - index as f64 * 2.0;
                assert!(engine.observe_book(symbol, now, now, px, px + 0.1));
            }
            let now = start + horizon;
            let long_leg = |legs: &[TargetLeg]| {
                legs.iter()
                    .find(|leg| leg.target_notional_usd > 0.0)
                    .map(|leg| leg.symbol_id)
            };
            assert_eq!(
                engine.compute_targets(now, &[]).and_then(long_leg),
                Some(1001)
            );
            let ts = now + later;
            for (index, symbol) in (1001..=1004).enumerate() {
                let px = 104.0 - index as f64 * 2.0;
                assert!(engine.observe_book(symbol, ts, ts, px, px + 0.1));
            }
            let updated = engine.compute_targets(ts, &[]);
            assert_eq!(updated.is_some(), expected_long.is_some());
            assert_eq!(updated.and_then(long_leg), expected_long);
        }
    }

    #[test]
    fn clean_subset_ranking_trades_three_of_four_flat_universe_but_fails_closed_on_stale_held_leg()
    {
        let mut bad_cfg = cfg();
        bad_cfg.min_active_symbols = Some(1);
        assert!(CrossSectionalEngine::new(bad_cfg).is_err());

        let mut subset_cfg = cfg();
        subset_cfg.min_active_symbols = Some(3);
        let mut engine = CrossSectionalEngine::new(subset_cfg).unwrap();

        let anchor = 1_700_000_000_000_u64;
        let now = anchor + 86_400_000;
        for &(sym, px0, px1) in &[
            (1001, 100.0, 105.0),
            (1002, 100.0, 101.0),
            (1003, 100.0, 96.0),
        ] {
            assert!(engine.observe_book(sym, anchor, anchor, px0, px0 + 0.1));
            assert!(engine.observe_book(sym, now, now, px1, px1 + 0.1));
        }
        // Symbol 1004 is flat and lacks a 24h anchor (only observed now) -> 3-of-4 clean subset still trades.
        assert!(engine.observe_book(1004, now, now, 100.0, 100.1));
        let targets = engine.compute_targets(now, &[]).unwrap().to_vec();
        assert_eq!(targets.len(), 3);
        assert_eq!(
            targets
                .iter()
                .find(|t| t.symbol_id == 1001)
                .unwrap()
                .target_notional_usd,
            50.0
        );
        assert_eq!(
            targets
                .iter()
                .find(|t| t.symbol_id == 1002)
                .unwrap()
                .target_notional_usd,
            0.0
        );
        assert_eq!(
            targets
                .iter()
                .find(|t| t.symbol_id == 1003)
                .unwrap()
                .target_notional_usd,
            -50.0
        );
        // Dollar-neutrality invariant preserved across the clean subset.
        let net_target: f64 = targets.iter().map(|t| t.target_notional_usd).sum();
        assert!(net_target.abs() < 1e-9);

        // Held-leg fail-closed invariant: if 1004 has an open position and lacks anchor/freshness, return None.
        let t_check = now + 1_000;
        for &(sym, px1) in &[(1001, 105.0), (1002, 101.0), (1003, 96.0)] {
            assert!(engine.observe_book(sym, t_check, t_check, px1, px1 + 0.1));
        }
        assert!(
            engine
                .compute_targets(t_check, &[(1001, 50.0), (1004, -50.0)])
                .is_none()
        );
        // Also fails closed when all 4 had anchors but a currently held leg (1003) goes stale on current BBO.
        let stale_now = t_check + 6_000;
        for &(sym, px1) in &[(1001, 105.0), (1002, 101.0)] {
            assert!(engine.observe_book(sym, stale_now, stale_now, px1, px1 + 0.1));
        }
        assert!(
            engine
                .compute_targets(stale_now, &[(1001, 50.0), (1003, -50.0)])
                .is_none()
        );
    }

    #[test]
    fn reversal_direction_buys_losers_shorts_winners_and_respects_hysteresis() {
        let mut c = cfg();
        c.direction = CrossSectionalDirection::Reversal;
        let mut engine = CrossSectionalEngine::new(c).unwrap();
        let t0 = 1_700_000_000_000_u64;
        let h = 3_600_000_u64;
        for step in [0, h] {
            for &sym in &[1001, 1002, 1003, 1004] {
                assert!(engine.observe_book(sym, t0 + step, t0 + step, 100.0, 100.1));
            }
        }
        let t1 = t0 + 86_400_000;
        // 1004 dropped -400 bps (winner in Reversal -> Long), 1001 rose +500 bps (-> Short).
        for &(sym, px1) in &[(1001, 105.0), (1002, 101.0), (1003, 99.0), (1004, 96.0)] {
            assert!(engine.observe_book(sym, t1, t1, px1, px1 + 0.1));
        }
        let targets1 = engine.compute_targets(t1, &[]).unwrap().to_vec();
        assert_eq!(
            targets1
                .iter()
                .find(|t| t.symbol_id == 1004)
                .unwrap()
                .target_notional_usd,
            50.0
        );
        assert_eq!(
            targets1
                .iter()
                .find(|t| t.symbol_id == 1001)
                .unwrap()
                .target_notional_usd,
            -50.0
        );
        // Raw return_bps remains actual asset return even when ranking direction is Reversal.
        let expected_ret_1004 = (96.05 / 100.05 - 1.0) * 10_000.0;
        assert!(
            (targets1
                .iter()
                .find(|t| t.symbol_id == 1004)
                .unwrap()
                .return_bps
                - expected_ret_1004)
                .abs()
                < 1e-6
        );

        // Next rebalance 1h later: 1003 drops to -410 bps (vs incumbent long 1004 at -400 bps),
        // within 15 bps hysteresis -> holds 1004 long without churn.
        let t2 = t1 + h;
        for &(sym, px2) in &[(1001, 105.0), (1002, 101.0), (1003, 95.9), (1004, 96.0)] {
            assert!(engine.observe_book(sym, t2, t2, px2, px2 + 0.1));
        }
        let targets2 = engine
            .compute_targets(t2, &[(1004, 50.0), (1001, -50.0)])
            .unwrap();
        assert_eq!(
            targets2
                .iter()
                .find(|t| t.symbol_id == 1004)
                .unwrap()
                .target_notional_usd,
            50.0
        );
        assert_eq!(
            targets2
                .iter()
                .find(|t| t.symbol_id == 1003)
                .unwrap()
                .target_notional_usd,
            0.0
        );
    }

    #[test]
    fn vol_target_sizing_scales_pairs_dollar_neutrally_and_sits_out_extreme_vol() {
        let mut c = cfg();
        c.direction = CrossSectionalDirection::Reversal;
        c.leg_notional_usd = 100.0;
        c.vol_target_pnl_usd = Some(0.5);
        c.min_leg_notional_usd = Some(20.0);
        let mut engine = CrossSectionalEngine::new(c.clone()).unwrap();
        let t0 = 1_700_000_000_000_u64;
        let step_ms = 3_600_000_u64;

        // Populate 6 distinct 18m+ buckets with moderate spread vol (~100 bps/h) -> scaled notional in (20, 100).
        for step in 0..=5 {
            let ts = t0 + step * step_ms;
            let osc = if step % 2 == 0 { 0.0 } else { 0.6 };
            for &(sym, px) in &[
                (1001, 100.0 + osc),
                (1002, 100.0),
                (1003, 100.0),
                (1004, 100.0 - osc),
            ] {
                assert!(engine.observe_book(sym, ts, ts, px, px + 0.1));
            }
        }
        let now = t0 + 86_400_000;
        for &(sym, px) in &[(1001, 105.0), (1002, 100.5), (1003, 99.5), (1004, 95.0)] {
            assert!(engine.observe_book(sym, now, now, px, px + 0.1));
        }
        let scaled = engine.compute_targets(now, &[]).unwrap().to_vec();
        let long_u = scaled
            .iter()
            .find(|t| t.symbol_id == 1004)
            .unwrap()
            .target_notional_usd;
        let short_u = scaled
            .iter()
            .find(|t| t.symbol_id == 1001)
            .unwrap()
            .target_notional_usd;
        assert!((20.0..100.0).contains(&long_u), "got long_u={long_u}");
        assert!((long_u + short_u).abs() < 1e-9);

        // Extreme volatility episode drops implied size below min_leg_notional_usd (20) -> sits out at 0.0.
        let mut engine_hi = CrossSectionalEngine::new(c).unwrap();
        for step in 0..=5 {
            let ts = t0 + step * step_ms;
            let osc = if step % 2 == 0 { 0.0 } else { 8.0 };
            for &(sym, px) in &[
                (1001, 100.0 + osc),
                (1002, 100.0),
                (1003, 100.0),
                (1004, 100.0 - osc),
            ] {
                assert!(engine_hi.observe_book(sym, ts, ts, px, px + 0.1));
            }
        }
        for &(sym, px) in &[(1001, 105.0), (1002, 100.5), (1003, 99.5), (1004, 95.0)] {
            assert!(engine_hi.observe_book(sym, now, now, px, px + 0.1));
        }
        let sat_out = engine_hi.compute_targets(now, &[]).unwrap();
        assert!(
            sat_out.iter().all(|t| t.target_notional_usd.abs() < 1e-9),
            "extreme vol should sit out below min_leg_notional_usd: {sat_out:?}"
        );
    }

    #[test]
    fn twenty_four_market_funding_carry_uses_post_only_and_mm_one_sided_veto_filter() {
        let mut c = cfg();
        c.symbols = (1001..=1024).collect();
        c.direction = CrossSectionalDirection::FundingCarry;
        c.legs_per_side = 2;
        c.min_active_symbols = Some(20);
        c.rebalance_interval_ms = 28_800_000;
        c.min_spread_return_bps = 1.0;
        c.max_funding_staleness_ms = 7_200_000;
        c.ioc_fallback_max_slippage_bps = None;
        let mut engine = CrossSectionalEngine::new(c.clone()).unwrap();

        let now = 1_700_086_400_000_u64;
        // Populate 24 markets: 1001..1002 have deeply negative funding (-4, -3 bps/hr -> long earns carry),
        // 1023..1024 have high positive funding (+4, +5 bps/hr -> short earns carry), middle are near zero.
        for (idx, &sym) in c.symbols.iter().enumerate() {
            assert!(engine.observe_book(sym, now, now, 100.0, 100.1));
            let rate_bps = -4.0 + (idx as f64) * 0.4;
            assert!(engine.observe_funding(sym, now - 60_000, now - 60_000, rate_bps));
        }

        let targets = engine.compute_targets(now, &[]).unwrap().to_vec();
        assert_eq!(targets.len(), 24);
        assert_eq!(
            targets
                .iter()
                .find(|t| t.symbol_id == 1001)
                .unwrap()
                .target_notional_usd,
            50.0
        );
        assert_eq!(
            targets
                .iter()
                .find(|t| t.symbol_id == 1024)
                .unwrap()
                .target_notional_usd,
            -50.0
        );
        assert_eq!(
            plan_leg_order(0.5, 0.0, 100.0, 100.1, 0.01, 0.01, 10.0, 0, &c),
            LegAction::PostOnlyPeg {
                params: OrderParams {
                    side: Side::Buy,
                    size: 0.5,
                    price: 100.0,
                    order_type: OrderType::PostOnly,
                    reduce_only: false,
                }
            }
        );
        // Without IOC fallback, expired maker chase never crosses the spread.
        assert_eq!(
            plan_leg_order(
                0.5,
                0.0,
                100.0,
                100.1,
                0.01,
                0.01,
                10.0,
                c.maker_chase_window_ms,
                &c
            ),
            LegAction::ExpireUnfilled
        );

        // MM one-sided filter across 24 markets with tail_count = 3:
        // Top-carry tail (1001..=1003, negative funding -> favorable long): block ASK entry.
        // Bottom-carry tail (1022..=1024, high positive funding -> favorable short): block BID entry.
        // Interior markets (e.g. 1012): allow both sides cleanly.
        assert_eq!(engine.mm_blocked_entry_sides(1001, now, 3), [false, true]);
        assert_eq!(engine.mm_blocked_entry_sides(1024, now, 3), [true, false]);
        assert_eq!(engine.mm_blocked_entry_sides(1012, now, 3), [false, false]);

        // Stale funding > max_funding_staleness_ms drops asset from carry quorum cleanly.
        assert_eq!(
            engine.mm_blocked_entry_sides(1001, now + 8_000_000, 3),
            [false, false]
        );
    }
}

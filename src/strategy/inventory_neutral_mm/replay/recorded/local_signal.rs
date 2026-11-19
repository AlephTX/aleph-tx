//! Receipt-causal Core-only entry participation for offline research.
//! Quotes, cancels, fill accounting and all inventory limits use the existing
//! lifecycle. An unready signal may withdraw an entry, never a possible exit.
use super::*;

const BUCKET_MS: u64 = 100;
const BUCKETS: usize = 64;

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

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
    #[serde(default)]
    pub vol_shock_mult: Option<f64>,
    #[serde(default)]
    pub vol_recovery_mult: Option<f64>,
    #[serde(default)]
    pub min_shock_bps_floor: Option<f64>,
    #[serde(default)]
    pub min_recovery_bps_floor: Option<f64>,
    #[serde(default)]
    pub entry_touch_offset_bps: Option<f64>,
    /// Optional passive reducing exit offset from local BBO touch (`>= 0` bps) so
    /// reversal positions close passively as Maker before `position_timeout_secs` IOC flatten.
    #[serde(default)]
    pub exit_touch_offset_bps: Option<f64>,
    /// Optional round-trip profit target (bps over episode entry VWAP + fees) held
    /// until `exit_chase_start_fraction`, then decayed linearly to scratch touch.
    #[serde(default)]
    pub exit_min_profit_bps: Option<f64>,
    /// Fraction of `position_timeout_secs` in `[0, 1]` before passive exit chases touch.
    #[serde(default)]
    pub exit_chase_start_fraction: Option<f64>,
    /// Cooldown (ms) holding new entries flat after inventory transitions to flat (`S7-3`).
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub post_exit_cooldown_ms: u64,
    /// Optional causal flow imbalance magnitude `(0, 1)` that holds `exit_min_profit_bps`
    /// past `exit_chase_start_fraction` while flow remains in our position's favor (`age_f < 0.85`, `S7-3`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_exit_while_flow_favorable_min_imbalance: Option<f64>,
}

#[inline]
fn ordered_clamp(val: f64, lo: f64, hi: f64) -> f64 {
    if lo >= hi { hi } else { val.clamp(lo, hi) }
}

impl LocalReversalFilter {
    pub(super) fn effective_vol_floors(self) -> (f64, f64) {
        let shock_floor = self.min_shock_bps_floor.unwrap_or(0.5);
        let recovery_floor = self.min_recovery_bps_floor.unwrap_or(0.2 * shock_floor);
        (shock_floor, recovery_floor)
    }

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
        if let Some(k) = self.vol_shock_mult {
            ensure!(
                k.is_finite() && k > 0.0 && k <= 20.0,
                "invalid local reversal vol_shock_mult"
            );
            let (shock_floor, recovery_floor) = self.effective_vol_floors();
            ensure!(
                shock_floor.is_finite() && shock_floor > 0.0 && shock_floor <= self.shock_bps,
                "invalid local reversal min_shock_bps_floor"
            );
            ensure!(
                recovery_floor.is_finite()
                    && recovery_floor > 0.0
                    && recovery_floor <= self.recovery_bps,
                "invalid local reversal min_recovery_bps_floor"
            );
        } else {
            ensure!(
                self.vol_recovery_mult.is_none()
                    && self.min_shock_bps_floor.is_none()
                    && self.min_recovery_bps_floor.is_none(),
                "vol_recovery_mult, min_shock_bps_floor, and min_recovery_bps_floor require vol_shock_mult"
            );
        }
        if let Some(k_rec) = self.vol_recovery_mult {
            ensure!(
                k_rec.is_finite() && k_rec > 0.0 && k_rec <= 20.0,
                "invalid local reversal vol_recovery_mult"
            );
        }
        if let Some(offset_bps) = self.entry_touch_offset_bps {
            ensure!(
                offset_bps.is_finite() && (0.0..=50.0).contains(&offset_bps),
                "invalid local reversal entry_touch_offset_bps"
            );
        }
        if let Some(exit_bps) = self.exit_touch_offset_bps {
            ensure!(
                exit_bps.is_finite() && (0.0..=50.0).contains(&exit_bps),
                "invalid local reversal exit_touch_offset_bps"
            );
        }
        if let Some(profit_bps) = self.exit_min_profit_bps {
            ensure!(
                self.exit_touch_offset_bps.is_some()
                    && profit_bps.is_finite()
                    && (-20.0..=50.0).contains(&profit_bps),
                "invalid local reversal exit_min_profit_bps"
            );
        }
        if let Some(frac) = self.exit_chase_start_fraction {
            ensure!(
                self.exit_touch_offset_bps.is_some()
                    && frac.is_finite()
                    && (0.0..=1.0).contains(&frac),
                "invalid local reversal exit_chase_start_fraction"
            );
        }
        ensure!(
            self.post_exit_cooldown_ms <= 60_000,
            "invalid local reversal post_exit_cooldown_ms"
        );
        if let Some(min_imb) = self.hold_exit_while_flow_favorable_min_imbalance {
            ensure!(
                self.exit_min_profit_bps.is_some()
                    && min_imb.is_finite()
                    && min_imb > 0.0
                    && min_imb < 1.0,
                "invalid local reversal hold_exit_while_flow_favorable_min_imbalance"
            );
        }
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

    fn causal_sigma_1s_bps(&self, at: u64) -> Option<f64> {
        let mut sum_sq = 0.0;
        let mut count = 0u32;
        for bucket in &self.buckets {
            if bucket.mid <= 0.0
                || bucket.book_at > at
                || at.saturating_sub(bucket.book_at) > 6_400
                || bucket.slot < 10
            {
                continue;
            }
            let prior_slot = bucket.slot - 10;
            let prior = self.buckets[(prior_slot % BUCKETS as u64) as usize];
            if prior.slot == prior_slot
                && prior.mid > 0.0
                && prior.book_at <= bucket.book_at
                && bucket.book_at.saturating_sub(prior.book_at) <= 1_200
            {
                let ret_bps = (bucket.mid / prior.mid - 1.0) * 10_000.0;
                if ret_bps.is_finite() {
                    sum_sq += ret_bps * ret_bps;
                    count += 1;
                }
            }
        }
        (count >= 5).then(|| (sum_sq / f64::from(count)).sqrt())
    }

    pub(super) fn recent_recovery_flow_imbalance(&self, at: u64) -> Option<f64> {
        self.online_since?;
        if at.checked_sub(self.last_trade_source?)? > self.policy.max_age_ms {
            return None;
        }
        let slot = at / BUCKET_MS;
        let start_slot = slot.checked_sub(self.policy.recovery_ms / BUCKET_MS)?;
        self.flow(start_slot, slot).map(|(imb, _)| imb)
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
        let (shock_threshold, recovery_threshold) = if let Some(k_shock) = policy.vol_shock_mult {
            let sigma_1s = self.causal_sigma_1s_bps(at)?;
            let (shock_floor, recovery_floor) = policy.effective_vol_floors();
            let eff_shock = ordered_clamp(k_shock * sigma_1s, shock_floor, policy.shock_bps);
            let rec_mult = policy.vol_recovery_mult.unwrap_or(0.35 * k_shock);
            let eff_recovery =
                ordered_clamp(rec_mult * sigma_1s, recovery_floor, policy.recovery_bps);
            (eff_shock, eff_recovery)
        } else {
            (policy.shock_bps, policy.recovery_bps)
        };
        let shock = (pivot.mid / old.mid - 1.0) * 10_000.0;
        let recovery = (now.mid / pivot.mid - 1.0) * 10_000.0;
        Some([
            volume >= policy.min_shock_notional
                && shock <= -shock_threshold
                && recovery >= recovery_threshold
                && shock_flow <= -policy.flow_imbalance
                && recovery_flow >= 0.0,
            volume >= policy.min_shock_notional
                && shock >= shock_threshold
                && recovery <= -recovery_threshold
                && shock_flow >= policy.flow_imbalance
                && recovery_flow <= 0.0,
        ])
    }
}

#[derive(Clone, Copy, Debug)]
pub(in crate::strategy::inventory_neutral_mm) struct LocalBookContext {
    pub bid_touch: f64,
    pub ask_touch: f64,
    pub pricing_mid: f64,
    pub entry_vwap: Option<f64>,
    pub age_fraction: f64,
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
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub cooldown_withdrawn_targets: u64,
    pub rejected_trade_times: u64,
    #[serde(skip)]
    entry_until_ms: [u64; 2],
    #[serde(skip)]
    was_holding_inventory: bool,
    #[serde(skip)]
    post_exit_cooldown_until_ms: u64,
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
            cooldown_withdrawn_targets: 0,
            rejected_trade_times: 0,
            entry_until_ms: [0; 2],
            was_holding_inventory: false,
            post_exit_cooldown_until_ms: 0,
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

    pub(in crate::strategy::inventory_neutral_mm) fn active_touch_sides(
        &self,
        positions: [f64; 2],
        step_size: f64,
    ) -> [bool; 2] {
        let half_step = (step_size * 0.5).max(components::FLOAT_EPSILON);
        if self.policy.exit_touch_offset_bps.is_some() {
            let [bid_closable, ask_closable] =
                components::closable_reducing_sizes(positions, step_size);
            let entry_ok = self.policy.entry_touch_offset_bps.is_some();
            let opposite_signs = (positions[0] >= half_step && positions[1] <= -half_step)
                || (positions[0] <= -half_step && positions[1] >= half_step);
            let bid_entry_touch = entry_ok
                && !opposite_signs
                && bid_closable < half_step
                && positions[1] >= -half_step;
            let ask_entry_touch = entry_ok
                && !opposite_signs
                && ask_closable < half_step
                && positions[1] <= half_step;
            return [
                bid_entry_touch || bid_closable >= half_step,
                ask_entry_touch || ask_closable >= half_step,
            ];
        }
        let bid_touch_override = (positions.iter().all(|&p| p >= 0.0)
            && self.policy.entry_touch_offset_bps.is_some())
            || (positions.iter().all(|&p| p <= -half_step)
                && self.policy.exit_touch_offset_bps.is_some());
        let ask_touch_override = (positions.iter().all(|&p| p <= 0.0)
            && self.policy.entry_touch_offset_bps.is_some())
            || (positions.iter().all(|&p| p >= half_step)
                && self.policy.exit_touch_offset_bps.is_some());
        [bid_touch_override, ask_touch_override]
    }

    pub(in crate::strategy::inventory_neutral_mm) fn is_reducing_exit_active(
        &self,
        positions: [f64; 2],
        step_size: f64,
    ) -> [bool; 2] {
        if self.policy.exit_touch_offset_bps.is_none() {
            return [false, false];
        }
        let [bid_closable, ask_closable] =
            components::closable_reducing_sizes(positions, step_size);
        let half_step = (step_size * 0.5).max(components::FLOAT_EPSILON);
        [bid_closable >= half_step, ask_closable >= half_step]
    }

    #[cfg(test)]
    pub(in crate::strategy::inventory_neutral_mm) fn apply(
        &mut self,
        target: &mut QuoteTarget,
        positions: [f64; 2],
        config: &InventoryNeutralMMConfig,
    ) {
        self.apply_with_context(target, positions, config, None);
    }

    #[cfg(test)]
    pub(in crate::strategy::inventory_neutral_mm) fn apply_with_book(
        &mut self,
        target: &mut QuoteTarget,
        positions: [f64; 2],
        config: &InventoryNeutralMMConfig,
        book_touches_and_fair: Option<(f64, f64, f64)>,
    ) {
        let ctx =
            book_touches_and_fair.map(|(bid_touch, ask_touch, pricing_mid)| LocalBookContext {
                bid_touch,
                ask_touch,
                pricing_mid,
                entry_vwap: None,
                age_fraction: 0.0,
            });
        self.apply_with_context(target, positions, config, ctx);
    }

    pub(in crate::strategy::inventory_neutral_mm) fn apply_with_context(
        &mut self,
        target: &mut QuoteTarget,
        positions: [f64; 2],
        config: &InventoryNeutralMMConfig,
        book_ctx: Option<LocalBookContext>,
    ) {
        self.decisions += 1;
        let at = now_ms();
        let signal = self.signal.lock();
        let ready = signal.allowed(at);
        let recent_flow = self
            .policy
            .hold_exit_while_flow_favorable_min_imbalance
            .and_then(|_| signal.recent_recovery_flow_imbalance(at));
        self.rejected_trade_times = signal.rejected_trade_times;
        drop(signal);
        self.ready_decisions += u64::from(ready.is_some());
        let half_step = (config.step_size * 0.5).max(components::FLOAT_EPSILON);
        let holding_now = positions[0].abs() >= half_step || positions[1].abs() >= half_step;
        if self.policy.post_exit_cooldown_ms > 0 {
            if holding_now {
                self.was_holding_inventory = true;
            } else if self.was_holding_inventory {
                self.was_holding_inventory = false;
                self.entry_until_ms = [0; 2];
                self.post_exit_cooldown_until_ms =
                    at.saturating_add(self.policy.post_exit_cooldown_ms);
            }
        }
        let raw = ready.unwrap_or([false; 2]);
        let in_post_exit_cooldown = self.policy.post_exit_cooldown_ms > 0
            && !holding_now
            && at < self.post_exit_cooldown_until_ms;
        let [buy, sell] = if in_post_exit_cooldown {
            [false, false]
        } else {
            self.with_hold(at, ready)
        };
        // Both account publications and confirmed fills must agree this side
        // cannot reduce inventory when `exit_touch_offset_bps` is disabled.
        let exit_enabled = self.policy.exit_touch_offset_bps.is_some();
        let [bid_closable, ask_closable] =
            components::closable_reducing_sizes(positions, config.step_size);
        let opposite_signs = (positions[0] >= half_step && positions[1] <= -half_step)
            || (positions[0] <= -half_step && positions[1] >= half_step);
        let bid_entry = if exit_enabled {
            !opposite_signs
                && bid_closable < half_step
                && positions[1] >= -half_step
                && target.bid_size >= components::min_quotable_size(config, target.bid_price)
        } else {
            positions.iter().all(|&p| p >= 0.0)
                && target.bid_size >= components::min_quotable_size(config, target.bid_price)
        };
        let bid_withdraw_eligible = bid_entry
            || (exit_enabled
                && opposite_signs
                && target.bid_size >= components::min_quotable_size(config, target.bid_price));
        if bid_withdraw_eligible {
            if buy {
                if bid_entry {
                    self.admitted_bid_targets += 1;
                    self.held_bid_targets += u64::from(!raw[0]);
                }
            } else {
                self.withdrawn_bid_targets += 1;
                if in_post_exit_cooldown && bid_entry {
                    self.cooldown_withdrawn_targets += 1;
                }
                target.bid_size = 0.0;
            }
        }
        let ask_entry = if exit_enabled {
            !opposite_signs
                && ask_closable < half_step
                && positions[1] <= half_step
                && target.ask_size >= components::min_quotable_size(config, target.ask_price)
        } else {
            positions.iter().all(|&p| p <= 0.0)
                && target.ask_size >= components::min_quotable_size(config, target.ask_price)
        };
        let ask_withdraw_eligible = ask_entry
            || (exit_enabled
                && opposite_signs
                && target.ask_size >= components::min_quotable_size(config, target.ask_price));
        if ask_withdraw_eligible {
            if sell {
                if ask_entry {
                    self.admitted_ask_targets += 1;
                    self.held_ask_targets += u64::from(!raw[1]);
                }
            } else {
                self.withdrawn_ask_targets += 1;
                if in_post_exit_cooldown && ask_entry {
                    self.cooldown_withdrawn_targets += 1;
                }
                target.ask_size = 0.0;
            }
        }
        let valid_book = book_ctx.filter(|ctx| {
            ctx.bid_touch.is_finite()
                && ctx.ask_touch.is_finite()
                && ctx.pricing_mid.is_finite()
                && config.tick_size.is_finite()
                && ctx.bid_touch > 0.0
                && ctx.ask_touch > ctx.bid_touch
                && ctx.pricing_mid > 0.0
                && config.tick_size > 0.0
        });
        let clamp_reducing_sizes = |target: &mut QuoteTarget| {
            if exit_enabled && ask_closable >= half_step {
                let capped = components::round_down_to_step(
                    target.ask_size.min(ask_closable),
                    config.step_size,
                );
                target.ask_size = if capped + components::FLOAT_EPSILON
                    < components::min_quotable_size(config, target.ask_price)
                {
                    0.0
                } else {
                    capped
                };
            }
            if exit_enabled && bid_closable >= half_step {
                let capped = components::round_down_to_step(
                    target.bid_size.min(bid_closable),
                    config.step_size,
                );
                target.bid_size = if capped + components::FLOAT_EPSILON
                    < components::min_quotable_size(config, target.bid_price)
                {
                    0.0
                } else {
                    capped
                };
            }
        };
        let Some(ctx) = valid_book else {
            clamp_reducing_sizes(target);
            return;
        };
        let floor_tick = |px: f64| {
            (((px / config.tick_size) + components::FLOAT_EPSILON).floor()) * config.tick_size
        };
        let ceil_tick = |px: f64| {
            (((px / config.tick_size) - components::FLOAT_EPSILON).ceil()) * config.tick_size
        };
        let fee_bounds = MakerPriceBounds::new_asymmetric(
            ctx.pricing_mid,
            ctx.pricing_mid,
            config.maker_fee_bps.max(0.0),
            0.0,
            0.0,
            config.tick_size,
        );
        if let Some(offset_bps) = self.policy.entry_touch_offset_bps {
            if bid_entry && buy {
                let desired_bid = floor_tick(ctx.bid_touch * (1.0 - offset_bps / 10_000.0));
                let one_tick_below_ask = floor_tick(ctx.ask_touch - config.tick_size);
                let fee_max_bid = fee_bounds.map_or_else(
                    || floor_tick(ctx.pricing_mid * (1.0 - config.maker_fee_bps / 10_000.0)),
                    |b| b.max_bid,
                );
                let capped_bid = desired_bid.min(one_tick_below_ask).min(fee_max_bid);
                if capped_bid.is_finite()
                    && capped_bid > target.bid_price
                    && (target.ask_size == 0.0 || capped_bid < target.ask_price)
                {
                    target.bid_price = capped_bid;
                }
            }
            if ask_entry && sell {
                let desired_ask = ceil_tick(ctx.ask_touch * (1.0 + offset_bps / 10_000.0));
                let one_tick_above_bid = ceil_tick(ctx.bid_touch + config.tick_size);
                let fee_min_ask = fee_bounds.map_or_else(
                    || ceil_tick(ctx.pricing_mid * (1.0 + config.maker_fee_bps / 10_000.0)),
                    |b| b.min_ask,
                );
                let capped_ask = desired_ask.max(one_tick_above_bid).max(fee_min_ask);
                if capped_ask.is_finite()
                    && capped_ask < target.ask_price
                    && (target.bid_size == 0.0 || capped_ask > target.bid_price)
                {
                    target.ask_price = capped_ask;
                }
            }
        }
        if let Some(exit_offset_bps) = self.policy.exit_touch_offset_bps {
            let chase_start = self.policy.exit_chase_start_fraction.unwrap_or(0.4);
            let age_f = if ctx.age_fraction.is_finite() {
                ctx.age_fraction.clamp(0.0, 1.0)
            } else {
                0.0
            };
            let base_hold_weight = if age_f <= chase_start {
                1.0
            } else {
                ((1.0 - age_f) / (1.0 - chase_start).max(1e-6)).clamp(0.0, 1.0)
            };
            let side_hold_weight = |is_ask_close: bool| -> f64 {
                if let Some(min_imb) = self.policy.hold_exit_while_flow_favorable_min_imbalance
                    && age_f < 0.85
                    && let Some(flow_imb) = recent_flow
                {
                    let favorable = if is_ask_close {
                        flow_imb >= min_imb
                    } else {
                        flow_imb <= -min_imb
                    };
                    if favorable {
                        return 1.0;
                    }
                }
                base_hold_weight
            };
            let ask_hold_weight = side_hold_weight(true);
            if ask_closable >= half_step && target.ask_size >= config.step_size {
                let one_tick_above_bid = ceil_tick(ctx.bid_touch + config.tick_size);
                let touch_ask = ceil_tick(ctx.ask_touch * (1.0 + exit_offset_bps / 10_000.0))
                    .max(one_tick_above_bid);
                let fee_min_ask = fee_bounds.map_or_else(
                    || {
                        ceil_tick(
                            ctx.pricing_mid * (1.0 + config.maker_fee_bps.max(0.0) / 10_000.0),
                        )
                    },
                    |b| b.min_ask,
                );
                let vwap_floor = match (self.policy.exit_min_profit_bps, ctx.entry_vwap) {
                    (Some(min_prof), Some(vwap)) if vwap.is_finite() && vwap > 0.0 => {
                        let eff_bps =
                            ask_hold_weight * (2.0 * config.maker_fee_bps.max(0.0) + min_prof);
                        ceil_tick(vwap * (1.0 + eff_bps / 10_000.0))
                    }
                    _ => 0.0,
                };
                let desired_ask = if ask_hold_weight <= 0.0 {
                    touch_ask.max(one_tick_above_bid)
                } else {
                    touch_ask.max(fee_min_ask).max(vwap_floor)
                };
                if desired_ask.is_finite()
                    && desired_ask < target.ask_price
                    && (target.bid_size == 0.0 || desired_ask > target.bid_price)
                    && ask_closable + components::FLOAT_EPSILON
                        >= components::min_quotable_size(config, desired_ask)
                {
                    target.ask_price = desired_ask;
                }
            }
            let bid_hold_weight = side_hold_weight(false);
            if bid_closable >= half_step && target.bid_size >= config.step_size {
                let one_tick_below_ask = floor_tick(ctx.ask_touch - config.tick_size);
                let touch_bid = floor_tick(ctx.bid_touch * (1.0 - exit_offset_bps / 10_000.0))
                    .min(one_tick_below_ask);
                let fee_max_bid = fee_bounds.map_or_else(
                    || {
                        floor_tick(
                            ctx.pricing_mid * (1.0 - config.maker_fee_bps.max(0.0) / 10_000.0),
                        )
                    },
                    |b| b.max_bid,
                );
                let vwap_ceil = match (self.policy.exit_min_profit_bps, ctx.entry_vwap) {
                    (Some(min_prof), Some(vwap)) if vwap.is_finite() && vwap > 0.0 => {
                        let eff_bps =
                            bid_hold_weight * (2.0 * config.maker_fee_bps.max(0.0) + min_prof);
                        floor_tick(vwap * (1.0 - eff_bps / 10_000.0))
                    }
                    _ => 0.0,
                };
                let desired_bid = if bid_hold_weight <= 0.0 {
                    touch_bid.min(one_tick_below_ask)
                } else {
                    let base = touch_bid.min(fee_max_bid);
                    if vwap_ceil > 0.0 {
                        base.min(vwap_ceil)
                    } else {
                        base
                    }
                };
                if desired_bid.is_finite()
                    && desired_bid > target.bid_price
                    && (target.ask_size == 0.0 || desired_bid < target.ask_price)
                    && bid_closable + components::FLOAT_EPSILON
                        >= components::min_quotable_size(config, desired_bid)
                {
                    target.bid_price = desired_bid;
                }
            }
            clamp_reducing_sizes(target);
        }
    }
}

#[cfg(test)]
mod tests;

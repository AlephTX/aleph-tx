//! Receipt-causal Arm C buy-side crash pause & resume guard (`research-replay` only).
//! Implements frozen card `CORE-ETH-BUY-CRASH-PAUSE-RESUME-V1` strictly on
//! causal arrival timestamps (`available_at_ms` / `now_ms()`), failing closed on
//! stale/missing local or external evidence while never suppressing sell quotes
//! or reduce-only inventory-closing buys.
use super::*;

fn default_window_ms() -> u64 {
    2_000
}
fn default_max_age_ms() -> u64 {
    1_000
}
fn default_local_ret_shock_bps() -> f64 {
    -6.0
}
fn default_sell_imbalance_max() -> f64 {
    -0.65
}
fn default_min_sell_notional_usd() -> f64 {
    15_000.0
}
fn default_ext_common_shock_bps() -> f64 {
    -4.0
}
fn default_min_divergence_bps() -> f64 {
    -2.5
}
fn default_resume_calm_window_ms() -> u64 {
    2_000
}
fn default_resume_min_pause_ms() -> u64 {
    1_000
}
fn default_resume_imbalance_abs_max() -> f64 {
    0.35
}
fn default_resume_l1_bid_depth_ratio_min() -> f64 {
    0.50
}
fn default_resume_external_reprice_min_bps() -> f64 {
    -2.5
}
fn default_max_pause_ttl_ms() -> u64 {
    15_000
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Policy {
    pub enabled: bool,
    #[serde(default = "default_window_ms")]
    pub window_ms: u64,
    #[serde(default = "default_max_age_ms")]
    pub max_age_ms: u64,
    #[serde(default = "default_local_ret_shock_bps")]
    pub local_ret_shock_bps: f64,
    #[serde(default = "default_sell_imbalance_max")]
    pub sell_imbalance_max: f64,
    #[serde(default = "default_min_sell_notional_usd")]
    pub min_sell_notional_usd: f64,
    #[serde(default = "default_ext_common_shock_bps")]
    pub ext_common_shock_bps: f64,
    #[serde(default = "default_min_divergence_bps")]
    pub min_divergence_bps: f64,
    #[serde(default = "default_resume_calm_window_ms")]
    pub resume_calm_window_ms: u64,
    #[serde(default = "default_resume_min_pause_ms")]
    pub resume_min_pause_ms: u64,
    #[serde(default = "default_resume_imbalance_abs_max")]
    pub resume_imbalance_abs_max: f64,
    #[serde(default = "default_resume_l1_bid_depth_ratio_min")]
    pub resume_l1_bid_depth_ratio_min: f64,
    #[serde(default = "default_resume_external_reprice_min_bps")]
    pub resume_external_reprice_min_bps: f64,
    #[serde(default = "default_max_pause_ttl_ms")]
    pub max_pause_ttl_ms: u64,
}

impl Policy {
    #[cfg(test)]
    pub(super) fn frozen_v1(enabled: bool) -> Self {
        Self {
            enabled,
            window_ms: default_window_ms(),
            max_age_ms: default_max_age_ms(),
            local_ret_shock_bps: default_local_ret_shock_bps(),
            sell_imbalance_max: default_sell_imbalance_max(),
            min_sell_notional_usd: default_min_sell_notional_usd(),
            ext_common_shock_bps: default_ext_common_shock_bps(),
            min_divergence_bps: default_min_divergence_bps(),
            resume_calm_window_ms: default_resume_calm_window_ms(),
            resume_min_pause_ms: default_resume_min_pause_ms(),
            resume_imbalance_abs_max: default_resume_imbalance_abs_max(),
            resume_l1_bid_depth_ratio_min: default_resume_l1_bid_depth_ratio_min(),
            resume_external_reprice_min_bps: default_resume_external_reprice_min_bps(),
            max_pause_ttl_ms: default_max_pause_ttl_ms(),
        }
    }

    pub(super) fn validate(self) -> Result<()> {
        ensure!(
            (200..=10_000).contains(&self.window_ms)
                && (1..=5_000).contains(&self.max_age_ms)
                && self.local_ret_shock_bps.is_finite()
                && (-500.0..=-0.01).contains(&self.local_ret_shock_bps)
                && self.sell_imbalance_max.is_finite()
                && self.sell_imbalance_max > -1.0
                && self.sell_imbalance_max < 0.0
                && self.min_sell_notional_usd.is_finite()
                && self.min_sell_notional_usd > 0.0
                && self.ext_common_shock_bps.is_finite()
                && (-500.0..=-0.01).contains(&self.ext_common_shock_bps)
                && self.min_divergence_bps.is_finite()
                && (-500.0..=0.0).contains(&self.min_divergence_bps)
                && (200..=10_000).contains(&self.resume_calm_window_ms)
                && self.resume_min_pause_ms <= 60_000
                && self.resume_imbalance_abs_max.is_finite()
                && self.resume_imbalance_abs_max > 0.0
                && self.resume_imbalance_abs_max <= 1.0
                && self.resume_l1_bid_depth_ratio_min.is_finite()
                && self.resume_l1_bid_depth_ratio_min > 0.0
                && self.resume_l1_bid_depth_ratio_min <= 1.0
                && self.resume_external_reprice_min_bps.is_finite()
                && (-500.0..=0.0).contains(&self.resume_external_reprice_min_bps)
                && self.max_pause_ttl_ms >= self.resume_min_pause_ms
                && self.max_pause_ttl_ms <= 120_000,
            "invalid crash_pause_filter policy"
        );
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct LocalBookSample {
    available_at_ms: u64,
    book_ts_ms: u64,
    mid: f64,
    bid_usd: f64,
}

#[derive(Clone, Copy)]
struct ExternalSample {
    available_at_ms: u64,
    observed_ms: u64,
    mid: f64,
}

#[derive(Clone, Copy)]
struct TradeSample {
    available_at_ms: u64,
    notional_usd: f64,
    is_buy: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Regime {
    Normal,
    PausedLocalSell {
        trigger_ms: u64,
        trigger_ext_mid: f64,
        pre_shock_bid_usd: f64,
        blocked_resume_recorded: bool,
    },
    CommonInfoShock {
        min_bid_usd: f64,
        stable_ref_ext_mid: f64,
        stable_since_ms: Option<u64>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PermitOutcome {
    Allowed,
    NearMissAllowed,
    PausedLocalSell,
    CommonShock,
    StaleOrMissing,
}

pub(super) struct Signal {
    policy: Policy,
    local_history: VecDeque<LocalBookSample>,
    ext_history: VecDeque<ExternalSample>,
    trade_history: VecDeque<TradeSample>,
    trade_online_since: Option<u64>,
    ext_epoch: u64,
    ext_update_id: u64,
    regime: Regime,
    last_eval_ms: Option<u64>,
    in_stale_fail_closed: bool,
    near_miss_active: bool,

    pub rejected_ext_updates: u64,
    pub ext_resets: u64,
    pub rejected_trade_times: u64,
    pub triggered_local_sell_pressure: u64,
    pub near_miss_below_threshold: u64,
    pub common_shock_triggers: u64,
    pub common_shock_kept_paused_or_blocked_resume: u64,
    pub stale_or_missing_fail_closed_episodes: u64,
    pub resumed_episodes: u64,
    pub ttl_expired_episodes: u64,
}

impl Signal {
    pub(super) fn new(policy: Policy) -> Self {
        Self {
            policy,
            local_history: VecDeque::new(),
            ext_history: VecDeque::new(),
            trade_history: VecDeque::new(),
            trade_online_since: None,
            ext_epoch: 0,
            ext_update_id: 0,
            regime: Regime::Normal,
            last_eval_ms: None,
            in_stale_fail_closed: false,
            near_miss_active: false,
            rejected_ext_updates: 0,
            ext_resets: 0,
            rejected_trade_times: 0,
            triggered_local_sell_pressure: 0,
            near_miss_below_threshold: 0,
            common_shock_triggers: 0,
            common_shock_kept_paused_or_blocked_resume: 0,
            stale_or_missing_fail_closed_episodes: 0,
            resumed_episodes: 0,
            ttl_expired_episodes: 0,
        }
    }

    fn history_retention_ms(&self) -> u64 {
        self.policy
            .window_ms
            .max(self.policy.resume_calm_window_ms)
            .saturating_add(self.policy.max_pause_ttl_ms)
            .saturating_add(self.policy.max_age_ms * 2)
    }

    pub(super) fn feed(&mut self, at_ms: u64, online: bool) {
        if !online {
            self.trade_online_since = None;
            self.trade_history.clear();
        } else if self.trade_online_since.is_none() {
            self.trade_history.clear();
            self.trade_online_since = Some(at_ms);
        }
    }

    pub(super) fn book(&mut self, at_ms: u64, book: &Book) {
        if !book.fresh(at_ms, self.policy.max_age_ms) {
            return;
        }
        let Some((bid, ask)) = book.touch() else {
            return;
        };
        let mid = 0.5 * (bid[0] + ask[0]);
        let bid_usd = bid[0] * bid[1];
        if !mid.is_finite() || mid <= 0.0 || !bid_usd.is_finite() || bid_usd <= 0.0 {
            return;
        }
        self.local_history.push_back(LocalBookSample {
            available_at_ms: at_ms,
            book_ts_ms: book.timestamp_ms,
            mid,
            bid_usd,
        });
        let cutoff = at_ms.saturating_sub(self.history_retention_ms());
        while self
            .local_history
            .front()
            .is_some_and(|s| s.available_at_ms < cutoff)
        {
            self.local_history.pop_front();
        }
    }

    pub(super) fn update_external(
        &mut self,
        at_ms: u64,
        observed_ms: u64,
        epoch: u64,
        mid: Option<f64>,
        update_id: Option<u64>,
    ) -> Result<()> {
        ensure!(
            observed_ms <= at_ms
                && epoch > 0
                && mid.is_none_or(valid_price)
                && mid.is_some() == update_id.is_some()
                && update_id != Some(0),
            "invalid external anchor event for crash_pause_filter"
        );
        if epoch < self.ext_epoch
            || epoch == self.ext_epoch && update_id.is_some_and(|id| id <= self.ext_update_id)
        {
            self.rejected_ext_updates += 1;
            self.ext_history.clear();
            return Ok(());
        }
        if epoch != self.ext_epoch || mid.is_none() {
            self.ext_history.clear();
            if epoch != self.ext_epoch {
                self.ext_update_id = 0;
            }
            self.ext_epoch = epoch;
            self.ext_resets += 1;
        }
        if let (Some(mid), Some(id)) = (mid, update_id) {
            self.ext_update_id = id;
            self.ext_history.push_back(ExternalSample {
                available_at_ms: at_ms,
                observed_ms,
                mid,
            });
            let cutoff = at_ms.saturating_sub(self.history_retention_ms());
            while self
                .ext_history
                .front()
                .is_some_and(|s| s.available_at_ms < cutoff)
            {
                self.ext_history.pop_front();
            }
            ensure!(
                self.ext_history.len() <= 200_000,
                "crash_pause external anchor density exceeds bound"
            );
        }
        Ok(())
    }

    pub(super) fn trade(&mut self, at_ms: u64, source_ms: u64, notional_usd: f64, is_buy: bool) {
        let Some(since) = self.trade_online_since else {
            return;
        };
        if source_ms < since
            || source_ms > at_ms
            || at_ms - source_ms > self.policy.max_age_ms
            || !notional_usd.is_finite()
            || notional_usd <= 0.0
        {
            self.rejected_trade_times += 1;
            return;
        }
        self.trade_history.push_back(TradeSample {
            available_at_ms: at_ms,
            notional_usd,
            is_buy,
        });
        let cutoff = at_ms.saturating_sub(self.history_retention_ms());
        while self
            .trade_history
            .front()
            .is_some_and(|s| s.available_at_ms < cutoff)
        {
            self.trade_history.pop_front();
        }
    }

    fn flow_window(&self, from_exclusive_ms: u64, until_inclusive_ms: u64) -> (f64, f64) {
        let mut buy = 0.0;
        let mut sell = 0.0;
        for tr in self.trade_history.iter().rev() {
            if tr.available_at_ms > until_inclusive_ms {
                continue;
            }
            if tr.available_at_ms <= from_exclusive_ms {
                break;
            }
            if tr.is_buy {
                buy += tr.notional_usd;
            } else {
                sell += tr.notional_usd;
            }
        }
        (buy, sell)
    }

    fn fresh_local_pair(&self, at_ms: u64) -> Option<(LocalBookSample, LocalBookSample, f64)> {
        let cutoff = at_ms.checked_sub(self.policy.window_ms)?;
        let cur = *self
            .local_history
            .iter()
            .rev()
            .find(|s| s.available_at_ms <= at_ms)?;
        if at_ms - cur.available_at_ms > self.policy.max_age_ms
            || at_ms < cur.book_ts_ms
            || at_ms - cur.book_ts_ms > self.policy.max_age_ms
        {
            return None;
        }
        let past = *self
            .local_history
            .iter()
            .rev()
            .find(|s| s.available_at_ms <= cutoff)?;
        if cutoff - past.available_at_ms > self.policy.max_age_ms
            || cutoff < past.book_ts_ms
            || cutoff - past.book_ts_ms > self.policy.max_age_ms
        {
            return None;
        }
        let mut pre_bids: Vec<f64> = self
            .local_history
            .iter()
            .filter(|s| s.available_at_ms >= cutoff && s.available_at_ms <= cutoff + 500)
            .map(|s| s.bid_usd)
            .collect();
        let pre_shock_bid_usd = if pre_bids.is_empty() {
            past.bid_usd
        } else {
            pre_bids.sort_unstable_by(f64::total_cmp);
            pre_bids[pre_bids.len() / 2]
        };
        Some((cur, past, pre_shock_bid_usd))
    }

    fn fresh_external_pair(&self, at_ms: u64) -> Option<(ExternalSample, ExternalSample)> {
        let cutoff = at_ms.checked_sub(self.policy.window_ms)?;
        let cur = *self
            .ext_history
            .iter()
            .rev()
            .find(|s| s.available_at_ms <= at_ms)?;
        if at_ms - cur.available_at_ms > self.policy.max_age_ms
            || at_ms - cur.observed_ms > self.policy.max_age_ms
        {
            return None;
        }
        let past = *self
            .ext_history
            .iter()
            .rev()
            .find(|s| s.available_at_ms <= cutoff)?;
        if cutoff - past.available_at_ms > self.policy.max_age_ms
            || cutoff - past.observed_ms > self.policy.max_age_ms
        {
            return None;
        }
        Some((cur, past))
    }

    pub(super) fn observe(&mut self, at_ms: u64) {
        let _ = self.evaluate(at_ms);
    }

    fn reset_common_shock_stability(&mut self) {
        if let Regime::CommonInfoShock {
            stable_since_ms, ..
        } = &mut self.regime
        {
            *stable_since_ms = None;
        }
    }

    fn evaluate(&mut self, at_ms: u64) -> PermitOutcome {
        if self.last_eval_ms.is_some_and(|prev| at_ms < prev) {
            if !self.in_stale_fail_closed {
                self.in_stale_fail_closed = true;
                self.stale_or_missing_fail_closed_episodes += 1;
            }
            self.near_miss_active = false;
            self.reset_common_shock_stability();
            return PermitOutcome::StaleOrMissing;
        }
        self.last_eval_ms = Some(at_ms);

        let online_ready = self
            .trade_online_since
            .is_some_and(|since| at_ms >= since.saturating_add(self.policy.window_ms));
        let Some((local_cur, local_past, pre_shock_bid_usd)) = (if online_ready {
            self.fresh_local_pair(at_ms)
        } else {
            None
        }) else {
            if !self.in_stale_fail_closed {
                self.in_stale_fail_closed = true;
                self.stale_or_missing_fail_closed_episodes += 1;
            }
            self.near_miss_active = false;
            self.reset_common_shock_stability();
            return PermitOutcome::StaleOrMissing;
        };
        let Some((ext_cur, ext_past)) = self.fresh_external_pair(at_ms) else {
            if !self.in_stale_fail_closed {
                self.in_stale_fail_closed = true;
                self.stale_or_missing_fail_closed_episodes += 1;
            }
            self.near_miss_active = false;
            self.reset_common_shock_stability();
            return PermitOutcome::StaleOrMissing;
        };
        self.in_stale_fail_closed = false;

        let local_ret_bps = (local_cur.mid / local_past.mid - 1.0) * 10_000.0;
        let ext_ret_bps = (ext_cur.mid / ext_past.mid - 1.0) * 10_000.0;
        let div_bps = local_ret_bps - ext_ret_bps;
        let (buy_w, sell_w) = self.flow_window(at_ms.saturating_sub(self.policy.window_ms), at_ms);
        let tot_w = buy_w + sell_w;
        let flow_imb = (tot_w > 0.0).then(|| (buy_w - sell_w) / tot_w);

        let local_sell_hit = local_ret_bps <= self.policy.local_ret_shock_bps
            && flow_imb.is_some_and(|imb| imb <= self.policy.sell_imbalance_max)
            && sell_w >= self.policy.min_sell_notional_usd
            && ext_ret_bps > self.policy.ext_common_shock_bps
            && div_bps <= self.policy.min_divergence_bps;
        let common_shock_hit = local_ret_bps <= self.policy.local_ret_shock_bps && !local_sell_hit;

        let (cb, cs) = self.flow_window(
            at_ms.saturating_sub(self.policy.resume_calm_window_ms),
            at_ms,
        );
        let calm_tot = cb + cs;
        let calm_imb = if calm_tot > 0.0 {
            (cb - cs) / calm_tot
        } else {
            0.0
        };
        let flow_calm = calm_imb.abs() <= self.policy.resume_imbalance_abs_max;

        match self.regime {
            Regime::Normal => {
                if local_sell_hit {
                    self.near_miss_active = false;
                    self.triggered_local_sell_pressure += 1;
                    self.regime = Regime::PausedLocalSell {
                        trigger_ms: at_ms,
                        trigger_ext_mid: ext_cur.mid,
                        pre_shock_bid_usd,
                        blocked_resume_recorded: false,
                    };
                    return PermitOutcome::PausedLocalSell;
                }
                if common_shock_hit {
                    self.near_miss_active = false;
                    self.common_shock_triggers += 1;
                    self.common_shock_kept_paused_or_blocked_resume += 1;
                    self.regime = Regime::CommonInfoShock {
                        min_bid_usd: pre_shock_bid_usd,
                        stable_ref_ext_mid: ext_cur.mid,
                        stable_since_ms: None,
                    };
                    return PermitOutcome::CommonShock;
                }
                let is_near_miss = local_ret_bps <= self.policy.local_ret_shock_bps * 0.5;
                if is_near_miss {
                    if !self.near_miss_active {
                        self.near_miss_below_threshold += 1;
                        self.near_miss_active = true;
                    }
                    return PermitOutcome::NearMissAllowed;
                }
                self.near_miss_active = false;
                PermitOutcome::Allowed
            }
            Regime::PausedLocalSell {
                trigger_ms,
                trigger_ext_mid,
                pre_shock_bid_usd,
                mut blocked_resume_recorded,
            } => {
                self.near_miss_active = false;
                let elapsed = at_ms.saturating_sub(trigger_ms);
                let ext_since_bps = (ext_cur.mid / trigger_ext_mid - 1.0) * 10_000.0;
                let ext_repriced_down = ext_since_bps < self.policy.resume_external_reprice_min_bps;
                if ext_repriced_down && !blocked_resume_recorded {
                    blocked_resume_recorded = true;
                    self.common_shock_kept_paused_or_blocked_resume += 1;
                }
                let depth_ok = local_cur.bid_usd
                    >= pre_shock_bid_usd.max(1.0) * self.policy.resume_l1_bid_depth_ratio_min;
                let all_resume_ok = elapsed >= self.policy.resume_min_pause_ms
                    && !ext_repriced_down
                    && flow_calm
                    && depth_ok
                    && !local_sell_hit
                    && !common_shock_hit
                    && ext_ret_bps > self.policy.ext_common_shock_bps;
                if all_resume_ok {
                    self.resumed_episodes += 1;
                    self.regime = Regime::Normal;
                    return PermitOutcome::Allowed;
                }
                // Never bypass resume safety criteria on TTL expiry: either re-arm
                // if active local sell pressure continues, or hand over to
                // CommonInfoShock retaining depth, flow, and anchor stability guards.
                if elapsed >= self.policy.max_pause_ttl_ms {
                    self.ttl_expired_episodes += 1;
                    if local_sell_hit {
                        self.triggered_local_sell_pressure += 1;
                        self.regime = Regime::PausedLocalSell {
                            trigger_ms: at_ms,
                            trigger_ext_mid: ext_cur.mid,
                            pre_shock_bid_usd,
                            blocked_resume_recorded: false,
                        };
                        return PermitOutcome::PausedLocalSell;
                    }
                    if (common_shock_hit
                        || ext_repriced_down
                        || ext_ret_bps <= self.policy.ext_common_shock_bps)
                        && !blocked_resume_recorded
                    {
                        self.common_shock_kept_paused_or_blocked_resume += 1;
                    }
                    self.regime = Regime::CommonInfoShock {
                        min_bid_usd: pre_shock_bid_usd,
                        stable_ref_ext_mid: ext_cur.mid,
                        stable_since_ms: None,
                    };
                    return PermitOutcome::CommonShock;
                }
                self.regime = Regime::PausedLocalSell {
                    trigger_ms,
                    trigger_ext_mid,
                    pre_shock_bid_usd,
                    blocked_resume_recorded,
                };
                if ext_repriced_down {
                    PermitOutcome::CommonShock
                } else {
                    PermitOutcome::PausedLocalSell
                }
            }
            Regime::CommonInfoShock {
                min_bid_usd,
                mut stable_ref_ext_mid,
                mut stable_since_ms,
            } => {
                self.near_miss_active = false;
                if local_sell_hit {
                    self.triggered_local_sell_pressure += 1;
                    self.regime = Regime::PausedLocalSell {
                        trigger_ms: at_ms,
                        trigger_ext_mid: ext_cur.mid,
                        pre_shock_bid_usd: min_bid_usd.max(pre_shock_bid_usd),
                        blocked_resume_recorded: false,
                    };
                    return PermitOutcome::PausedLocalSell;
                }
                let depth_ok = local_cur.bid_usd
                    >= min_bid_usd.max(1.0) * self.policy.resume_l1_bid_depth_ratio_min;
                let ext_since_stable_bps = (ext_cur.mid / stable_ref_ext_mid - 1.0) * 10_000.0;
                let ext_sliding_down =
                    ext_since_stable_bps < self.policy.resume_external_reprice_min_bps;
                if common_shock_hit
                    || ext_ret_bps <= self.policy.ext_common_shock_bps
                    || ext_sliding_down
                    || !flow_calm
                    || !depth_ok
                {
                    stable_since_ms = None;
                    if ext_sliding_down
                        || ext_ret_bps <= self.policy.ext_common_shock_bps
                        || common_shock_hit
                    {
                        stable_ref_ext_mid = ext_cur.mid;
                    }
                    self.regime = Regime::CommonInfoShock {
                        min_bid_usd,
                        stable_ref_ext_mid,
                        stable_since_ms,
                    };
                    return PermitOutcome::CommonShock;
                }
                if stable_since_ms.is_none() {
                    stable_ref_ext_mid = ext_cur.mid;
                }
                let since = stable_since_ms.unwrap_or(at_ms);
                if at_ms.saturating_sub(since) >= self.policy.resume_calm_window_ms {
                    self.resumed_episodes += 1;
                    self.regime = Regime::Normal;
                    return PermitOutcome::Allowed;
                }
                self.regime = Regime::CommonInfoShock {
                    min_bid_usd,
                    stable_ref_ext_mid,
                    stable_since_ms: Some(since),
                };
                PermitOutcome::CommonShock
            }
        }
    }
}

#[derive(Serialize)]
pub(in crate::strategy::inventory_neutral_mm) struct Participation {
    policy: Policy,
    #[serde(skip)]
    signal: Arc<PlMutex<Signal>>,
    decisions: u64,
    admitted_decisions: u64,
    withdrawn_bid_targets: u64,
    withdrawn_bid_by_local_sell_pressure: u64,
    withdrawn_bid_by_common_shock: u64,
    withdrawn_bid_by_stale_or_missing: u64,
    triggered_local_sell_pressure: u64,
    near_miss_below_threshold: u64,
    common_shock_triggers: u64,
    common_shock_kept_paused_or_blocked_resume: u64,
    stale_or_missing_fail_closed_episodes: u64,
    resumed_episodes: u64,
    ttl_expired_episodes: u64,
    rejected_ext_updates: u64,
    ext_resets: u64,
    rejected_trade_times: u64,
}

impl Participation {
    pub(super) fn new(policy: Policy, signal: Arc<PlMutex<Signal>>) -> Self {
        Self {
            policy,
            signal,
            decisions: 0,
            admitted_decisions: 0,
            withdrawn_bid_targets: 0,
            withdrawn_bid_by_local_sell_pressure: 0,
            withdrawn_bid_by_common_shock: 0,
            withdrawn_bid_by_stale_or_missing: 0,
            triggered_local_sell_pressure: 0,
            near_miss_below_threshold: 0,
            common_shock_triggers: 0,
            common_shock_kept_paused_or_blocked_resume: 0,
            stale_or_missing_fail_closed_episodes: 0,
            resumed_episodes: 0,
            ttl_expired_episodes: 0,
            rejected_ext_updates: 0,
            ext_resets: 0,
            rejected_trade_times: 0,
        }
    }

    pub(in crate::strategy::inventory_neutral_mm) fn apply(
        &mut self,
        target: &mut QuoteTarget,
        positions: [f64; 2],
        config: &InventoryNeutralMMConfig,
    ) -> Option<f64> {
        self.decisions += 1;
        let outcome = {
            let mut signal = self.signal.lock();
            let out = signal.evaluate(now_ms());
            self.triggered_local_sell_pressure = signal.triggered_local_sell_pressure;
            self.near_miss_below_threshold = signal.near_miss_below_threshold;
            self.common_shock_triggers = signal.common_shock_triggers;
            self.common_shock_kept_paused_or_blocked_resume =
                signal.common_shock_kept_paused_or_blocked_resume;
            self.stale_or_missing_fail_closed_episodes =
                signal.stale_or_missing_fail_closed_episodes;
            self.resumed_episodes = signal.resumed_episodes;
            self.ttl_expired_episodes = signal.ttl_expired_episodes;
            self.rejected_ext_updates = signal.rejected_ext_updates;
            self.ext_resets = signal.ext_resets;
            self.rejected_trade_times = signal.rejected_trade_times;
            out
        };
        let allowed = matches!(
            outcome,
            PermitOutcome::Allowed | PermitOutcome::NearMissAllowed
        );
        if allowed {
            self.admitted_decisions += 1;
        }
        if allowed || !self.policy.enabled {
            return None;
        }
        // Give execution the same aggregate cap used for reduction ladders. It
        // also replaces any resting non-reduce-only bids during the pause.
        let [closable_bid, _] = components::closable_reducing_sizes(positions, config.step_size);
        let min_size = components::min_quotable_size(config, target.bid_price);
        let capped_size = target.bid_size.min(closable_bid);
        let capped_size = if capped_size >= min_size {
            capped_size
        } else {
            0.0
        };
        if capped_size < target.bid_size && target.bid_size >= min_size {
            self.withdrawn_bid_targets += 1;
            match outcome {
                PermitOutcome::PausedLocalSell => self.withdrawn_bid_by_local_sell_pressure += 1,
                PermitOutcome::CommonShock => self.withdrawn_bid_by_common_shock += 1,
                PermitOutcome::StaleOrMissing => self.withdrawn_bid_by_stale_or_missing += 1,
                PermitOutcome::Allowed | PermitOutcome::NearMissAllowed => {}
            }
        }
        target.bid_size = capped_size;
        Some(closable_bid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_book(ts: u64, mid: f64, bid_qty: f64) -> Book {
        let half = 0.05;
        Book {
            timestamp_ms: ts,
            bids: vec![[mid - half, bid_qty]],
            asks: vec![[mid + half, bid_qty]],
        }
    }

    #[test]
    fn stale_external_reference_at_return_cutoff_withdraws_permission() {
        let mut signal = Signal::new(Policy::frozen_v1(true));
        signal.feed(1_000, true);
        signal.book(1_700, &mk_book(1_700, 1_000.0, 100.0));
        signal
            .update_external(1_000, 100, 1, Some(1_000.0), Some(1))
            .unwrap();
        signal.book(3_700, &mk_book(3_700, 1_000.0, 100.0));
        signal
            .update_external(3_700, 3_700, 1, Some(1_000.0), Some(2))
            .unwrap();
        assert_eq!(signal.evaluate(3_700), PermitOutcome::StaleOrMissing);
    }

    #[test]
    fn local_sell_pressure_pauses_and_safely_resumes_when_flow_and_depth_recover() {
        let mut s = Signal::new(Policy::frozen_v1(true));
        s.feed(1_000, true);
        // Warm 2s calm window with 100k L1 bid depth ($1000 * 100 = $100,000) and calm ext anchor.
        for t in (1_000..=3_000).step_by(200) {
            s.book(t, &mk_book(t, 1_000.0, 100.0));
            s.update_external(t, t, 1, Some(1_000.0), Some(t)).unwrap();
        }
        assert_eq!(s.evaluate(3_000), PermitOutcome::Allowed);

        // Local drops -8 bp ($1000 -> $999.20), ext drops -1 bp ($1000 -> $999.90),
        // heavy aggressive sells ($20k sell vs $0 buy -> imb -1.0), depth shrinks to $20k.
        s.book(3_500, &mk_book(3_500, 999.2, 20.0));
        s.update_external(3_500, 3_500, 1, Some(999.9), Some(3_500))
            .unwrap();
        s.trade(3_500, 3_495, 20_000.0, false);
        assert_eq!(s.evaluate(3_500), PermitOutcome::PausedLocalSell);
        assert_eq!(s.triggered_local_sell_pressure, 1);

        // At +500ms (t=4000): depth back to $80k & balanced flow, but < resume_min_pause_ms (1000ms) -> stay paused.
        s.book(4_000, &mk_book(4_000, 999.5, 80.0));
        s.update_external(4_000, 4_000, 1, Some(999.95), Some(4_000))
            .unwrap();
        assert_eq!(s.evaluate(4_000), PermitOutcome::PausedLocalSell);

        // Still paused at t=4_600 while recent 2s flow contains the $20k sell burst (|imb| = 1.0 > 0.35).
        s.book(4_600, &mk_book(4_600, 999.6, 80.0));
        s.update_external(4_600, 4_600, 1, Some(999.95), Some(4_600))
            .unwrap();
        assert_eq!(s.evaluate(4_600), PermitOutcome::PausedLocalSell);

        // Once 2s calm window passes without further heavy selling (t=5_600) and depth >= 50%, resumes cleanly.
        s.book(5_600, &mk_book(5_600, 999.7, 60.0));
        s.update_external(5_600, 5_600, 1, Some(999.95), Some(5_600))
            .unwrap();
        assert_eq!(s.evaluate(5_600), PermitOutcome::Allowed);
        assert_eq!(s.resumed_episodes, 1);
    }

    #[test]
    fn common_info_shock_blocks_unsafe_resume_when_external_anchor_reprices_down() {
        let mut s = Signal::new(Policy::frozen_v1(true));
        s.feed(1_000, true);
        for t in (1_000..=3_000).step_by(200) {
            s.book(t, &mk_book(t, 1_000.0, 100.0));
            s.update_external(t, t, 1, Some(1_000.0), Some(t)).unwrap();
        }
        assert_eq!(s.evaluate(3_000), PermitOutcome::Allowed);

        // Initial local sell pressure trigger (div = -5 bp, ext = -2 bp).
        s.book(3_500, &mk_book(3_500, 999.3, 30.0));
        s.update_external(3_500, 3_500, 1, Some(999.8), Some(3_500))
            .unwrap();
        s.trade(3_500, 3_495, 25_000.0, false);
        assert_eq!(s.evaluate(3_500), PermitOutcome::PausedLocalSell);

        // Even after calm local depth refills & 2s trade lull, external anchor drops another -6 bp -> blocked!
        s.book(6_000, &mk_book(6_000, 998.7, 90.0));
        s.update_external(6_000, 6_000, 1, Some(999.2), Some(6_000))
            .unwrap();
        assert_eq!(s.evaluate(6_000), PermitOutcome::CommonShock);
        assert_eq!(s.common_shock_kept_paused_or_blocked_resume, 1);
        assert_eq!(s.resumed_episodes, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn stale_fail_closed_withdraws_buy_entry_only_and_preserves_exits() {
        let _clock = ReplayClock::open(10_000, false);
        let config = InventoryNeutralMMConfig::default();
        let policy = Policy::frozen_v1(true);
        let sig = Arc::new(PlMutex::new(Signal::new(policy)));
        let mut part = Participation::new(policy, Arc::clone(&sig));
        sig.lock().feed(10_000, true);
        sig.lock().book(10_000, &mk_book(10_000, 100.0, 10.0));
        // Missing 2s warmup & missing Binance anchor -> StaleOrMissing fail-closed.
        let mut flat_target = QuoteTarget {
            bid_price: 99.9,
            ask_price: 100.1,
            bid_size: 1.0,
            ask_size: 1.0,
        };
        assert_eq!(part.apply(&mut flat_target, [0.0, 0.0], &config), Some(0.0));
        assert_eq!(flat_target.bid_size, 0.0);
        assert_eq!(flat_target.ask_size, 1.0);
        assert_eq!(part.withdrawn_bid_by_stale_or_missing, 1);

        // With short inventory, BUY is reducing and MUST NOT be suppressed even while stale!
        let mut short_target = QuoteTarget {
            bid_price: 99.9,
            ask_price: 100.1,
            bid_size: 1.0,
            ask_size: 1.0,
        };
        let cap = part
            .apply(&mut short_target, [-0.7, -0.5], &config)
            .expect("paused bid must use the reduce-only execution path");
        assert_eq!(cap, 0.5);
        assert_eq!(short_target.bid_size, 0.5);
        assert_eq!(short_target.ask_size, 1.0);
        let bids = components::build_reducing_grid_plan(
            &config,
            Side::Buy,
            OrderType::PostOnly,
            short_target.bid_price,
            short_target.bid_size,
            cap,
        );
        assert!(!bids.is_empty());
        assert!(bids.iter().all(|order| order.reduce_only));
        assert!(bids.iter().map(|order| order.size).sum::<f64>() <= cap);

        let mut disputed_target = short_target;
        assert_eq!(
            part.apply(&mut disputed_target, [-0.5, 0.5], &config),
            Some(0.0)
        );
        assert_eq!(disputed_target.bid_size, 0.0);
    }

    #[test]
    fn ttl_expiry_with_repriced_external_anchor_hands_off_to_common_shock_and_waits_for_stabilization()
     {
        let mut s = Signal::new(Policy::frozen_v1(true));
        s.feed(1_000, true);
        for t in (1_000..=3_000).step_by(500) {
            s.book(t, &mk_book(t, 1_000.0, 100.0));
            s.update_external(t, t, 1, Some(1_000.0), Some(t)).unwrap();
        }
        assert_eq!(s.evaluate(3_000), PermitOutcome::Allowed);

        // Local sell pressure trigger at t=3_500 (trigger_ext_mid=999.9).
        s.book(3_500, &mk_book(3_500, 999.2, 20.0));
        s.update_external(3_500, 3_500, 1, Some(999.9), Some(3_500))
            .unwrap();
        s.trade(3_500, 3_495, 20_000.0, false);
        assert_eq!(s.evaluate(3_500), PermitOutcome::PausedLocalSell);

        // At t=4_000 sell trade is still in 2s calm window (flow_calm=false); by t=4_500 ext reprices
        // -2.80 bp below trigger_ext_mid (999.9 -> 999.62) and settles at 999.30 (-6.0 bp) while
        // every rolling 2s ext return remains > -4.0 bp. Must NEVER bypass guard at 15s TTL!
        let mut ext_mid = 999.75;
        for t in (4_000..=18_500).step_by(500) {
            ext_mid = match t {
                4_000 => 999.75,
                4_500 => 999.62,
                6_000 => 999.50,
                9_000 => 999.38,
                12_000 => 999.30,
                _ => ext_mid,
            };
            s.book(t, &mk_book(t, ext_mid - 0.1, 80.0));
            s.update_external(t, t, 1, Some(ext_mid), Some(t)).unwrap();
            let out = s.evaluate(t);
            if t == 4_000 {
                assert_eq!(out, PermitOutcome::PausedLocalSell);
            } else {
                assert_eq!(
                    out,
                    PermitOutcome::CommonShock,
                    "must stay paused at t={t} even at/before 15s TTL when ext anchor repriced down"
                );
            }
        }
        assert_eq!(s.ttl_expired_episodes, 1);
        assert_eq!(s.resumed_episodes, 0);

        // Handoff into CommonInfoShock occurred at t=18_500; stabilization requires a full 2s calm hold.
        for t in (19_000..=20_500).step_by(500) {
            s.book(t, &mk_book(t, ext_mid - 0.1, 80.0));
            s.update_external(t, t, 1, Some(ext_mid), Some(t)).unwrap();
            assert_eq!(s.evaluate(t), PermitOutcome::CommonShock);
        }
        s.book(21_000, &mk_book(21_000, ext_mid - 0.1, 80.0));
        s.update_external(21_000, 21_000, 1, Some(ext_mid), Some(21_000))
            .unwrap();
        assert_eq!(s.evaluate(21_000), PermitOutcome::Allowed);
        assert_eq!(s.resumed_episodes, 1);
    }

    #[test]
    fn ttl_expiry_never_bypasses_unrecovered_depth_or_sell_flow() {
        let mut s = Signal::new(Policy::frozen_v1(true));
        s.feed(1_000, true);
        for t in (1_000..=3_000).step_by(500) {
            s.book(t, &mk_book(t, 1_000.0, 100.0));
            s.update_external(t, t, 1, Some(1_000.0), Some(t)).unwrap();
        }
        assert_eq!(s.evaluate(3_000), PermitOutcome::Allowed);

        // Local sell pressure triggers with $100k pre_shock_bid_usd, depth drops to $20k.
        s.book(3_500, &mk_book(3_500, 999.2, 20.0));
        s.update_external(3_500, 3_500, 1, Some(999.9), Some(3_500))
            .unwrap();
        s.trade(3_500, 3_495, 20_000.0, false);
        assert_eq!(s.evaluate(3_500), PermitOutcome::PausedLocalSell);

        // External stays completely flat at 999.9 and flow is calm, but L1 bid depth stays hollowed ($30k < $50k).
        for t in (4_000..=21_000).step_by(500) {
            s.book(t, &mk_book(t, 999.8, 30.0));
            s.update_external(t, t, 1, Some(999.9), Some(t)).unwrap();
            let out = s.evaluate(t);
            if t < 18_500 {
                assert_eq!(out, PermitOutcome::PausedLocalSell);
            } else {
                assert_eq!(
                    out,
                    PermitOutcome::CommonShock,
                    "TTL expiry at t={t} must not bypass missing bid depth"
                );
            }
        }
        assert_eq!(s.ttl_expired_episodes, 1);
        assert_eq!(s.resumed_episodes, 0);

        // Once depth refills >= 50% ($60k >= $50k) and holds for 2s stabilization window, safely resumes.
        for t in (21_500..=23_000).step_by(500) {
            s.book(t, &mk_book(t, 999.85, 60.0));
            s.update_external(t, t, 1, Some(999.9), Some(t)).unwrap();
            assert_eq!(s.evaluate(t), PermitOutcome::CommonShock);
        }
        s.book(23_500, &mk_book(23_500, 999.85, 60.0));
        s.update_external(23_500, 23_500, 1, Some(999.9), Some(23_500))
            .unwrap();
        assert_eq!(s.evaluate(23_500), PermitOutcome::Allowed);
        assert_eq!(s.resumed_episodes, 1);
    }

    #[test]
    fn ttl_handoff_common_shock_resets_stabilization_on_staircase_drop_or_stale_gap() {
        let mut s = Signal::new(Policy::frozen_v1(true));
        s.feed(1_000, true);
        for t in (1_000..=3_000).step_by(500) {
            s.book(t, &mk_book(t, 1_000.0, 100.0));
            s.update_external(t, t, 1, Some(1_000.0), Some(t)).unwrap();
        }
        assert_eq!(s.evaluate(3_000), PermitOutcome::Allowed);

        s.book(3_500, &mk_book(3_500, 999.2, 20.0));
        s.update_external(3_500, 3_500, 1, Some(999.9), Some(3_500))
            .unwrap();
        s.trade(3_500, 3_495, 20_000.0, false);
        assert_eq!(s.evaluate(3_500), PermitOutcome::PausedLocalSell);

        // Keep depth thin to force TTL handoff into CommonInfoShock at t=18_500.
        for t in (4_000..=18_500).step_by(500) {
            s.book(t, &mk_book(t, 999.2, 25.0));
            s.update_external(t, t, 1, Some(999.5), Some(t)).unwrap();
            let _ = s.evaluate(t);
        }
        assert_eq!(s.ttl_expired_episodes, 1);

        // Recover depth & calm flow, begin stabilization at 19_000, then staircase drop -3 bp at 20_000.
        for t in [19_000, 19_500] {
            s.book(t, &mk_book(t, 999.4, 80.0));
            s.update_external(t, t, 1, Some(999.5), Some(t)).unwrap();
            assert_eq!(s.evaluate(t), PermitOutcome::CommonShock);
        }
        s.book(20_000, &mk_book(20_000, 999.1, 80.0));
        s.update_external(20_000, 20_000, 1, Some(999.2), Some(20_000))
            .unwrap();
        assert_eq!(s.evaluate(20_000), PermitOutcome::CommonShock);

        // Next tick restarts stability at 20_500, then at 21_500 book refreshes while external has a brief
        // >1000ms stale gap (missed until 22_200 -> StaleOrMissing resets stability at 22_200 so 22_500
        // cannot piggyback on 20_500).
        s.book(20_500, &mk_book(20_500, 999.15, 80.0));
        s.update_external(20_500, 20_500, 1, Some(999.2), Some(20_500))
            .unwrap();
        assert_eq!(s.evaluate(20_500), PermitOutcome::CommonShock);
        s.book(21_000, &mk_book(21_000, 999.15, 80.0));
        s.update_external(21_000, 21_000, 1, Some(999.2), Some(21_000))
            .unwrap();
        assert_eq!(s.evaluate(21_000), PermitOutcome::CommonShock);
        s.book(21_500, &mk_book(21_500, 999.15, 80.0));
        s.book(22_200, &mk_book(22_200, 999.15, 80.0));
        assert_eq!(s.evaluate(22_200), PermitOutcome::StaleOrMissing);

        // Fresh external samples resume at 22_200; must hold full 2s (until 24_500) before resuming.
        s.update_external(22_200, 22_200, 1, Some(999.2), Some(22_200))
            .unwrap();
        for t in (22_500..=24_000).step_by(500) {
            s.book(t, &mk_book(t, 999.15, 80.0));
            s.update_external(t, t, 1, Some(999.2), Some(t)).unwrap();
            assert_eq!(s.evaluate(t), PermitOutcome::CommonShock);
        }
        s.book(24_500, &mk_book(24_500, 999.15, 80.0));
        s.update_external(24_500, 24_500, 1, Some(999.2), Some(24_500))
            .unwrap();
        assert_eq!(s.evaluate(24_500), PermitOutcome::Allowed);
        assert_eq!(s.resumed_episodes, 1);
    }
}

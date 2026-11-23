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
        entered_ms: u64,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommonShockReason {
    EntryExternalConfirmed,
    EntryLocalCriteriaUnmet,
    LocalPauseExtReprice,
    LocalPauseTtlHandover,
    RecoveryRetriggerOrExtSliding,
    RecoveryFlowOrDepthBlocked,
    RecoveryCalmCountdown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub(in crate::strategy::inventory_neutral_mm) struct CommonShockReasonCounts {
    entry_external_confirmed: u64,
    entry_local_criteria_unmet: u64,
    local_pause_ext_reprice: u64,
    local_pause_ttl_handover: u64,
    recovery_retrigger_or_ext_sliding: u64,
    recovery_flow_or_depth_blocked: u64,
    recovery_calm_countdown: u64,
}

impl CommonShockReasonCounts {
    fn record(&mut self, reason: CommonShockReason) {
        match reason {
            CommonShockReason::EntryExternalConfirmed => self.entry_external_confirmed += 1,
            CommonShockReason::EntryLocalCriteriaUnmet => self.entry_local_criteria_unmet += 1,
            CommonShockReason::LocalPauseExtReprice => self.local_pause_ext_reprice += 1,
            CommonShockReason::LocalPauseTtlHandover => self.local_pause_ttl_handover += 1,
            CommonShockReason::RecoveryRetriggerOrExtSliding => {
                self.recovery_retrigger_or_ext_sliding += 1;
            }
            CommonShockReason::RecoveryFlowOrDepthBlocked => {
                self.recovery_flow_or_depth_blocked += 1;
            }
            CommonShockReason::RecoveryCalmCountdown => self.recovery_calm_countdown += 1,
        }
    }

    #[cfg(test)]
    fn total(&self) -> u64 {
        self.entry_external_confirmed
            + self.entry_local_criteria_unmet
            + self.local_pause_ext_reprice
            + self.local_pause_ttl_handover
            + self.recovery_retrigger_or_ext_sliding
            + self.recovery_flow_or_depth_blocked
            + self.recovery_calm_countdown
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub(in crate::strategy::inventory_neutral_mm) struct CommonShockAttribution {
    signal_entries_external_confirmed: u64,
    signal_entries_local_criteria_unmet: u64,
    signal_entries_unmet_sell_notional: u64,
    signal_entries_unmet_sell_imbalance: u64,
    signal_entries_unmet_divergence: u64,
    signal_entries_from_ttl_handover: u64,
    episodes_started: u64,
    episodes_completed: u64,
    episodes_completed_to_normal: u64,
    episodes_completed_to_local_sell: u64,
    completed_duration_ms_total: u64,
    completed_duration_ms_max: u64,
    active_at_last_eval: bool,
    active_elapsed_ms_at_last_eval: u64,
    decisions_by_reason: CommonShockReasonCounts,
    withdrawn_targets_by_reason: CommonShockReasonCounts,
    would_withdraw_targets_by_reason: CommonShockReasonCounts,
    rejected_target_reduced_to_zero: u64,
    rejected_target_reduced_retaining_reducing_bid: u64,
    rejected_target_retained_full_reducing_bid: u64,
    rejected_target_sub_min_or_zero: u64,
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
    pub signal_entries_external_confirmed: u64,
    pub signal_entries_local_criteria_unmet: u64,
    pub signal_entries_unmet_sell_notional: u64,
    pub signal_entries_unmet_sell_imbalance: u64,
    pub signal_entries_unmet_divergence: u64,
    pub signal_entries_from_ttl_handover: u64,
    pub common_shock_episodes_started: u64,
    pub common_shock_episodes_completed_to_normal: u64,
    pub common_shock_episodes_completed_to_local_sell: u64,
    pub common_shock_completed_duration_ms_total: u64,
    pub common_shock_completed_duration_ms_max: u64,
    pub common_shock_active_at_last_eval: bool,
    pub common_shock_active_elapsed_ms_at_last_eval: u64,
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
            signal_entries_external_confirmed: 0,
            signal_entries_local_criteria_unmet: 0,
            signal_entries_unmet_sell_notional: 0,
            signal_entries_unmet_sell_imbalance: 0,
            signal_entries_unmet_divergence: 0,
            signal_entries_from_ttl_handover: 0,
            common_shock_episodes_started: 0,
            common_shock_episodes_completed_to_normal: 0,
            common_shock_episodes_completed_to_local_sell: 0,
            common_shock_completed_duration_ms_total: 0,
            common_shock_completed_duration_ms_max: 0,
            common_shock_active_at_last_eval: false,
            common_shock_active_elapsed_ms_at_last_eval: 0,
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
        let _ = self.evaluate_detailed(at_ms);
    }

    fn reset_common_shock_stability(&mut self) {
        if let Regime::CommonInfoShock {
            stable_since_ms, ..
        } = &mut self.regime
        {
            *stable_since_ms = None;
        }
    }

    fn start_common_shock_episode(
        &mut self,
        at_ms: u64,
        min_bid_usd: f64,
        stable_ref_ext_mid: f64,
    ) {
        self.common_shock_episodes_started += 1;
        self.regime = Regime::CommonInfoShock {
            entered_ms: at_ms,
            min_bid_usd,
            stable_ref_ext_mid,
            stable_since_ms: None,
        };
        self.sync_active_episode_clock();
    }

    fn complete_common_shock_episode(&mut self, at_ms: u64, entered_ms: u64, to_normal: bool) {
        let dur = at_ms.saturating_sub(entered_ms);
        if to_normal {
            self.common_shock_episodes_completed_to_normal += 1;
        } else {
            self.common_shock_episodes_completed_to_local_sell += 1;
        }
        self.common_shock_completed_duration_ms_total += dur;
        self.common_shock_completed_duration_ms_max =
            self.common_shock_completed_duration_ms_max.max(dur);
    }

    fn sync_active_episode_clock(&mut self) {
        match self.regime {
            Regime::CommonInfoShock { entered_ms, .. } => {
                self.common_shock_active_at_last_eval = true;
                self.common_shock_active_elapsed_ms_at_last_eval = self
                    .last_eval_ms
                    .unwrap_or(entered_ms)
                    .saturating_sub(entered_ms);
            }
            Regime::Normal | Regime::PausedLocalSell { .. } => {
                self.common_shock_active_at_last_eval = false;
                self.common_shock_active_elapsed_ms_at_last_eval = 0;
            }
        }
    }

    #[cfg(test)]
    fn evaluate(&mut self, at_ms: u64) -> PermitOutcome {
        self.evaluate_detailed(at_ms).0
    }

    fn evaluate_detailed(&mut self, at_ms: u64) -> (PermitOutcome, Option<CommonShockReason>) {
        if self.last_eval_ms.is_some_and(|prev| at_ms < prev) {
            if !self.in_stale_fail_closed {
                self.in_stale_fail_closed = true;
                self.stale_or_missing_fail_closed_episodes += 1;
            }
            self.near_miss_active = false;
            self.reset_common_shock_stability();
            self.sync_active_episode_clock();
            return (PermitOutcome::StaleOrMissing, None);
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
            self.sync_active_episode_clock();
            return (PermitOutcome::StaleOrMissing, None);
        };
        let Some((ext_cur, ext_past)) = self.fresh_external_pair(at_ms) else {
            if !self.in_stale_fail_closed {
                self.in_stale_fail_closed = true;
                self.stale_or_missing_fail_closed_episodes += 1;
            }
            self.near_miss_active = false;
            self.reset_common_shock_stability();
            self.sync_active_episode_clock();
            return (PermitOutcome::StaleOrMissing, None);
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
                    self.sync_active_episode_clock();
                    return (PermitOutcome::PausedLocalSell, None);
                }
                if common_shock_hit {
                    self.near_miss_active = false;
                    self.common_shock_triggers += 1;
                    self.common_shock_kept_paused_or_blocked_resume += 1;
                    let reason = if ext_ret_bps <= self.policy.ext_common_shock_bps {
                        self.signal_entries_external_confirmed += 1;
                        CommonShockReason::EntryExternalConfirmed
                    } else {
                        self.signal_entries_local_criteria_unmet += 1;
                        if sell_w < self.policy.min_sell_notional_usd {
                            self.signal_entries_unmet_sell_notional += 1;
                        }
                        if !flow_imb.is_some_and(|imb| imb <= self.policy.sell_imbalance_max) {
                            self.signal_entries_unmet_sell_imbalance += 1;
                        }
                        if div_bps > self.policy.min_divergence_bps {
                            self.signal_entries_unmet_divergence += 1;
                        }
                        CommonShockReason::EntryLocalCriteriaUnmet
                    };
                    self.start_common_shock_episode(at_ms, pre_shock_bid_usd, ext_cur.mid);
                    return (PermitOutcome::CommonShock, Some(reason));
                }
                let is_near_miss = local_ret_bps <= self.policy.local_ret_shock_bps * 0.5;
                if is_near_miss {
                    if !self.near_miss_active {
                        self.near_miss_below_threshold += 1;
                        self.near_miss_active = true;
                    }
                    self.sync_active_episode_clock();
                    return (PermitOutcome::NearMissAllowed, None);
                }
                self.near_miss_active = false;
                self.sync_active_episode_clock();
                (PermitOutcome::Allowed, None)
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
                    self.sync_active_episode_clock();
                    return (PermitOutcome::Allowed, None);
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
                        self.sync_active_episode_clock();
                        return (PermitOutcome::PausedLocalSell, None);
                    }
                    if (common_shock_hit
                        || ext_repriced_down
                        || ext_ret_bps <= self.policy.ext_common_shock_bps)
                        && !blocked_resume_recorded
                    {
                        self.common_shock_kept_paused_or_blocked_resume += 1;
                    }
                    self.signal_entries_from_ttl_handover += 1;
                    self.start_common_shock_episode(at_ms, pre_shock_bid_usd, ext_cur.mid);
                    return (
                        PermitOutcome::CommonShock,
                        Some(CommonShockReason::LocalPauseTtlHandover),
                    );
                }
                self.regime = Regime::PausedLocalSell {
                    trigger_ms,
                    trigger_ext_mid,
                    pre_shock_bid_usd,
                    blocked_resume_recorded,
                };
                self.sync_active_episode_clock();
                if ext_repriced_down {
                    (
                        PermitOutcome::CommonShock,
                        Some(CommonShockReason::LocalPauseExtReprice),
                    )
                } else {
                    (PermitOutcome::PausedLocalSell, None)
                }
            }
            Regime::CommonInfoShock {
                entered_ms,
                min_bid_usd,
                mut stable_ref_ext_mid,
                mut stable_since_ms,
            } => {
                self.near_miss_active = false;
                if local_sell_hit {
                    self.complete_common_shock_episode(at_ms, entered_ms, false);
                    self.triggered_local_sell_pressure += 1;
                    self.regime = Regime::PausedLocalSell {
                        trigger_ms: at_ms,
                        trigger_ext_mid: ext_cur.mid,
                        pre_shock_bid_usd: min_bid_usd.max(pre_shock_bid_usd),
                        blocked_resume_recorded: false,
                    };
                    self.sync_active_episode_clock();
                    return (PermitOutcome::PausedLocalSell, None);
                }
                let depth_ok = local_cur.bid_usd
                    >= min_bid_usd.max(1.0) * self.policy.resume_l1_bid_depth_ratio_min;
                let ext_since_stable_bps = (ext_cur.mid / stable_ref_ext_mid - 1.0) * 10_000.0;
                let ext_sliding_down =
                    ext_since_stable_bps < self.policy.resume_external_reprice_min_bps;
                let ext_slide_or_retrigger = common_shock_hit
                    || ext_ret_bps <= self.policy.ext_common_shock_bps
                    || ext_sliding_down;
                if ext_slide_or_retrigger || !flow_calm || !depth_ok {
                    stable_since_ms = None;
                    if ext_slide_or_retrigger {
                        stable_ref_ext_mid = ext_cur.mid;
                    }
                    self.regime = Regime::CommonInfoShock {
                        entered_ms,
                        min_bid_usd,
                        stable_ref_ext_mid,
                        stable_since_ms,
                    };
                    self.sync_active_episode_clock();
                    let reason = if ext_slide_or_retrigger {
                        CommonShockReason::RecoveryRetriggerOrExtSliding
                    } else {
                        CommonShockReason::RecoveryFlowOrDepthBlocked
                    };
                    return (PermitOutcome::CommonShock, Some(reason));
                }
                if stable_since_ms.is_none() {
                    stable_ref_ext_mid = ext_cur.mid;
                }
                let since = stable_since_ms.unwrap_or(at_ms);
                if at_ms.saturating_sub(since) >= self.policy.resume_calm_window_ms {
                    self.resumed_episodes += 1;
                    self.complete_common_shock_episode(at_ms, entered_ms, true);
                    self.regime = Regime::Normal;
                    self.sync_active_episode_clock();
                    return (PermitOutcome::Allowed, None);
                }
                self.regime = Regime::CommonInfoShock {
                    entered_ms,
                    min_bid_usd,
                    stable_ref_ext_mid,
                    stable_since_ms: Some(since),
                };
                self.sync_active_episode_clock();
                (
                    PermitOutcome::CommonShock,
                    Some(CommonShockReason::RecoveryCalmCountdown),
                )
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
    common_shock_attribution: CommonShockAttribution,
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
            common_shock_attribution: CommonShockAttribution::default(),
        }
    }

    fn sync_signal_counters(&mut self, signal: &Signal) {
        self.triggered_local_sell_pressure = signal.triggered_local_sell_pressure;
        self.near_miss_below_threshold = signal.near_miss_below_threshold;
        self.common_shock_triggers = signal.common_shock_triggers;
        self.common_shock_kept_paused_or_blocked_resume =
            signal.common_shock_kept_paused_or_blocked_resume;
        self.stale_or_missing_fail_closed_episodes = signal.stale_or_missing_fail_closed_episodes;
        self.resumed_episodes = signal.resumed_episodes;
        self.ttl_expired_episodes = signal.ttl_expired_episodes;
        self.rejected_ext_updates = signal.rejected_ext_updates;
        self.ext_resets = signal.ext_resets;
        self.rejected_trade_times = signal.rejected_trade_times;

        let attr = &mut self.common_shock_attribution;
        attr.signal_entries_external_confirmed = signal.signal_entries_external_confirmed;
        attr.signal_entries_local_criteria_unmet = signal.signal_entries_local_criteria_unmet;
        attr.signal_entries_unmet_sell_notional = signal.signal_entries_unmet_sell_notional;
        attr.signal_entries_unmet_sell_imbalance = signal.signal_entries_unmet_sell_imbalance;
        attr.signal_entries_unmet_divergence = signal.signal_entries_unmet_divergence;
        attr.signal_entries_from_ttl_handover = signal.signal_entries_from_ttl_handover;
        attr.episodes_started = signal.common_shock_episodes_started;
        attr.episodes_completed = signal.common_shock_episodes_completed_to_normal
            + signal.common_shock_episodes_completed_to_local_sell;
        attr.episodes_completed_to_normal = signal.common_shock_episodes_completed_to_normal;
        attr.episodes_completed_to_local_sell =
            signal.common_shock_episodes_completed_to_local_sell;
        attr.completed_duration_ms_total = signal.common_shock_completed_duration_ms_total;
        attr.completed_duration_ms_max = signal.common_shock_completed_duration_ms_max;
        attr.active_at_last_eval = signal.common_shock_active_at_last_eval;
        attr.active_elapsed_ms_at_last_eval = signal.common_shock_active_elapsed_ms_at_last_eval;
    }

    pub(in crate::strategy::inventory_neutral_mm) fn apply(
        &mut self,
        target: &mut QuoteTarget,
        positions: [f64; 2],
        config: &InventoryNeutralMMConfig,
    ) -> Option<f64> {
        self.decisions += 1;
        let signal_arc = Arc::clone(&self.signal);
        let (outcome, shock_reason) = {
            let mut signal = signal_arc.lock();
            let evaluated = signal.evaluate_detailed(now_ms());
            self.sync_signal_counters(&signal);
            evaluated
        };
        if let Some(reason) = shock_reason {
            self.common_shock_attribution
                .decisions_by_reason
                .record(reason);
        }
        let allowed = matches!(
            outcome,
            PermitOutcome::Allowed | PermitOutcome::NearMissAllowed
        );
        if allowed {
            self.admitted_decisions += 1;
            return None;
        }
        // Compute pure reducing cap diagnostics for both active and shadow arms
        // without mutating `target.bid_size` or active withdrawal counters in shadow mode.
        let [closable_bid, _] = components::closable_reducing_sizes(positions, config.step_size);
        let min_size = components::min_quotable_size(config, target.bid_price);
        let capped_size = target.bid_size.min(closable_bid);
        let capped_size = if capped_size >= min_size {
            capped_size
        } else {
            0.0
        };
        let would_withdraw = capped_size < target.bid_size && target.bid_size >= min_size;
        if target.bid_size.is_nan() || target.bid_size < min_size {
            self.common_shock_attribution
                .rejected_target_sub_min_or_zero += 1;
        } else if would_withdraw {
            if capped_size <= 0.0 {
                self.common_shock_attribution
                    .rejected_target_reduced_to_zero += 1;
            } else {
                self.common_shock_attribution
                    .rejected_target_reduced_retaining_reducing_bid += 1;
            }
            if let Some(reason) = shock_reason {
                self.common_shock_attribution
                    .would_withdraw_targets_by_reason
                    .record(reason);
            }
        } else {
            self.common_shock_attribution
                .rejected_target_retained_full_reducing_bid += 1;
        }
        if !self.policy.enabled {
            return None;
        }
        // Give execution the same aggregate cap used for reduction ladders. It
        // also replaces any resting non-reduce-only bids during the pause.
        if would_withdraw {
            self.withdrawn_bid_targets += 1;
            match outcome {
                PermitOutcome::PausedLocalSell => self.withdrawn_bid_by_local_sell_pressure += 1,
                PermitOutcome::CommonShock => {
                    self.withdrawn_bid_by_common_shock += 1;
                    if let Some(reason) = shock_reason {
                        self.common_shock_attribution
                            .withdrawn_targets_by_reason
                            .record(reason);
                    }
                }
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

    #[test]
    fn common_shock_attribution_splits_external_confirmed_local_unmet_and_recovery_stages() {
        let mut s = Signal::new(Policy::frozen_v1(true));
        s.feed(1_000, true);
        for t in (1_000..=3_000).step_by(500) {
            s.book(t, &mk_book(t, 1_000.0, 100.0));
            s.update_external(t, t, 1, Some(1_000.0), Some(t)).unwrap();
        }
        assert_eq!(s.evaluate(3_000), PermitOutcome::Allowed);

        // Episode 1: Local-criteria-unmet entry — local drops -7 bp while external stays flat (0 bp),
        // sell flow is small ($5k < $15k) and balanced (+$5k buy vs -$5k sell -> imb 0.0), div = -7 bp.
        s.trade(3_400, 3_400, 5_000.0, true);
        s.trade(3_450, 3_450, 5_000.0, false);
        s.book(3_500, &mk_book(3_500, 999.3, 80.0));
        s.update_external(3_500, 3_500, 1, Some(1_000.0), Some(3_500))
            .unwrap();
        assert_eq!(
            s.evaluate_detailed(3_500),
            (
                PermitOutcome::CommonShock,
                Some(CommonShockReason::EntryLocalCriteriaUnmet)
            )
        );
        assert_eq!(s.signal_entries_local_criteria_unmet, 1);
        assert_eq!(s.signal_entries_external_confirmed, 0);
        assert_eq!(s.signal_entries_unmet_sell_notional, 1);
        assert_eq!(s.signal_entries_unmet_sell_imbalance, 1);
        assert_eq!(s.signal_entries_unmet_divergence, 0);
        assert_eq!(s.common_shock_episodes_started, 1);
        assert!(s.common_shock_active_at_last_eval);
        assert_eq!(s.common_shock_active_elapsed_ms_at_last_eval, 0);

        // Recovery stage 1: RecoveryFlowOrDepthBlocked (depth drops to $20k < $50k while ext stays flat).
        s.book(4_000, &mk_book(4_000, 999.6, 20.0));
        s.update_external(4_000, 4_000, 1, Some(1_000.0), Some(4_000))
            .unwrap();
        assert_eq!(
            s.evaluate_detailed(4_000),
            (
                PermitOutcome::CommonShock,
                Some(CommonShockReason::RecoveryFlowOrDepthBlocked)
            )
        );
        assert_eq!(s.common_shock_active_elapsed_ms_at_last_eval, 500);

        // Recovery stage 2: RecoveryRetriggerOrExtSliding (external drops -3 bp -> ext_sliding_down).
        s.book(4_500, &mk_book(4_500, 999.4, 80.0));
        s.update_external(4_500, 4_500, 1, Some(999.7), Some(4_500))
            .unwrap();
        assert_eq!(
            s.evaluate_detailed(4_500),
            (
                PermitOutcome::CommonShock,
                Some(CommonShockReason::RecoveryRetriggerOrExtSliding)
            )
        );

        // Recovery stage 3: RecoveryCalmCountdown (for 5_500..7_000; note 4_500 trades aged out by 5_500).
        s.book(5_500, &mk_book(5_500, 999.65, 80.0));
        s.update_external(5_500, 5_500, 1, Some(999.7), Some(5_500))
            .unwrap();
        assert_eq!(
            s.evaluate_detailed(5_500),
            (
                PermitOutcome::CommonShock,
                Some(CommonShockReason::RecoveryCalmCountdown)
            )
        );
        for t in [6_000, 6_500, 7_000] {
            s.book(t, &mk_book(t, 999.65, 80.0));
            s.update_external(t, t, 1, Some(999.7), Some(t)).unwrap();
            assert_eq!(
                s.evaluate_detailed(t),
                (
                    PermitOutcome::CommonShock,
                    Some(CommonShockReason::RecoveryCalmCountdown)
                )
            );
        }
        // Recovery completes at t=7_500 (7_500 - 5_500 = 2_000 ms calm). Duration = 7_500 - 3_500 = 4_000 ms.
        s.book(7_500, &mk_book(7_500, 999.65, 80.0));
        s.update_external(7_500, 7_500, 1, Some(999.7), Some(7_500))
            .unwrap();
        assert_eq!(s.evaluate_detailed(7_500), (PermitOutcome::Allowed, None));
        assert!(!s.common_shock_active_at_last_eval);
        assert_eq!(s.common_shock_active_elapsed_ms_at_last_eval, 0);
        assert_eq!(s.common_shock_episodes_completed_to_normal, 1);
        assert_eq!(s.common_shock_completed_duration_ms_total, 4_000);
        assert_eq!(s.common_shock_completed_duration_ms_max, 4_000);

        // Warm 2s calm base up to t=10_000, then Episode 2: ExternalConfirmed (-7 bp local AND -5 bp ext).
        for t in (8_000..=10_000).step_by(500) {
            s.book(t, &mk_book(t, 999.7, 80.0));
            s.update_external(t, t, 1, Some(999.7), Some(t)).unwrap();
        }
        assert_eq!(s.evaluate(10_000), PermitOutcome::Allowed);
        s.book(10_500, &mk_book(10_500, 999.0, 80.0));
        s.update_external(10_500, 10_500, 1, Some(999.2), Some(10_500))
            .unwrap();
        assert_eq!(
            s.evaluate_detailed(10_500),
            (
                PermitOutcome::CommonShock,
                Some(CommonShockReason::EntryExternalConfirmed)
            )
        );
        assert_eq!(s.signal_entries_external_confirmed, 1);
        assert_eq!(s.common_shock_episodes_started, 2);
        assert!(s.common_shock_active_at_last_eval);

        // Also test unmet divergence specifically: first recover Ep2, warm 2s, then heavy sell ($20k)
        // where local -6.5 bp AND ext -5 bp? No, keep ext -3.8 bp (> -4.0 bp) and div = -6.5 - (-3.8) = -2.7 <= -2.5;
        // instead use ext = -4.5 bp -> external confirmed; to hit unmet_divergence keep ext = -3.9 bp (> -4.0 bp),
        // local = -6.1 bp (div = -2.2 > -2.5) with heavy sell ($20k).
        for t in (11_500..=14_500).step_by(500) {
            s.book(t, &mk_book(t, 999.2, 80.0));
            s.update_external(t, t, 1, Some(999.2), Some(t)).unwrap();
            let _ = s.evaluate_detailed(t);
        }
        assert_eq!(s.common_shock_episodes_completed_to_normal, 2);
        for t in (15_000..=17_000).step_by(500) {
            s.book(t, &mk_book(t, 1_000.0, 100.0));
            s.update_external(t, t, 1, Some(1_000.0), Some(t)).unwrap();
        }
        assert_eq!(s.evaluate(17_000), PermitOutcome::Allowed);
        s.trade(17_400, 17_400, 20_000.0, false);
        s.book(17_500, &mk_book(17_500, 999.39, 80.0));
        s.update_external(17_500, 17_500, 1, Some(999.61), Some(17_500))
            .unwrap();
        assert_eq!(
            s.evaluate_detailed(17_500),
            (
                PermitOutcome::CommonShock,
                Some(CommonShockReason::EntryLocalCriteriaUnmet)
            )
        );
        assert_eq!(s.signal_entries_local_criteria_unmet, 2);
        assert_eq!(s.signal_entries_unmet_sell_notional, 1);
        assert_eq!(s.signal_entries_unmet_sell_imbalance, 1);
        assert_eq!(s.signal_entries_unmet_divergence, 1);
    }

    #[test]
    fn common_shock_attribution_tracks_local_pause_ext_reprice_ttl_handover_and_reentry() {
        let mut s = Signal::new(Policy::frozen_v1(true));
        s.feed(1_000, true);
        for t in (1_000..=3_000).step_by(500) {
            s.book(t, &mk_book(t, 1_000.0, 100.0));
            s.update_external(t, t, 1, Some(1_000.0), Some(t)).unwrap();
        }
        assert_eq!(s.evaluate(3_000), PermitOutcome::Allowed);

        // Local sell pressure triggers at t=3_500 (trigger_ext_mid = 999.9).
        s.book(3_500, &mk_book(3_500, 999.2, 20.0));
        s.update_external(3_500, 3_500, 1, Some(999.9), Some(3_500))
            .unwrap();
        s.trade(3_500, 3_495, 20_000.0, false);
        assert_eq!(
            s.evaluate_detailed(3_500),
            (PermitOutcome::PausedLocalSell, None)
        );

        // At t=4_500 (<15s TTL), external reprices down -3.0 bp -> LocalPauseExtReprice (does not start CommonInfoShock episode).
        s.book(4_500, &mk_book(4_500, 999.4, 80.0));
        s.update_external(4_500, 4_500, 1, Some(999.6), Some(4_500))
            .unwrap();
        assert_eq!(
            s.evaluate_detailed(4_500),
            (
                PermitOutcome::CommonShock,
                Some(CommonShockReason::LocalPauseExtReprice)
            )
        );
        assert_eq!(s.common_shock_episodes_started, 0);

        // Hold external repriced & low depth through TTL (t=18_500) -> LocalPauseTtlHandover starts episode!
        for t in (5_000..=18_000).step_by(500) {
            s.book(t, &mk_book(t, 999.4, 25.0));
            s.update_external(t, t, 1, Some(999.6), Some(t)).unwrap();
            let _ = s.evaluate_detailed(t);
        }
        s.book(18_500, &mk_book(18_500, 999.4, 25.0));
        s.update_external(18_500, 18_500, 1, Some(999.6), Some(18_500))
            .unwrap();
        assert_eq!(
            s.evaluate_detailed(18_500),
            (
                PermitOutcome::CommonShock,
                Some(CommonShockReason::LocalPauseTtlHandover)
            )
        );
        assert_eq!(s.signal_entries_from_ttl_handover, 1);
        assert_eq!(s.common_shock_episodes_started, 1);
        assert!(s.common_shock_active_at_last_eval);

        // Fresh local sell pressure hits inside CommonInfoShock at t=21_000 -> completes episode to local_sell (dur=2_500).
        s.book(19_500, &mk_book(19_500, 999.5, 80.0));
        s.update_external(19_500, 19_500, 1, Some(999.6), Some(19_500))
            .unwrap();
        assert_eq!(
            s.evaluate_detailed(19_500),
            (
                PermitOutcome::CommonShock,
                Some(CommonShockReason::RecoveryCalmCountdown)
            )
        );
        s.trade(20_900, 20_900, 25_000.0, false);
        s.book(21_000, &mk_book(21_000, 998.7, 30.0));
        s.update_external(21_000, 21_000, 1, Some(999.5), Some(21_000))
            .unwrap();
        assert_eq!(
            s.evaluate_detailed(21_000),
            (PermitOutcome::PausedLocalSell, None)
        );
        assert!(!s.common_shock_active_at_last_eval);
        assert_eq!(s.common_shock_episodes_completed_to_local_sell, 1);
        assert_eq!(s.common_shock_episodes_completed_to_normal, 0);
        assert_eq!(s.common_shock_completed_duration_ms_total, 2_500);
        assert_eq!(s.common_shock_completed_duration_ms_max, 2_500);
    }

    #[tokio::test(start_paused = true)]
    async fn participation_attribution_partitions_shadow_and_active_reducing_bid_dispositions() {
        let config = InventoryNeutralMMConfig::default();

        for enabled in [false, true] {
            let _clock = ReplayClock::open(1_000, false);
            let policy = Policy::frozen_v1(enabled);
            let sig = Arc::new(PlMutex::new(Signal::new(policy)));
            let mut part = Participation::new(policy, Arc::clone(&sig));
            sig.lock().feed(1_000, true);
            for t in (1_000..=3_000).step_by(500) {
                sig.lock().book(t, &mk_book(t, 1_000.0, 100.0));
                sig.lock()
                    .update_external(t, t, 1, Some(1_000.0), Some(t))
                    .unwrap();
            }
            // Advance replay clock to t=3_000: Admitted normal decision.
            tokio::time::advance(Duration::from_millis(2_000)).await;
            let mut t0 = QuoteTarget {
                bid_price: 999.0,
                ask_price: 1_001.0,
                bid_size: 1.0,
                ask_size: 1.0,
            };
            assert_eq!(part.apply(&mut t0, [0.0, 0.0], &config), None);

            // Advance to t=3_500: External-confirmed CommonShock entry (flat -> reduced_to_zero).
            tokio::time::advance(Duration::from_millis(500)).await;
            sig.lock().book(3_500, &mk_book(3_500, 999.0, 80.0));
            sig.lock()
                .update_external(3_500, 3_500, 1, Some(999.4), Some(3_500))
                .unwrap();
            let mut t1 = QuoteTarget {
                bid_price: 999.0,
                ask_price: 1_001.0,
                bid_size: 1.0,
                ask_size: 1.0,
            };
            let res1 = part.apply(&mut t1, [0.0, 0.0], &config);
            assert_eq!(res1, if enabled { Some(0.0) } else { None });
            assert_eq!(t1.bid_size, if enabled { 0.0 } else { 1.0 });

            // Advance to t=3_600: RecoveryRetriggerOrExtSliding (2s rolling ext return is still -6 bp),
            // partial short [-0.7, -0.5] -> reduced retaining 0.5 reducing bid.
            tokio::time::advance(Duration::from_millis(100)).await;
            sig.lock().book(3_600, &mk_book(3_600, 999.1, 80.0));
            sig.lock()
                .update_external(3_600, 3_600, 1, Some(999.4), Some(3_600))
                .unwrap();
            let mut t2 = QuoteTarget {
                bid_price: 999.0,
                ask_price: 1_001.0,
                bid_size: 1.0,
                ask_size: 1.0,
            };
            let res2 = part.apply(&mut t2, [-0.7, -0.5], &config);
            assert_eq!(res2, if enabled { Some(0.5) } else { None });
            assert_eq!(t2.bid_size, if enabled { 0.5 } else { 1.0 });

            // Advance with signal.observe() across 4_000..=5_500 while anchor holds steady at 999.4.
            // observe() must not inflate Participation's decision or target attribution counts.
            for t in (4_000..=5_500).step_by(500) {
                tokio::time::advance(Duration::from_millis(
                    t - if t == 4_000 { 3_600 } else { t - 500 },
                ))
                .await;
                sig.lock().book(t, &mk_book(t, 999.2, 80.0));
                sig.lock()
                    .update_external(t, t, 1, Some(999.4), Some(t))
                    .unwrap();
                sig.lock().observe(t);
            }

            // Advance to t=5_600: RecoveryCalmCountdown (stable_since_ms = 5_500, elapsed 100ms < 2_000ms),
            // full short [-2.0, -2.0] -> retained full 1.0 reducing bid.
            tokio::time::advance(Duration::from_millis(100)).await;
            sig.lock().book(5_600, &mk_book(5_600, 999.2, 80.0));
            sig.lock()
                .update_external(5_600, 5_600, 1, Some(999.4), Some(5_600))
                .unwrap();
            let mut t3 = QuoteTarget {
                bid_price: 999.0,
                ask_price: 1_001.0,
                bid_size: 1.0,
                ask_size: 1.0,
            };
            let res3 = part.apply(&mut t3, [-2.0, -2.0], &config);
            assert_eq!(res3, if enabled { Some(2.0) } else { None });
            assert_eq!(t3.bid_size, 1.0);

            // Advance to t=5_700: RecoveryCalmCountdown, sub-min bid_size (0.005 < min_size ~0.013) -> sub_min_or_zero.
            tokio::time::advance(Duration::from_millis(100)).await;
            sig.lock().book(5_700, &mk_book(5_700, 999.2, 80.0));
            sig.lock()
                .update_external(5_700, 5_700, 1, Some(999.4), Some(5_700))
                .unwrap();
            let mut t4 = QuoteTarget {
                bid_price: 999.0,
                ask_price: 1_001.0,
                bid_size: 0.005,
                ask_size: 1.0,
            };
            let res4 = part.apply(&mut t4, [0.0, 0.0], &config);
            assert_eq!(res4, if enabled { Some(0.0) } else { None });
            assert_eq!(t4.bid_size, if enabled { 0.0 } else { 0.005 });

            let attr = &part.common_shock_attribution;
            assert_eq!(part.decisions, 5);
            assert_eq!(part.admitted_decisions, 1);
            assert_eq!(attr.signal_entries_external_confirmed, 1);
            assert_eq!(attr.decisions_by_reason.entry_external_confirmed, 1);
            assert_eq!(
                attr.decisions_by_reason.recovery_retrigger_or_ext_sliding,
                1
            );
            assert_eq!(attr.decisions_by_reason.recovery_calm_countdown, 2);
            assert_eq!(attr.decisions_by_reason.total(), 4);
            assert_eq!(attr.rejected_target_reduced_to_zero, 1);
            assert_eq!(attr.rejected_target_reduced_retaining_reducing_bid, 1);
            assert_eq!(attr.rejected_target_retained_full_reducing_bid, 1);
            assert_eq!(attr.rejected_target_sub_min_or_zero, 1);
            assert_eq!(
                attr.rejected_target_reduced_to_zero
                    + attr.rejected_target_reduced_retaining_reducing_bid
                    + attr.rejected_target_retained_full_reducing_bid
                    + attr.rejected_target_sub_min_or_zero,
                part.decisions - part.admitted_decisions
            );
            assert_eq!(
                attr.would_withdraw_targets_by_reason
                    .entry_external_confirmed,
                1
            );
            assert_eq!(
                attr.would_withdraw_targets_by_reason
                    .recovery_retrigger_or_ext_sliding,
                1
            );
            assert_eq!(attr.would_withdraw_targets_by_reason.total(), 2);
            if enabled {
                assert_eq!(part.withdrawn_bid_targets, 2);
                assert_eq!(part.withdrawn_bid_by_common_shock, 2);
                assert_eq!(
                    attr.withdrawn_targets_by_reason,
                    attr.would_withdraw_targets_by_reason
                );
                assert_eq!(
                    attr.withdrawn_targets_by_reason.total(),
                    part.withdrawn_bid_by_common_shock
                );
            } else {
                assert_eq!(part.withdrawn_bid_targets, 0);
                assert_eq!(part.withdrawn_bid_by_common_shock, 0);
                assert_eq!(attr.withdrawn_targets_by_reason.total(), 0);
            }
        }
    }
}

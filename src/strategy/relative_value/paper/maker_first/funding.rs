//! Funding avoidance, source-time witnessed settlement lifecycle (`ARB-IMP-05`),
//! and carry-drift threshold adjustments. Timers are independent of public
//! event arrival so a quiet or missing tape cannot skip a settlement barrier.

use super::*;
use std::collections::{BTreeMap, BTreeSet};

const FUNDING_HOUR_MS: u64 = 3_600_000;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlatAtFunding {
    pub settlement_guard_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct FundingFlatReport {
    pub mode: &'static str,
    pub settlement_guard_ms: u64,
    pub planned_slots_by_leg: [u64; 2],
    pub verified_slots_by_leg: [u64; 2],
    pub pause_windows: usize,
    pub planned_pause_ms: u64,
    pub boundary_checks: usize,
    pub ignored_prestart_prints: usize,
    pub max_trade_delivery_ms: u64,
    pub failure: Option<&'static str>,
}

#[derive(Clone, Copy)]
pub(super) enum TimerKind {
    Drain,
    Check,
    Slot([bool; 2]),
    Resume,
}

pub(super) struct FundingState {
    pub timers: VecDeque<(u64, TimerKind)>,
    pub resume_after_ms: u64,
    pauses: Vec<(u64, u64)>,
    pub report: FundingFlatReport,
}

impl FlatAtFunding {
    pub(super) fn validate(self, config: &MakerFirstConfig) -> Result<()> {
        let guard = self.settlement_guard_ms;
        let cancel = config.transport_ms
            + config.maker_cancel_delay_ms
            + config.trade_delivery_grace_ms
            + config.trade_clock_guard_ms
            + config.fill_notice_ms;
        let close = 2 * config.transport_ms + config.taker_delay_ms.iter().sum::<u64>();
        ensure!(
            guard > config.trade_clock_guard_ms
                && guard <= 60_000
                && config.trade_delivery_grace_ms > 0
                && config.drain_ms > cancel + close
                && config.pair.legs.iter().all(|leg| {
                    leg.funding.interval_ms >= 60_000
                        && config.drain_ms + 2 * guard < leg.funding.interval_ms
                }),
            "invalid flat-at-funding guard or drain budget"
        );
        Ok(())
    }
}

impl FundingState {
    pub fn new(config: &MakerFirstConfig) -> Result<Option<Self>> {
        let Some(policy) = config.flat_at_funding else {
            return Ok(None);
        };
        policy.validate(config)?;
        let guard = policy.settlement_guard_ms;
        let lead = guard + config.drain_ms;
        // Include edge windows: a replay starting just after a slot must stay
        // flat until its guard ends; an upcoming slot may require an early drain.
        let scan_start = config.start_ms.saturating_sub(lead + 1);
        let scan_end = config.end_ms + lead;
        let mut slots = BTreeMap::<u64, [bool; 2]>::new();
        let mut planned = [0; 2];
        for (i, leg) in config.pair.legs.iter().enumerate() {
            let (first, count) = leg
                .funding
                .window(scan_start, scan_end)
                .ok_or_else(|| anyhow::anyhow!("funding schedule overflow"))?;
            for n in 0..count {
                let at = first + n * leg.funding.interval_ms;
                slots.entry(at).or_default()[i] = true;
                planned[i] += u64::from(at > config.start_ms && at <= config.end_ms);
            }
        }
        let mut timers = Vec::new();
        let mut pauses: Vec<(u64, u64)> = Vec::new();
        for (slot, legs) in slots {
            let start = slot.saturating_sub(lead).max(config.start_ms);
            let resume = slot + guard + 1;
            if resume <= config.start_ms || start > config.end_ms {
                continue;
            }
            let end = resume.min(config.end_ms);
            if let Some(last) = pauses.last_mut().filter(|last| start <= last.1) {
                last.1 = last.1.max(end);
            } else {
                pauses.push((start, end));
            }
            timers.push((start, 0, TimerKind::Drain));
            let check = slot.saturating_sub(guard).max(config.start_ms);
            if check <= config.end_ms {
                timers.push((check, 1, TimerKind::Check));
            }
            if slot > config.start_ms && slot <= config.end_ms {
                timers.push((slot, 2, TimerKind::Slot(legs)));
            }
            if resume <= config.end_ms {
                timers.push((resume, 3, TimerKind::Resume));
            }
        }
        timers.sort_by_key(|(at, priority, _)| (*at, *priority));
        Ok(Some(Self {
            report: FundingFlatReport {
                mode: "flat_at_funding_no_payment_imputation",
                settlement_guard_ms: guard,
                planned_slots_by_leg: planned,
                verified_slots_by_leg: [0; 2],
                pause_windows: pauses.len(),
                planned_pause_ms: pauses.iter().map(|(a, b)| b - a).sum(),
                boundary_checks: 0,
                ignored_prestart_prints: 0,
                max_trade_delivery_ms: 0,
                failure: None,
            },
            timers: timers.into_iter().map(|(at, _, kind)| (at, kind)).collect(),
            resume_after_ms: 0,
            pauses,
        }))
    }

    pub fn paused(&self, now: u64) -> bool {
        self.pauses
            .iter()
            .any(|(start, end)| *start <= now && now < *end)
    }

    pub fn next_pause(&self, now: u64) -> Option<u64> {
        self.pauses
            .iter()
            .find_map(|(start, _)| (*start > now).then_some(*start))
    }

    pub fn next_flat_deadline(&self, now: u64) -> Option<u64> {
        self.timers.iter().find_map(|(at, kind)| {
            (*at > now && matches!(kind, TimerKind::Check | TimerKind::Slot(_))).then_some(*at)
        })
    }
}

/// Witnessed `source_time` funding settlement policy (`ARB-IMP-05`). Allows positions
/// to remain open across funding slots when every held slot is reconciled via explicit
/// `WitnessedSettlement` cash bounds after the `trade_delivery_grace_ms` watermark.
///
/// `max_settlement_arrival_lag_ms` is an **inclusive** upper bound relative to the slot
/// boundary (`at_ms <= slot_ms + max_settlement_arrival_lag_ms` is accepted; any event or
/// timer advance strictly past `deadline_ms` without a witness fails closed).
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FundingSettlementPolicy {
    pub settlement_guard_ms: u64,
    pub max_settlement_arrival_lag_ms: u64,
}

impl FundingSettlementPolicy {
    pub(super) fn pre_cancel_lead_ms(self, config: &MakerFirstConfig) -> u64 {
        self.settlement_guard_ms
            .saturating_add(config.transport_ms)
            .saturating_add(config.maker_cancel_delay_ms)
            .saturating_add(config.trade_clock_guard_ms)
    }

    pub(super) fn validate(self, config: &MakerFirstConfig) -> Result<()> {
        let guard = self.settlement_guard_ms;
        ensure!(
            guard > config.trade_clock_guard_ms
                && guard <= 60_000
                && config.trade_delivery_grace_ms > 0
                && self.max_settlement_arrival_lag_ms
                    >= config.trade_delivery_grace_ms + config.trade_clock_guard_ms
                && self.max_settlement_arrival_lag_ms <= 300_000,
            "invalid funding settlement guard or arrival lag"
        );
        validate_funding_guard(guard, &config.pair.legs)?;
        let pre_cancel_lead = self.pre_cancel_lead_ms(config);
        ensure!(
            config.pair.legs.iter().all(|leg| {
                leg.funding.interval_ms >= 60_000
                    && pre_cancel_lead + guard < leg.funding.interval_ms
                    && self.max_settlement_arrival_lag_ms < leg.funding.interval_ms / 2
            }),
            "funding schedule interval too short for settlement lifecycle"
        );
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct FundingLifecycleReport {
    pub mode: &'static str,
    pub settlement_guard_ms: u64,
    pub max_settlement_arrival_lag_ms: u64,
    pub planned_slots_by_leg: [u64; 2],
    pub resolved_slots_by_leg: [u64; 2],
    pub held_slots_by_leg: [u64; 2],
    pub flat_slots_by_leg: [u64; 2],
    pub witnessed_payments: usize,
    pub unattributed_cross_cycle_payments: usize,
    pub late_pre_slot_fills_attributed: usize,
    pub post_slot_fills_excluded: usize,
    pub boundary_guard_blocks: usize,
    pub ignored_prestart_prints: usize,
    pub max_trade_delivery_ms: u64,
    #[serde(with = "rust_decimal::serde::str")]
    pub witnessed_funding_cash_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub witnessed_funding_cash_upper_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub price_pnl_usd: Option<Decimal>,
    pub cash_identity_reconciled: bool,
    pub failure: Option<&'static str>,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct FundingPayout {
    pub cycle_id: Option<usize>,
    pub low: Decimal,
    pub high: Decimal,
}

#[derive(Debug, Clone, Copy)]
struct FillRecord {
    cycle_id: Option<usize>,
    source_ms: u64,
    receipt_ms: u64,
    signed_base: Decimal,
}

#[derive(Debug, Clone, Copy)]
struct PendingSlot {
    slot_ms: u64,
    watermark_ms: u64,
    deadline_ms: u64,
    witness: Option<WitnessedSettlement>,
}

pub(super) struct FundingLifecycleState {
    start_ms: u64,
    end_ms: u64,
    guard_ms: u64,
    schedules: [FundingSchedule; 2],
    guard_windows: Vec<(u64, u64)>,
    timers: VecDeque<u64>,
    base_pos_before_unsettled: [Decimal; 2],
    base_pos_by_cycle_before_unsettled: [BTreeMap<usize, Decimal>; 2],
    unattributed_base_before_unsettled: [Decimal; 2],
    unsettled_fills: [Vec<FillRecord>; 2],
    last_resolved_slot_ms: [u64; 2],
    pending_slots: [VecDeque<PendingSlot>; 2],
    seen_witness_slots: [BTreeSet<u64>; 2],
    pub report: FundingLifecycleReport,
}

impl FundingLifecycleState {
    pub fn new(config: &MakerFirstConfig) -> Result<Option<Self>> {
        let Some(policy) = config.funding_settlement else {
            return Ok(None);
        };
        policy.validate(config)?;
        let guard = policy.settlement_guard_ms;
        let pre_cancel_lead = policy.pre_cancel_lead_ms(config);
        let delivery_watermark_offset = config
            .trade_delivery_grace_ms
            .saturating_add(config.trade_clock_guard_ms);

        let scan_start = config.start_ms.saturating_sub(pre_cancel_lead + 1);
        let scan_end = config.end_ms + pre_cancel_lead;
        let mut all_slots = BTreeSet::new();
        for leg in &config.pair.legs {
            let (first, count) = leg
                .funding
                .window(scan_start, scan_end)
                .ok_or_else(|| anyhow::anyhow!("funding schedule overflow"))?;
            for n in 0..count {
                all_slots.insert(first + n * leg.funding.interval_ms);
            }
        }
        let mut guard_windows: Vec<(u64, u64)> = Vec::new();
        let mut wakeups = Vec::new();
        for slot in all_slots {
            let start = slot.saturating_sub(pre_cancel_lead).max(config.start_ms);
            let end = (slot + guard + 1).min(config.end_ms);
            if end <= config.start_ms || start > config.end_ms {
                continue;
            }
            if let Some(last) = guard_windows.last_mut().filter(|last| start <= last.1) {
                last.1 = last.1.max(end);
            } else {
                guard_windows.push((start, end));
            }
        }
        for &(start, end) in &guard_windows {
            wakeups.push(start);
            if end < config.end_ms {
                wakeups.push(end);
            }
        }

        let mut planned = [0; 2];
        let mut pending_slots: [VecDeque<PendingSlot>; 2] = Default::default();
        for (i, leg) in config.pair.legs.iter().enumerate() {
            let (first, count) = leg
                .funding
                .window(config.start_ms, config.end_ms)
                .ok_or_else(|| anyhow::anyhow!("funding schedule overflow"))?;
            planned[i] = count;
            for n in 0..count {
                let slot_ms = first + n * leg.funding.interval_ms;
                let watermark_ms = slot_ms.saturating_add(delivery_watermark_offset);
                let deadline_ms = slot_ms
                    .saturating_add(policy.max_settlement_arrival_lag_ms)
                    .max(watermark_ms);
                pending_slots[i].push_back(PendingSlot {
                    slot_ms,
                    watermark_ms,
                    deadline_ms,
                    witness: None,
                });
                if watermark_ms <= config.end_ms {
                    wakeups.push(watermark_ms);
                }
                // `deadline_ms` is inclusive; fire expiry check strictly after `deadline_ms`
                // so a witness event delivered at `at_ms == deadline_ms` is accepted first.
                let expiry_wakeup = deadline_ms.saturating_add(1);
                if expiry_wakeup <= config.end_ms {
                    wakeups.push(expiry_wakeup);
                }
            }
        }
        wakeups.sort_unstable();
        wakeups.dedup();

        Ok(Some(Self {
            start_ms: config.start_ms,
            end_ms: config.end_ms,
            guard_ms: guard,
            schedules: [config.pair.legs[0].funding, config.pair.legs[1].funding],
            guard_windows,
            timers: wakeups.into(),
            base_pos_before_unsettled: [Decimal::ZERO; 2],
            base_pos_by_cycle_before_unsettled: Default::default(),
            unattributed_base_before_unsettled: [Decimal::ZERO; 2],
            unsettled_fills: Default::default(),
            last_resolved_slot_ms: [0; 2],
            pending_slots,
            seen_witness_slots: Default::default(),
            report: FundingLifecycleReport {
                mode: "witnessed_source_time_settlement",
                settlement_guard_ms: guard,
                max_settlement_arrival_lag_ms: policy.max_settlement_arrival_lag_ms,
                planned_slots_by_leg: planned,
                resolved_slots_by_leg: [0; 2],
                held_slots_by_leg: [0; 2],
                flat_slots_by_leg: [0; 2],
                witnessed_payments: 0,
                unattributed_cross_cycle_payments: 0,
                late_pre_slot_fills_attributed: 0,
                post_slot_fills_excluded: 0,
                boundary_guard_blocks: 0,
                ignored_prestart_prints: 0,
                max_trade_delivery_ms: 0,
                witnessed_funding_cash_usd: Decimal::ZERO,
                witnessed_funding_cash_upper_usd: Decimal::ZERO,
                price_pnl_usd: None,
                cash_identity_reconciled: false,
                failure: None,
            },
        }))
    }

    pub fn next_timer_due(&self, until: u64) -> Option<u64> {
        self.timers.front().copied().filter(|at| *at <= until)
    }

    pub fn pop_timer_due(&mut self, until: u64) -> Option<u64> {
        let at = self.next_timer_due(until)?;
        self.timers.pop_front();
        Some(at)
    }

    pub fn guard_active(&self, now: u64) -> bool {
        self.guard_windows
            .iter()
            .any(|(start, end)| *start <= now && now < *end)
    }

    pub fn on_fill(
        &mut self,
        leg: usize,
        cycle_id: Option<usize>,
        source_ms: u64,
        receipt_ms: u64,
        signed_base: Decimal,
    ) -> Option<&'static str> {
        if signed_base.is_zero() {
            return None;
        }
        if source_ms <= self.last_resolved_slot_ms[leg] {
            return Some("late_fill_after_funding_watermark");
        }
        if self.schedules[leg].near_payment(source_ms, self.guard_ms) {
            return Some("funding_boundary_ambiguous_fill");
        }
        self.unsettled_fills[leg].push(FillRecord {
            cycle_id,
            source_ms,
            receipt_ms,
            signed_base,
        });
        None
    }

    pub fn on_witness(
        &mut self,
        leg: usize,
        now: u64,
        settlement: WitnessedSettlement,
    ) -> Result<Option<&'static str>> {
        ensure!(
            leg < 2
                && settlement.valid(self.schedules[leg], self.guard_ms)
                && settlement.at_ms > self.start_ms
                && settlement.at_ms <= self.end_ms
                && settlement.at_ms <= now
                && settlement.exchange_time_ms <= now,
            "invalid or non-causal witnessed funding settlement"
        );
        if !self.seen_witness_slots[leg].insert(settlement.at_ms) {
            return Ok(Some("duplicate_funding_settlement"));
        }
        if let Some(slot) = self.pending_slots[leg]
            .iter_mut()
            .find(|slot| slot.slot_ms == settlement.at_ms)
        {
            if now > slot.deadline_ms {
                return Ok(Some("missing_witnessed_funding_settlement"));
            }
            slot.witness = Some(settlement);
        }
        Ok(None)
    }

    pub fn has_unwatermarked_slot(&self, leg: usize, now: u64) -> bool {
        self.pending_slots[leg]
            .iter()
            .any(|slot| slot.slot_ms <= now && now < slot.watermark_ms)
    }

    fn advance_slot_fills(
        &mut self,
        leg: usize,
        slot_ms: u64,
        pos_at_slot: Decimal,
    ) -> Option<usize> {
        let mut late_pre = 0usize;
        let mut post = 0usize;
        let mut retained = Vec::new();
        for fill in self.unsettled_fills[leg].drain(..) {
            if fill.source_ms <= slot_ms {
                if fill.receipt_ms > slot_ms {
                    late_pre += 1;
                }
                if let Some(cid) = fill.cycle_id {
                    let entry = self.base_pos_by_cycle_before_unsettled[leg]
                        .entry(cid)
                        .or_insert(Decimal::ZERO);
                    *entry += fill.signed_base;
                    if entry.is_zero() {
                        self.base_pos_by_cycle_before_unsettled[leg].remove(&cid);
                    }
                } else {
                    self.unattributed_base_before_unsettled[leg] += fill.signed_base;
                }
            } else {
                post += 1;
                retained.push(fill);
            }
        }
        self.unsettled_fills[leg] = retained;
        self.base_pos_before_unsettled[leg] = pos_at_slot;
        self.last_resolved_slot_ms[leg] = slot_ms;
        self.report.late_pre_slot_fills_attributed += late_pre;
        self.report.post_slot_fills_excluded += post;
        if pos_at_slot.is_zero() {
            return None;
        }
        if self.unattributed_base_before_unsettled[leg].is_zero()
            && self.base_pos_by_cycle_before_unsettled[leg].len() == 1
        {
            self.base_pos_by_cycle_before_unsettled[leg]
                .keys()
                .next()
                .copied()
        } else {
            None
        }
    }

    pub fn resolve_ready(
        &mut self,
        now: u64,
        at_cutoff: bool,
    ) -> (Vec<FundingPayout>, Option<&'static str>) {
        let mut payouts = Vec::new();
        for leg in 0..2 {
            while let Some(front) = self.pending_slots[leg].front().copied() {
                if now < front.watermark_ms {
                    break;
                }
                let slot_ms = front.slot_ms;
                let pre_delta: Decimal = self.unsettled_fills[leg]
                    .iter()
                    .filter(|f| f.source_ms <= slot_ms)
                    .map(|f| f.signed_base)
                    .sum();
                let pos_at_slot = self.base_pos_before_unsettled[leg] + pre_delta;
                let deadline_expired = if at_cutoff {
                    now >= front.deadline_ms
                } else {
                    now > front.deadline_ms
                };
                if pos_at_slot.is_zero() {
                    let slot = self.pending_slots[leg]
                        .pop_front()
                        .expect("pending slot front exists");
                    self.advance_slot_fills(leg, slot.slot_ms, pos_at_slot);
                    self.report.resolved_slots_by_leg[leg] += 1;
                    self.report.flat_slots_by_leg[leg] += 1;
                } else if let Some(witness) = front.witness {
                    let slot = self.pending_slots[leg]
                        .pop_front()
                        .expect("pending slot front exists");
                    let (low, high) = witness.cash_bounds(pos_at_slot);
                    let owner_cycle = self.advance_slot_fills(leg, slot.slot_ms, pos_at_slot);
                    self.report.resolved_slots_by_leg[leg] += 1;
                    self.report.held_slots_by_leg[leg] += 1;
                    self.report.witnessed_payments += 1;
                    if owner_cycle.is_none() {
                        self.report.unattributed_cross_cycle_payments += 1;
                    }
                    self.report.witnessed_funding_cash_usd += low;
                    self.report.witnessed_funding_cash_upper_usd += high;
                    payouts.push(FundingPayout {
                        cycle_id: owner_cycle,
                        low,
                        high,
                    });
                } else if deadline_expired {
                    return (payouts, Some("missing_witnessed_funding_settlement"));
                } else {
                    break;
                }
            }
        }
        (payouts, None)
    }
}

/// Carry-drift threshold adjustment policy (`ARB-IMP-05`). Penalizes entries opposing the
/// witnessed hourly carry differential at 100% up to `max_adverse_penalty_bps`, while capping
/// any favorable carry credit via `favorable_credit_fraction` and `max_favorable_credit_bps`.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CarryDriftAdjustment {
    pub expected_hold_ms: u64,
    pub max_rate_age_ms: u64,
    #[serde(with = "rust_decimal::serde::str")]
    pub favorable_credit_fraction: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_favorable_credit_bps: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_adverse_penalty_bps: Decimal,
    #[serde(default)]
    pub require_fresh_rates: bool,
}

impl CarryDriftAdjustment {
    pub(super) fn validate(self, config: &MakerFirstConfig) -> Result<()> {
        let max_bps = Decimal::from(1_000);
        ensure!(
            config.pair.mode.is_pair_hedge()
                && (1..=DAY_MS).contains(&self.expected_hold_ms)
                && (1..=DAY_MS).contains(&self.max_rate_age_ms)
                && (Decimal::ZERO..=Decimal::ONE).contains(&self.favorable_credit_fraction)
                && (Decimal::ZERO..=max_bps).contains(&self.max_favorable_credit_bps)
                && self.max_adverse_penalty_bps > Decimal::ZERO
                && self.max_adverse_penalty_bps <= max_bps,
            "invalid carry drift adjustment policy"
        );
        Ok(())
    }
}

#[derive(Debug, Default, Serialize)]
pub struct CarryDriftReport {
    pub rate_updates_by_leg: [usize; 2],
    pub conflicting_rate_events: usize,
    pub conflicting_rate_blocks: usize,
    pub stale_or_missing_blocks: usize,
    pub adverse_adjustments: usize,
    pub favorable_adjustments: usize,
    pub adverse_carry_blocked_entries: usize,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub min_adjustment_bps: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub max_adjustment_bps: Option<Decimal>,
}

#[derive(Debug, Clone, Copy, Default)]
enum LegRateState {
    #[default]
    Unset,
    Valid {
        source_ms: u64,
        hourly: Decimal,
    },
    Conflicted {
        source_ms: u64,
    },
}

#[derive(Debug, Default)]
pub(super) struct CarryDriftState {
    leg_states: [LegRateState; 2],
    pub report: CarryDriftReport,
}

impl CarryDriftState {
    pub fn observe_rate(
        &mut self,
        leg: usize,
        source_ms: u64,
        now: u64,
        rate_per_settlement: Decimal,
        interval_ms: u64,
    ) -> Result<()> {
        ensure!(
            leg < 2
                && source_ms > 0
                && source_ms <= now
                && interval_ms > 0
                && rate_per_settlement.abs() <= Decimal::ONE,
            "invalid funding rate event"
        );
        let hourly =
            rate_per_settlement * Decimal::from(FUNDING_HOUR_MS) / Decimal::from(interval_ms);
        match self.leg_states[leg] {
            LegRateState::Unset => {
                self.leg_states[leg] = LegRateState::Valid { source_ms, hourly };
                self.report.rate_updates_by_leg[leg] += 1;
            }
            LegRateState::Valid {
                source_ms: prev_ms,
                hourly: prev_hourly,
            } => {
                if source_ms < prev_ms {
                    return Ok(());
                }
                if source_ms == prev_ms {
                    if hourly == prev_hourly {
                        // Duplicate identical observation at the same source_ms is idempotent.
                        return Ok(());
                    }
                    // Same-source_ms conflicting values suspend carry adjustment fail-closed
                    // regardless of arrival order until a strictly newer unambiguous source_ms.
                    self.leg_states[leg] = LegRateState::Conflicted { source_ms };
                    self.report.conflicting_rate_events += 1;
                    return Ok(());
                }
                self.leg_states[leg] = LegRateState::Valid { source_ms, hourly };
                self.report.rate_updates_by_leg[leg] += 1;
            }
            LegRateState::Conflicted {
                source_ms: conflict_ms,
            } => {
                if source_ms <= conflict_ms {
                    if source_ms == conflict_ms {
                        self.report.conflicting_rate_events += 1;
                    }
                    return Ok(());
                }
                self.leg_states[leg] = LegRateState::Valid { source_ms, hourly };
                self.report.rate_updates_by_leg[leg] += 1;
            }
        }
        Ok(())
    }

    pub fn adjustment_bps(
        &self,
        policy: CarryDriftAdjustment,
        maker_leg: usize,
        side: HedgeSide,
        now: u64,
    ) -> std::result::Result<Decimal, &'static str> {
        let [(src_0, rate_0), (src_1, rate_1)] = match self.leg_states {
            [LegRateState::Conflicted { .. }, _] | [_, LegRateState::Conflicted { .. }] => {
                return Err("carry_rate_conflict");
            }
            [
                LegRateState::Valid {
                    source_ms: s0,
                    hourly: r0,
                },
                LegRateState::Valid {
                    source_ms: s1,
                    hourly: r1,
                },
            ] => [(s0, r0), (s1, r1)],
            _ => {
                return if policy.require_fresh_rates {
                    Err("carry_rate_missing_or_stale")
                } else {
                    Ok(Decimal::ZERO)
                };
            }
        };
        if now.saturating_sub(src_0) > policy.max_rate_age_ms
            || now.saturating_sub(src_1) > policy.max_rate_age_ms
        {
            return if policy.require_fresh_rates {
                Err("carry_rate_missing_or_stale")
            } else {
                Ok(Decimal::ZERO)
            };
        }
        let rates = [rate_0, rate_1];
        let j = 1 - maker_leg;
        let net_hourly_frac = match side {
            HedgeSide::Buy => rates[j] - rates[maker_leg],
            HedgeSide::Sell => rates[maker_leg] - rates[j],
        };
        let raw_bps = net_hourly_frac * BPS * Decimal::from(policy.expected_hold_ms)
            / Decimal::from(FUNDING_HOUR_MS);
        let adj = if raw_bps < Decimal::ZERO {
            raw_bps.max(-policy.max_adverse_penalty_bps)
        } else {
            (raw_bps * policy.favorable_credit_fraction).min(policy.max_favorable_credit_bps)
        };
        Ok(adj)
    }

    pub fn record_adjustment(&mut self, adj: Decimal) {
        if adj < Decimal::ZERO {
            self.report.adverse_adjustments += 1;
        } else if adj > Decimal::ZERO {
            self.report.favorable_adjustments += 1;
        }
        self.report.min_adjustment_bps = Some(
            self.report
                .min_adjustment_bps
                .map_or(adj, |prev| prev.min(adj)),
        );
        self.report.max_adjustment_bps = Some(
            self.report
                .max_adjustment_bps
                .map_or(adj, |prev| prev.max(adj)),
        );
    }

    pub fn record_stale_block(&mut self) {
        self.report.stale_or_missing_blocks += 1;
    }

    pub fn record_conflict_block(&mut self) {
        self.report.conflicting_rate_blocks += 1;
    }

    pub fn record_adverse_block(&mut self) {
        self.report.adverse_carry_blocked_entries += 1;
    }
}

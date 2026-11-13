//! Funding avoidance, not funding accounting. Timers are independent of public
//! event arrival so a quiet or missing tape cannot skip a settlement barrier.

use super::*;

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
}

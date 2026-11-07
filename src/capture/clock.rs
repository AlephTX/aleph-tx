use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LighterClockPolicy {
    pub max_rtt_ms: u64,
    pub valid_for_ms: u64,
    pub drift_margin_ms: u64,
}

impl LighterClockPolicy {
    pub(super) fn valid(self) -> bool {
        (1..=2_000).contains(&self.max_rtt_ms)
            && (1..=600_000).contains(&self.valid_for_ms)
            && self.drift_margin_ms <= 100
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ClockInterval {
    pub lower_ms: i64,
    pub upper_ms: i64,
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct LighterClockAudit {
    pub probes: usize,
    pub first_received_at_ms: Option<u64>,
    pub interval: Option<ClockInterval>,
    pub conflicting: bool,
}

impl LighterClockAudit {
    pub(super) fn observe(
        &mut self,
        policy: LighterClockPolicy,
        sent: u64,
        received: u64,
        published: u64,
    ) {
        if self.conflicting
            || sent == 0
            || published == 0
            || received
                .checked_sub(sent)
                .is_none_or(|rtt| rtt > policy.max_rtt_ms)
            || self
                .first_received_at_ms
                .is_some_and(|first| received.abs_diff(first) > policy.valid_for_ms)
        {
            return;
        }
        // The matching subscription publication occurs between write and read.
        // Allow a millisecond for the precision of the recorded clock values.
        let margin = i128::from(policy.drift_margin_ms) + 1;
        let lower = i128::from(published) - i128::from(received) - margin;
        let upper = i128::from(published) - i128::from(sent) + margin;
        let (Ok(lower_ms), Ok(upper_ms)) = (i64::try_from(lower), i64::try_from(upper)) else {
            return;
        };
        let mut interval = ClockInterval { lower_ms, upper_ms };
        if let Some(prior) = self.interval {
            interval.lower_ms = interval.lower_ms.max(prior.lower_ms);
            interval.upper_ms = interval.upper_ms.min(prior.upper_ms);
        }
        self.probes += 1;
        self.first_received_at_ms.get_or_insert(received);
        self.conflicting = interval.lower_ms > interval.upper_ms;
        self.interval = (!self.conflicting).then_some(interval);
    }

    pub(super) fn correction(self, policy: LighterClockPolicy, received: u64) -> Option<u64> {
        let first = self.first_received_at_ms?;
        if received.checked_sub(first)? > policy.valid_for_ms {
            return None;
        }
        let upper = self.interval?.upper_ms.max(0) as u64;
        (upper <= 1_000).then_some(upper)
    }
}

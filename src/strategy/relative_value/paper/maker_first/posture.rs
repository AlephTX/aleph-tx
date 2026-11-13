//! Externally selected turnover entry direction. Full hedging is unchanged. No prediction model lives here.
use super::*;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnoverPosture {
    #[default]
    Neutral,
    LongFirst,
    ShortFirst,
}

impl TurnoverPosture {
    pub(super) fn index(self) -> usize {
        match self {
            Self::Neutral => 0,
            Self::LongFirst => 1,
            Self::ShortFirst => 2,
        }
    }

    pub(super) fn permits(self, side: HedgeSide) -> bool {
        match self {
            Self::Neutral => true,
            Self::LongFirst => side == HedgeSide::Buy,
            Self::ShortFirst => side == HedgeSide::Sell,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnoverPosturePolicy {
    pub initial: TurnoverPosture,
    pub max_control_ttl_ms: u64,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnoverControl {
    pub revision: u64,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub posture: TurnoverPosture,
}

#[derive(Debug, Default, Serialize)]
pub struct TurnoverPostureReport {
    pub controls_accepted: usize,
    pub controls_rejected: usize,
    pub controls_expired: usize,
    pub posture_changes: usize,
    /// Cohorts in neutral / long_first / short_first order. Unfinished cycles
    /// remain in the started count and never become completed zero-cost trades.
    pub cycles_started: [usize; 3],
    pub cycles_completed: [usize; 3],
    pub realized_pnl: [Decimal; 3],
    pub final_posture: TurnoverPosture,
}

#[derive(Default)]
pub(super) struct PostureState {
    pub posture: TurnoverPosture,
    revision: u64,
    expires_at_ms: Option<u64>,
}

impl PostureState {
    pub fn new(policy: Option<TurnoverPosturePolicy>) -> Self {
        Self {
            posture: policy.map_or(TurnoverPosture::Neutral, |p| p.initial),
            ..Self::default()
        }
    }

    pub fn control(&mut self, policy: TurnoverPosturePolicy, at: u64, c: TurnoverControl) -> bool {
        if c.revision <= self.revision
            || c.issued_at_ms > at
            || c.expires_at_ms <= at
            || c.expires_at_ms.saturating_sub(c.issued_at_ms) > policy.max_control_ttl_ms
        {
            return false;
        }
        self.revision = c.revision;
        self.expires_at_ms = Some(c.expires_at_ms);
        self.posture = c.posture;
        true
    }

    pub fn expire(&mut self, at: u64) -> bool {
        if self.expires_at_ms.is_some_and(|t| at >= t) {
            self.expires_at_ms = None;
            self.posture = TurnoverPosture::Neutral;
            true
        } else {
            false
        }
    }
}

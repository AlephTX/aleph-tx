//! Deterministic 5-state defensive regime controller (`OPT-07`).
//!
//! Transitions are monotonic on deterioration (escalating immediately on wide
//! local spreads, sharp short-horizon momentum cascades, or external veto floors)
//! and cooldown-gated on recovery, requiring a fresh uncrossed book and clean
//! tracker/account state before relaxing back toward `Normal`.

use crate::config::DefensePolicyConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum DefenseRegime {
    #[default]
    Normal,
    Caution,
    SideDefense {
        block_buy: bool,
        block_sell: bool,
    },
    ReduceOnly,
    Halt,
}

impl DefenseRegime {
    #[inline]
    pub(super) fn severity(self) -> u8 {
        match self {
            Self::Normal => 0,
            Self::Caution => 1,
            Self::SideDefense { .. } => 2,
            Self::ReduceOnly => 3,
            Self::Halt => 4,
        }
    }

    #[inline]
    pub(super) fn as_telemetry_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Caution => "caution",
            Self::SideDefense {
                block_buy: true,
                block_sell: true,
            } => "side_defense_both",
            Self::SideDefense {
                block_buy: true,
                block_sell: false,
            } => "side_defense_buy",
            Self::SideDefense {
                block_buy: false,
                block_sell: true,
            } => "side_defense_sell",
            Self::SideDefense {
                block_buy: false,
                block_sell: false,
            } => "normal",
            Self::ReduceOnly => "reduce_only",
            Self::Halt => "halt",
        }
    }

    #[inline]
    pub(super) fn combine(self, other: Self) -> Self {
        match (self, other) {
            (
                Self::SideDefense {
                    block_buy: ab,
                    block_sell: as_,
                },
                Self::SideDefense {
                    block_buy: bb,
                    block_sell: bs,
                },
            ) => Self::SideDefense {
                block_buy: ab || bb,
                block_sell: as_ || bs,
            },
            _ if self.severity() >= other.severity() => self,
            _ => other,
        }
    }

    #[inline]
    pub(super) fn escalates_beyond(self, current: Self) -> bool {
        if self.severity() > current.severity() {
            return true;
        }
        matches!(
            (self, current),
            (
                Self::SideDefense {
                    block_buy: nb,
                    block_sell: ns
                },
                Self::SideDefense {
                    block_buy: cb,
                    block_sell: cs
                }
            ) if (nb && !cb) || (ns && !cs)
        )
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct DefenseStateMachine {
    regime: DefenseRegime,
    escalated_at_ms: Option<u64>,
    external_floor: DefenseRegime,
}

impl DefenseStateMachine {
    pub(super) fn regime(&self) -> DefenseRegime {
        self.regime
    }

    /// Optional deterministic external defense floor hook (e.g. for future `OPT-06`
    /// public event/news telemetry). May only restrict risk, never enlarge limits.
    #[allow(dead_code)]
    pub(super) fn set_external_floor(&mut self, floor: DefenseRegime, now_ms: u64) {
        self.external_floor = floor;
        if floor.escalates_beyond(self.regime) {
            self.regime = self.regime.combine(floor);
            self.escalated_at_ms = Some(now_ms);
        }
    }

    pub(super) fn evaluate(
        &mut self,
        policy: Option<DefensePolicyConfig>,
        now_ms: u64,
        local_spread_bps: f64,
        momentum_bps: f64,
        can_recover: bool,
    ) -> DefenseRegime {
        let Some(policy) = policy else {
            self.regime = self.external_floor;
            return self.regime;
        };

        let raw = if !local_spread_bps.is_finite()
            || !momentum_bps.is_finite()
            || local_spread_bps >= policy.halt_spread_bps
        {
            if !momentum_bps.is_finite() {
                DefenseRegime::ReduceOnly
            } else {
                DefenseRegime::Halt
            }
        } else if local_spread_bps >= policy.reduce_only_spread_bps {
            DefenseRegime::ReduceOnly
        } else if momentum_bps.abs() >= policy.side_defense_momentum_bps {
            DefenseRegime::SideDefense {
                block_buy: momentum_bps <= -policy.side_defense_momentum_bps,
                block_sell: momentum_bps >= policy.side_defense_momentum_bps,
            }
        } else if local_spread_bps >= policy.caution_spread_bps {
            DefenseRegime::Caution
        } else {
            DefenseRegime::Normal
        };

        let desired = raw.combine(self.external_floor);
        if desired.escalates_beyond(self.regime) {
            self.regime = self.regime.combine(desired);
            self.escalated_at_ms = Some(now_ms);
        } else if desired == self.regime && self.regime != DefenseRegime::Normal {
            self.escalated_at_ms = Some(now_ms);
        } else if can_recover
            && self
                .escalated_at_ms
                .is_none_or(|ts| now_ms.saturating_sub(ts) >= policy.cooldown_ms)
        {
            self.regime = desired;
            self.escalated_at_ms = (desired != DefenseRegime::Normal).then_some(now_ms);
        }
        self.regime
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> DefensePolicyConfig {
        DefensePolicyConfig {
            caution_spread_bps: 8.0,
            caution_widen_bps: 2.0,
            caution_size_scale: 0.5,
            side_defense_momentum_bps: 6.0,
            reduce_only_spread_bps: 20.0,
            halt_spread_bps: 45.0,
            cooldown_ms: 3_000,
        }
    }

    #[test]
    fn defense_machine_escalates_immediately_and_holds_until_cooldown_with_clean_book() {
        let mut sm = DefenseStateMachine::default();
        let pol = Some(policy());

        assert_eq!(
            sm.evaluate(pol, 1_000, 2.0, 0.5, true),
            DefenseRegime::Normal
        );
        // Immediate escalation into SideDefense on downward momentum shock.
        assert_eq!(
            sm.evaluate(pol, 1_100, 3.0, -7.5, true),
            DefenseRegime::SideDefense {
                block_buy: true,
                block_sell: false,
            }
        );
        // Reversing momentum without recovery adds ask block via combine instead of prematurely clearing buy block.
        assert_eq!(
            sm.evaluate(pol, 1_200, 3.0, 7.5, true),
            DefenseRegime::SideDefense {
                block_buy: true,
                block_sell: true,
            }
        );
        // Escalate straight to Halt on spread blowout.
        assert_eq!(
            sm.evaluate(pol, 1_300, 50.0, 0.0, true),
            DefenseRegime::Halt
        );
        // Even if spread normalizes at 4_299ms (< 3000ms), stay in Halt.
        assert_eq!(sm.evaluate(pol, 4_299, 1.0, 0.0, true), DefenseRegime::Halt);
        // Even past cooldown, if `can_recover == false`, remain in Halt.
        assert_eq!(
            sm.evaluate(pol, 5_000, 1.0, 0.0, false),
            DefenseRegime::Halt
        );
        // With `can_recover == true` and cooldown elapsed, recover cleanly to Normal.
        assert_eq!(
            sm.evaluate(pol, 5_100, 1.0, 0.0, true),
            DefenseRegime::Normal
        );
    }

    #[test]
    fn external_defense_floor_overrides_normal_state() {
        let mut sm = DefenseStateMachine::default();
        let pol = Some(policy());
        sm.set_external_floor(DefenseRegime::ReduceOnly, 10_000);
        assert_eq!(
            sm.evaluate(pol, 20_000, 1.0, 0.0, true),
            DefenseRegime::ReduceOnly
        );
        sm.set_external_floor(DefenseRegime::Normal, 20_000);
        assert_eq!(
            sm.evaluate(pol, 24_000, 1.0, 0.0, true),
            DefenseRegime::Normal
        );
    }
}

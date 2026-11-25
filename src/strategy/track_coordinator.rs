//! Cross-track capital isolation, self-trade prevention, and scoped tail-risk supervisor
//! enforcing `AGENTS.md` non-cross-subsidy rules across concurrent Scenarios 1 / 2 / 3.
//!
//! Contains ZERO imports from `crate::exchanges::*`: coordinates independent per-track
//! [`DailyLossGuard`] state alongside [`evaluate_tail_risk`], ensuring profits in one
//! strategy track never rescue losses in another and venue-local incidents remain isolated
//! while global stablecoin/collateral incidents escalate monotonically.

use crate::daily_loss::DailyLossGuard;
use crate::strategy::quote_pipeline::StrategyMode;
use crate::strategy::tail_risk::{
    StablecoinPegSample, TailRiskAction, TailRiskAssessment, TailRiskConfig, VenueMarginSnapshot,
    evaluate_tail_risk,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};

fn sim_utc_day(now_ms: u64) -> i32 {
    let days_since_unix = (now_ms / 86_400_000).min(i32::MAX as u64 - 719_163) as i32;
    719_163 + days_since_unix
}

fn normalize_key(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

/// Static registration describing one isolated strategy track and its risk domains.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackSpec {
    pub track_id: String,
    pub mode: StrategyMode,
    /// Distinct venue/clearing domain names touched by this track (e.g. `["lighter_core"]`).
    pub venue_domains: Vec<String>,
    /// Canonical market identifier guarded against cross-track wash/STP overlap.
    pub market_key: String,
    pub allocated_capital_usd: f64,
    pub daily_loss_limit_pct: f64,
    pub max_gross_notional_usd: f64,
}

impl TrackSpec {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.track_id.trim().is_empty(),
            "track_id must be non-empty"
        );
        ensure!(
            !self.market_key.trim().is_empty(),
            "track '{}' market_key must be non-empty",
            self.track_id
        );
        ensure!(
            !self.venue_domains.is_empty(),
            "track '{}' must declare at least one venue_domain",
            self.track_id
        );
        let mut seen_domains = HashSet::new();
        for domain in &self.venue_domains {
            let norm = normalize_key(domain);
            ensure!(
                !norm.is_empty(),
                "track '{}' contains empty venue_domain",
                self.track_id
            );
            ensure!(
                seen_domains.insert(norm),
                "track '{}' contains duplicate venue_domain '{}'",
                self.track_id,
                domain
            );
        }
        ensure!(
            self.allocated_capital_usd.is_finite() && self.allocated_capital_usd > 0.0,
            "track '{}' allocated_capital_usd must be finite and > 0",
            self.track_id
        );
        ensure!(
            self.daily_loss_limit_pct.is_finite()
                && self.daily_loss_limit_pct > 0.0
                && self.daily_loss_limit_pct < 1.0,
            "track '{}' daily_loss_limit_pct must be finite and in (0, 1)",
            self.track_id
        );
        ensure!(
            self.max_gross_notional_usd.is_finite() && self.max_gross_notional_usd > 0.0,
            "track '{}' max_gross_notional_usd must be finite and > 0",
            self.track_id
        );
        Ok(())
    }
}

/// Real-time mark-to-market equity observation for one registered strategy track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackEquityInput {
    pub track_id: String,
    pub equity_usd: f64,
}

/// Per-track runtime sizing & flattening directive emitted by [`TrackCoordinator::evaluate_fleet`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackAdmission {
    pub track_id: String,
    pub mode: StrategyMode,
    pub track_risk_scale: f64,
    pub effective_risk_scale: f64,
    pub allow_new_entries: bool,
    pub force_flatten: bool,
    pub tail_action: TailRiskAction,
    pub reasons: Vec<String>,
}

/// Combined fleet-wide tail-risk and isolated per-track admission verdict.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FleetRiskReport {
    pub now_ms: u64,
    pub global_tail_assessment: TailRiskAssessment,
    pub tracks: Vec<TrackAdmission>,
}

struct RegisteredTrack {
    spec: TrackSpec,
    normalized_domains: BTreeSet<String>,
    guard: DailyLossGuard,
}

/// Supervises independent capital/loss budgets across concurrent Scenarios 1, 2, and 3.
pub struct TrackCoordinator {
    tail_risk_cfg: TailRiskConfig,
    tracks: Vec<RegisteredTrack>,
    track_index: HashMap<String, usize>,
    last_eval_ms: Option<u64>,
}

impl TrackCoordinator {
    /// Construct an isolated fleet supervisor, failing closed on any duplicate track ID,
    /// invalid configuration, or `(venue_domain, market_key)` self-trade overlap.
    pub fn new(
        tail_risk_cfg: TailRiskConfig,
        specs: Vec<TrackSpec>,
        initial_utc_day: i32,
    ) -> Result<Self> {
        tail_risk_cfg.validate()?;
        ensure!(!specs.is_empty(), "at least one track must be registered");

        let mut tracks = Vec::with_capacity(specs.len());
        let mut track_index = HashMap::with_capacity(specs.len());
        let mut owned_markets: HashSet<(String, String)> = HashSet::new();

        for spec in specs {
            spec.validate()?;
            ensure!(
                !track_index.contains_key(&spec.track_id),
                "duplicate track_id '{}'",
                spec.track_id
            );
            let market_norm = normalize_key(&spec.market_key);
            let mut normalized_domains = BTreeSet::new();
            for domain in &spec.venue_domains {
                let d_norm = normalize_key(domain);
                ensure!(
                    owned_markets.insert((d_norm.clone(), market_norm.clone())),
                    "cross-track venue/market overlap on ({}, {}) violates isolation",
                    domain,
                    spec.market_key
                );
                normalized_domains.insert(d_norm);
            }
            let guard = DailyLossGuard::open(
                None,
                spec.daily_loss_limit_pct,
                spec.allocated_capital_usd,
                initial_utc_day,
            )
            .with_context(|| format!("open DailyLossGuard for track '{}'", spec.track_id))?;

            let idx = tracks.len();
            track_index.insert(spec.track_id.clone(), idx);
            tracks.push(RegisteredTrack {
                spec,
                normalized_domains,
                guard,
            });
        }

        Ok(Self {
            tail_risk_cfg,
            tracks,
            track_index,
            last_eval_ms: None,
        })
    }

    /// Evaluate isolated per-track drawdown brakes and scoped global tail-risk gates.
    ///
    /// Performs ALL timestamp, equity coverage, and tail-risk validations upfront before
    /// mutating ANY track [`DailyLossGuard`] or `self.last_eval_ms` so rejected calls
    /// never leave partial side effects.
    pub fn evaluate_fleet(
        &mut self,
        now_ms: u64,
        equities: &[TrackEquityInput],
        venues: &[VenueMarginSnapshot],
        pegs: &[StablecoinPegSample],
    ) -> Result<FleetRiskReport> {
        ensure!(now_ms > 0, "now_ms must be > 0");
        if let Some(prev) = self.last_eval_ms {
            ensure!(
                now_ms >= prev,
                "regressed coordinator timestamp {now_ms} < {prev}"
            );
        }

        // Upfront exact 1-to-1 equity mapping check before any mutation.
        let mut equity_by_idx: Vec<Option<f64>> = vec![None; self.tracks.len()];
        for input in equities {
            let Some(&idx) = self.track_index.get(&input.track_id) else {
                bail!("unknown track_id '{}' in equities input", input.track_id);
            };
            ensure!(
                input.equity_usd.is_finite(),
                "track '{}' equity_usd must be finite",
                input.track_id
            );
            if equity_by_idx[idx].is_some() {
                bail!("duplicate equity_usd entry for track '{}'", input.track_id);
            }
            equity_by_idx[idx] = Some(input.equity_usd);
        }
        let mut resolved_equities = Vec::with_capacity(self.tracks.len());
        for (idx, opt_eq) in equity_by_idx.into_iter().enumerate() {
            let eq = opt_eq.with_context(|| {
                format!(
                    "missing equity_usd for registered track '{}'",
                    self.tracks[idx].spec.track_id
                )
            })?;
            resolved_equities.push(eq);
        }

        let global_tail = evaluate_tail_risk(now_ms, venues, pegs, &self.tail_risk_cfg)?;
        let eval_day = sim_utc_day(now_ms);
        for track in &self.tracks {
            let guard_ce_day = 719_163
                + (track
                    .guard
                    .snapshot()
                    .daily_loss_day_start_ms
                    .div_euclid(86_400_000) as i32);
            ensure!(
                eval_day >= guard_ce_day,
                "regressed UTC day {eval_day} < track '{}' guard day {guard_ce_day}",
                track.spec.track_id
            );
        }

        // Now all validations succeeded -> update isolated per-track guards atomically.
        self.last_eval_ms = Some(now_ms);
        let present_domains: HashSet<String> =
            venues.iter().map(|v| normalize_key(&v.venue)).collect();
        let mut admissions = Vec::with_capacity(self.tracks.len());

        for (track, equity_usd) in self.tracks.iter_mut().zip(resolved_equities) {
            let breach_err = track.guard.check(equity_usd, eval_day).err();
            let track_scale = if breach_err.is_some() {
                0.0
            } else {
                track.guard.risk_scale()
            };

            let mut scoped_action = TailRiskAction::Normal;
            let mut reasons = Vec::new();
            if track_scale <= 0.0 {
                scoped_action = TailRiskAction::HaltAndFlatten;
                reasons.push("track_daily_loss_breached".to_string());
            } else if track_scale < 1.0 - 1e-12 {
                reasons.push(format!(
                    "track_drawdown_brake_tier:{}",
                    track.guard.brake_tier()
                ));
            }

            for reason in &global_tail.reasons {
                let (applies, severity) =
                    classify_reason_for_track(reason, &track.normalized_domains);
                if applies {
                    scoped_action = scoped_action.max(severity);
                    reasons.push(reason.clone());
                }
            }
            for required_domain in &track.normalized_domains {
                if !present_domains.contains(required_domain) {
                    scoped_action = scoped_action.max(TailRiskAction::ReduceOnly);
                    reasons.push(format!("missing_venue_snapshot:{required_domain}"));
                }
            }

            let effective_scale = match scoped_action {
                TailRiskAction::Normal => track_scale,
                TailRiskAction::ReduceOnly | TailRiskAction::HaltAndFlatten => 0.0,
            };
            let allow_new_entries =
                scoped_action == TailRiskAction::Normal && effective_scale > 0.0;
            let force_flatten = scoped_action == TailRiskAction::HaltAndFlatten;

            admissions.push(TrackAdmission {
                track_id: track.spec.track_id.clone(),
                mode: track.spec.mode,
                track_risk_scale: track_scale,
                effective_risk_scale: effective_scale,
                allow_new_entries,
                force_flatten,
                tail_action: scoped_action,
                reasons,
            });
        }

        Ok(FleetRiskReport {
            now_ms,
            global_tail_assessment: global_tail,
            tracks: admissions,
        })
    }
}

fn classify_reason_for_track(
    reason: &str,
    track_domains: &BTreeSet<String>,
) -> (bool, TailRiskAction) {
    if let Some((prefix, venue)) = reason.split_once(':') {
        let venue_norm = normalize_key(venue);
        if matches!(
            prefix,
            "liq_buffer_halt"
                | "liq_buffer_reduce_only"
                | "venue_non_positive_equity"
                | "margin_util_halt"
                | "margin_util_reduce_only"
                | "adl_critical"
                | "adl_high"
        ) {
            let severity = if matches!(
                prefix,
                "liq_buffer_halt"
                    | "venue_non_positive_equity"
                    | "margin_util_halt"
                    | "adl_critical"
            ) {
                TailRiskAction::HaltAndFlatten
            } else {
                TailRiskAction::ReduceOnly
            };
            return (track_domains.contains(&venue_norm), severity);
        }
        if matches!(prefix, "stablecoin_depeg_halt") {
            return (true, TailRiskAction::HaltAndFlatten);
        }
        if matches!(
            prefix,
            "stablecoin_depeg_reduce_only" | "missing_peg_coverage" | "stale_or_skewed_peg"
        ) {
            return (true, TailRiskAction::ReduceOnly);
        }
    }
    if reason == "cross_venue_equity_imbalance" {
        return (true, TailRiskAction::ReduceOnly);
    }
    // Fail safe on any unrecognized reason code.
    (true, TailRiskAction::ReduceOnly)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str, mode: StrategyMode, domain: &str, market: &str) -> TrackSpec {
        TrackSpec {
            track_id: id.into(),
            mode,
            venue_domains: vec![domain.into()],
            market_key: market.into(),
            allocated_capital_usd: 1_000.0,
            daily_loss_limit_pct: 0.05,
            max_gross_notional_usd: 2_000.0,
        }
    }

    fn healthy_venue(name: &str) -> VenueMarginSnapshot {
        VenueMarginSnapshot {
            venue: name.into(),
            collateral_asset: "USDC".into(),
            equity_usd: 1_000.0,
            margin_used_usd: 100.0,
            liquidation_distance_bps: Some(2_000.0),
            adl_level: crate::strategy::tail_risk::AdlRiskLevel::Low,
        }
    }

    #[test]
    fn coordinator_rejects_cross_track_venue_market_overlap() {
        let specs = vec![
            track("s1_mm", StrategyMode::NeutralMarketMaking, "venue_a", "BTC"),
            track(
                "s2_reb",
                StrategyMode::RebateLiquidityMaking,
                "VENUE_A",
                "btc",
            ),
        ];
        assert!(TrackCoordinator::new(TailRiskConfig::default(), specs, 739_000).is_err());
    }

    #[test]
    fn coordinator_enforces_zero_cross_subsidy_and_scopes_venue_halts() {
        let now_ms = 1_700_000_000_000_u64;
        let specs = vec![
            track("s1_mm", StrategyMode::NeutralMarketMaking, "venue_a", "ETH"),
            track("s3_basis", StrategyMode::CrossVenueBasis, "venue_b", "ANT"),
        ];
        let mut coord =
            TrackCoordinator::new(TailRiskConfig::default(), specs, sim_utc_day(now_ms)).unwrap();

        // Track 1 profitable (+20%) while Track 2 breaches daily loss limit (-10%) -> zero cross-subsidy!
        let r1 = coord
            .evaluate_fleet(
                now_ms + 1_000,
                &[
                    TrackEquityInput {
                        track_id: "s1_mm".into(),
                        equity_usd: 1_200.0,
                    },
                    TrackEquityInput {
                        track_id: "s3_basis".into(),
                        equity_usd: 900.0,
                    },
                ],
                &[healthy_venue("venue_a"), healthy_venue("venue_b")],
                &[],
            )
            .unwrap();
        assert!(r1.tracks[0].allow_new_entries && r1.tracks[0].effective_risk_scale == 1.0);
        assert!(!r1.tracks[1].allow_new_entries && r1.tracks[1].force_flatten);
        assert_eq!(r1.tracks[1].effective_risk_scale, 0.0);

        // Fresh coordinator: venue_b margin util halt affects ONLY venue_b track, while USDC depeg halts BOTH!
        let mut coord2 = TrackCoordinator::new(
            TailRiskConfig::default(),
            vec![
                track("s1_mm", StrategyMode::NeutralMarketMaking, "venue_a", "ETH"),
                track("s3_basis", StrategyMode::CrossVenueBasis, "venue_b", "ANT"),
            ],
            sim_utc_day(now_ms),
        )
        .unwrap();
        let mut bad_venue_b = healthy_venue("venue_b");
        bad_venue_b.margin_used_usd = 900.0; // 90% >= 85% halt
        let r_scoped = coord2
            .evaluate_fleet(
                now_ms + 2_000,
                &[
                    TrackEquityInput {
                        track_id: "s1_mm".into(),
                        equity_usd: 1_000.0,
                    },
                    TrackEquityInput {
                        track_id: "s3_basis".into(),
                        equity_usd: 1_000.0,
                    },
                ],
                &[healthy_venue("venue_a"), bad_venue_b],
                &[],
            )
            .unwrap();
        assert!(r_scoped.tracks[0].allow_new_entries);
        assert!(r_scoped.tracks[1].force_flatten);

        // Bad NaN equity fails closed atomically without mutating last_eval_ms or guards.
        assert!(
            coord2
                .evaluate_fleet(
                    now_ms + 3_000,
                    &[
                        TrackEquityInput {
                            track_id: "s1_mm".into(),
                            equity_usd: f64::NAN,
                        },
                        TrackEquityInput {
                            track_id: "s3_basis".into(),
                            equity_usd: 1_000.0,
                        },
                    ],
                    &[healthy_venue("venue_a")],
                    &[],
                )
                .is_err()
        );

        // Global stablecoin depeg halts ALL tracks monotonically.
        let r_depeg = coord2
            .evaluate_fleet(
                now_ms + 2_500, // still accepted cleanly because NaN error was atomic!
                &[
                    TrackEquityInput {
                        track_id: "s1_mm".into(),
                        equity_usd: 1_000.0,
                    },
                    TrackEquityInput {
                        track_id: "s3_basis".into(),
                        equity_usd: 1_000.0,
                    },
                ],
                &[healthy_venue("venue_a"), healthy_venue("venue_b")],
                &[StablecoinPegSample {
                    asset: "USDC".into(),
                    price_usd: 0.99, // -100 bps >= 75 bps halt
                    available_at_ms: now_ms + 2_400,
                }],
            )
            .unwrap();
        assert!(r_depeg.tracks.iter().all(|t| t.force_flatten));

        // Regressed UTC day against an active future-day guard fails closed atomically.
        let mut coord3 = TrackCoordinator::new(
            TailRiskConfig::default(),
            vec![track(
                "s1_mm",
                StrategyMode::NeutralMarketMaking,
                "venue_a",
                "ETH",
            )],
            sim_utc_day(now_ms) + 2,
        )
        .unwrap();
        assert!(
            coord3
                .evaluate_fleet(
                    now_ms + 1_000,
                    &[TrackEquityInput {
                        track_id: "s1_mm".into(),
                        equity_usd: 1_000.0,
                    }],
                    &[healthy_venue("venue_a")],
                    &[],
                )
                .is_err()
        );
        // Rejecting the regressed day did not advance last_eval_ms.
        assert!(
            coord3
                .evaluate_fleet(
                    now_ms + 2 * 86_400_000 + 500,
                    &[TrackEquityInput {
                        track_id: "s1_mm".into(),
                        equity_usd: 1_000.0,
                    }],
                    &[healthy_venue("venue_a")],
                    &[],
                )
                .is_ok()
        );
    }

    #[test]
    fn coordinator_forces_reduce_only_when_required_venue_snapshot_is_missing() {
        let now_ms = 1_700_000_000_000_u64;
        let specs = vec![
            track("s1_mm", StrategyMode::NeutralMarketMaking, "venue_a", "ETH"),
            TrackSpec {
                track_id: "s3_basis".into(),
                mode: StrategyMode::CrossVenueBasis,
                venue_domains: vec!["venue_b".into(), "venue_c".into()],
                market_key: "ANT".into(),
                allocated_capital_usd: 1_000.0,
                daily_loss_limit_pct: 0.05,
                max_gross_notional_usd: 2_000.0,
            },
        ];
        let mut coord =
            TrackCoordinator::new(TailRiskConfig::default(), specs, sim_utc_day(now_ms)).unwrap();
        let eq = [
            TrackEquityInput {
                track_id: "s1_mm".into(),
                equity_usd: 1_000.0,
            },
            TrackEquityInput {
                track_id: "s3_basis".into(),
                equity_usd: 1_000.0,
            },
        ];

        // 1. All 3 healthy -> both Normal.
        let r1 = coord
            .evaluate_fleet(
                now_ms + 1_000,
                &eq,
                &[
                    healthy_venue("venue_a"),
                    healthy_venue("venue_b"),
                    healthy_venue("venue_c"),
                ],
                &[],
            )
            .unwrap();
        assert!(r1.tracks[0].allow_new_entries && r1.tracks[1].allow_new_entries);

        // 2. venue_b high margin utilization -> Track B HaltAndFlatten, Track A unaffected.
        let mut bad_b = healthy_venue("venue_b");
        bad_b.margin_used_usd = 900.0;
        let r2 = coord
            .evaluate_fleet(
                now_ms + 2_000,
                &eq,
                &[healthy_venue("venue_a"), bad_b, healthy_venue("venue_c")],
                &[],
            )
            .unwrap();
        assert!(r2.tracks[0].allow_new_entries && !r2.tracks[1].allow_new_entries);
        assert!(r2.tracks[1].force_flatten);

        // 3. venue_b omitted -> Track B stays blocked (ReduceOnly), cannot re-enable entries.
        let r3 = coord
            .evaluate_fleet(
                now_ms + 3_000,
                &eq,
                &[healthy_venue("venue_a"), healthy_venue("venue_c")],
                &[],
            )
            .unwrap();
        assert!(r3.tracks[0].allow_new_entries);
        assert!(!r3.tracks[1].allow_new_entries && !r3.tracks[1].force_flatten);
        assert_eq!(r3.tracks[1].tail_action, TailRiskAction::ReduceOnly);
        assert_eq!(r3.tracks[1].effective_risk_scale, 0.0);
        assert!(
            r3.tracks[1]
                .reasons
                .iter()
                .any(|r| r == "missing_venue_snapshot:venue_b")
        );

        // 4. venue_b omitted AND venue_c high margin -> Track B escalates to HaltAndFlatten.
        let mut bad_c = healthy_venue("venue_c");
        bad_c.margin_used_usd = 900.0;
        let r4 = coord
            .evaluate_fleet(now_ms + 4_000, &eq, &[healthy_venue("venue_a"), bad_c], &[])
            .unwrap();
        assert!(r4.tracks[0].allow_new_entries);
        assert!(r4.tracks[1].force_flatten);

        // 5. Healthy venue_b + venue_c restored -> Track B recovers.
        let r5 = coord
            .evaluate_fleet(
                now_ms + 5_000,
                &eq,
                &[
                    healthy_venue("venue_a"),
                    healthy_venue("venue_b"),
                    healthy_venue("venue_c"),
                ],
                &[],
            )
            .unwrap();
        assert!(r5.tracks[0].allow_new_entries && r5.tracks[1].allow_new_entries);
    }
}

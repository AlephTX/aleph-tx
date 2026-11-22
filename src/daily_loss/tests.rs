use super::*;

fn temp_path() -> PathBuf {
    std::env::temp_dir()
        .join(format!("aleph-daily-loss-{}", uuid::Uuid::new_v4()))
        .join("state.json")
}

#[test]
fn restart_same_day_keeps_original_baseline() {
    let path = temp_path();
    let mut guard = DailyLossGuard::open(Some(path.clone()), 0.05, 1000.0, 10).unwrap();
    assert!(guard.check(970.0, 10).unwrap() < 0.0);
    drop(guard);

    // Restart after a 3% drawdown: the budget must not re-anchor at 970.
    let mut guard = DailyLossGuard::open(Some(path.clone()), 0.05, 970.0, 10).unwrap();
    assert_eq!(guard.start_equity(), 1000.0);
    assert!(guard.check(940.0, 10).is_err());
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn breach_is_sticky_for_the_day_and_clears_next_day() {
    let path = temp_path();
    let mut guard = DailyLossGuard::open(Some(path.clone()), 0.05, 1000.0, 10).unwrap();
    assert!(guard.check(900.0, 10).is_err());
    drop(guard);

    assert!(DailyLossGuard::open(Some(path.clone()), 0.05, 900.0, 10).is_err());
    let guard = DailyLossGuard::open(Some(path.clone()), 0.05, 900.0, 11).unwrap();
    assert_eq!(guard.start_equity(), 900.0);
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn utc_midnight_rolls_the_baseline() {
    let mut guard = DailyLossGuard::open(None, 0.05, 1000.0, 10).unwrap();
    assert_eq!(guard.check(960.0, 11).unwrap(), 0.0);
    assert_eq!(guard.start_equity(), 960.0);
    assert!(guard.check(920.0, 11).is_ok());
    assert!(guard.check(910.0, 11).is_err());
}

#[test]
fn a_backward_clock_cannot_rearm_a_breached_budget() {
    let path = temp_path();
    let mut guard = DailyLossGuard::open(Some(path.clone()), 0.05, 1000.0, 10).unwrap();
    assert!(guard.check(940.0, 9).is_err());
    assert_eq!(guard.start_equity(), 1000.0);
    assert!(guard.check(940.0, 10).is_err());
    assert!(guard.check(1000.0, 10).is_err());
    assert!(DailyLossGuard::open(Some(path.clone()), 0.05, 940.0, 9).is_err());
    assert!(DailyLossGuard::open(Some(path.clone()), 0.05, 940.0, 10).is_err());
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn corrupt_state_refuses_to_reset_budget() {
    let path = temp_path();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"{not json").unwrap();
    assert!(DailyLossGuard::open(Some(path.clone()), 0.05, 1000.0, 10).is_err());
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn invalid_inputs_are_rejected() {
    assert!(DailyLossGuard::open(None, 0.0, 1000.0, 10).is_err());
    assert!(DailyLossGuard::open(None, 1.0, 1000.0, 10).is_err());
    assert!(DailyLossGuard::open(None, 0.05, 0.0, 10).is_err());
    let mut guard = DailyLossGuard::open(None, 0.05, 1000.0, 10).unwrap();
    assert_eq!(guard.check(f64::NAN, 10).unwrap(), 0.0);
}

#[test]
fn zero_or_negative_equity_trips_and_persists_breach() {
    for equity in [0.0, -5.0] {
        let path = temp_path();
        let mut guard = DailyLossGuard::open(Some(path.clone()), 0.05, 1000.0, 10).unwrap();
        let err = guard.check(equity, 10).unwrap_err();
        assert!(err.to_string().contains("non-positive equity"), "{err}");
        assert!(DailyLossGuard::open(Some(path.clone()), 0.05, 100.0, 10).is_err());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}

#[test]
fn legacy_three_field_state_loads_and_arms_brake_tier() {
    let path = temp_path();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    // Legacy state without peak_equity or brake_tier.
    fs::write(
        &path,
        br#"{"utc_day":10,"start_equity":1000.0,"breached":false}"#,
    )
    .unwrap();

    // Restart with equity=960 (r_dd = 40/50 = 0.80 >= 0.75 -> tier 3, 0.25x scale).
    let guard = DailyLossGuard::open(Some(path.clone()), 0.05, 960.0, 10).unwrap();
    assert_eq!(guard.start_equity(), 1000.0);
    assert_eq!(guard.peak_equity(), 1000.0);
    assert_eq!(guard.brake_tier(), 3);
    assert_eq!(guard.risk_scale(), 0.25);
    assert!((guard.drawdown_budget_ratio(960.0) - 0.80).abs() < 1e-12);

    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn progressive_brake_escalates_hysteresis_and_recovers_without_deadlock() {
    let mut guard = DailyLossGuard::open(None, 0.05, 1000.0, 10).unwrap();
    assert_eq!(guard.brake_tier(), 0);
    assert_eq!(guard.risk_scale(), 1.0);

    // Budget = $50. Drop $12.50 (r_dd = 0.25) -> Tier 1 (0.75x).
    guard.check(987.5, 10).unwrap();
    assert_eq!(guard.brake_tier(), 1);
    assert_eq!(guard.risk_scale(), 0.75);

    // Partial recovery to r_dd = 0.22 stays in Tier 1 (hysteresis < 0.20 required).
    guard.check(989.0, 10).unwrap();
    assert_eq!(guard.brake_tier(), 1);

    // Deeper drop to r_dd = 0.50 -> Tier 2 (0.50x).
    guard.check(975.0, 10).unwrap();
    assert_eq!(guard.brake_tier(), 2);
    assert_eq!(guard.risk_scale(), 0.50);

    // Recovery to r_dd = 0.47 stays in Tier 2 (hysteresis < 0.45 required).
    guard.check(976.5, 10).unwrap();
    assert_eq!(guard.brake_tier(), 2);

    // Drop to r_dd = 0.75 -> Tier 3 (0.25x).
    guard.check(962.5, 10).unwrap();
    assert_eq!(guard.brake_tier(), 3);
    assert_eq!(guard.risk_scale(), 0.25);

    // Recovery to r_dd = 0.72 stays in Tier 3 (hysteresis < 0.70 required).
    guard.check(964.0, 10).unwrap();
    assert_eq!(guard.brake_tier(), 3);

    // Drop below 0.70 -> drops to Tier 2, then below 0.20 -> drops to Tier 0.
    guard.check(966.0, 10).unwrap(); // r_dd = 0.68 -> Tier 2
    assert_eq!(guard.brake_tier(), 2);
    guard.check(991.0, 10).unwrap(); // r_dd = 0.18 -> cascades to Tier 0 (no 260-period deadlock!)
    assert_eq!(guard.brake_tier(), 0);
    assert_eq!(guard.risk_scale(), 1.0);
}

#[test]
fn intraday_rally_drawdown_engages_brake_without_false_breach_and_persists() {
    let path = temp_path();
    let mut guard = DailyLossGuard::open(Some(path.clone()), 0.05, 1000.0, 10).unwrap();
    // Rally to 1060 -> peak becomes 1060, pnl_pct = +6%.
    assert!(guard.check(1060.0, 10).unwrap() > 0.05);
    assert_eq!(guard.peak_equity(), 1060.0);
    assert_eq!(guard.brake_tier(), 0);

    // Give back $50 from peak (equity=1010 -> pnl_pct=+1%, still well above -5% daily breach,
    // yet r_dd = (1060 - 1010) / 50 = 1.0 >= 0.75 -> Tier 3 soft brake!).
    assert!(guard.check(1010.0, 10).unwrap() > 0.0);
    assert_eq!(guard.brake_tier(), 3);
    assert_eq!(guard.risk_scale(), 0.25);
    drop(guard);

    // Restart same day at 1010 remembers peak=1060 & Tier 3!
    let mut reloaded = DailyLossGuard::open(Some(path.clone()), 0.05, 1010.0, 10).unwrap();
    assert_eq!(reloaded.peak_equity(), 1060.0);
    assert_eq!(reloaded.brake_tier(), 3);
    assert_eq!(reloaded.risk_scale(), 0.25);

    // New equity high 1065 clears the brake completely.
    reloaded.check(1065.0, 10).unwrap();
    assert_eq!(reloaded.peak_equity(), 1065.0);
    assert_eq!(reloaded.brake_tier(), 0);
    assert_eq!(reloaded.risk_scale(), 1.0);

    // UTC midnight resets baseline, peak, and tier.
    reloaded.check(1020.0, 11).unwrap();
    assert_eq!(reloaded.start_equity(), 1020.0);
    assert_eq!(reloaded.peak_equity(), 1020.0);
    assert_eq!(reloaded.brake_tier(), 0);
    assert_eq!(reloaded.risk_scale(), 1.0);

    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn corrupt_brake_tier_or_negative_peak_equity_is_rejected() {
    for bad in [
        r#"{"utc_day":10,"start_equity":1000.0,"breached":false,"peak_equity":1000.0,"brake_tier":5}"#,
        r#"{"utc_day":10,"start_equity":1000.0,"breached":false,"peak_equity":1000.0,"brake_tier":4}"#,
        r#"{"utc_day":10,"start_equity":1000.0,"breached":false,"peak_equity":-1.0,"brake_tier":0}"#,
        r#"{"utc_day":10,"start_equity":1000.0,"breached":false,"peak_equity":900.0,"brake_tier":0}"#,
    ] {
        let path = temp_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bad).unwrap();
        assert!(
            DailyLossGuard::open(Some(path.clone()), 0.05, 1000.0, 10).is_err(),
            "expected error for corrupt state: {bad}"
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}

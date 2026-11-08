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

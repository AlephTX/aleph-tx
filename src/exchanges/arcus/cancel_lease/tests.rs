use super::*;
use serde_json::json;

const WALL: u64 = 1_800_000_000_000_000_000;
const ADDRESS: &str = "0x1111111111111111111111111111111111111111";

fn policy() -> CancelLeasePolicy {
    CancelLeasePolicy {
        lifetime: Duration::from_secs(60),
        refresh_after: Duration::from_secs(10),
        stop_before_expiry: Duration::from_secs(5),
        max_clock_step: Duration::from_millis(100),
    }
}

fn lease(scope: Option<u16>) -> CancelLease {
    CancelLease::new(Account::new(ADDRESS, 2).unwrap(), scope, policy()).unwrap()
}

fn ack(request: &CancelRenewal) -> ScheduledCancelReceipt {
    serde_json::from_value(json!({"address":ADDRESS,"accountIndex":2,
        "marketId":request.market_id(),"time":request.deadline_us(),"status":"scheduled"}))
    .unwrap()
}

#[test]
fn fresh_and_restarted_instances_never_restore_protection() {
    let now = Instant::now();
    let mut original = lease(Some(88));
    assert!(!original.allows_new_exposure(now));
    assert!(original.needs_renewal(now));
    let request = original.begin_renewal(WALL, now).unwrap();
    original
        .confirm_renewal(
            &ack(&request),
            WALL + 100_000_000,
            now + Duration::from_millis(100),
        )
        .unwrap();
    assert!(original.allows_new_exposure(now + Duration::from_secs(1)));
    assert!(!lease(Some(88)).allows_new_exposure(now + Duration::from_secs(1)));
}

#[test]
fn roundtrip_time_is_not_added_to_deadline_or_refresh_clock() {
    let now = Instant::now();
    let mut guard = lease(Some(88));
    let request = guard.begin_renewal(WALL, now).unwrap();
    assert!(!guard.allows_new_exposure(now));
    assert!(!guard.needs_renewal(now));
    guard
        .confirm_renewal(
            &ack(&request),
            WALL + 2_000_000_000,
            now + Duration::from_secs(2),
        )
        .unwrap();
    assert!(!guard.needs_renewal(now + Duration::from_secs(9)));
    assert!(guard.needs_renewal(now + Duration::from_secs(10)));
    assert!(guard.allows_new_exposure(now + Duration::from_secs(54)));
    assert!(!guard.allows_new_exposure(now + Duration::from_secs(55)));
    assert!(!guard.allows_new_exposure(now + Duration::from_secs(60)));
    assert!(!guard.allows_new_exposure(now + Duration::from_secs(1)));
}

#[test]
fn renewal_failure_never_falls_back_to_previously_confirmed_lease() {
    let now = Instant::now();
    let mut guard = lease(None);
    let first = guard.begin_renewal(WALL, now).unwrap();
    guard.confirm_renewal(&ack(&first), WALL, now).unwrap();
    assert!(guard.allows_new_exposure(now));
    let second = guard
        .begin_renewal(WALL + 10_000_000_000, now + Duration::from_secs(10))
        .unwrap();
    assert!(second.deadline_us() > first.deadline_us());
    assert!(!guard.allows_new_exposure(now + Duration::from_secs(10)));
    guard.invalidate();
    assert!(!guard.allows_new_exposure(now + Duration::from_secs(11)));
    assert!(
        guard
            .confirm_renewal(
                &ack(&second),
                WALL + 11_000_000_000,
                now + Duration::from_secs(11)
            )
            .is_err()
    );
    assert!(guard.requires_reconciliation(now + Duration::from_secs(12)));
    assert!(!guard.needs_renewal(now + Duration::from_secs(12)));
    assert!(
        guard
            .begin_renewal(WALL + 12_000_000_000, now + Duration::from_secs(12))
            .is_err()
    );
}

#[test]
fn only_one_pending_request_and_strictly_later_deadlines() {
    let now = Instant::now();
    let mut pending = lease(Some(88));
    pending.begin_renewal(WALL, now).unwrap();
    assert!(pending.begin_renewal(WALL + 1_000, now).is_err());
    assert!(pending.requires_reconciliation(now));
    for next in [WALL, WALL - 1_000, WALL + 1_000] {
        let mut guard = lease(Some(88));
        let first = guard.begin_renewal(WALL, now).unwrap();
        guard.confirm_renewal(&ack(&first), WALL, now).unwrap();
        assert_eq!(
            guard
                .begin_renewal(next, now + Duration::from_micros(1))
                .is_ok(),
            next > WALL
        );
        assert!(!guard.allows_new_exposure(now + Duration::from_micros(1)));
    }
}

#[test]
fn mismatched_scope_account_deadline_and_disarm_cannot_confirm_protection() {
    let now = Instant::now();
    for (field, value) in [
        ("marketId", json!(null)),
        ("marketId", json!(89)),
        ("accountIndex", json!(3)),
        (
            "address",
            json!("0x2222222222222222222222222222222222222222"),
        ),
        ("time", json!(WALL / 1_000 + 59_000_000)),
        ("status", json!("disarmed")),
    ] {
        let mut guard = lease(Some(88));
        let request = guard.begin_renewal(WALL, now).unwrap();
        let mut response = json!({"address":ADDRESS,"accountIndex":2,"marketId":88,
            "time":request.deadline_us(),"status":"scheduled"});
        response[field] = value;
        let receipt = serde_json::from_value(response).unwrap();
        assert!(guard.confirm_renewal(&receipt, WALL, now).is_err());
        assert!(!guard.allows_new_exposure(now));
        assert!(guard.confirm_renewal(&ack(&request), WALL, now).is_err());
    }
}

#[test]
fn old_response_cannot_reopen_gate_during_new_request() {
    let now = Instant::now();
    let mut guard = lease(Some(88));
    let first = guard.begin_renewal(WALL, now).unwrap();
    guard.confirm_renewal(&ack(&first), WALL, now).unwrap();
    let second = guard
        .begin_renewal(WALL + 1_000_000_000, now + Duration::from_secs(1))
        .unwrap();
    assert!(
        guard
            .confirm_renewal(
                &ack(&first),
                WALL + 1_000_000_000,
                now + Duration::from_secs(1)
            )
            .is_err()
    );
    assert!(!guard.allows_new_exposure(now + Duration::from_secs(1)));
    // A bad response is sticky: neither the old nor new ACK can clear recovery.
    assert!(
        guard
            .confirm_renewal(
                &ack(&second),
                WALL + 1_000_000_000,
                now + Duration::from_secs(1)
            )
            .is_err()
    );
}

#[test]
fn clock_step_or_reversal_consumes_attempt_without_opening_gate() {
    let now = Instant::now();
    for (wall, mono) in [
        (WALL - 1, now + Duration::from_millis(100)),
        (WALL + 2_000_000_000, now + Duration::from_millis(100)),
        (WALL + 100_000_000, now - Duration::from_nanos(1)),
    ] {
        let mut guard = lease(None);
        let request = guard.begin_renewal(WALL, now).unwrap();
        assert!(guard.confirm_renewal(&ack(&request), wall, mono).is_err());
        assert!(!guard.allows_new_exposure(now));
    }
}

#[test]
fn slow_reply_inside_safety_margin_is_not_protection() {
    let now = Instant::now();
    for elapsed in [55, 60, 61] {
        let mut guard = lease(Some(88));
        let request = guard.begin_renewal(WALL, now).unwrap();
        assert!(
            guard
                .confirm_renewal(
                    &ack(&request),
                    WALL + elapsed * 1_000_000_000,
                    now + Duration::from_secs(elapsed)
                )
                .is_err()
        );
        assert!(!guard.allows_new_exposure(now + Duration::from_secs(elapsed)));
    }
}

#[test]
fn bad_policy_and_overflows_are_rejected() {
    for change in [0, 1, 2, 3, 4] {
        let mut p = policy();
        match change {
            0 => p.lifetime = Duration::from_secs(5),
            1 => p.lifetime = Duration::from_secs(301),
            2 => p.refresh_after = Duration::from_secs(55),
            3 => p.max_clock_step = Duration::from_secs(6),
            _ => p.stop_before_expiry = Duration::ZERO,
        }
        assert!(CancelLease::new(Account::new(ADDRESS, 2).unwrap(), None, p).is_err());
    }
    for bad in [0, u64::MAX] {
        let mut guard = lease(None);
        assert!(guard.begin_renewal(bad, Instant::now()).is_err());
        assert!(!guard.allows_new_exposure(Instant::now()));
    }
}

#[test]
fn expired_previous_switch_cannot_be_rearmed_without_reconciliation() {
    let now = Instant::now();
    let mut guard = lease(Some(88));
    let first = guard.begin_renewal(WALL, now).unwrap();
    guard.confirm_renewal(&ack(&first), WALL, now).unwrap();
    let later = now + Duration::from_secs(60);
    assert!(guard.requires_reconciliation(later));
    assert!(!guard.needs_renewal(later));
    assert!(guard.begin_renewal(WALL + 60_000_000_000, later).is_err());
    assert!(!guard.allows_new_exposure(later));
}

#[test]
fn new_ack_after_old_deadline_cannot_hide_protection_gap() {
    let now = Instant::now();
    let mut guard = lease(Some(88));
    let first = guard.begin_renewal(WALL, now).unwrap();
    guard.confirm_renewal(&ack(&first), WALL, now).unwrap();
    let second = guard
        .begin_renewal(WALL + 50_000_000_000, now + Duration::from_secs(50))
        .unwrap();
    // New deadline is 110 seconds, but the previous one may fire at 60.
    assert!(
        guard
            .confirm_renewal(
                &ack(&second),
                WALL + 60_000_000_000,
                now + Duration::from_secs(60)
            )
            .is_err()
    );
    assert!(guard.requires_reconciliation(now + Duration::from_secs(60)));
    assert!(!guard.allows_new_exposure(now + Duration::from_secs(60)));
}

use super::AccountStats;
use super::components::RiskSnapshot;
use crate::telemetry::TelemetryCollector;
use std::time::Duration;

pub(super) fn reconcile_interval(base_secs: u64, consecutive_failures: u32) -> Duration {
    let backoff_mult = 2u64.saturating_pow(consecutive_failures.min(4));
    Duration::from_secs(base_secs.saturating_mul(backoff_mult))
}

pub(super) struct TelemetrySync {
    pub fill_count: u64,
    pub total_fees: f64,
    pub confirmed_position: f64,
    pub pending_exposure: f64,
    pub effective_position: f64,
}

pub(super) fn sync_telemetry_snapshot(
    telemetry: &mut TelemetryCollector,
    account_stats: &AccountStats,
    risk: &RiskSnapshot,
    ts: &TelemetrySync,
) {
    telemetry.fill_count = ts.fill_count;
    telemetry.total_fees_paid = ts.total_fees;
    telemetry.raw_available_balance = risk.raw_available_balance;
    telemetry.available_balance = risk.available_balance;
    telemetry.portfolio_value = account_stats.portfolio_value;
    telemetry.quote_position = risk.position_for_quoting;
    telemetry.tracker_confirmed_position = ts.confirmed_position;
    telemetry.tracker_pending_exposure = ts.pending_exposure;
    telemetry.tracker_effective_position = ts.effective_position;
    telemetry.worst_case_long = risk.worst_case_long;
    telemetry.worst_case_short = risk.worst_case_short;
    telemetry.usable_balance = risk.usable_balance;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn reconcile_interval_exponentially_backs_off_and_caps() {
        assert_eq!(reconcile_interval(30, 0), Duration::from_secs(30));
        assert_eq!(reconcile_interval(30, 1), Duration::from_secs(60));
        assert_eq!(reconcile_interval(30, 2), Duration::from_secs(120));
        assert_eq!(reconcile_interval(30, 5), Duration::from_secs(480));
    }

    #[test]
    fn sync_telemetry_snapshot_copies_risk_and_account_fields() {
        let mut telemetry = TelemetryCollector::new();
        let account_stats = AccountStats {
            available_balance: 90.0,
            portfolio_value: 120.0,
            position: 0.0,
            leverage: 0.0,
            margin_usage: 0.0,
            last_update: Instant::now(),
        };
        let risk = RiskSnapshot {
            raw_available_balance: 90.0,
            position_for_quoting: 0.0,
            worst_case_long: 0.15,
            worst_case_short: -0.14,
            base_order_size: 0.015,
            max_position: 0.20,
            inventory_urgency_threshold: 0.08,
            min_available_balance: 10.0,
            available_balance: 70.0,
            usable_balance: 49.0,
            margin_per_eth: 200.0,
            grid_multiplier: 2.0,
        };

        sync_telemetry_snapshot(&mut telemetry, &account_stats, &risk, &TelemetrySync {
            fill_count: 12,
            total_fees: 1.25,
            confirmed_position: 0.01,
            pending_exposure: -0.03,
            effective_position: -0.02,
        });

        assert_eq!(telemetry.fill_count, 12);
        assert_eq!(telemetry.total_fees_paid, 1.25);
        assert_eq!(telemetry.raw_available_balance, 90.0);
        assert_eq!(telemetry.available_balance, 70.0);
        assert_eq!(telemetry.portfolio_value, 120.0);
        assert_eq!(telemetry.quote_position, 0.0);
        assert_eq!(telemetry.tracker_confirmed_position, 0.01);
        assert_eq!(telemetry.tracker_pending_exposure, -0.03);
        assert_eq!(telemetry.tracker_effective_position, -0.02);
        assert_eq!(telemetry.worst_case_long, 0.15);
        assert_eq!(telemetry.worst_case_short, -0.14);
        assert_eq!(telemetry.usable_balance, 49.0);
    }
}

//! Telemetry Module - Structured metrics collection for production observability
//!
//! Exports key trading metrics via structured logging for monitoring systems.

use std::time::Instant;
use tracing::{info, warn};

/// Telemetry collector for strategy metrics
#[derive(Debug, Clone)]
pub struct TelemetryCollector {
    /// Total orders placed
    pub orders_placed: u64,
    /// Total orders rejected by exchange
    pub orders_rejected: u64,
    /// Margin cooldown events (insufficient margin)
    pub margin_cooldown_events: u64,
    /// Current spread size in basis points
    pub spread_size_bps: f64,
    /// Adverse selection score (higher = more toxic flow)
    pub adverse_selection_score: f64,
    /// Last margin cooldown timestamp
    last_margin_cooldown: Option<Instant>,
    /// Total fills received
    pub fill_count: u64,
    /// Total fees paid in USD
    pub total_fees_paid: f64,
    /// Current available balance
    pub available_balance: f64,
    /// Raw available balance reported by exchange before safety haircut
    pub raw_available_balance: f64,
    /// Current portfolio value (equity)
    pub portfolio_value: f64,
    /// Effective position including pending exposure
    pub effective_position: f64,
    /// Worst-case long exposure
    pub worst_case_long: f64,
    /// Worst-case short exposure
    pub worst_case_short: f64,
    /// Usable balance after strategy haircut
    pub usable_balance: f64,
    /// Session start time for fill rate calculation
    session_start: Instant,
}

impl Default for TelemetryCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl TelemetryCollector {
    /// Create a new telemetry collector
    pub fn new() -> Self {
        Self {
            orders_placed: 0,
            orders_rejected: 0,
            margin_cooldown_events: 0,
            spread_size_bps: 0.0,
            adverse_selection_score: 0.0,
            last_margin_cooldown: None,
            fill_count: 0,
            total_fees_paid: 0.0,
            available_balance: 0.0,
            raw_available_balance: 0.0,
            portfolio_value: 0.0,
            effective_position: 0.0,
            worst_case_long: 0.0,
            worst_case_short: 0.0,
            usable_balance: 0.0,
            session_start: Instant::now(),
        }
    }

    /// Record a successful order placement
    pub fn record_order_placed(&mut self) {
        self.orders_placed += 1;
        info!(
            metric = "order_placed",
            total = self.orders_placed,
            "Order placed successfully"
        );
    }

    /// Record an order rejection
    pub fn record_order_rejected(&mut self, reason: &str) {
        self.orders_rejected += 1;
        warn!(
            metric = "order_rejected",
            reason = reason,
            total = self.orders_rejected,
            "Order rejected by exchange"
        );
    }

    /// Record a margin cooldown event
    pub fn record_margin_cooldown(&mut self, duration_secs: u64) {
        self.margin_cooldown_events += 1;
        self.last_margin_cooldown = Some(Instant::now());

        warn!(
            metric = "margin_cooldown",
            duration_secs = duration_secs,
            total_events = self.margin_cooldown_events,
            "Margin cooldown triggered - insufficient collateral"
        );
    }

    /// Update spread size metric
    pub fn update_spread_size(&mut self, spread_bps: f64) {
        self.spread_size_bps = spread_bps;
    }

    /// Update adverse selection score
    pub fn update_adverse_selection(&mut self, score: f64) {
        self.adverse_selection_score = score;
    }

    /// Record a fill event with fee
    pub fn record_fill(&mut self, fee: f64) {
        self.fill_count += 1;
        self.total_fees_paid += fee;
    }

    /// Get fill rate (fills per minute)
    pub fn fill_rate(&self) -> f64 {
        let elapsed_min = self.session_start.elapsed().as_secs_f64() / 60.0;
        if elapsed_min < 0.01 {
            return 0.0;
        }
        self.fill_count as f64 / elapsed_min
    }

    /// Export all metrics as structured log
    pub fn export_metrics(&self) {
        info!(
            metric = "telemetry_snapshot",
            orders_placed = self.orders_placed,
            orders_rejected = self.orders_rejected,
            margin_cooldown_events = self.margin_cooldown_events,
            spread_size_bps = self.spread_size_bps,
            adverse_selection_score = self.adverse_selection_score,
            fill_count = self.fill_count,
            total_fees_paid = format!("{:.4}", self.total_fees_paid).as_str(),
            fill_rate = format!("{:.2}", self.fill_rate()).as_str(),
            available_balance = format!("{:.2}", self.available_balance).as_str(),
            raw_available_balance = format!("{:.2}", self.raw_available_balance).as_str(),
            portfolio_value = format!("{:.2}", self.portfolio_value).as_str(),
            effective_position = format!("{:.4}", self.effective_position).as_str(),
            worst_case_long = format!("{:.4}", self.worst_case_long).as_str(),
            worst_case_short = format!("{:.4}", self.worst_case_short).as_str(),
            usable_balance = format!("{:.2}", self.usable_balance).as_str(),
            "Telemetry snapshot"
        );
    }

    /// Get rejection rate (0.0 to 1.0)
    pub fn rejection_rate(&self) -> f64 {
        if self.orders_placed == 0 {
            return 0.0;
        }
        self.orders_rejected as f64 / (self.orders_placed + self.orders_rejected) as f64
    }

    /// Check if currently in margin cooldown
    pub fn is_in_margin_cooldown(&self, cooldown_duration_secs: u64) -> bool {
        if let Some(last_cooldown) = self.last_margin_cooldown {
            last_cooldown.elapsed().as_secs() < cooldown_duration_secs
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_telemetry_collector_new() {
        let collector = TelemetryCollector::new();
        assert_eq!(collector.orders_placed, 0);
        assert_eq!(collector.orders_rejected, 0);
        assert_eq!(collector.margin_cooldown_events, 0);
    }

    #[test]
    fn test_record_order_placed() {
        let mut collector = TelemetryCollector::new();
        collector.record_order_placed();
        collector.record_order_placed();
        assert_eq!(collector.orders_placed, 2);
    }

    #[test]
    fn test_record_order_rejected() {
        let mut collector = TelemetryCollector::new();
        collector.record_order_rejected("insufficient margin");
        assert_eq!(collector.orders_rejected, 1);
    }

    #[test]
    fn test_rejection_rate() {
        let mut collector = TelemetryCollector::new();
        assert_eq!(collector.rejection_rate(), 0.0);

        collector.record_order_placed();
        collector.record_order_placed();
        collector.record_order_rejected("test");
        // 2 placed, 1 rejected = 1/3 = 0.333...
        assert!((collector.rejection_rate() - 0.333).abs() < 0.01);
    }

    #[test]
    fn test_margin_cooldown_tracking() {
        let mut collector = TelemetryCollector::new();
        assert!(!collector.is_in_margin_cooldown(5));

        collector.record_margin_cooldown(5);
        assert_eq!(collector.margin_cooldown_events, 1);
        assert!(collector.is_in_margin_cooldown(5));

        // Wait 1 second
        std::thread::sleep(std::time::Duration::from_secs(1));
        assert!(collector.is_in_margin_cooldown(5));

        // After 6 seconds, should be out of cooldown
        std::thread::sleep(std::time::Duration::from_secs(6));
        assert!(!collector.is_in_margin_cooldown(5));
    }

    #[test]
    fn test_update_metrics() {
        let mut collector = TelemetryCollector::new();
        collector.update_spread_size(12.5);
        collector.update_adverse_selection(2.3);

        assert_eq!(collector.spread_size_bps, 12.5);
        assert_eq!(collector.adverse_selection_score, 2.3);
    }
}

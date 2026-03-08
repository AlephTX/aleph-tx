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

    /// Export all metrics as structured log
    pub fn export_metrics(&self) {
        info!(
            metric = "telemetry_snapshot",
            orders_placed = self.orders_placed,
            orders_rejected = self.orders_rejected,
            margin_cooldown_events = self.margin_cooldown_events,
            spread_size_bps = self.spread_size_bps,
            adverse_selection_score = self.adverse_selection_score,
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

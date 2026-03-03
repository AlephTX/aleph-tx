//! Lighter Market Maker Strategy
//!
//! Demonstrates the "No Boomerang" execution philosophy:
//! 1. Read shadow ledger instantly (<1μs)
//! 2. Fire HTTP orders directly from Rust
//! 3. Update in_flight_pos optimistically
//! 4. Background WS events reconcile the truth
//!
//! # Strategy Logic
//!
//! - Quotes around mid price with configurable spread
//! - Respects max exposure limits (real_pos + in_flight_pos)
//! - Cancels stale orders periodically
//! - Supports graceful shutdown via tokio::sync::watch channel

use crate::error::Result;
use crate::lighter_orders::LighterHttpClient;
use crate::shadow_ledger::{OrderSide, ShadowLedger};
use crate::shm_reader::ShmReader;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct LighterMarketMaker {
    symbol_id: u16,
    market_id: u16,
    spread_bps: u32,
    order_size: f64,
    max_exposure: f64,
    http_client: LighterHttpClient,
    ledger: Arc<RwLock<ShadowLedger>>,
    shm_reader: ShmReader,
    // Order management
    our_orders: HashMap<u64, Instant>, // order_id -> placed_at
    order_ttl: Duration,                // Time-to-live for orders
}

impl LighterMarketMaker {
    pub fn new(
        symbol_id: u16,
        market_id: u16,
        api_key: String,
        private_key: String,
        ledger: Arc<RwLock<ShadowLedger>>,
        shm_reader: ShmReader,
    ) -> Result<Self> {
        let http_client = LighterHttpClient::new(api_key, private_key)?;

        Ok(Self {
            symbol_id,
            market_id,
            spread_bps: 10, // 0.1% spread
            order_size: 0.001,
            max_exposure: 0.01,
            http_client,
            ledger,
            shm_reader,
            our_orders: HashMap::new(),
            order_ttl: Duration::from_secs(30), // Cancel orders after 30s
        })
    }

    /// Main strategy loop with graceful shutdown support
    ///
    /// # Arguments
    ///
    /// * `shutdown` - Optional shutdown signal receiver. When the signal is received,
    ///   the strategy will cancel all orders and exit gracefully.
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` on graceful shutdown, or an error if a critical failure occurs.
    pub async fn run(
        &mut self,
        mut shutdown: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> Result<()> {
        tracing::info!(
            "🎯 Lighter MM started: symbol={} market={} spread={}bps",
            self.symbol_id,
            self.market_id,
            self.spread_bps
        );

        loop {
            // Check for shutdown signal
            if let Some(ref mut rx) = shutdown
                && *rx.borrow() {
                    tracing::info!("Shutdown signal received, canceling all orders...");
                    self.cancel_all_orders().await;
                    return Ok(());
                }

            // Step 1: Read shadow ledger (instant, <1μs)
            // Read once to avoid race conditions
            let (total_exposure, ledger_state) = {
                let ledger = self.ledger.read();
                let exposure = ledger.total_exposure();
                (exposure, Arc::clone(&self.ledger))
            };

            // Step 2: Check risk limits
            if total_exposure.abs() >= self.max_exposure {
                tracing::warn!(
                    "⚠️  Max exposure reached: {:.4} >= {:.4}, skipping quotes",
                    total_exposure,
                    self.max_exposure
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }

            // Step 3: Read market data from shared memory
            let exchanges = self.shm_reader.read_all_exchanges(self.symbol_id);
            let lighter_bbo = exchanges
                .iter()
                .find(|(exch_id, _)| *exch_id == 2) // Exchange 2 = Lighter
                .map(|(_, msg)| msg);

            if lighter_bbo.is_none() || lighter_bbo.unwrap().bid_price == 0.0 {
                tracing::debug!("No BBO data available for Lighter, waiting...");
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }

            let bbo = lighter_bbo.unwrap();
            let mid = (bbo.bid_price + bbo.ask_price) / 2.0;

            // Step 4: Calculate quotes
            let spread = mid * (self.spread_bps as f64) / 10000.0;
            let our_bid = mid - spread / 2.0;
            let our_ask = mid + spread / 2.0;

            // Step 5: Cancel stale orders
            self.cancel_stale_orders().await;

            // Step 6: Fire orders with optimistic accounting
            // Place buy order
            if total_exposure < self.max_exposure {
                match self
                    .http_client
                    .place_order_optimistic(
                        Arc::clone(&ledger_state),
                        self.market_id,
                        self.symbol_id,
                        OrderSide::Buy,
                        our_bid,
                        self.order_size,
                    )
                    .await
                {
                    Ok(order_id) => {
                        tracing::info!("📈 Buy order placed: id={} price={:.2}", order_id, our_bid);
                        self.our_orders.insert(order_id, Instant::now());
                    }
                    Err(e) => {
                        tracing::error!("❌ Buy order failed: {}", e);
                    }
                }
            }

            // Place sell order
            if total_exposure > -self.max_exposure {
                match self
                    .http_client
                    .place_order_optimistic(
                        Arc::clone(&ledger_state),
                        self.market_id,
                        self.symbol_id,
                        OrderSide::Sell,
                        our_ask,
                        self.order_size,
                    )
                    .await
                {
                    Ok(order_id) => {
                        tracing::info!("📉 Sell order placed: id={} price={:.2}", order_id, our_ask);
                        self.our_orders.insert(order_id, Instant::now());
                    }
                    Err(e) => {
                        tracing::error!("❌ Sell order failed: {}", e);
                    }
                }
            }

            // Step 7: Sleep before next iteration
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Cancel all stale orders (older than order_ttl)
    async fn cancel_stale_orders(&mut self) {
        let now = Instant::now();
        let stale_orders: Vec<u64> = self
            .our_orders
            .iter()
            .filter(|(_, placed_at)| now.duration_since(**placed_at) > self.order_ttl)
            .map(|(order_id, _)| *order_id)
            .collect();

        for order_id in stale_orders {
            match self.http_client.cancel_order(order_id).await {
                Ok(_) => {
                    tracing::info!("🚫 Canceled stale order: id={}", order_id);
                    self.our_orders.remove(&order_id);
                }
                Err(e) => {
                    tracing::warn!("Failed to cancel order {}: {}", order_id, e);
                }
            }
        }
    }

    /// Cancel all active orders (used during shutdown)
    async fn cancel_all_orders(&mut self) {
        let order_ids: Vec<u64> = self.our_orders.keys().copied().collect();

        for order_id in order_ids {
            match self.http_client.cancel_order(order_id).await {
                Ok(_) => {
                    tracing::info!("🚫 Canceled order: id={}", order_id);
                }
                Err(e) => {
                    tracing::warn!("Failed to cancel order {}: {}", order_id, e);
                }
            }
        }

        self.our_orders.clear();
    }
}

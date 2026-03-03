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
//! - Incremental quoting: only requotes if price moves > threshold
//! - Cancels stale orders periodically
//! - Supports graceful shutdown via tokio::sync::watch channel

use crate::error::Result;
use crate::lighter_orders::LighterHttpClient;
use crate::shadow_ledger::{OrderSide, ShadowLedger};
use crate::shm_reader::ShmReader;
use parking_lot::RwLock;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
struct ActiveOrder {
    order_id: u64,
    side: OrderSide,
    price: f64,
    size: f64,
    placed_at: Instant,
}

pub struct LighterMarketMaker {
    symbol_id: u16,
    market_id: u16,
    spread_bps: u32,
    order_size: f64,
    max_exposure: f64,
    requote_threshold_bps: f64, // Only requote if price moves > this threshold
    http_client: LighterHttpClient,
    ledger: Arc<RwLock<ShadowLedger>>,
    shm_reader: ShmReader,
    // Order management
    active_bid: Option<ActiveOrder>,
    active_ask: Option<ActiveOrder>,
    order_ttl: Duration, // Time-to-live for orders
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
            spread_bps: 10,              // 0.1% spread
            order_size: 0.001,
            max_exposure: 0.01,
            requote_threshold_bps: 5.0,  // Only requote if price moves > 0.05%
            http_client,
            ledger,
            shm_reader,
            active_bid: None,
            active_ask: None,
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
            "🎯 Lighter MM started: symbol={} market={} spread={}bps requote_threshold={}bps",
            self.symbol_id,
            self.market_id,
            self.spread_bps,
            self.requote_threshold_bps
        );

        loop {
            // Check for shutdown signal
            if let Some(ref mut rx) = shutdown {
                if *rx.borrow() {
                    tracing::info!("Shutdown signal received, canceling all orders...");
                    self.cancel_all_orders().await;
                    return Ok(());
                }
            }

            // Step 1: Read shadow ledger (instant, <1μs)
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

            // Step 6: Check if we need to requote (incremental quoting)
            let should_requote_bid = self.should_requote(&self.active_bid, our_bid);
            let should_requote_ask = self.should_requote(&self.active_ask, our_ask);

            // Step 7: Place/update orders only if needed
            // Place buy order
            if should_requote_bid && total_exposure < self.max_exposure {
                // Cancel existing bid if any
                if let Some(ref order) = self.active_bid {
                    let _ = self.http_client.cancel_order(order.order_id).await;
                    self.active_bid = None;
                }

                // Place new bid
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
                        self.active_bid = Some(ActiveOrder {
                            order_id,
                            side: OrderSide::Buy,
                            price: our_bid,
                            size: self.order_size,
                            placed_at: Instant::now(),
                        });
                    }
                    Err(e) => {
                        tracing::error!("❌ Buy order failed: {}", e);
                    }
                }
            }

            // Place sell order
            if should_requote_ask && total_exposure > -self.max_exposure {
                // Cancel existing ask if any
                if let Some(ref order) = self.active_ask {
                    let _ = self.http_client.cancel_order(order.order_id).await;
                    self.active_ask = None;
                }

                // Place new ask
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
                        self.active_ask = Some(ActiveOrder {
                            order_id,
                            side: OrderSide::Sell,
                            price: our_ask,
                            size: self.order_size,
                            placed_at: Instant::now(),
                        });
                    }
                    Err(e) => {
                        tracing::error!("❌ Sell order failed: {}", e);
                    }
                }
            }

            // Step 8: Sleep before next iteration
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Check if we should requote an order based on price deviation
    fn should_requote(&self, active_order: &Option<ActiveOrder>, new_price: f64) -> bool {
        match active_order {
            None => true, // No active order, should place one
            Some(order) => {
                // Calculate price deviation in bps
                let price_diff = (new_price - order.price).abs();
                let deviation_bps = (price_diff / order.price) * 10000.0;

                if deviation_bps > self.requote_threshold_bps {
                    tracing::debug!(
                        "Price moved {:.2}bps (threshold: {:.2}bps), requoting order",
                        deviation_bps,
                        self.requote_threshold_bps
                    );
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Cancel stale orders (older than order_ttl)
    async fn cancel_stale_orders(&mut self) {
        let now = Instant::now();

        // Check bid
        if let Some(ref order) = self.active_bid {
            if now.duration_since(order.placed_at) > self.order_ttl {
                tracing::info!("🚫 Canceling stale bid: id={}", order.order_id);
                let _ = self.http_client.cancel_order(order.order_id).await;
                self.active_bid = None;
            }
        }

        // Check ask
        if let Some(ref order) = self.active_ask {
            if now.duration_since(order.placed_at) > self.order_ttl {
                tracing::info!("🚫 Canceling stale ask: id={}", order.order_id);
                let _ = self.http_client.cancel_order(order.order_id).await;
                self.active_ask = None;
            }
        }
    }

    /// Cancel all active orders (used during shutdown)
    async fn cancel_all_orders(&mut self) {
        if let Some(ref order) = self.active_bid {
            tracing::info!("🚫 Canceling bid: id={}", order.order_id);
            let _ = self.http_client.cancel_order(order.order_id).await;
        }

        if let Some(ref order) = self.active_ask {
            tracing::info!("🚫 Canceling ask: id={}", order.order_id);
            let _ = self.http_client.cancel_order(order.order_id).await;
        }

        self.active_bid = None;
        self.active_ask = None;
    }
}

//! Lighter DEX HTTP Order Execution
//!
//! This module implements the "No Boomerang" execution philosophy:
//! - Rust directly executes orders via HTTP Keep-Alive (reqwest)
//! - Optimistic accounting: update in_flight_pos BEFORE API responds
//! - Background WS events from Go reconcile the truth

use crate::error::{Result, TradingError};
use crate::lighter_ffi::LighterSigner;
use crate::shadow_ledger::{OrderSide, ShadowLedger};
use parking_lot::RwLock;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Lighter REST API client with Keep-Alive connection pooling
pub struct LighterHttpClient {
    client: Client,
    base_url: String,
    signer: LighterSigner,
    nonce: Arc<parking_lot::Mutex<i64>>,
    client_order_counter: Arc<parking_lot::Mutex<i64>>,
}

impl LighterHttpClient {
    /// Create a new Lighter HTTP client with Keep-Alive enabled
    pub fn new(
        private_key: String,
        api_key_index: i64,
        account_index: i64,
    ) -> Result<Self> {
        let client = Client::builder()
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60))
            .timeout(Duration::from_secs(2))
            .build()
            .map_err(TradingError::Network)?;

        let base_url = "https://mainnet.zklighter.elliot.ai".to_string();

        // Initialize Go signer via FFI
        let signer = LighterSigner::new(
            &base_url,
            &private_key,
            304, // Mainnet chain_id (not 1!)
            api_key_index,
            account_index,
        )
        .map_err(|e| TradingError::OrderFailed(format!("Signer init failed: {}", e)))?;

        Ok(Self {
            client,
            base_url,
            signer,
            nonce: Arc::new(parking_lot::Mutex::new(1)), // Start from 1
            client_order_counter: Arc::new(parking_lot::Mutex::new(1)), // Start from 1
        })
    }

    /// Place a limit order with optimistic accounting and retry logic
    ///
    /// CRITICAL: This function updates in_flight_pos BEFORE the API responds.
    /// The background WS event consumer will reconcile the truth.
    ///
    /// Implements exponential backoff retry for transient network failures.
    pub async fn place_order_optimistic(
        &self,
        ledger: Arc<RwLock<ShadowLedger>>,
        market_id: u16,
        _symbol_id: u16,
        side: OrderSide,
        price: f64,
        size: f64,
    ) -> Result<String> {
        // Step 1: Optimistically update in_flight_pos (BEFORE API call)
        let signed_size = side.sign() * size;

        {
            let mut ledger_guard = ledger.write();
            ledger_guard.add_in_flight(signed_size);
        } // Lock released immediately

        // Step 2: Prepare order request
        let order_req = CreateOrderRequest {
            market_id,
            side: side.to_string(),
            order_type: "limit".to_string(),
            price,
            size,
            time_in_force: "gtc".to_string(),
        };

        // Step 3: Retry logic with exponential backoff
        const MAX_RETRIES: u32 = 3;
        let mut retries = 0;

        loop {
            match self.send_order(&order_req).await {
                Ok(tx_hash) => {
                    tracing::info!(
                        "✅ Order placed: tx_hash={} side={} price={} size={} (optimistic in_flight updated)",
                        tx_hash,
                        side,
                        price,
                        size
                    );
                    return Ok(tx_hash);
                }
                Err(e) if retries < MAX_RETRIES => {
                    retries += 1;
                    let backoff_ms = 100 * 2u64.pow(retries - 1); // 100ms, 200ms, 400ms
                    tracing::warn!(
                        "Order attempt {} failed: {}, retrying in {}ms...",
                        retries,
                        e,
                        backoff_ms
                    );
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                }
                Err(e) => {
                    // All retries exhausted, rollback in_flight_pos
                    let mut ledger_guard = ledger.write();
                    ledger_guard.add_in_flight(-signed_size);

                    tracing::error!(
                        "❌ Order failed after {} retries, rolled back in_flight: {}",
                        MAX_RETRIES,
                        e
                    );

                    return Err(TradingError::OrderFailedAfterRetries {
                        retries: MAX_RETRIES,
                        reason: e.to_string(),
                    });
                }
            }
        }
    }

    /// Internal method to send order HTTP request
    async fn send_order(&self, order_req: &CreateOrderRequest) -> Result<String> {
        // Get next nonce
        let nonce = {
            let mut n = self.nonce.lock();
            let current = *n;
            *n += 1;
            current
        };

        // Order expiry: use -1 for default (28 days, handled by SDK)
        let order_expiry = -1i64;

        // Get next client_order_index (simple counter)
        let client_order_index = {
            let mut counter = self.client_order_counter.lock();
            let current = *counter;
            *counter += 1;
            current
        };

        // Convert price to Lighter format (price * 100, in cents)
        // E.g., $2061.50 -> 206150
        let price_int = (order_req.price * 100.0) as u32;

        // Convert size to base_amount (size * 10^6)
        let base_amount = (order_req.size * 1_000_000.0) as i64;

        // Sign transaction via FFI
        let signed_tx = self
            .signer
            .sign_create_order(
                order_req.market_id as u8,
                client_order_index,
                base_amount,
                price_int,
                order_req.side == "ask",
                0, // order_type: 0 = Limit
                1, // time_in_force: 1 = GTC
                false,
                0,
                order_expiry, // order_expiry: 30 days from now
                nonce,
            )
            .map_err(|e| TradingError::OrderFailed(format!("Signing failed: {}", e)))?;

        tracing::info!(
            "📝 Signed order: tx_type={} tx_hash={} nonce={}",
            signed_tx.tx_type,
            signed_tx.tx_hash,
            nonce
        );
        tracing::debug!("   Full tx_info: {}", signed_tx.tx_info);

        // Send via HTTP multipart/form-data (as per Python SDK)
        let form = reqwest::multipart::Form::new()
            .text("tx_type", signed_tx.tx_type.to_string())
            .text("tx_info", signed_tx.tx_info.clone());

        tracing::debug!(
            "📤 HTTP Request: POST {}/api/v1/sendTx",
            self.base_url
        );
        tracing::debug!("   Content-Type: multipart/form-data");
        tracing::debug!("   tx_type: {}", signed_tx.tx_type);
        tracing::debug!("   tx_info (first 200 chars): {}", &signed_tx.tx_info[..std::cmp::min(200, signed_tx.tx_info.len())]);

        let response = self
            .client
            .post(format!("{}/api/v1/sendTx", self.base_url))
            .multipart(form)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|_| "Unable to read error body".to_string());
            return Err(TradingError::OrderFailed(format!(
                "HTTP {}: {}",
                status, error_body
            )));
        }

        let resp_text = response.text().await?;
        tracing::info!("✅ Order response: {}", resp_text);

        Ok(signed_tx.tx_hash)
    }

    /// Cancel an order
    pub async fn cancel_order(&self, _order_id: String) -> Result<()> {
        // TODO: Implement cancel via FFI + HTTP
        tracing::warn!("Cancel order not yet implemented");
        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct CreateOrderRequest {
    market_id: u16,
    side: String,
    order_type: String,
    price: f64,
    size: f64,
    time_in_force: String,
}


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
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;

/// Lighter REST API client with Keep-Alive connection pooling
pub struct LighterHttpClient {
    client: Client,
    base_url: String,
    signer: LighterSigner,
    api_key_index: i64,
    account_index: i64,
    nonce: Arc<parking_lot::Mutex<i64>>,
    nonce_initialized: Arc<parking_lot::Mutex<bool>>,
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
            api_key_index,
            account_index,
            nonce: Arc::new(parking_lot::Mutex::new(0)),
            nonce_initialized: Arc::new(parking_lot::Mutex::new(false)),
            client_order_counter: Arc::new(parking_lot::Mutex::new(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64
            )),
        })
    }

    /// Query the next valid nonce from the server (only called once at startup or on error)
    async fn get_next_nonce(&self) -> Result<i64> {
        let url = format!(
            "{}/api/v1/nextNonce?account_index={}&api_key_index={}",
            self.base_url, self.account_index, self.api_key_index
        );

        let response = self.client.get(&url).send().await?;

        if !response.status().is_success() {
            return Err(TradingError::OrderFailed(format!(
                "Failed to get nonce: HTTP {}",
                response.status()
            )));
        }

        #[derive(serde::Deserialize)]
        struct NonceResponse {
            nonce: i64,
        }

        let nonce_resp: NonceResponse = response.json().await?;
        tracing::debug!("📡 Fetched nonce from server: {}", nonce_resp.nonce);
        Ok(nonce_resp.nonce)
    }

    /// Get next nonce (lazy initialization + local increment)
    async fn next_nonce(&self) -> Result<i64> {
        // Check if nonce is initialized
        let initialized = *self.nonce_initialized.lock();

        if !initialized {
            // First time: fetch from server
            let server_nonce = self.get_next_nonce().await?;
            let mut nonce = self.nonce.lock();
            *nonce = server_nonce;
            drop(nonce);

            let mut init = self.nonce_initialized.lock();
            *init = true;
            drop(init);

            tracing::info!("✅ Nonce initialized from server: {}", server_nonce);
            return Ok(server_nonce);
        }

        // Subsequent calls: increment locally
        let mut nonce = self.nonce.lock();
        *nonce += 1;
        let current = *nonce;
        Ok(current)
    }

    /// Reset nonce (called when we get "invalid nonce" error)
    async fn reset_nonce(&self) -> Result<()> {
        tracing::warn!("🔄 Resetting nonce due to error...");
        let server_nonce = self.get_next_nonce().await?;
        let mut nonce = self.nonce.lock();
        *nonce = server_nonce;
        tracing::info!("✅ Nonce reset to: {}", server_nonce);
        Ok(())
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
        // Get next nonce (lazy init + local increment)
        let nonce = self.next_nonce().await?;

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

            // Check if it's an invalid nonce error
            if error_body.contains("invalid nonce") || error_body.contains("21104") {
                tracing::warn!("⚠️  Invalid nonce detected, will reset on next request");
                // Reset nonce for next time
                let _ = self.reset_nonce().await;
            }

            return Err(TradingError::OrderFailed(format!(
                "HTTP {}: {}",
                status, error_body
            )));
        }

        let resp_text = response.text().await?;
        tracing::info!("✅ Order response: {}", resp_text);

        Ok(signed_tx.tx_hash)
    }

    /// Place a market order to close position (emergency exit)
    pub async fn place_market_order(
        &self,
        market_id: u16,
        side: OrderSide,
        size: f64,
        current_price: f64, // Use current market price for IOC orders
    ) -> Result<String> {
        // Get next nonce (lazy init + local increment)
        let nonce = self.next_nonce().await?;

        // Order expiry: use -1 for default
        let order_expiry = -1i64;

        // Get next client_order_index
        let client_order_index = {
            let mut counter = self.client_order_counter.lock();
            let current = *counter;
            *counter += 1;
            current
        };

        // For market orders, use current price with aggressive slippage tolerance
        // Buy: use ask + 5% slippage, Sell: use bid - 5% slippage (more aggressive for closing)
        let slippage_price = match side {
            OrderSide::Buy => current_price * 1.05,  // Pay up to 5% more
            OrderSide::Sell => current_price * 0.95, // Accept 5% less
        };
        let price_int = (slippage_price * 100.0) as u32;

        // Convert size to base_amount
        let base_amount = (size * 1_000_000.0) as i64;

        // Sign transaction via FFI
        // Use reduce_only to close position without requiring additional margin
        let signed_tx = self
            .signer
            .sign_create_order(
                market_id as u8,
                client_order_index,
                base_amount,
                price_int,
                side == OrderSide::Sell,
                0, // order_type: 0 = Limit (more reliable than IOC)
                1, // time_in_force: 1 = GTC
                true, // reduce_only: true (only close existing position, no new margin needed)
                0u32, // trigger_price: 0 (no trigger)
                order_expiry,
                nonce,
            )
            .map_err(|e| TradingError::OrderFailed(format!("Market order signing failed: {}", e)))?;

        tracing::info!(
            "📝 Signed market order: tx_type={} tx_hash={} side={} size={} nonce={}",
            signed_tx.tx_type,
            signed_tx.tx_hash,
            side,
            size,
            nonce
        );

        // Send via HTTP
        let form = reqwest::multipart::Form::new()
            .text("tx_type", signed_tx.tx_type.to_string())
            .text("tx_info", signed_tx.tx_info.clone());

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
                "Market order HTTP {}: {}",
                status, error_body
            )));
        }

        let resp_text = response.text().await?;
        tracing::info!("✅ Market order response: {}", resp_text);

        Ok(signed_tx.tx_hash)
    }

    /// Cancel an order
    pub async fn cancel_order(&self, market_index: u8, order_index: i64) -> Result<()> {
        // Get next nonce (lazy init + local increment)
        let nonce = self.next_nonce().await?;

        // Sign cancel order transaction via FFI
        let signed_tx = self
            .signer
            .sign_cancel_order(market_index, order_index, nonce)
            .map_err(|e| TradingError::OrderFailed(format!("Cancel signing failed: {}", e)))?;

        tracing::info!(
            "📝 Signed cancel: tx_type={} tx_hash={} order_index={} nonce={}",
            signed_tx.tx_type,
            signed_tx.tx_hash,
            order_index,
            nonce
        );

        // Send via HTTP
        let form = reqwest::multipart::Form::new()
            .text("tx_type", signed_tx.tx_type.to_string())
            .text("tx_info", signed_tx.tx_info);

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
                "Cancel failed HTTP {}: {}",
                status, error_body
            )));
        }

        let resp_text = response.text().await?;
        tracing::info!("✅ Order cancelled: order_index={} response={}", order_index, resp_text);

        Ok(())
    }

    /// Cancel all open orders for this account
    pub async fn cancel_all_open_orders(&self, market_id: u8) -> Result<()> {
        use serde::Deserialize;

        #[derive(Deserialize)]
        struct OrderResponse {
            #[serde(rename = "orderIndex")]
            order_index: String,
        }

        #[derive(Deserialize)]
        struct ApiResponse {
            data: Vec<OrderResponse>,
        }

        // Query all open orders
        let url = format!(
            "{}/api/v1/accounts/{}/orders?status=open",
            self.base_url, self.account_index
        );

        let response = self.client.get(&url).send().await?;

        if !response.status().is_success() {
            return Err(TradingError::OrderFailed(
                "Failed to fetch open orders".to_string()
            ));
        }

        let api_response: ApiResponse = response.json().await?;

        tracing::info!("📋 Found {} open orders to cancel", api_response.data.len());

        // Cancel each order
        for order in api_response.data {
            if let Ok(order_index) = order.order_index.parse::<i64>() {
                match self.cancel_order(market_id, order_index).await {
                    Ok(_) => tracing::info!("✅ Cancelled order {}", order_index),
                    Err(e) => tracing::warn!("⚠️ Failed to cancel order {}: {:?}", order_index, e),
                }
                // Small delay to avoid rate limiting
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }

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


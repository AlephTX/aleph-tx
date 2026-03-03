//! Lighter DEX HTTP Order Execution
//!
//! This module implements the "No Boomerang" execution philosophy:
//! - Rust directly executes orders via HTTP Keep-Alive (reqwest)
//! - Optimistic accounting: update in_flight_pos BEFORE API responds
//! - Background WS events from Go reconcile the truth

use crate::error::{Result, TradingError};
use crate::shadow_ledger::{OrderSide, ShadowLedger};
use parking_lot::RwLock;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

/// Lighter REST API client with Keep-Alive connection pooling
pub struct LighterHttpClient {
    client: Client,
    base_url: String,
    api_key: String,
    private_key: String,
}

impl LighterHttpClient {
    /// Create a new Lighter HTTP client with Keep-Alive enabled
    pub fn new(api_key: String, private_key: String) -> Result<Self> {
        let client = Client::builder()
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60))
            .timeout(Duration::from_secs(2)) // 2s timeout (increased from 500ms)
            .build()
            .map_err(TradingError::Network)?;

        Ok(Self {
            client,
            base_url: "https://mainnet.zklighter.elliot.ai/api/v1".to_string(),
            api_key,
            private_key,
        })
    }

    /// Sign a request payload using HMAC-SHA256
    fn sign_request(&self, payload: &str) -> String {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        let mut mac = Hmac::<Sha256>::new_from_slice(self.private_key.as_bytes())
            .expect("HMAC can take key of any size");
        mac.update(payload.as_bytes());
        hex::encode(mac.finalize().into_bytes())
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
    ) -> Result<u64> {
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
                Ok(order_id) => {
                    // Step 4: Register order in shadow ledger
                    {
                        let mut ledger_guard = ledger.write();
                        ledger_guard.register_order(
                            order_id,
                            market_id,
                            side,
                            price,
                            size,
                        );
                    }

                    tracing::info!(
                        "✅ Order placed: id={} side={} price={} size={} (optimistic in_flight updated)",
                        order_id,
                        side,
                        price,
                        size
                    );
                    return Ok(order_id);
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
    async fn send_order(&self, order_req: &CreateOrderRequest) -> Result<u64> {
        // Serialize request for signing
        let payload = serde_json::to_string(order_req)
            .map_err(|e| TradingError::OrderFailed(format!("Serialization error: {}", e)))?;

        // Sign the request
        let signature = self.sign_request(&payload);

        // Send HTTP request
        let response = self
            .client
            .post(format!("{}/orders", self.base_url))
            .header("X-API-Key", &self.api_key)
            .header("X-Signature", signature)
            .header("Content-Type", "application/json")
            .body(payload)
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

        let order_resp: CreateOrderResponse = response.json().await?;
        Ok(order_resp.order_id)
    }

    /// Cancel an order
    pub async fn cancel_order(&self, order_id: u64) -> Result<()> {
        let response = self
            .client
            .delete(format!("{}/orders/{}", self.base_url, order_id))
            .header("X-API-Key", &self.api_key)
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

        tracing::info!("🚫 Order canceled: id={}", order_id);
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

#[derive(Debug, Deserialize)]
struct CreateOrderResponse {
    order_id: u64,
}


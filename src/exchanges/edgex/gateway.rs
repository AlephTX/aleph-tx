//! EdgeX Exchange trait implementation
//!
//! Wraps EdgeXClient to implement the unified Exchange trait with full L2 signature support.

use super::client::EdgeXClient;
use super::model::{
    CancelAllOrderRequest, CancelOrderRequest, CreateOrderRequest, OrderSide, OrderType,
    TimeInForce,
};
use crate::exchange::{BatchOrderParams, BatchOrderResult, Exchange, OrderInfo, OrderResult, Side};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// EdgeX Gateway configuration
pub struct EdgeXConfig {
    pub account_id: u64,
    pub contract_id: u64,
    pub synthetic_asset_id: String,
    pub collateral_asset_id: String,
    pub fee_asset_id: String,
    pub price_decimals: u32,
    pub size_decimals: u32,
    pub resolution: u64,
    pub collateral_resolution: u64,
    pub fee_rate: f64,
}

impl EdgeXConfig {
    /// Load configuration from environment variables and config.toml
    pub fn from_env() -> Result<Self> {
        // Load account_id from .env.edgex (sensitive)
        let account_id = std::env::var("EDGEX_ACCOUNT_ID")
            .map_err(|_| anyhow!("EDGEX_ACCOUNT_ID not set in .env.edgex"))?
            .parse()?;

        // Load from config.toml (non-sensitive)
        let app_config = crate::config::AppConfig::load_default();
        let edgex_cfg = &app_config.edgex;

        let contract_id = edgex_cfg
            .contract_id
            .ok_or_else(|| anyhow!("contract_id not set in config.toml [edgex]"))?;

        let synthetic_asset_id = edgex_cfg
            .synthetic_asset_id
            .clone()
            .ok_or_else(|| anyhow!("synthetic_asset_id not set in config.toml [edgex]"))?;

        let collateral_asset_id = edgex_cfg
            .collateral_asset_id
            .clone()
            .ok_or_else(|| anyhow!("collateral_asset_id not set in config.toml [edgex]"))?;

        let fee_asset_id = edgex_cfg
            .fee_asset_id
            .clone()
            .ok_or_else(|| anyhow!("fee_asset_id not set in config.toml [edgex]"))?;

        let price_decimals = edgex_cfg
            .price_decimals
            .ok_or_else(|| anyhow!("price_decimals not set in config.toml [edgex]"))?;

        let size_decimals = edgex_cfg
            .size_decimals
            .ok_or_else(|| anyhow!("size_decimals not set in config.toml [edgex]"))?;

        let resolution = edgex_cfg
            .resolution
            .ok_or_else(|| anyhow!("resolution not set in config.toml [edgex]"))?;

        let collateral_resolution = edgex_cfg
            .collateral_resolution
            .ok_or_else(|| anyhow!("collateral_resolution not set in config.toml [edgex]"))?;

        let fee_rate = edgex_cfg
            .fee_rate
            .ok_or_else(|| anyhow!("fee_rate not set in config.toml [edgex]"))?;

        Ok(Self {
            account_id,
            contract_id,
            synthetic_asset_id,
            collateral_asset_id,
            fee_asset_id,
            price_decimals,
            size_decimals,
            resolution,
            collateral_resolution,
            fee_rate,
        })
    }
}

pub struct EdgeXGateway {
    client: Arc<EdgeXClient>,
    config: EdgeXConfig,
}

impl EdgeXGateway {
    pub fn new(client: Arc<EdgeXClient>, config: EdgeXConfig) -> Self {
        Self { client, config }
    }

    fn edgex_to_side(side: &OrderSide) -> Side {
        match side {
            OrderSide::Buy => Side::Buy,
            OrderSide::Sell => Side::Sell,
        }
    }

    fn side_to_edgex(side: Side) -> OrderSide {
        match side {
            Side::Buy => OrderSide::Buy,
            Side::Sell => OrderSide::Sell,
        }
    }

    async fn create_order_internal(
        &self,
        side: Side,
        size: f64,
        price: f64,
    ) -> Result<OrderResult> {
        let is_buy = matches!(side, Side::Buy);

        // Generate client_order_id first (needed for nonce calculation)
        let client_order_id = Uuid::new_v4().to_string();

        // Calculate l2_nonce from client_order_id as per EdgeX requirement:
        // l2Nonce = hexToLong(sha256(clientOrderId).substring(0,8))
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(client_order_id.as_bytes());
        let hash_result = hasher.finalize();
        let hex_str = format!("{:x}", hash_result);
        let l2_nonce = u64::from_str_radix(&hex_str[0..8], 16)
            .map_err(|e| anyhow!("Failed to parse nonce from hash: {}", e))?;

        // Calculate values for L2 signature
        let value_dm = price * size; // Decimal value (e.g., 1983.22 * 0.01 = 19.8322)

        // For L2 signature: amount_collateral = int(value_dm * collateral_resolution)
        let amount_collateral = (value_dm * self.config.collateral_resolution as f64) as u64;

        // For L2 signature: amount_synthetic = int(size * resolution)
        // Resolution comes from metadata (starkExResolution)
        let amount_synthetic = (size * self.config.resolution as f64) as u64;

        // Calculate fee: amount_fee = ceil(value_dm * fee_rate * collateral_resolution)
        let amount_fee =
            (value_dm * self.config.fee_rate * self.config.collateral_resolution as f64).ceil()
                as u64;

        // Generate expiration times
        // l2_expire_time: 60 days from now in milliseconds
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let l2_expire_time_ms = now_ms + (60 * 24 * 60 * 60 * 1000); // 60 days in ms
        let expire_time = l2_expire_time_ms - 864_000_000; // 10 days earlier

        // Convert l2_expire_time to hours for both signature AND request
        let l2_expire_time_hours = l2_expire_time_ms / (60 * 60 * 1000);

        // Calculate L2 signature hash
        tracing::debug!(
            "L2 signature inputs: synthetic_asset={}, collateral_asset={}, fee_asset={}, is_buy={}, amount_synthetic={}, amount_collateral={}, amount_fee={}, nonce={}, account_id={}, expire_time_hours={}",
            self.config.synthetic_asset_id,
            self.config.collateral_asset_id,
            self.config.fee_asset_id,
            is_buy,
            amount_synthetic,
            amount_collateral,
            amount_fee,
            l2_nonce,
            self.config.account_id,
            l2_expire_time_hours
        );

        let hash = self.client.signature_manager.calc_limit_order_hash(
            &self.config.synthetic_asset_id,
            &self.config.collateral_asset_id,
            &self.config.fee_asset_id,
            is_buy,
            amount_synthetic,
            amount_collateral,
            amount_fee,
            l2_nonce,
            self.config.account_id,
            l2_expire_time_hours,
        )?;

        // Sign the hash
        let l2_signature = self.client.signature_manager.sign_l2_action(hash)?;

        // Create order request with correct field formats
        let req = CreateOrderRequest {
            price: format!("{:.2}", price), // Round to 2 decimals to avoid floating point issues
            size: format!("{:.4}", size),   // Round to 4 decimals
            r#type: OrderType::Limit,
            time_in_force: TimeInForce::PostOnly,
            reduce_only: false, // Not a reduce-only order
            account_id: self.config.account_id,
            contract_id: self.config.contract_id,
            side: Self::side_to_edgex(side),
            client_order_id: client_order_id.clone(),
            expire_time,
            l2_nonce,
            l2_value: format!("{:.6}", value_dm), // Decimal value with 6 decimals (USDC precision)
            l2_size: format!("{:.4}", size),      // Decimal size with 4 decimals
            l2_limit_fee: format!(
                "{:.6}",
                amount_fee as f64 / self.config.collateral_resolution as f64
            ), // Convert back to decimal
            l2_expire_time: l2_expire_time_ms,    // Use milliseconds for the request
            l2_signature,
        };

        // Submit order
        let resp = self
            .client
            .create_order(&req)
            .await
            .map_err(|e| anyhow!("EdgeX create_order failed: {}", e))?;

        // Debug: Log the full response
        tracing::debug!(
            "EdgeX API Response: {}",
            serde_json::to_string_pretty(&resp).unwrap_or_else(|_| format!("{:?}", resp))
        );

        // EdgeX uses a wrapper format: {"code": "...", "data": {...}, "errorParam": {...}}
        // Check for error code
        if let Some(code) = resp.get("code").and_then(|v| v.as_str())
            && code != "SUCCESS"
            && code != "OK"
        {
            let error_msg = resp
                .get("errorParam")
                .and_then(|v| serde_json::to_string(v).ok())
                .unwrap_or_else(|| code.to_string());
            return Err(anyhow!("EdgeX API error: {} - {}", code, error_msg));
        }

        // Extract order_id from data field
        let order_id = resp
            .get("data")
            .and_then(|data| data.get("orderId"))
            .and_then(|v| v.as_str())
            .or_else(|| {
                resp.get("data")
                    .and_then(|data| data.get("order_id"))
                    .and_then(|v| v.as_str())
            })
            .ok_or_else(|| anyhow!("Missing orderId in response data"))?;

        Ok(OrderResult {
            tx_hash: order_id.to_string(),
            client_order_index: l2_nonce as i64,
        })
    }
}

#[async_trait]
impl Exchange for EdgeXGateway {
    async fn buy(&self, size: f64, price: f64) -> Result<OrderResult> {
        self.create_order_internal(Side::Buy, size, price).await
    }

    async fn sell(&self, size: f64, price: f64) -> Result<OrderResult> {
        self.create_order_internal(Side::Sell, size, price).await
    }

    async fn place_batch(&self, params: BatchOrderParams) -> Result<BatchOrderResult> {
        // EdgeX doesn't have native batch API, execute sequentially
        let bid_result = self.buy(params.bid_size, params.bid_price).await?;
        let ask_result = self.sell(params.ask_size, params.ask_price).await?;

        Ok(BatchOrderResult {
            tx_hashes: vec![bid_result.tx_hash.clone(), ask_result.tx_hash.clone()],
            bid_client_order_index: bid_result.client_order_index,
            ask_client_order_index: ask_result.client_order_index,
        })
    }

    async fn cancel_order(&self, order_id: i64) -> Result<()> {
        let req = CancelOrderRequest {
            account_id: self.config.account_id,
            order_id: Some(order_id as u64),
            client_order_id: None,
            contract_id: self.config.contract_id,
        };

        self.client
            .cancel_order(&req)
            .await
            .map_err(|e| anyhow!("EdgeX cancel_order failed: {}", e))?;
        Ok(())
    }

    async fn cancel_all(&self) -> Result<u32> {
        let req = CancelAllOrderRequest {
            account_id: self.config.account_id,
            filter_contract_id_list: vec![self.config.contract_id],
        };

        self.client
            .cancel_all_orders(&req)
            .await
            .map_err(|e| anyhow!("EdgeX cancel_all failed: {}", e))?;
        Ok(0) // EdgeX doesn't return count
    }

    async fn get_active_orders(&self) -> Result<Vec<OrderInfo>> {
        let orders = self
            .client
            .get_open_orders(self.config.account_id)
            .await
            .map_err(|e| anyhow!("EdgeX get_open_orders failed: {}", e))?;

        // Filter by contract_id
        Ok(orders
            .into_iter()
            .filter(|o| o.contract_id == self.config.contract_id)
            .map(|o| OrderInfo {
                order_id: o.order_id.to_string(),
                client_order_index: 0,
                side: Self::edgex_to_side(&o.side),
                price: o.price.parse().unwrap_or(0.0),
                size: o.size.parse().unwrap_or(0.0),
                filled: o.filled_size.parse().unwrap_or(0.0),
            })
            .collect())
    }

    async fn close_all_positions(&self, current_price: f64) -> Result<()> {
        let positions = self
            .client
            .get_positions(self.config.account_id)
            .await
            .map_err(|e| anyhow!("EdgeX get_positions failed: {}", e))?;

        for pos in positions {
            let pos_contract_id: u64 = pos.contract_id.parse().unwrap_or(0);
            if pos_contract_id != self.config.contract_id {
                continue;
            }

            let size: f64 = pos.open_size.parse().unwrap_or(0.0);
            if size.abs() < 0.0001 {
                continue;
            }

            // Close position with market order
            let side = if size > 0.0 { Side::Sell } else { Side::Buy };
            self.create_order_internal(side, size.abs(), current_price)
                .await?;
        }

        Ok(())
    }
}

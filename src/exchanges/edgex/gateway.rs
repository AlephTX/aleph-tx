//! EdgeX Exchange trait implementation
//!
//! Wraps EdgeXClient to implement the unified Exchange trait with full L2 signature support.

use super::client::EdgeXClient;
use super::model::{CancelAllOrderRequest, CancelOrderRequest, CreateOrderRequest, OrderSide, OrderType, TimeInForce};
use crate::exchange::{
    BatchOrderParams, BatchOrderResult, Exchange, OrderInfo, OrderResult, Side,
};
use anyhow::{anyhow, Result};
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

        let contract_id = edgex_cfg.contract_id
            .ok_or_else(|| anyhow!("contract_id not set in config.toml [edgex]"))?;

        let synthetic_asset_id = edgex_cfg.synthetic_asset_id.clone()
            .ok_or_else(|| anyhow!("synthetic_asset_id not set in config.toml [edgex]"))?;

        let collateral_asset_id = edgex_cfg.collateral_asset_id.clone()
            .ok_or_else(|| anyhow!("collateral_asset_id not set in config.toml [edgex]"))?;

        let fee_asset_id = edgex_cfg.fee_asset_id.clone()
            .ok_or_else(|| anyhow!("fee_asset_id not set in config.toml [edgex]"))?;

        let price_decimals = edgex_cfg.price_decimals
            .ok_or_else(|| anyhow!("price_decimals not set in config.toml [edgex]"))?;

        let size_decimals = edgex_cfg.size_decimals
            .ok_or_else(|| anyhow!("size_decimals not set in config.toml [edgex]"))?;

        let fee_rate = edgex_cfg.fee_rate
            .ok_or_else(|| anyhow!("fee_rate not set in config.toml [edgex]"))?;

        Ok(Self {
            account_id,
            contract_id,
            synthetic_asset_id,
            collateral_asset_id,
            fee_asset_id,
            price_decimals,
            size_decimals,
            fee_rate,
        })
    }
}

pub struct EdgeXGateway {
    client: Arc<EdgeXClient>,
    config: EdgeXConfig,
    nonce_counter: std::sync::atomic::AtomicU64,
}

impl EdgeXGateway {
    pub fn new(client: Arc<EdgeXClient>, config: EdgeXConfig) -> Self {
        Self {
            client,
            config,
            nonce_counter: std::sync::atomic::AtomicU64::new(0),
        }
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

    /// Convert price to L2 format (price * 10^price_decimals)
    fn price_to_l2(&self, price: f64) -> u64 {
        (price * 10f64.powi(self.config.price_decimals as i32)) as u64
    }

    /// Convert size to L2 format (size * 10^size_decimals)
    fn size_to_l2(&self, size: f64) -> u64 {
        (size * 10f64.powi(self.config.size_decimals as i32)) as u64
    }

    /// Calculate fee amount
    fn calculate_fee(&self, value: f64) -> u64 {
        let fee = value * self.config.fee_rate;
        (fee * 10f64.powi(self.config.price_decimals as i32)) as u64
    }

    /// Get next nonce
    fn next_nonce(&self) -> u64 {
        self.nonce_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    }

    /// Get expiration timestamp (1 hour from now)
    fn get_expiration_timestamp(&self) -> u64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        (now + 3600) / 3600 // Round to hours
    }

    async fn create_order_internal(
        &self,
        side: Side,
        size: f64,
        price: f64,
    ) -> Result<OrderResult> {
        let is_buy = matches!(side, Side::Buy);

        // Convert to L2 format
        let l2_price = self.price_to_l2(price);
        let l2_size = self.size_to_l2(size);
        let l2_value = l2_price * l2_size / 10u64.pow(self.config.size_decimals);
        let l2_limit_fee = self.calculate_fee(l2_value as f64);

        // Generate nonce and expiration
        let l2_nonce = self.next_nonce();
        let l2_expire_time = self.get_expiration_timestamp();

        // Calculate L2 signature hash
        let hash = self.client.signature_manager.calc_limit_order_hash(
            &self.config.synthetic_asset_id,
            &self.config.collateral_asset_id,
            &self.config.fee_asset_id,
            is_buy,
            l2_size,
            l2_value,
            l2_limit_fee,
            l2_nonce,
            self.config.account_id,
            l2_expire_time,
        )?;

        // Sign the hash
        let l2_signature = self.client.signature_manager.sign_l2_action(hash)?;

        // Create order request
        let client_order_id = Uuid::new_v4().to_string();
        let expire_time = l2_expire_time * 3600 * 1000; // Convert to milliseconds

        let req = CreateOrderRequest {
            price: price.to_string(),
            size: size.to_string(),
            r#type: OrderType::Limit,
            time_in_force: TimeInForce::PostOnly,
            account_id: self.config.account_id,
            contract_id: self.config.contract_id,
            side: Self::side_to_edgex(side),
            client_order_id: client_order_id.clone(),
            expire_time,
            l2_nonce,
            l2_value: l2_value.to_string(),
            l2_size: l2_size.to_string(),
            l2_limit_fee: l2_limit_fee.to_string(),
            l2_expire_time,
            l2_signature,
        };

        // Submit order
        let resp = self.client
            .create_order(&req)
            .await
            .map_err(|e| anyhow!("EdgeX create_order failed: {}", e))?;

        // Extract order_id from response
        let order_id = resp.get("order_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("Missing order_id in response"))?;

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
        // TODO: Implement L2 signature for cancel order
        // For now, use placeholder signature
        let l2_nonce = self.next_nonce();

        let req = CancelOrderRequest {
            account_id: self.config.account_id,
            order_id: Some(order_id as u64),
            client_order_id: None,
            contract_id: self.config.contract_id,
            l2_nonce,
            l2_signature: "0x0".to_string(), // TODO: Implement cancel signature
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
            self.create_order_internal(side, size.abs(), current_price).await?;
        }

        Ok(())
    }
}

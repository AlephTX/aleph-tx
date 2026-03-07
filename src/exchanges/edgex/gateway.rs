//! EdgeX Exchange trait implementation
//!
//! Wraps EdgeXClient to implement the unified Exchange trait.
//! NOTE: This is a simplified implementation. Full L2 signature logic needs proper integration.

use super::client::EdgeXClient;
use super::model::{CancelAllOrderRequest, CancelOrderRequest, CreateOrderRequest, OrderSide, OrderType, TimeInForce};
use crate::exchange::{
    BatchOrderParams, BatchOrderResult, Exchange, OrderInfo, OrderResult, Side,
};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct EdgeXGateway {
    client: Arc<EdgeXClient>,
    account_id: u64,
    contract_id: u64,
}

impl EdgeXGateway {
    pub fn new(client: Arc<EdgeXClient>, account_id: u64, contract_id: u64) -> Self {
        Self {
            client,
            account_id,
            contract_id,
        }
    }

    fn side_to_edgex(side: Side) -> OrderSide {
        match side {
            Side::Buy => OrderSide::Buy,
            Side::Sell => OrderSide::Sell,
        }
    }

    fn edgex_to_side(side: &OrderSide) -> Side {
        match side {
            OrderSide::Buy => Side::Buy,
            OrderSide::Sell => Side::Sell,
        }
    }

    async fn create_order_internal(
        &self,
        _side: Side,
        _size: f64,
        _price: f64,
    ) -> Result<OrderResult> {
        // TODO: Implement full L2 signature logic
        // This requires proper integration with SignatureManager.calc_limit_order_hash
        Err(anyhow!("EdgeX order execution not yet implemented - requires L2 signature integration"))
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
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // TODO: Proper L2 signature for cancel
        let req = CancelOrderRequest {
            account_id: self.account_id,
            order_id: Some(order_id as u64),
            client_order_id: None,
            contract_id: self.contract_id,
            l2_nonce: now,
            l2_signature: "0x0".to_string(), // Placeholder
        };

        self.client
            .cancel_order(&req)
            .await
            .map_err(|e| anyhow!("EdgeX cancel_order failed: {}", e))?;
        Ok(())
    }

    async fn cancel_all(&self) -> Result<u32> {
        let req = CancelAllOrderRequest {
            account_id: self.account_id,
            filter_contract_id_list: vec![self.contract_id],
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
            .get_open_orders(self.account_id)
            .await
            .map_err(|e| anyhow!("EdgeX get_open_orders failed: {}", e))?;

        // Filter by contract_id
        Ok(orders
            .into_iter()
            .filter(|o| o.contract_id == self.contract_id)
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

    async fn close_all_positions(&self, _current_price: f64) -> Result<()> {
        let positions = self
            .client
            .get_positions(self.account_id)
            .await
            .map_err(|e| anyhow!("EdgeX get_positions failed: {}", e))?;

        for pos in positions {
            let pos_contract_id: u64 = pos.contract_id.parse().unwrap_or(0);
            if pos_contract_id != self.contract_id {
                continue;
            }

            let size: f64 = pos.open_size.parse().unwrap_or(0.0);
            if size.abs() < 0.0001 {
                continue;
            }

            // TODO: Implement position closing with proper L2 signature
            return Err(anyhow!("EdgeX position closing not yet implemented"));
        }

        Ok(())
    }
}


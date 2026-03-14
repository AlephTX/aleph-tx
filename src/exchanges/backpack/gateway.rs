//! Backpack Exchange trait implementation
//!
//! Wraps BackpackClient to implement the unified Exchange trait.

use super::client::BackpackClient;
use super::model::BackpackOrderRequest;
use crate::exchange::{BatchOrderParams, BatchOrderResult, Exchange, OrderInfo, OrderResult};
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use std::sync::Arc;

pub struct BackpackGateway {
    client: Arc<BackpackClient>,
    symbol: String,
}

impl BackpackGateway {
    pub fn new(client: Arc<BackpackClient>, symbol: String) -> Self {
        Self { client, symbol }
    }
}

#[async_trait]
impl Exchange for BackpackGateway {
    async fn buy(&self, size: f64, price: f64) -> Result<OrderResult> {
        let order = BackpackOrderRequest {
            symbol: self.symbol.clone(),
            side: "Bid".to_string(),
            order_type: "Limit".to_string(),
            price: price.to_string(),
            quantity: size.to_string(),
            client_id: None,
            post_only: Some(true),
            time_in_force: None,
        };

        let resp = self.client.create_order(&order).await?;
        Ok(OrderResult {
            tx_hash: resp.id.clone(),
            client_order_index: 0, // Backpack doesn't use nonce
        })
    }

    async fn sell(&self, size: f64, price: f64) -> Result<OrderResult> {
        let order = BackpackOrderRequest {
            symbol: self.symbol.clone(),
            side: "Ask".to_string(),
            order_type: "Limit".to_string(),
            price: price.to_string(),
            quantity: size.to_string(),
            client_id: None,
            post_only: Some(true),
            time_in_force: None,
        };

        let resp = self.client.create_order(&order).await?;
        Ok(OrderResult {
            tx_hash: resp.id.clone(),
            client_order_index: 0,
        })
    }

    async fn place_batch(&self, params: BatchOrderParams) -> Result<BatchOrderResult> {
        // Backpack doesn't have native batch API, execute sequentially
        let bid_result = self.buy(params.bid_size, params.bid_price).await?;
        let ask_result = self.sell(params.ask_size, params.ask_price).await?;

        Ok(BatchOrderResult {
            tx_hashes: vec![bid_result.tx_hash.clone(), ask_result.tx_hash.clone()],
            bid_client_order_index: bid_result.client_order_index,
            ask_client_order_index: ask_result.client_order_index,
        })
    }

    async fn cancel_order(&self, _order_id: i64) -> Result<()> {
        // Backpack uses string order IDs, not supported in generic trait
        Err(anyhow!("cancel_order by ID not supported for Backpack"))
    }

    async fn cancel_all(&self) -> Result<u32> {
        self.client.cancel_all_orders(&self.symbol).await?;
        Ok(0) // Backpack doesn't return count
    }

    async fn get_active_orders(&self) -> Result<Vec<OrderInfo>> {
        // Backpack doesn't have a direct "get open orders" API
        // Would need to implement via order history filtering
        Ok(vec![])
    }

    async fn close_all_positions(&self, current_price: f64) -> Result<()> {
        let positions = self.client.get_open_positions().await?;

        for pos in positions {
            if pos.symbol != self.symbol {
                continue;
            }

            let qty: f64 = pos.quantity.parse().unwrap_or(0.0);
            if qty.abs() < 0.0001 {
                continue;
            }

            // Reverse position with market order
            let side = if qty > 0.0 { "Ask" } else { "Bid" };
            let order = BackpackOrderRequest {
                symbol: self.symbol.clone(),
                side: side.to_string(),
                order_type: "Market".to_string(),
                price: current_price.to_string(),
                quantity: qty.abs().to_string(),
                client_id: None,
                post_only: None,
                time_in_force: None,
            };

            self.client.create_order(&order).await?;
        }

        Ok(())
    }
}

//! Backpack Exchange trait implementation
//!
//! Wraps BackpackClient to implement the unified Exchange trait.

use super::client::BackpackClient;
use super::model::BackpackOrderRequest;
use crate::error::TradingError;
use crate::exchange::{
    BatchAction, BatchOrderParams, BatchOrderResult, BatchResult, Exchange, OrderInfo, OrderParams,
    OrderResult, OrderType, PlaceResult,
};
// use anyhow::anyhow;
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

    pub async fn place_order(&self, params: OrderParams) -> anyhow::Result<OrderResult> {
        let side = match params.side {
            crate::exchange::Side::Buy => "Bid",
            crate::exchange::Side::Sell => "Ask",
        };
        let order = BackpackOrderRequest {
            symbol: self.symbol.clone(),
            side: side.to_string(),
            order_type: "Limit".to_string(),
            price: params.price.to_string(),
            quantity: params.size.to_string(),
            client_id: None,
            post_only: Some(true),
            time_in_force: None,
        };

        let resp = self.client.create_order(&order).await.map_err(|e| {
            let err_str = e.to_string();
            // The provided diff for error handling was syntactically incorrect and referenced undefined variables.
            // Reverting to original error handling for now, as the instruction was ambiguous on how to fix it.
            // If specific error codes or messages from Backpack API need to be handled, they should be added here.
            if err_str.contains("insufficient balance") || err_str.contains("insufficient funds") {
                TradingError::InsufficientMargin
            } else if err_str.contains("Rate limit") || err_str.contains("429") {
                TradingError::ApiError { status: 429, message: err_str }
            } else {
                TradingError::OrderFailed(err_str)
            }
        })?;
        Ok(OrderResult {
            tx_hash: resp.id.clone(),
            client_order_index: 0,
        })
    }
}

#[async_trait]
impl Exchange for BackpackGateway {
    async fn buy(&self, size: f64, price: f64) -> anyhow::Result<OrderResult> {
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

    async fn sell(&self, size: f64, price: f64) -> anyhow::Result<OrderResult> {
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

    async fn place_batch(&self, params: BatchOrderParams) -> anyhow::Result<BatchOrderResult> {
        // Backpack doesn't have native batch API, execute sequentially
        let bid_result = self.buy(params.bid_size, params.bid_price).await?;
        let ask_result = self.sell(params.ask_size, params.ask_price).await?;

        Ok(BatchOrderResult {
            tx_hashes: vec![bid_result.tx_hash.clone(), ask_result.tx_hash.clone()],
            bid_client_order_index: bid_result.client_order_index,
            ask_client_order_index: ask_result.client_order_index,
        })
    }

    async fn cancel_order(&self, _order_id: i64) -> anyhow::Result<()> {
        // Backpack uses string order IDs, not supported in generic trait
        Err(TradingError::OrderFailed("cancel_order by ID not supported for Backpack".to_string()).into())
    }

    async fn cancel_all(&self) -> anyhow::Result<u32> {
        self.client.cancel_all_orders(&self.symbol).await?;
        Ok(0) // Backpack doesn't return count
    }

    async fn get_active_orders(&self) -> anyhow::Result<Vec<OrderInfo>> {
        // Backpack doesn't have a direct "get open orders" API
        // Would need to implement via order history filtering
        Ok(vec![])
    }

    async fn close_all_positions(&self, current_price: f64) -> anyhow::Result<()> {
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

    async fn execute_batch(&self, actions: Vec<BatchAction>) -> anyhow::Result<BatchResult> {
        let mut tx_hashes = Vec::new();
        let mut place_results = Vec::new();

        for action in actions {
            match action {
                BatchAction::Cancel(id) => {
                    self.cancel_order(id).await?;
                }
                BatchAction::Place(params) => {
                    let side = params.side;
                    let price = params.price;
                    let size = params.size;
                    let res = self.place_order(params).await?;
                    tx_hashes.push(res.tx_hash);
                    place_results.push(PlaceResult {
                        client_order_index: res.client_order_index,
                        side,
                        price,
                        size,
                    });
                }
            }
        }

        Ok(BatchResult {
            tx_hashes,
            place_results,
        })
    }

    async fn get_account_stats(&self) -> anyhow::Result<crate::strategy::inventory_neutral_mm::AccountStats> {
        let stats = self.client.get_account_stats().await?;
        Ok(crate::strategy::inventory_neutral_mm::AccountStats {
            available_balance: stats.available_balance,
            portfolio_value: stats.portfolio_value,
            position: stats.position,
            leverage: stats.leverage,
            margin_usage: stats.margin_usage,
            last_update: std::time::Instant::now(),
        })
    }

    fn limit_order_type(&self) -> OrderType {
        OrderType::PostOnly
    }
}

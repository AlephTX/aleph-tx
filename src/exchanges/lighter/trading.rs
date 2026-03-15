//! Lighter Trading API - 币种无关的交易接口封装
//!
//! 基于 lighter_ffi (签名) + HTTP (执行) 的完整交易层。
//! 修复了 nonce 管理、价格格式、连接池复用、批量下单等问题。
//!
//! 功能：
//! - buy / sell — 单独下单
//! - place_batch — 一买一卖（真正的 sendTxBatch）
//! - cancel_order / cancel_all — 撤单
//! - get_order / get_active_orders / verify_order — 查询验证

use super::error::{LighterErrorCode, LighterErrorResponse};
use crate::error::TradingError;
use crate::order_tracker::{OrderSide as TrackerSide, OrderTracker};

use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as TokioMutex;

// ─── Types ───────────────────────────────────────────────────────────────────

use crate::exchange::{
    BatchAction, BatchOrderParams, BatchOrderResult, BatchResult, Exchange, OrderInfo, OrderParams,
    OrderResult, OrderType, PlaceResult, Side,
};
use async_trait::async_trait;

/// 订单详情（匹配 Lighter API 实际返回格式）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderDetail {
    pub order_index: i64,
    pub client_order_index: i64,
    pub order_id: String,
    pub market_index: u8,
    pub owner_account_index: i64,
    pub initial_base_amount: String,
    pub price: String,
    pub nonce: i64,
    pub remaining_base_amount: String,
    pub is_ask: bool,
    pub base_size: i64,
    pub base_price: i32,
    pub filled_base_amount: String,
    pub filled_quote_amount: String,
    pub side: String,
    #[serde(rename = "type")]
    pub order_type: String,
    pub time_in_force: String,
    pub reduce_only: bool,
    pub trigger_price: String,
    pub order_expiry: i64,
    pub status: String,
    pub trigger_status: String,
    pub block_height: i64,
    pub timestamp: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// API 响应：订单列表
#[derive(Debug, Deserialize)]
struct OrdersResponse {
    code: i32,
    message: Option<String>,
    orders: Vec<OrderDetail>,
}

/// API 响应：sendTx
#[derive(Debug, Deserialize)]
struct SendTxResponse {
    code: i32,
    message: Option<String>,
    #[allow(dead_code)]
    tx_hash: Option<String>,
}

/// API 响应：sendTxBatch
#[derive(Debug, Deserialize)]
struct SendTxBatchResponse {
    code: i32,
    message: Option<String>,
    tx_hash: Option<Vec<String>>,
}

/// API 响应：nextNonce
#[derive(Debug, Deserialize)]
struct NonceResponse {
    nonce: i64,
}

/// 仓位信息
#[derive(Debug, Clone, Deserialize)]
pub struct Position {
    pub market_id: u8,
    pub symbol: String,
    pub sign: i8,
    pub position: String,
    pub avg_entry_price: String,
    pub unrealized_pnl: String,
    pub liquidation_price: String,
}

/// 账户信息
#[derive(Debug, Deserialize)]
struct AccountInfo {
    positions: Vec<Position>,
}

#[derive(Debug, Deserialize)]
struct AccountsResponse {
    code: i32,
    accounts: Vec<AccountInfo>,
}

// ─── Client ──────────────────────────────────────────────────────────────────

/// Lighter 交易客户端（币种无关）
pub struct LighterTrading {
    signer: Arc<super::ffi::LighterSigner>,
    client: Client,
    base_url: String,
    send_tx_url: String,
    send_tx_batch_url: String,
    account_index: i64,
    api_key_index: i64,
    market_id: u8,
    size_multiplier: f64,
    price_multiplier: f64,
    nonce: AtomicI64,
    nonce_init: TokioMutex<bool>,
    client_order_counter: AtomicI64,
    /// Optional order tracker for per-order state machine (v5.0.0)
    order_tracker: Option<Arc<OrderTracker>>,
    /// Default order type for limit orders (Limit or LimitPostOnly)
    limit_order_type: OrderType,
}

#[async_trait]
impl Exchange for LighterTrading {
    async fn buy(&self, size: f64, price: f64) -> Result<OrderResult> {
        self.buy(size, price).await
    }

    async fn sell(&self, size: f64, price: f64) -> Result<OrderResult> {
        self.sell(size, price).await
    }

    async fn place_batch(&self, params: BatchOrderParams) -> Result<BatchOrderResult> {
        self.place_batch(params).await
    }

    async fn cancel_order(&self, order_id: i64) -> Result<()> {
        self.cancel_order(order_id).await
    }

    async fn cancel_all(&self) -> Result<u32> {
        self.cancel_all().await
    }

    async fn get_active_orders(&self) -> Result<Vec<OrderInfo>> {
        let orders = self.get_active_orders().await?;
        Ok(orders
            .into_iter()
            .map(|o| OrderInfo {
                order_id: o.order_index.to_string(),
                client_order_index: o.client_order_index,
                side: if o.is_ask { Side::Sell } else { Side::Buy },
                price: o.price.parse().unwrap_or(0.0),
                size: o.initial_base_amount.parse().unwrap_or(0.0),
                filled: o.filled_base_amount.parse().unwrap_or(0.0), // Corrected from executed_base_amount
            })
            .collect())
    }

    async fn close_all_positions(&self, current_price: f64) -> Result<()> {
        self.close_all_positions(current_price).await
    }

    async fn execute_batch(&self, actions: Vec<BatchAction>) -> Result<BatchResult> {
        self.execute_batch(actions).await
    }

    fn limit_order_type(&self) -> OrderType {
        self.limit_order_type
    }
}

impl LighterTrading {
    /// 创建交易客户端
    ///
    /// `market_id`: 市场索引 (0 = ETH perps)
    /// 自动从 orderBookDetails 获取价格/大小精度
    pub async fn new(market_id: u8) -> Result<Self> {
        let base_url = std::env::var("LIGHTER_BASE_URL")
            .unwrap_or_else(|_| "https://mainnet.zklighter.elliot.ai".to_string());
        let private_key = std::env::var("LIGHTER_PRIVATE_KEY")
            .or_else(|_| std::env::var("API_KEY_PRIVATE_KEY"))
            .map_err(|_| anyhow::anyhow!("LIGHTER_PRIVATE_KEY or API_KEY_PRIVATE_KEY not set"))?;
        let chain_id: i32 = std::env::var("LIGHTER_CHAIN_ID")
            .unwrap_or_else(|_| "304".to_string())
            .parse()?;
        let api_key_index: i64 = std::env::var("LIGHTER_API_KEY_INDEX")
            .map_err(|_| anyhow::anyhow!("LIGHTER_API_KEY_INDEX not set"))?
            .parse()?;
        let account_index: i64 = std::env::var("LIGHTER_ACCOUNT_INDEX")
            .map_err(|_| anyhow::anyhow!("LIGHTER_ACCOUNT_INDEX not set"))?
            .parse()?;

        let signer = Arc::new(
            super::ffi::LighterSigner::new(
                &base_url,
                &private_key,
                chain_id,
                api_key_index,
                account_index,
            )
            .map_err(|e| anyhow::anyhow!("Signer init failed: {}", e))?,
        );

        let client = Client::builder()
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60))
            .timeout(Duration::from_secs(15))
            .user_agent("AlephTX/5.0")
            .build()?;

        // 从 orderBookDetails 获取精度
        let (size_decimals, price_decimals) =
            Self::fetch_market_decimals(&client, &base_url, market_id).await?;
        tracing::info!(
            "Market {} decimals: size={} price={}",
            market_id,
            size_decimals,
            price_decimals
        );

        let counter_start = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::from_secs(0))
            .as_secs() as i64;

        let send_tx_url = format!("{}/api/v1/sendTx", base_url);
        let send_tx_batch_url = format!("{}/api/v1/sendTxBatch", base_url);
        let size_multiplier = 10f64.powi(size_decimals as i32);
        let price_multiplier = 10f64.powi(price_decimals as i32);

        Ok(Self {
            signer,
            client,
            base_url,
            send_tx_url,
            send_tx_batch_url,
            account_index,
            api_key_index,
            market_id,
            size_multiplier,
            price_multiplier,
            nonce: AtomicI64::new(0),
            nonce_init: TokioMutex::new(false),
            client_order_counter: AtomicI64::new(counter_start),
            order_tracker: None,
            limit_order_type: OrderType::Limit,
        })
    }

    /// 从 orderBookDetails 获取市场精度
    async fn fetch_market_decimals(
        client: &Client,
        base_url: &str,
        market_id: u8,
    ) -> Result<(u8, u8)> {
        #[derive(Deserialize)]
        struct MarketDetail {
            market_id: u8,
            size_decimals: u8,
            price_decimals: u8,
        }
        #[derive(Deserialize)]
        struct OBDetailsResp {
            order_book_details: Vec<MarketDetail>,
        }

        let url = format!("{}/api/v1/orderBookDetails", base_url);
        let resp = client.get(&url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("orderBookDetails HTTP {}", resp.status());
        }
        let details: OBDetailsResp = resp.json().await?;
        let market = details
            .order_book_details
            .iter()
            .find(|m| m.market_id == market_id)
            .ok_or_else(|| anyhow::anyhow!("Market {} not found in orderBookDetails", market_id))?;

        Ok((market.size_decimals, market.price_decimals))
    }

    /// Attach an order tracker for per-order state machine (v5.0.0)
    pub fn set_order_tracker(&mut self, tracker: Arc<OrderTracker>) {
        self.order_tracker = Some(tracker);
    }

    /// Enable Post-Only (ALO) mode for all limit orders
    pub fn set_post_only(&mut self, enabled: bool) {
        self.limit_order_type = if enabled {
            OrderType::PostOnly
        } else {
            OrderType::Limit
        };
    }

    // ─── Nonce 管理 ────────────────────────────────────────────────────────

    /// 从服务器获取 nonce
    async fn fetch_nonce(&self) -> Result<i64> {
        let url = format!(
            "{}/api/v1/nextNonce?account_index={}&api_key_index={}",
            self.base_url, self.account_index, self.api_key_index
        );
        let resp = self.client.get(&url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("Failed to fetch nonce: HTTP {}", resp.status());
        }
        let nonce_resp: NonceResponse = resp.json().await?;
        Ok(nonce_resp.nonce)
    }

    /// 获取当前 nonce（不递增）
    async fn get_nonce(&self) -> Result<i64> {
        {
            let mut initialized = self.nonce_init.lock().await;
            if !*initialized {
                let server_nonce = self.fetch_nonce().await?;
                self.nonce.store(server_nonce, Ordering::Release);
                *initialized = true;
                tracing::info!("Nonce initialized from server: {}", server_nonce);
                return Ok(server_nonce);
            }
        }
        Ok(self.nonce.load(Ordering::SeqCst))
    }

    /// 递增 nonce（仅在交易成功提交后调用）
    fn increment_nonce(&self) {
        self.nonce.fetch_add(1, Ordering::SeqCst);
    }

    /// 重置 nonce（遇到 invalid nonce 错误时调用）
    async fn reset_nonce(&self) -> Result<i64> {
        let mut initialized = self.nonce_init.lock().await;
        let server_nonce = self.fetch_nonce().await?;
        self.nonce.store(server_nonce, Ordering::Release);
        *initialized = true;
        tracing::warn!("Nonce reset to: {}", server_nonce);
        Ok(server_nonce)
    }

    /// 生成唯一 client_order_index
    fn next_client_order_index(&self) -> i64 {
        self.client_order_counter.fetch_add(1, Ordering::SeqCst)
    }

    // ─── Auth Token ──────────────────────────────────────────────────────

    /// 创建认证 token（用于查询需要认证的 API）
    /// Go feeder 用 deadline.Unix()（秒级绝对时间戳）
    async fn create_auth_token(&self) -> Result<String> {
        let deadline_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| anyhow::anyhow!("System clock error: {}", e))?
            .as_secs() as i64
            + 600;
        // Phase 3: Direct FFI call (<100us, no spawn_blocking needed)
        self.signer
            .create_auth_token(deadline_secs)
            .map_err(|e| anyhow::anyhow!("Auth token failed: {}", e))
    }

    // ─── 签名 + 发送 ─────────────────────────────────────────────────────

    /// 签名一笔订单（使用指定的 nonce）
    async fn sign_order_with_nonce(
        &self,
        side: Side,
        price: f64,
        size: f64,
        order_type: OrderType,
        reduce_only: bool,
        nonce: i64,
    ) -> Result<(u8, String, String, i64)> {
        let client_order_index = self.next_client_order_index();

        // 使用 round() 防止浮点截断: 2085.87 * 100 = 208587.0 而非 208586
        let price_int = (price * self.price_multiplier).round() as u32;
        let base_amount = (size * self.size_multiplier).round() as i64;

        let (ot, tif) = match order_type {
            OrderType::Limit => (0u8, 1u8),         // Limit + GTC
            OrderType::PostOnly => (0u8, 2u8),      // Limit + ALO (Add Liquidity Only / Post-Only)
            OrderType::Market => (1u8, 3u8),        // Market + IOC
            OrderType::Ioc => (0u8, 3u8),           // Limit + IOC
        };

        let market_id = self.market_id;
        let is_ask = side == Side::Sell;

        // Phase 3: Direct FFI call (<100us, no spawn_blocking needed)
        let signed = self
            .signer
            .sign_create_order(
                market_id,
                client_order_index,
                base_amount,
                price_int,
                is_ask,
                ot,
                tif,
                reduce_only,
                0u32,
                -1i64,
                nonce,
            )
            .map_err(|e| anyhow::anyhow!("Sign failed: {}", e))?;

        tracing::debug!(
            "Signed: tx_type={} price_int={} base_amount={} is_ask={} nonce={}",
            signed.tx_type,
            price_int,
            base_amount,
            is_ask,
            nonce
        );

        Ok((
            signed.tx_type,
            signed.tx_info,
            signed.tx_hash,
            client_order_index,
        ))
    }

    /// 签名一笔订单，返回 (tx_type, tx_info, tx_hash, client_order_index)
    /// Phase 3: Direct FFI call (<100us, no spawn_blocking needed)
    async fn sign_order(
        &self,
        side: Side,
        price: f64,
        size: f64,
        order_type: OrderType,
        reduce_only: bool,
    ) -> Result<(u8, String, String, i64)> {
        let nonce = self.get_nonce().await?;
        self.sign_order_with_nonce(side, price, size, order_type, reduce_only, nonce)
            .await
    }

    /// 发送单笔交易到 sendTx
    async fn send_tx(&self, tx_type: u8, tx_info: String) -> Result<SendTxResponse> {
        let form = reqwest::multipart::Form::new()
            .text("tx_type", tx_type.to_string())
            .text("tx_info", tx_info);

        let resp = self
            .client
            .post(&self.send_tx_url)
            .multipart(form)
            .send()
            .await?;

        let status = resp.status();
        let body = resp.text().await?;

        if body.contains("invalid nonce") || body.contains("21104") {
            let _ = self.reset_nonce().await;
            anyhow::bail!("Invalid nonce (reset scheduled), raw: {}", body);
        }

        if !status.is_success() {
            // Try to parse as structured error response
            if let Ok(error_resp) = serde_json::from_str::<LighterErrorResponse>(&body) {
                let error_code = error_resp.error_code();

                // Handle specific error codes
                if error_code.requires_nonce_reset() {
                    let _ = self.reset_nonce().await;
                    anyhow::bail!(
                        "Lighter error {}: {} (nonce reset)",
                        error_resp.code,
                        error_resp.message.as_deref().unwrap_or("unknown")
                    );
                }

                if error_code.is_margin_error() {
                    return Err(TradingError::InsufficientMargin.into());
                }

                anyhow::bail!(
                    "Lighter error {}: {}",
                    error_resp.code,
                    error_resp.message.as_deref().unwrap_or("unknown")
                );
            }

            // Fallback: check for margin error in body text
            if body.contains("not enough margin")
                || body.contains("insufficient margin")
                || body.contains("21301")
            {
                return Err(TradingError::InsufficientMargin.into());
            }

            anyhow::bail!("sendTx HTTP {}: {}", status, body);
        }

        let parsed: SendTxResponse = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!("Parse sendTx response: {} body={}", e, body))?;

        if parsed.code != 200 {
            // Check if this is a margin error
            let error_code = LighterErrorCode::from_code(parsed.code);
            if error_code.is_margin_error() {
                return Err(TradingError::InsufficientMargin.into());
            }
            anyhow::bail!(
                "sendTx code={}: {}",
                parsed.code,
                parsed.message.as_deref().unwrap_or("unknown")
            );
        }

        // Only increment nonce after successful submission
        self.increment_nonce();

        Ok(parsed)
    }

    /// 发送批量交易到 sendTxBatch
    async fn send_tx_batch(&self, txs: &[(u8, String)]) -> Result<SendTxBatchResponse> {
        // Python SDK: json.dumps([14, 14]) and json.dumps(["{...}", "{...}"])
        let tx_types_vec: Vec<u8> = txs.iter().map(|(t, _)| *t).collect();
        let tx_infos_vec: Vec<&str> = txs.iter().map(|(_, info)| info.as_str()).collect();

        let tx_types_json = serde_json::to_string(&tx_types_vec)?;
        let tx_infos_json = serde_json::to_string(&tx_infos_vec)?;

        let form = reqwest::multipart::Form::new()
            .text("tx_types", tx_types_json)
            .text("tx_infos", tx_infos_json);

        let resp = self
            .client
            .post(&self.send_tx_batch_url)
            .multipart(form)
            .send()
            .await?;

        let status = resp.status();
        let body = resp.text().await?;

        if body.contains("invalid nonce") || body.contains("21104") {
            let _ = self.reset_nonce().await;
            anyhow::bail!("Invalid nonce in batch (reset scheduled), raw: {}", body);
        }

        if !status.is_success() {
            // Check for specific error patterns
            if body.contains("not enough margin")
                || body.contains("insufficient margin")
                || body.contains("21711")
            {
                return Err(TradingError::InsufficientMargin.into());
            }
            anyhow::bail!("sendTxBatch HTTP {}: {}", status, body);
        }

        let parsed: SendTxBatchResponse = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!("Parse sendTxBatch: {} body={}", e, body))?;

        if parsed.code != 200 {
            // Check if this is a margin error
            let error_code = LighterErrorCode::from_code(parsed.code);
            if error_code.is_margin_error() {
                return Err(TradingError::InsufficientMargin.into());
            }
            anyhow::bail!(
                "sendTxBatch code={}: {}",
                parsed.code,
                parsed.message.as_deref().unwrap_or("unknown")
            );
        }

        // Increment nonce dynamically for the batch size
        for _ in 0..txs.len() {
            self.increment_nonce();
        }

        Ok(parsed)
    }

    // ─── 公开交易接口 ─────────────────────────────────────────────────────

    /// 通用下单 (v5.0.0: per-order tracking)
    pub async fn place_order(&self, params: OrderParams) -> Result<OrderResult> {
        let (tx_type, tx_info, tx_hash, client_order_index) = self
            .sign_order(
                params.side,
                params.price,
                params.size,
                params.order_type,
                params.reduce_only,
            )
            .await?;

        // Optimistic accounting: register per-order BEFORE API call
        let tracker_side = match params.side {
            Side::Buy => TrackerSide::Buy,
            Side::Sell => TrackerSide::Sell,
        };
        if let Some(ref tracker) = self.order_tracker {
            tracker.start_tracking(client_order_index, tracker_side, params.price, params.size);
        }

        tracing::info!(
            "Signed {} order: price={} size={} type={:?} tx_hash={} coi={}",
            params.side,
            params.price,
            params.size,
            params.order_type,
            tx_hash,
            client_order_index
        );

        match self.send_tx(tx_type, tx_info).await {
            Ok(_) => {
                tracing::info!("Order submitted: tx_hash={}", tx_hash);
                Ok(OrderResult {
                    tx_hash,
                    client_order_index,
                })
            }
            Err(e) => {
                // Rollback: mark order as failed (pending_exposure → 0 automatically)
                if let Some(ref tracker) = self.order_tracker {
                    tracker.mark_failed(client_order_index);
                }
                Err(e)
            }
        }
    }

    /// 下买单（限价）
    pub async fn buy(&self, size: f64, price: f64) -> Result<OrderResult> {
        self.place_order(OrderParams {
            size,
            price,
            side: Side::Buy,
            order_type: self.limit_order_type,
            reduce_only: false,
        })
        .await
    }

    /// 下卖单（限价）
    pub async fn sell(&self, size: f64, price: f64) -> Result<OrderResult> {
        self.place_order(OrderParams {
            size,
            price,
            side: Side::Sell,
            order_type: self.limit_order_type,
            reduce_only: false,
        })
        .await
    }

    /// 批量下单（一买一卖），使用 sendTxBatch 一次性提交 (v5.0.0: per-order tracking)
    pub async fn place_batch(&self, params: BatchOrderParams) -> Result<crate::exchange::BatchOrderResult> {
        // Get base nonce for batch
        let base_nonce = self.get_nonce().await?;

        // 签名买单 (nonce = base_nonce)
        let (bid_type, bid_info, _bid_hash, bid_coi) = self
            .sign_order_with_nonce(
                Side::Buy,
                params.bid_price,
                params.bid_size,
                self.limit_order_type,
                false,
                base_nonce,
            )
            .await?;

        // 签名卖单 (nonce = base_nonce + 1)
        let (ask_type, ask_info, _ask_hash, ask_coi) = self
            .sign_order_with_nonce(
                Side::Sell,
                params.ask_price,
                params.ask_size,
                self.limit_order_type,
                false,
                base_nonce + 1,
            )
            .await?;

        tracing::info!(
            "Signed batch: bid={} @ {} x {} / ask={} @ {} x {}",
            bid_coi,
            params.bid_price,
            params.bid_size,
            ask_coi,
            params.ask_price,
            params.ask_size
        );

        // Register BOTH orders independently (no net-value masking!)
        if let Some(ref tracker) = self.order_tracker {
            tracker.start_tracking(bid_coi, TrackerSide::Buy, params.bid_price, params.bid_size);
            tracker.start_tracking(
                ask_coi,
                TrackerSide::Sell,
                params.ask_price,
                params.ask_size,
            );
        }

        match self
            .send_tx_batch(&[(bid_type, bid_info), (ask_type, ask_info)])
            .await
        {
            Ok(batch_resp) => {
                let tx_hashes = batch_resp.tx_hash.unwrap_or_default();
                tracing::info!("Batch submitted: tx_hashes={:?}", tx_hashes);

                Ok(crate::exchange::BatchOrderResult {
                    tx_hashes,
                    bid_client_order_index: bid_coi,
                    ask_client_order_index: ask_coi,
                })
            }
            Err(e) => {
                // Rollback: mark both orders as failed
                if let Some(ref tracker) = self.order_tracker {
                    tracker.mark_failed(bid_coi);
                    tracker.mark_failed(ask_coi);
                }
                Err(e)
            }
        }
    }

    /// 核心：通用批量执行（支持混合 挂单/撤单），并在单一 RTT 内提交
    pub async fn execute_batch(&self, actions: Vec<BatchAction>) -> Result<BatchResult> {
        if actions.is_empty() {
            return Ok(BatchResult {
                tx_hashes: vec![],
                place_results: vec![],
            });
        }

        let mut txs = Vec::with_capacity(actions.len());
        let mut place_results = Vec::new();
        let market_id = self.market_id;

        // 获取起始 Nonce
        let base_nonce = self.get_nonce().await?;

        for (i, action) in actions.into_iter().enumerate() {
            let nonce = base_nonce + (i as i64);
            match action {
                BatchAction::Place(params) => {
                    let (tx_type, tx_info, _hash, coi) = self
                        .sign_order_with_nonce(
                            params.side,
                            params.price,
                            params.size,
                            params.order_type,
                            params.reduce_only,
                            nonce,
                        )
                        .await?;

                    // 预注册订单追踪
                    if let Some(ref tracker) = self.order_tracker {
                        let tracker_side = match params.side {
                            Side::Buy => TrackerSide::Buy,
                            Side::Sell => TrackerSide::Sell,
                        };
                        tracker.start_tracking(coi, tracker_side, params.price, params.size);
                    }

                    txs.push((tx_type, tx_info));
                    place_results.push(PlaceResult {
                        client_order_index: coi,
                        side: params.side,
                        price: params.price,
                        size: params.size,
                    });
                }
                BatchAction::Cancel(order_index) => {
                    let signed = self
                        .signer
                        .sign_cancel_order(market_id, order_index, nonce)
                        .map_err(|e| anyhow::anyhow!("Sign batch cancel failed: {}", e))?;
                    txs.push((signed.tx_type, signed.tx_info));
                }
            }
        }

        tracing::info!("Submitting batch of {} mixed actions", txs.len());

        match self.send_tx_batch(&txs).await {
            Ok(resp) => {
                let hashes = resp.tx_hash.unwrap_or_default();
                Ok(BatchResult {
                    tx_hashes: hashes,
                    place_results,
                })
            }
            Err(e) => {
                // 回滚：所有在该批次中的 Place 订单标记为失败
                if let Some(ref tracker) = self.order_tracker {
                    for res in place_results {
                        tracker.mark_failed(res.client_order_index);
                    }
                }
                Err(e)
            }
        }
    }

    // ─── 撤单 ────────────────────────────────────────────────────────────

    /// 撤销单笔订单
    pub async fn cancel_order(&self, order_index: i64) -> Result<()> {
        let nonce = self.get_nonce().await?;
        let market_id = self.market_id;
        // Phase 3: Direct FFI call (<100us, no spawn_blocking needed)
        let signed = self
            .signer
            .sign_cancel_order(market_id, order_index, nonce)
            .map_err(|e| anyhow::anyhow!("Sign cancel failed: {}", e))?;

        self.send_tx(signed.tx_type, signed.tx_info).await?;
        tracing::info!("Cancelled order: order_index={}", order_index);
        Ok(())
    }

    /// 撤销所有活跃订单 (Batched for speed)
    pub async fn cancel_all(&self) -> Result<u32> {
        let orders = self.get_active_orders().await?;
        let count = orders.len() as u32;

        if count == 0 {
            tracing::info!("No active orders to cancel");
            return Ok(0);
        }

        let market_id = self.market_id;
        let mut success_batches = 0;
        let mut total_chunks = 0;

        // 1. Process in chunks of 50 to avoid rate limits and sequence issues
        for chunk_orders in orders.chunks(50) {
            total_chunks += 1;
            let mut txs = Vec::with_capacity(chunk_orders.len());
            
            // Get fresh nonce for each chunk. If a previous batch failed and triggered a reset, this handles it.
            let base_nonce = self.get_nonce().await?;

            for (i, order) in chunk_orders.iter().enumerate() {
                let nonce = base_nonce + (i as i64);
                match self.signer.sign_cancel_order(market_id, order.order_index, nonce) {
                    Ok(signed) => txs.push((signed.tx_type, signed.tx_info)),
                    Err(e) => tracing::warn!("Failed to sign cancel for order {}: {}", order.order_index, e),
                }
            }

            if txs.is_empty() {
                continue;
            }

            // 2. Dispatch the chunk
            match self.send_tx_batch(&txs).await {
                Ok(resp) => {
                    let hashes = resp.tx_hash.unwrap_or_default();
                    tracing::info!("Batch cancel submitted ({} orders): {:?}", txs.len(), hashes);
                    success_batches += 1;
                }
                Err(e) => {
                    tracing::error!("Batch cancel failed: {}", e);
                }
            }
            
            // 3. Small sleep to ensure sequence order and avoid rate limits
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        tracing::info!(
            "Cancelled {} orders (dispatched {}/{} batches successfully)",
            count, success_batches, total_chunks
        );

        Ok(count)
    }

    // ─── 查询 ────────────────────────────────────────────────────────────

    /// 查询活跃订单
    pub async fn get_active_orders(&self) -> Result<Vec<OrderDetail>> {
        let auth = self.create_auth_token().await?;
        tracing::debug!(
            "Auth token (first 40 chars): {}",
            &auth[..std::cmp::min(40, auth.len())]
        );

        let url = format!(
            "{}/api/v1/accountActiveOrders?account_index={}&market_id={}",
            self.base_url, self.account_index, self.market_id
        );

        let resp = self
            .client
            .get(&url)
            .header("Authorization", &auth)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("get_active_orders HTTP {}: {}", status, body);
        }

        let orders_resp: OrdersResponse = resp.json().await?;
        if orders_resp.code != 200 {
            anyhow::bail!(
                "get_active_orders code={}: {}",
                orders_resp.code,
                orders_resp.message.as_deref().unwrap_or("unknown")
            );
        }

        Ok(orders_resp.orders)
    }

    /// 通过 order_index 查询单笔订单（从活跃订单中查找）
    pub async fn get_order(&self, order_index: i64) -> Result<OrderDetail> {
        let orders = self.get_active_orders().await?;
        orders
            .into_iter()
            .find(|o| o.order_index == order_index)
            .ok_or_else(|| anyhow::anyhow!("Order {} not found in active orders", order_index))
    }

    /// 验证订单是否符合预期
    pub async fn verify_order(
        &self,
        order_index: i64,
        expected_side: Side,
        expected_price: f64,
        expected_size: f64,
    ) -> Result<bool> {
        let order = self.get_order(order_index).await?;

        let side_ok = match expected_side {
            Side::Sell => order.is_ask,
            Side::Buy => !order.is_ask,
        };

        let price_f: f64 = order.price.parse().unwrap_or(0.0);
        let price_ok = (price_f - expected_price).abs() < 0.01;

        let size_f: f64 = order.initial_base_amount.parse().unwrap_or(0.0);
        let size_ok = (size_f - expected_size).abs() < 0.0001;

        let status_ok = order.status == "open" || order.status == "pending";

        Ok(side_ok && price_ok && size_ok && status_ok)
    }

    // ─── 仓位管理 ────────────────────────────────────────────────────────

    /// 查询当前仓位
    pub async fn get_position(&self) -> Result<Option<Position>> {
        let url = format!(
            "{}/api/v1/account?by=index&value={}",
            self.base_url, self.account_index
        );
        let resp = self.client.get(&url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("get_position HTTP {}", resp.status());
        }
        let acct_resp: AccountsResponse = resp.json().await?;
        if acct_resp.code != 200 || acct_resp.accounts.is_empty() {
            anyhow::bail!("get_position: no account data");
        }
        let pos = acct_resp.accounts[0]
            .positions
            .iter()
            .find(|p| p.market_id == self.market_id)
            .cloned();
        Ok(pos)
    }

    /// 关闭所有仓位：先撤销所有订单，再用市价单平仓
    pub async fn close_all_positions(&self, current_price: f64) -> Result<()> {
        // 1. 撤销所有活跃订单
        self.cancel_all().await?;
        tokio::time::sleep(Duration::from_millis(500)).await;

        // 2. 查询当前仓位
        let pos = self.get_position().await?;
        let pos = match pos {
            Some(p) => p,
            None => {
                tracing::info!("No position found for market {}", self.market_id);
                return Ok(());
            }
        };

        let size: f64 = pos.position.parse().unwrap_or(0.0);
        if size.abs() < 0.0001 {
            tracing::info!("Position is zero, nothing to close");
            return Ok(());
        }

        // 3. 确定平仓方向和价格（带 2% 滑点保护，不能太大否则 quote amount 可能低于最低限额）
        let (side, close_price) = if pos.sign > 0 {
            // 多头 → 卖出平仓
            (Side::Sell, current_price * 0.98)
        } else {
            // 空头 → 买入平仓
            (Side::Buy, current_price * 1.02)
        };

        tracing::info!(
            "Closing position: {} {} @ ~${:.2} (reduce_only)",
            side,
            size.abs(),
            close_price
        );

        // 4. 下 reduce_only 限价单平仓
        self.place_order(OrderParams {
            size: size.abs(),
            price: close_price,
            side,
            order_type: OrderType::Limit,
            reduce_only: true,
        })
        .await?;

        tracing::info!("Close position order submitted");
        Ok(())
    }
}

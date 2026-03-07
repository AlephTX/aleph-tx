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

use crate::shadow_ledger::ShadowLedger;
use parking_lot::RwLock;

use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as TokioMutex;

// ─── Types ───────────────────────────────────────────────────────────────────

/// 订单方向
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Side::Buy => write!(f, "buy"),
            Side::Sell => write!(f, "sell"),
        }
    }
}

/// 订单类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderType {
    Limit,
    Market,
}

/// 单笔订单参数
#[derive(Debug, Clone)]
pub struct OrderParams {
    pub size: f64,
    pub price: f64,
    pub side: Side,
    pub order_type: OrderType,
    pub reduce_only: bool,
}
/// 批量订单参数（一买一卖）
#[derive(Debug, Clone)]
pub struct BatchOrderParams {
    pub bid_price: f64,
    pub ask_price: f64,
    pub bid_size: f64,
    pub ask_size: f64,
}

/// 下单结果
#[derive(Debug, Clone)]
pub struct OrderResult {
    pub tx_hash: String,
    pub client_order_index: i64,
}

/// 批量下单结果
#[derive(Debug, Clone)]
pub struct BatchOrderResult {
    pub tx_hashes: Vec<String>,
    pub bid_client_order_index: i64,
    pub ask_client_order_index: i64,
}

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
    signer: Arc<crate::lighter_ffi::LighterSigner>,
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
    /// Optional shadow ledger for optimistic position tracking
    ledger: Option<Arc<RwLock<ShadowLedger>>>,
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

        let signer = Arc::new(crate::lighter_ffi::LighterSigner::new(
            &base_url, &private_key, chain_id, api_key_index, account_index,
        )
        .map_err(|e| anyhow::anyhow!("Signer init failed: {}", e))?);

        let client = Client::builder()
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60))
            .timeout(Duration::from_secs(15))
            .build()?;

        // 从 orderBookDetails 获取精度
        let (size_decimals, price_decimals) =
            Self::fetch_market_decimals(&client, &base_url, market_id).await?;
        tracing::info!(
            "Market {} decimals: size={} price={}",
            market_id, size_decimals, price_decimals
        );

        let counter_start = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
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
            ledger: None,
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

    /// Attach a shadow ledger for optimistic position tracking
    pub fn set_ledger(&mut self, ledger: Arc<RwLock<ShadowLedger>>) {
        self.ledger = Some(ledger);
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
            .unwrap()
            .as_secs() as i64
            + 600;
        let signer = Arc::clone(&self.signer);
        tokio::task::spawn_blocking(move || {
            signer.create_auth_token(deadline_secs)
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking join: {}", e))?
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
            OrderType::Limit => (0u8, 1u8),  // Limit + GTC
            OrderType::Market => (1u8, 3u8), // Market + IOC
        };

        let market_id = self.market_id;
        let is_ask = side == Side::Sell;
        let signer = Arc::clone(&self.signer);

        let signed = tokio::task::spawn_blocking(move || {
            signer.sign_create_order(
                market_id, client_order_index, base_amount, price_int,
                is_ask, ot, tif, reduce_only, 0u32, -1i64, nonce,
            )
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking join: {}", e))?
        .map_err(|e| anyhow::anyhow!("Sign failed: {}", e))?;

        tracing::debug!(
            "Signed: tx_type={} price_int={} base_amount={} is_ask={} nonce={}",
            signed.tx_type, price_int, base_amount, is_ask, nonce
        );

        Ok((signed.tx_type, signed.tx_info, signed.tx_hash, client_order_index))
    }

    /// 签名一笔订单，返回 (tx_type, tx_info, tx_hash, client_order_index)
    /// FFI 调用通过 spawn_blocking 避免阻塞 tokio async 线程
    async fn sign_order(
        &self,
        side: Side,
        price: f64,
        size: f64,
        order_type: OrderType,
        reduce_only: bool,
    ) -> Result<(u8, String, String, i64)> {
        let nonce = self.get_nonce().await?;
        self.sign_order_with_nonce(side, price, size, order_type, reduce_only, nonce).await
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
            anyhow::bail!("sendTx HTTP {}: {}", status, body);
        }

        let parsed: SendTxResponse = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!("Parse sendTx response: {} body={}", e, body))?;

        if parsed.code != 200 {
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
    async fn send_tx_batch(
        &self,
        txs: &[(u8, String)],
    ) -> Result<SendTxBatchResponse> {
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
            anyhow::bail!("sendTxBatch HTTP {}: {}", status, body);
        }

        let parsed: SendTxBatchResponse = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!("Parse sendTxBatch: {} body={}", e, body))?;

        if parsed.code != 200 {
            anyhow::bail!(
                "sendTxBatch code={}: {}",
                parsed.code,
                parsed.message.as_deref().unwrap_or("unknown")
            );
        }

        // Increment nonce twice for batch (2 orders)
        self.increment_nonce();
        self.increment_nonce();

        Ok(parsed)
    }

    // ─── 公开交易接口 ─────────────────────────────────────────────────────

    /// 通用下单
    pub async fn place_order(&self, params: OrderParams) -> Result<OrderResult> {
        // Optimistic accounting: update in_flight before API call
        let signed_size = match params.side {
            Side::Buy => params.size,
            Side::Sell => -params.size,
        };
        if let Some(ref ledger) = self.ledger {
            ledger.write().add_in_flight(signed_size);
        }

        let (tx_type, tx_info, tx_hash, client_order_index) = self
            .sign_order(params.side, params.price, params.size, params.order_type, params.reduce_only)
            .await?;

        tracing::info!(
            "Signed {} order: price={} size={} tx_hash={} coi={}",
            params.side, params.price, params.size, tx_hash, client_order_index
        );

        match self.send_tx(tx_type, tx_info).await {
            Ok(_) => {
                tracing::info!("Order submitted: tx_hash={}", tx_hash);
                Ok(OrderResult { tx_hash, client_order_index })
            }
            Err(e) => {
                // Rollback in_flight on failure
                if let Some(ref ledger) = self.ledger {
                    ledger.write().add_in_flight(-signed_size);
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
            order_type: OrderType::Limit,
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
            order_type: OrderType::Limit,
            reduce_only: false,
        })
        .await
    }

    /// 批量下单（一买一卖），使用 sendTxBatch 一次性提交
    pub async fn place_batch(&self, params: BatchOrderParams) -> Result<BatchOrderResult> {
        // Get base nonce for batch
        let base_nonce = self.get_nonce().await?;

        // 签名买单 (nonce = base_nonce)
        let (bid_type, bid_info, _bid_hash, bid_coi) = self
            .sign_order_with_nonce(Side::Buy, params.bid_price, params.bid_size, OrderType::Limit, false, base_nonce)
            .await?;

        // 签名卖单 (nonce = base_nonce + 1)
        let (ask_type, ask_info, _ask_hash, ask_coi) = self
            .sign_order_with_nonce(Side::Sell, params.ask_price, params.ask_size, OrderType::Limit, false, base_nonce + 1)
            .await?;

        tracing::info!(
            "Signed batch: bid={} @ {} x {} / ask={} @ {} x {}",
            bid_coi, params.bid_price, params.bid_size, ask_coi, params.ask_price, params.ask_size
        );

        let batch_resp = self
            .send_tx_batch(&[(bid_type, bid_info), (ask_type, ask_info)])
            .await?;

        let tx_hashes = batch_resp.tx_hash.unwrap_or_default();
        tracing::info!("Batch submitted: tx_hashes={:?}", tx_hashes);

        Ok(BatchOrderResult {
            tx_hashes,
            bid_client_order_index: bid_coi,
            ask_client_order_index: ask_coi,
        })
    }

    // ─── 撤单 ────────────────────────────────────────────────────────────

    /// 撤销单笔订单
    pub async fn cancel_order(&self, order_index: i64) -> Result<()> {
        let nonce = self.get_nonce().await?;
        let market_id = self.market_id;
        let signer = Arc::clone(&self.signer);
        let signed = tokio::task::spawn_blocking(move || {
            signer.sign_cancel_order(market_id, order_index, nonce)
        })
        .await
        .map_err(|e| anyhow::anyhow!("spawn_blocking join: {}", e))?
        .map_err(|e| anyhow::anyhow!("Sign cancel failed: {}", e))?;

        self.send_tx(signed.tx_type, signed.tx_info).await?;
        tracing::info!("Cancelled order: order_index={}", order_index);
        Ok(())
    }

    /// 撤销所有活跃订单
    pub async fn cancel_all(&self) -> Result<u32> {
        let orders = self.get_active_orders().await?;
        let count = orders.len() as u32;

        for order in &orders {
            if let Err(e) = self.cancel_order(order.order_index).await {
                tracing::warn!("Failed to cancel order {}: {}", order.order_index, e);
            }
            // 避免触发限流
            tokio::time::sleep(Duration::from_millis(80)).await;
        }

        tracing::info!("Cancelled {} orders", count);
        Ok(count)
    }

    // ─── 查询 ────────────────────────────────────────────────────────────

    /// 查询活跃订单
    pub async fn get_active_orders(&self) -> Result<Vec<OrderDetail>> {
        let auth = self.create_auth_token().await?;
        tracing::debug!("Auth token (first 40 chars): {}", &auth[..std::cmp::min(40, auth.len())]);

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
            side, size.abs(), close_price
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

// ─── Exchange Trait 实现 ─────────────────────────────────────────────────────

use crate::exchange::{
    Exchange, OrderInfo, OrderResult as ExchangeOrderResult,
    BatchOrderParams as ExchangeBatchParams, BatchOrderResult as ExchangeBatchResult,
    Side as ExchangeSide,
};
use async_trait::async_trait;

#[async_trait]
impl Exchange for LighterTrading {
    async fn buy(&self, size: f64, price: f64) -> Result<ExchangeOrderResult> {
        let result = self.buy(size, price).await?;
        Ok(ExchangeOrderResult {
            tx_hash: result.tx_hash,
            client_order_index: result.client_order_index,
        })
    }

    async fn sell(&self, size: f64, price: f64) -> Result<ExchangeOrderResult> {
        let result = self.sell(size, price).await?;
        Ok(ExchangeOrderResult {
            tx_hash: result.tx_hash,
            client_order_index: result.client_order_index,
        })
    }

    async fn place_batch(&self, params: ExchangeBatchParams) -> Result<ExchangeBatchResult> {
        let lighter_params = BatchOrderParams {
            bid_price: params.bid_price,
            ask_price: params.ask_price,
            bid_size: params.bid_size,
            ask_size: params.ask_size,
        };
        let result = self.place_batch(lighter_params).await?;
        Ok(ExchangeBatchResult {
            tx_hashes: result.tx_hashes,
            bid_client_order_index: result.bid_client_order_index,
            ask_client_order_index: result.ask_client_order_index,
        })
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
            .map(|o| {
                let side = if o.is_ask {
                    ExchangeSide::Sell
                } else {
                    ExchangeSide::Buy
                };
                let price: f64 = o.price.parse().unwrap_or(0.0);
                let size: f64 = o.initial_base_amount.parse().unwrap_or(0.0);
                let filled: f64 = o.filled_base_amount.parse().unwrap_or(0.0);
                OrderInfo {
                    order_id: o.order_id,
                    client_order_index: o.client_order_index,
                    side,
                    price,
                    size,
                    filled,
                }
            })
            .collect())
    }

    async fn close_all_positions(&self, current_price: f64) -> Result<()> {
        self.close_all_positions(current_price).await
    }
}

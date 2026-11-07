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

use super::error::LighterErrorResponse;
use crate::error::TradingError;
use crate::order_tracker::{OrderSide as TrackerSide, OrderTracker};

use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as TokioMutex;

mod recovery;

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
    pub market_index: i64,
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
    #[allow(dead_code)]
    tx_hash: Option<String>,
}

/// API 响应：sendTxBatch
#[derive(Debug, Deserialize)]
struct SendTxBatchResponse {
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
    pub market_id: i64,
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
    index: i64,
    positions: Vec<Position>,
    #[serde(default)]
    transaction_time: u64,
}

#[derive(Debug, Deserialize)]
struct AccountsResponse {
    code: i32,
    accounts: Vec<AccountInfo>,
}

#[derive(Deserialize)]
struct MarketDetail {
    market_id: i64,
    symbol: String,
    market_type: String,
    status: String,
    size_decimals: u8,
    price_decimals: u8,
}

pub struct LighterCredentials {
    base_url: String,
    private_key: String,
    chain_id: i32,
    api_key_index: i64,
    account_index: i64,
}

impl LighterCredentials {
    pub fn account_index(&self) -> i64 {
        self.account_index
    }

    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let required = |key| {
            get(key)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("{key} is required"))
        };
        let private_key =
            required("LIGHTER_PRIVATE_KEY").or_else(|_| required("API_KEY_PRIVATE_KEY"))?;
        let private_key = private_key.strip_prefix("0x").unwrap_or(&private_key);
        anyhow::ensure!(
            private_key.len() == 80 && private_key.bytes().all(|c| c.is_ascii_hexdigit()),
            "Lighter API private key must be 40 bytes in hex"
        );
        let account_index = required("LIGHTER_ACCOUNT_INDEX")?.parse()?;
        let api_key_index = required("LIGHTER_API_KEY_INDEX")?.parse()?;
        anyhow::ensure!(
            account_index > 0 && (0..=254).contains(&api_key_index),
            "Invalid Lighter account or API key index"
        );
        Ok(Self {
            base_url: get("LIGHTER_BASE_URL")
                .unwrap_or_else(|| "https://mainnet.zklighter.elliot.ai".into())
                .trim_end_matches('/')
                .to_owned(),
            private_key: private_key.to_owned(),
            chain_id: get("LIGHTER_CHAIN_ID")
                .unwrap_or_else(|| "304".into())
                .parse()?,
            account_index,
            api_key_index,
        })
    }

    pub fn network(&self) -> Result<crate::exchanges::hyperliquid::Network> {
        use crate::exchanges::hyperliquid::Network;
        match (self.base_url.as_str(), self.chain_id) {
            ("https://mainnet.zklighter.elliot.ai", 304) => Ok(Network::Mainnet),
            ("https://testnet.zklighter.elliot.ai", 300) => Ok(Network::Testnet),
            _ => anyhow::bail!(
                "Lighter REST endpoint and chain ID must identify the same supported network"
            ),
        }
    }
}

/// Cancellation-safe submission guard. Dropping an in-flight HTTP future must
/// not silently make its uncertain order eligible for further risk taking.
struct SubmissionGuard<'a> {
    tracker: Option<&'a OrderTracker>,
    armed: bool,
}

impl<'a> SubmissionGuard<'a> {
    fn new(tracker: Option<&'a OrderTracker>) -> Self {
        Self {
            tracker,
            armed: true,
        }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SubmissionGuard<'_> {
    fn drop(&mut self) {
        if self.armed
            && let Some(tracker) = self.tracker
        {
            tracker.require_reconciliation();
        }
    }
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
    market_id: i64,
    market_symbol: String,
    size_multiplier: f64,
    price_multiplier: f64,
    /// Serialize nonce allocation, signing and submission across all writers.
    submission_lock: TokioMutex<()>,
    nonce: AtomicI64,
    nonce_init: TokioMutex<bool>,
    client_order_counter: AtomicI64,
    /// Optional order tracker for per-order state machine (v5.0.0)
    order_tracker: Option<Arc<OrderTracker>>,
    /// Default order type for limit orders (Limit or LimitPostOnly)
    limit_order_type: OrderType,
    quote_expiry: Option<Duration>,
}

#[async_trait]
impl Exchange for LighterTrading {
    async fn buy(&self, size: f64, price: f64) -> Result<OrderResult> {
        self.buy_internal(size, price).await
    }

    async fn sell(&self, size: f64, price: f64) -> Result<OrderResult> {
        self.sell_internal(size, price).await
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
        orders
            .into_iter()
            .map(|o| {
                let price: f64 = o.price.parse()?;
                let size: f64 = o.initial_base_amount.parse()?;
                let filled: f64 = o.filled_base_amount.parse()?;
                anyhow::ensure!(
                    price.is_finite()
                        && price > 0.0
                        && size.is_finite()
                        && size > 0.0
                        && filled.is_finite()
                        && filled >= 0.0
                        && filled <= size,
                    "Invalid active order snapshot"
                );
                Ok(OrderInfo {
                    order_id: o.order_index.to_string(),
                    client_order_index: o.client_order_index,
                    side: if o.is_ask { Side::Sell } else { Side::Buy },
                    price,
                    size,
                    filled,
                })
            })
            .collect()
    }

    async fn close_all_positions(&self, current_price: f64) -> Result<()> {
        self.close_all_positions(current_price).await
    }

    async fn recover_flat_without_feed(&self) -> Result<()> {
        self.recover_flat().await
    }

    async fn execute_batch(&self, actions: Vec<BatchAction>) -> Result<BatchResult> {
        self.execute_batch(actions).await
    }

    fn limit_order_type(&self) -> OrderType {
        self.limit_order_type
    }

    async fn get_account_stats(
        &self,
    ) -> Result<crate::strategy::inventory_neutral_mm::AccountStats> {
        let mut reader = crate::account_stats_reader::AccountStatsReader::open(
            &crate::shm_path::shm_path("aleph-account-stats")?,
        )?;
        let snapshot = reader.read();
        let stats: crate::strategy::inventory_neutral_mm::AccountStats = snapshot.into();
        anyhow::ensure!(
            stats.portfolio_value.is_finite()
                && stats.portfolio_value > 0.0
                && stats.last_update.elapsed() < Duration::from_secs(30),
            "Account statistics unavailable or stale"
        );
        Ok(stats)
    }
}

impl LighterTrading {
    pub fn account_index(&self) -> i64 {
        self.account_index
    }

    #[cfg(test)]
    pub(crate) fn offline_test_client(base_url: &str) -> Self {
        Self {
            signer: Arc::new(super::ffi::LighterSigner::offline_for_test()),
            client: Client::builder()
                .timeout(Duration::from_millis(500))
                .build()
                .unwrap(),
            base_url: base_url.to_owned(),
            send_tx_url: format!("{base_url}/api/v1/sendTx"),
            send_tx_batch_url: format!("{base_url}/api/v1/sendTxBatch"),
            account_index: 0,
            api_key_index: 0,
            market_id: 0,
            market_symbol: "ETH".into(),
            size_multiplier: 1e6,
            price_multiplier: 100.0,
            submission_lock: TokioMutex::new(()),
            nonce: AtomicI64::new(0),
            nonce_init: TokioMutex::new(false),
            client_order_counter: AtomicI64::new(43),
            order_tracker: None,
            limit_order_type: OrderType::PostOnly,
            quote_expiry: None,
        }
    }

    /// 创建交易客户端
    ///
    /// `market_id`: 市场索引 (0 = ETH perps)
    /// 自动从 orderBookDetails 获取价格/大小精度
    pub async fn new(market_id: impl Into<i64>) -> Result<Self> {
        Self::with_credentials(
            market_id,
            LighterCredentials::from_env(|key| std::env::var(key).ok())?,
        )
        .await
    }

    pub async fn with_credentials(
        market_id: impl Into<i64>,
        credentials: LighterCredentials,
    ) -> Result<Self> {
        let market_id = market_id.into();
        anyhow::ensure!(
            super::valid_market_id(market_id),
            "Invalid Lighter market ID"
        );
        let LighterCredentials {
            base_url,
            private_key,
            chain_id,
            api_key_index,
            account_index,
        } = credentials;

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
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .user_agent("AlephTX/5.0")
            .build()?;

        // orderBookDetails occasionally returns transient transport errors at
        // startup; retry a few times instead of aborting the whole strategy.
        let MarketDetail {
            symbol: market_symbol,
            size_decimals,
            price_decimals,
            ..
        } = Self::fetch_market_with_retry(&client, &base_url, market_id).await?;
        tracing::info!(
            "Market {} ({}) decimals: size={} price={}",
            market_id,
            market_symbol,
            size_decimals,
            price_decimals
        );

        let counter_start = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::from_secs(0))
            .as_millis() as i64;

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
            market_symbol,
            size_multiplier,
            price_multiplier,
            submission_lock: TokioMutex::new(()),
            nonce: AtomicI64::new(0),
            nonce_init: TokioMutex::new(false),
            client_order_counter: AtomicI64::new(counter_start),
            order_tracker: None,
            limit_order_type: OrderType::Limit,
            quote_expiry: None,
        })
    }

    /// 从 orderBookDetails 获取市场精度
    async fn fetch_market_with_retry(
        client: &Client,
        base_url: &str,
        market_id: i64,
    ) -> Result<MarketDetail> {
        let mut last_err = None;

        for attempt in 1..=5 {
            match Self::fetch_market(client, base_url, market_id).await {
                Ok(market) => return Ok(market),
                Err(err) => {
                    tracing::warn!(
                        "orderBookDetails attempt {}/5 failed for market {}: {}",
                        attempt,
                        market_id,
                        err
                    );
                    last_err = Some(err);
                    tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("orderBookDetails retry exhausted")))
    }

    /// 从 orderBookDetails 获取市场精度
    async fn fetch_market(client: &Client, base_url: &str, market_id: i64) -> Result<MarketDetail> {
        #[derive(Deserialize)]
        struct OBDetailsResp {
            order_book_details: Vec<MarketDetail>,
        }

        let url = format!("{base_url}/api/v1/orderBookDetails?market_id={market_id}");
        let resp = client.get(&url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("orderBookDetails HTTP {}", resp.status());
        }
        let details: OBDetailsResp = resp.json().await?;
        let market = details
            .order_book_details
            .into_iter()
            .find(|m| m.market_id == market_id)
            .ok_or_else(|| anyhow::anyhow!("Market {} not found in orderBookDetails", market_id))?;

        anyhow::ensure!(
            market.market_type == "perp" && market.status == "active",
            "Lighter market {market_id} is not an active perpetual"
        );
        anyhow::ensure!(
            market.size_decimals <= 12 && market.price_decimals <= 12,
            "Unsupported market precision"
        );
        Ok(market)
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

    pub fn set_supervised_quote_expiry(&mut self) {
        // The pinned signer requires at least five minutes of resting expiry.
        // Leave clock headroom while limiting exposure after a process crash.
        self.quote_expiry = Some(Duration::from_secs(600));
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
        anyhow::ensure!(
            nonce_resp.nonce >= 0 && nonce_resp.nonce <= i64::MAX - 1024,
            "Invalid server nonce"
        );
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
        let (price_int, base_amount) =
            encode_order(price, size, self.price_multiplier, self.size_multiplier)?;

        let (ot, tif, mut expiry) = order_wire_policy(order_type);
        if expiry < 0
            && let Some(ttl) = self.quote_expiry
        {
            expiry =
                i64::try_from((SystemTime::now().duration_since(UNIX_EPOCH)? + ttl).as_millis())?;
        }

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
                expiry,
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

        // Invalidate before the request: cancellation/timeouts can leave a consumed
        // nonce even when no response reaches this process.
        *self.nonce_init.lock().await = false;
        let resp = self
            .client
            .post(&self.send_tx_url)
            .multipart(form)
            .send()
            .await
            .map_err(|e| TradingError::SubmissionUncertain(e.to_string()))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| TradingError::SubmissionUncertain(e.to_string()))?;
        validate_submission_response(status, &body)?;
        let parsed: SendTxResponse = serde_json::from_str(&body)
            .map_err(|e| TradingError::SubmissionUncertain(e.to_string()))?;
        if parsed.tx_hash.as_ref().is_none_or(String::is_empty) {
            return Err(
                TradingError::SubmissionUncertain("Missing transaction hash".into()).into(),
            );
        }
        self.increment_nonce();
        *self.nonce_init.lock().await = true;

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

        // Invalidate before the request: cancellation/timeouts can leave a consumed
        // nonce even when no response reaches this process.
        *self.nonce_init.lock().await = false;
        let resp = self
            .client
            .post(&self.send_tx_batch_url)
            .multipart(form)
            .send()
            .await
            .map_err(|e| TradingError::SubmissionUncertain(e.to_string()))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| TradingError::SubmissionUncertain(e.to_string()))?;
        validate_submission_response(status, &body)?;
        let parsed: SendTxBatchResponse = serde_json::from_str(&body)
            .map_err(|e| TradingError::SubmissionUncertain(e.to_string()))?;
        if parsed
            .tx_hash
            .as_ref()
            .is_none_or(|hashes| hashes.len() != txs.len() || hashes.iter().any(String::is_empty))
        {
            return Err(TradingError::SubmissionUncertain(
                "Incomplete batch acknowledgement".into(),
            )
            .into());
        }
        self.nonce.fetch_add(txs.len() as i64, Ordering::SeqCst);
        *self.nonce_init.lock().await = true;

        Ok(parsed)
    }

    // ─── 公开交易接口 ─────────────────────────────────────────────────────

    /// Verify a strategy's coarser tick/step is representable at the venue.
    pub fn validate_precision(&self, tick: f64, step: f64) -> Result<()> {
        encode_order(tick, step, self.price_multiplier, self.size_multiplier)?;
        Ok(())
    }

    pub fn validate_symbol(&self, symbol: &str) -> Result<()> {
        anyhow::ensure!(
            self.market_symbol == symbol,
            "Lighter market {} is {}, expected {symbol}",
            self.market_id,
            self.market_symbol
        );
        Ok(())
    }

    fn normalize_order(&self, params: OrderParams) -> Result<OrderParams> {
        normalize_order(params, self.price_multiplier, self.size_multiplier)
    }

    fn ensure_can_place(&self, params: &OrderParams) -> Result<()> {
        if params.reduce_only {
            return Ok(());
        }
        self.ensure_open_risk_allowed()
    }

    fn ensure_open_risk_allowed(&self) -> Result<()> {
        anyhow::ensure!(
            !self
                .order_tracker
                .as_ref()
                .is_some_and(|tracker| tracker.needs_reconciliation()),
            "Order state requires reconciliation before new exposure"
        );
        Ok(())
    }

    /// 通用下单 (v5.0.0: per-order tracking)
    pub async fn place_order(&self, params: OrderParams) -> Result<OrderResult> {
        let _submission = self.submission_lock.lock().await;
        let params = self.normalize_order(params)?;
        self.ensure_can_place(&params)?;
        let (tx_type, tx_info, tx_hash, client_order_index) = self
            .sign_order(
                params.side,
                params.price,
                params.size,
                params.order_type,
                params.reduce_only,
            )
            .await?;
        self.ensure_can_place(&params)?;

        // Optimistic accounting: register per-order BEFORE API call
        let tracker_side = match params.side {
            Side::Buy => TrackerSide::Buy,
            Side::Sell => TrackerSide::Sell,
        };
        if let Some(ref tracker) = self.order_tracker {
            anyhow::ensure!(
                tracker.start_tracking(client_order_index, tracker_side, params.price, params.size),
                "Tracker refused order reservation"
            );
            tracker.mark_submission_uncertain(client_order_index);
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

        let mut guard = SubmissionGuard::new(self.order_tracker.as_deref());
        match self.send_tx(tx_type, tx_info).await {
            Ok(_) => {
                guard.disarm();
                tracing::info!("Order submitted: tx_hash={}", tx_hash);
                Ok(OrderResult {
                    tx_hash,
                    client_order_index,
                })
            }
            Err(e) => {
                // Rollback: mark order as failed (pending_exposure → 0 automatically)
                if !is_uncertain(&e) {
                    if let Some(tracker) = &self.order_tracker {
                        tracker.mark_failed(client_order_index);
                    }
                    guard.disarm();
                }
                Err(e)
            }
        }
    }

    /// 下买单（限价）
    pub async fn buy_internal(&self, size: f64, price: f64) -> Result<OrderResult> {
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
    pub async fn sell_internal(&self, size: f64, price: f64) -> Result<OrderResult> {
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
    pub async fn place_batch(
        &self,
        params: BatchOrderParams,
    ) -> Result<crate::exchange::BatchOrderResult> {
        let _submission = self.submission_lock.lock().await;
        self.ensure_open_risk_allowed()?;
        let bid = self.normalize_order(OrderParams {
            side: Side::Buy,
            price: params.bid_price,
            size: params.bid_size,
            order_type: self.limit_order_type,
            reduce_only: false,
        })?;
        let ask = self.normalize_order(OrderParams {
            side: Side::Sell,
            price: params.ask_price,
            size: params.ask_size,
            order_type: self.limit_order_type,
            reduce_only: false,
        })?;
        let params = BatchOrderParams {
            bid_price: bid.price,
            bid_size: bid.size,
            ask_price: ask.price,
            ask_size: ask.size,
        };
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
        self.ensure_open_risk_allowed()?;

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
            anyhow::ensure!(
                tracker.start_tracking(
                    bid_coi,
                    TrackerSide::Buy,
                    params.bid_price,
                    params.bid_size
                ),
                "Tracker refused bid reservation"
            );
            if !tracker.start_tracking(
                ask_coi,
                TrackerSide::Sell,
                params.ask_price,
                params.ask_size,
            ) {
                tracker.mark_failed(bid_coi);
                anyhow::bail!("Tracker refused ask reservation");
            }
        }

        if let Some(tracker) = &self.order_tracker {
            tracker.mark_submission_uncertain(bid_coi);
            tracker.mark_submission_uncertain(ask_coi);
        }
        let mut guard = SubmissionGuard::new(self.order_tracker.as_deref());
        match self
            .send_tx_batch(&[(bid_type, bid_info), (ask_type, ask_info)])
            .await
        {
            Ok(batch_resp) => {
                guard.disarm();
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
                // A batch may have been partially accepted. Keep every reservation
                // until authoritative order events/reconciliation resolve it.
                Err(e)
            }
        }
    }

    /// 核心：通用批量执行（支持混合 挂单/撤单），并在单一 RTT 内提交
    pub async fn execute_batch(&self, actions: Vec<BatchAction>) -> Result<BatchResult> {
        let _submission = self.submission_lock.lock().await;
        if actions.is_empty() {
            return Ok(BatchResult {
                tx_hashes: vec![],
                place_results: vec![],
            });
        }

        anyhow::ensure!(
            actions.len() <= 50,
            "At most 50 actions per transaction batch"
        );
        let mut txs = Vec::with_capacity(actions.len());
        let mut place_results = Vec::new();
        let market_id = self.market_id;
        let opens_risk = actions
            .iter()
            .any(|action| matches!(action, BatchAction::Place(order) if !order.reduce_only));

        // 获取起始 Nonce
        let base_nonce = self.get_nonce().await?;

        for (i, action) in actions.into_iter().enumerate() {
            let nonce = base_nonce + (i as i64);
            match action {
                BatchAction::Place(params) => {
                    let params = self.normalize_order(params)?;
                    self.ensure_can_place(&params)?;
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

        if opens_risk {
            self.ensure_open_risk_allowed()?;
        }
        // Register only after ALL actions have signed successfully. A local signing
        // failure must not leave phantom reservations for an unsent partial batch.
        if let Some(tracker) = &self.order_tracker {
            for order in &place_results {
                let side = if order.side == Side::Buy {
                    TrackerSide::Buy
                } else {
                    TrackerSide::Sell
                };
                if !tracker.start_tracking(order.client_order_index, side, order.price, order.size)
                {
                    for reserved in place_results
                        .iter()
                        .take_while(|r| r.client_order_index != order.client_order_index)
                    {
                        tracker.mark_failed(reserved.client_order_index);
                    }
                    anyhow::bail!("Tracker refused batch reservation");
                }
                tracker.mark_submission_uncertain(order.client_order_index);
            }
        }

        tracing::info!("Submitting batch of {} mixed actions", txs.len());

        let mut guard = SubmissionGuard::new(self.order_tracker.as_deref());
        match self.send_tx_batch(&txs).await {
            Ok(resp) => {
                guard.disarm();
                let hashes = resp.tx_hash.unwrap_or_default();
                Ok(BatchResult {
                    tx_hashes: hashes,
                    place_results,
                })
            }
            Err(e) => {
                // Partial acceptance cannot be ruled out for batch errors.
                Err(e)
            }
        }
    }

    // ─── 撤单 ────────────────────────────────────────────────────────────

    /// 撤销单笔订单
    pub async fn cancel_order(&self, order_index: i64) -> Result<()> {
        let _submission = self.submission_lock.lock().await;
        let nonce = self.get_nonce().await?;
        let market_id = self.market_id;
        // Phase 3: Direct FFI call (<100us, no spawn_blocking needed)
        let signed = self
            .signer
            .sign_cancel_order(market_id, order_index, nonce)
            .map_err(|e| anyhow::anyhow!("Sign cancel failed: {}", e))?;

        let mut guard = SubmissionGuard::new(self.order_tracker.as_deref());
        self.send_tx(signed.tx_type, signed.tx_info).await?;
        guard.disarm();
        tracing::info!(
            "Submitted cancellation for order: order_index={}",
            order_index
        );
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

        for chunk in orders.chunks(50) {
            let actions = chunk
                .iter()
                .map(|order| BatchAction::Cancel(order.order_index))
                .collect();
            self.execute_batch(actions).await?;
        }
        tracing::info!(
            "Submitted cancellation of {} orders; awaiting exchange confirmation",
            count
        );

        Ok(count)
    }

    // ─── 查询 ────────────────────────────────────────────────────────────

    /// 查询活跃订单
    pub async fn get_active_orders(&self) -> Result<Vec<OrderDetail>> {
        let auth = self.create_auth_token().await?;

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
        anyhow::ensure!(
            orders_resp
                .orders
                .iter()
                .all(|o| o.market_index == self.market_id
                    && o.owner_account_index == self.account_index),
            "Active-order response does not match the Lighter account and market"
        );

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
        Ok(self.position_snapshot().await?.0)
    }

    async fn position_snapshot(&self) -> Result<(Option<Position>, u64)> {
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
        let account = acct_resp
            .accounts
            .into_iter()
            .find(|account| account.index == self.account_index)
            .ok_or_else(|| {
                anyhow::anyhow!("Position response does not match the Lighter account")
            })?;
        let pos = account
            .positions
            .into_iter()
            .find(|p| p.market_id == self.market_id);
        Ok((pos, account.transaction_time))
    }

    pub async fn signed_position(&self) -> Result<rust_decimal::Decimal> {
        Ok(self.signed_position_snapshot().await?.0)
    }

    async fn signed_position_snapshot(&self) -> Result<(rust_decimal::Decimal, u64)> {
        use rust_decimal::Decimal;
        let (position, transaction_time) = self.position_snapshot().await?;
        let Some(position) = position else {
            return Ok((Decimal::ZERO, transaction_time));
        };
        let size: Decimal = position.position.parse()?;
        anyhow::ensure!(
            position.symbol == self.market_symbol
                && size >= Decimal::ZERO
                && (matches!(position.sign, -1 | 1) || (position.sign == 0 && size.is_zero())),
            "Invalid Lighter position snapshot"
        );
        Ok((size * Decimal::from(position.sign), transaction_time))
    }

    /// 关闭所有仓位：先撤销所有订单，再用市价单平仓
    pub async fn close_all_positions(&self, current_price: f64) -> Result<()> {
        if self.cancel_all().await? > 0 {
            for _ in 0..5 {
                if self.get_active_orders().await?.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            anyhow::ensure!(
                self.get_active_orders().await?.is_empty(),
                "Lighter has active orders after cancellation"
            );
        }

        let position: f64 = self.signed_position().await?.try_into()?;
        if position == 0.0 {
            tracing::info!("Position is zero, nothing to close");
            return Ok(());
        }

        self.place_order(self.close_order(position, current_price)?)
            .await?;
        tracing::info!("Close position order submitted");
        Ok(())
    }

    fn close_order(&self, position: f64, current_price: f64) -> Result<OrderParams> {
        anyhow::ensure!(
            position.is_finite()
                && position != 0.0
                && current_price.is_finite()
                && current_price > 0.0,
            "Invalid position or close price"
        );
        let (side, close_price) = if position > 0.0 {
            // 多头 → 卖出平仓
            (Side::Sell, current_price * 0.98)
        } else {
            // 空头 → 买入平仓
            (Side::Buy, current_price * 1.02)
        };

        let close_price = match side {
            Side::Sell => (close_price * self.price_multiplier).ceil() / self.price_multiplier,
            Side::Buy => (close_price * self.price_multiplier).floor() / self.price_multiplier,
        };
        let close_size = position.abs();

        tracing::info!(
            "Closing position: {} {} @ ~${:.2} (reduce_only)",
            side,
            close_size,
            close_price
        );

        Ok(OrderParams {
            size: close_size,
            price: close_price,
            side,
            order_type: OrderType::Ioc,
            reduce_only: true,
        })
    }
}

fn order_wire_policy(order_type: OrderType) -> (u8, u8, i64) {
    match order_type {
        OrderType::Limit => (0, 1, -1),
        OrderType::PostOnly => (0, 2, -1),
        OrderType::Market => (1, 0, 0),
        OrderType::Ioc => (0, 0, 0),
    }
}

/// Passive rounding preserves the caller's price bound and never increases size.
fn normalize_order(
    mut params: OrderParams,
    price_scale: f64,
    size_scale: f64,
) -> Result<OrderParams> {
    anyhow::ensure!(
        [params.price, params.size, price_scale, size_scale]
            .iter()
            .all(|v| v.is_finite() && *v > 0.0),
        "Order price, size and precision must be finite and positive"
    );
    let price_units = params.price * price_scale;
    let size_units = params.size * size_scale;
    let snap = |units: f64| {
        if (units - units.round()).abs() < 1e-7 {
            units.round()
        } else {
            units
        }
    };
    params.price = match params.side {
        Side::Buy => snap(price_units).floor(),
        Side::Sell => snap(price_units).ceil(),
    } / price_scale;
    params.size = snap(size_units).floor() / size_scale;
    encode_order(params.price, params.size, price_scale, size_scale)?;
    Ok(params)
}

/// Reject non-finite/out-of-range quantities before Rust's saturating float casts.
fn encode_order(price: f64, size: f64, price_scale: f64, size_scale: f64) -> Result<(u32, i64)> {
    anyhow::ensure!(
        [price, size, price_scale, size_scale]
            .iter()
            .all(|v| v.is_finite() && *v > 0.0),
        "Order price, size and precision must be finite and positive"
    );
    let price_units = (price * price_scale).round();
    let size_units = (size * size_scale).round();
    anyhow::ensure!(
        price_units >= 1.0 && price_units <= u32::MAX as f64,
        "Order price out of range"
    );
    anyhow::ensure!(
        size_units >= 1.0 && size_units < i64::MAX as f64,
        "Order size out of range"
    );
    anyhow::ensure!(
        (price * price_scale - price_units).abs() <= 1e-6
            && (size * size_scale - size_units).abs() <= 1e-6,
        "Order is not aligned to market tick/step size"
    );
    Ok((price_units as u32, size_units as i64))
}

fn is_uncertain(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<TradingError>(),
        Some(TradingError::SubmissionUncertain(_))
    )
}

fn validate_submission_response(status: reqwest::StatusCode, body: &str) -> Result<()> {
    if status.is_server_error() {
        return Err(TradingError::SubmissionUncertain(format!("HTTP {status}")).into());
    }
    let response: LighterErrorResponse = serde_json::from_str(body)
        .map_err(|e| TradingError::SubmissionUncertain(format!("HTTP {status}: {e}")))?;
    if response.code == 200 && status.is_success() {
        return Ok(());
    }
    if response.code == 200 {
        return Err(TradingError::SubmissionUncertain(format!("Conflicting HTTP {status}")).into());
    }
    if response.error_code().is_margin_error() {
        return Err(TradingError::InsufficientMargin.into());
    }
    anyhow::bail!(
        "Lighter rejected submission code={}: {}",
        response.code,
        response.message.as_deref().unwrap_or("unknown")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn transport_failure_invalidates_nonce_without_releasing_reservation() {
        let tracker = Arc::new(OrderTracker::new());
        assert!(tracker.start_tracking(42, TrackerSide::Buy, 100.0, 1.0));
        tracker.mark_submission_uncertain(42);
        let mut trading = LighterTrading::offline_test_client("http://127.0.0.1:1");
        trading.nonce.store(9, Ordering::Release);
        *trading.nonce_init.lock().await = true;
        trading.set_order_tracker(Arc::clone(&tracker));
        let error = trading.send_tx(14, "{}".into()).await.err().unwrap();
        assert!(is_uncertain(&error));
        assert!(!*trading.nonce_init.lock().await);
        assert_eq!(trading.nonce.load(Ordering::Acquire), 9);
        assert_eq!(tracker.worst_case_long(), 1.0);
    }

    #[test]
    fn submission_guard_blocks_new_exposure_after_cancellation() {
        let tracker = OrderTracker::new();
        {
            let mut guard = SubmissionGuard::new(Some(&tracker));
            guard.disarm();
        }
        assert!(!tracker.needs_reconciliation());
        {
            let _cancelled_request = SubmissionGuard::new(Some(&tracker));
        }
        assert!(tracker.needs_reconciliation());
    }

    #[test]
    fn normalization_preserves_price_limits_and_never_upsizes() {
        for side in [Side::Buy, Side::Sell] {
            let input = OrderParams {
                side,
                price: 100.005,
                size: 0.0019,
                order_type: OrderType::Limit,
                reduce_only: false,
            };
            let result = normalize_order(input, 100.0, 1000.0).unwrap();
            assert_eq!(result.size, 0.001);
            assert_eq!(result.price, if side == Side::Buy { 100.0 } else { 100.01 });
        }
    }

    #[test]
    fn order_encoding_rejects_saturating_casts_and_dust() {
        assert_eq!(
            encode_order(2085.87, 0.001, 100.0, 1e6).unwrap(),
            (208587, 1000)
        );
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -1.0, 1e30] {
            assert!(encode_order(value, 1.0, 100.0, 1e6).is_err());
            assert!(encode_order(100.0, value, 100.0, 1e6).is_err());
        }
        assert!(encode_order(100.0, 1e-12, 100.0, 1e6).is_err());
        assert!(encode_order(100.005, 1.0, 100.0, 1e6).is_err());
        assert!(encode_order(100.0, 0.0015, 100.0, 1000.0).is_err());
    }

    #[test]
    fn ambiguous_responses_preserve_exposure() {
        for (status, body) in [
            (502, "bad gateway"),
            (200, "truncated"),
            (503, r#"{"code":21301}"#),
            (400, r#"{"code":200}"#),
        ] {
            let error =
                validate_submission_response(reqwest::StatusCode::from_u16(status).unwrap(), body)
                    .unwrap_err();
            assert!(is_uncertain(&error));
        }
        let error =
            validate_submission_response(reqwest::StatusCode::OK, r#"{"code":21301}"#).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<TradingError>(),
            Some(TradingError::InsufficientMargin)
        ));
        assert!(validate_submission_response(reqwest::StatusCode::OK, r#"{"code":200}"#).is_ok());
    }
    #[test]
    fn lighter_ioc_and_resting_orders_use_distinct_expiry_policies() {
        assert_eq!(order_wire_policy(OrderType::Ioc), (0, 0, 0));
        assert_eq!(order_wire_policy(OrderType::Market), (1, 0, 0));
        assert_eq!(order_wire_policy(OrderType::Limit), (0, 1, -1));
        assert_eq!(order_wire_policy(OrderType::PostOnly), (0, 2, -1));
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn native_signer_preserves_mainnet_and_testnet_market_ids_offline() {
        for (chain, account, markets) in [(304, 1001, [0, 1]), (300, 1002, [4095, 4096])] {
            let signer = crate::lighter_ffi::LighterSigner::new(
                "http://127.0.0.1:1",
                &"01".repeat(40),
                chain,
                7,
                account,
            )
            .unwrap();
            for market in markets {
                let (kind, tif, expiry) = order_wire_policy(OrderType::Ioc);
                let signed = signer
                    .sign_create_order(
                        market, 1234, 100, 200_000, true, kind, tif, true, 0, expiry, 19,
                    )
                    .unwrap();
                let tx: serde_json::Value = serde_json::from_str(&signed.tx_info).unwrap();
                assert_eq!(tx["MarketIndex"], market);
                assert_eq!(tx["TimeInForce"], 0);
                assert_eq!(tx["OrderExpiry"], 0);
                assert_eq!(tx["ReduceOnly"], 1);
                assert_eq!(tx["AccountIndex"], account);
                assert_eq!(tx["ApiKeyIndex"], 7);
                assert_eq!(tx["Nonce"], 19);

                let order_index = (1i64 << 48) + 7;
                let canceled = signer.sign_cancel_order(market, order_index, 20).unwrap();
                let tx: serde_json::Value = serde_json::from_str(&canceled.tx_info).unwrap();
                assert_eq!(tx["MarketIndex"], market);
                assert_eq!(tx["Index"], order_index);
                assert_eq!(tx["Nonce"], 20);
            }
            assert!(signer.sign_cancel_order(255, (1i64 << 48) + 7, 20).is_err());
        }
    }
}

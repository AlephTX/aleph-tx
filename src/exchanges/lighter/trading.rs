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
use k256::elliptic_curve::zeroize::Zeroizing;
use parking_lot::Mutex as SyncMutex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as TokioMutex;

#[cfg(test)]
mod batch_journal_tests;
pub(crate) mod recovery;
#[cfg(test)]
mod serial_journal_trial;

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
    /// Absent in some responses; 0 is treated as unknown, never as a time.
    #[serde(default)]
    transaction_time: u64,
}

/// Exact Lighter position for the configured account and market, with the
/// evidence it rests on. Built only from one unambiguous account response.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PositionEvidence {
    pub account_index: i64,
    pub market_id: i64,
    /// Signed base quantity; zero only when the account lists no position
    /// (or a zero position) for this market.
    pub position: rust_decimal::Decimal,
    /// The account's `transaction_time` (Lighter's trade watermark, same
    /// units as order `transaction_time`); None when absent or 0. It says
    /// which trades the snapshot includes, not when it was generated.
    pub transaction_time: Option<u64>,
    /// Local monotonic time just before the request was sent. Consumers
    /// spend a local elapsed-time budget from here (request + wait +
    /// consumption). It does not prove when the venue generated the state or
    /// that the response is authentic; `transaction_time` is independent.
    pub requested_at: Instant,
    /// Local monotonic time after the body was parsed.
    pub received_at: Instant,
}

impl PositionEvidence {
    /// Whether this snapshot includes trades up to `fill_watermark` (an
    /// order `transaction_time`; 0 = no fills to cover). An unknown account
    /// watermark covers nothing but "no fills"; receipt time never stands in.
    pub fn covers(&self, fill_watermark: u64) -> bool {
        watermark_covers(self.transaction_time, fill_watermark)
    }

    /// Local age bound: from request start, so a slow request counts.
    pub fn age(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.requested_at)
    }
}

/// The coverage rule behind `PositionEvidence::covers`, shared with the
/// offline audit: an account watermark covers a fill watermark only when it
/// is known and not older. 0 means there are no fills to cover.
pub(crate) fn watermark_covers(transaction_time: Option<u64>, fill_watermark: u64) -> bool {
    fill_watermark == 0 || transaction_time.is_some_and(|t| t >= fill_watermark)
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

/// Lighter execution and settlement domain.
///
/// Core Mainnet (Chain ID 304, USDC collateral) and Robinhood Chain Mainnet
/// (Chain ID 466324, USDG collateral) share `Network::Mainnet` reference feeds
/// and wire format, but operate independent order books, sequencers, collateral
/// pools, and account namespaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LighterDomain {
    CoreMainnet,
    RobinhoodMainnet,
    CoreTestnet,
}

impl LighterDomain {
    pub const fn network(self) -> crate::exchanges::hyperliquid::Network {
        use crate::exchanges::hyperliquid::Network;
        match self {
            Self::CoreMainnet | Self::RobinhoodMainnet => Network::Mainnet,
            Self::CoreTestnet => Network::Testnet,
        }
    }

    pub const fn chain_id(self) -> i32 {
        match self {
            Self::CoreMainnet => 304,
            Self::RobinhoodMainnet => 466324,
            Self::CoreTestnet => 300,
        }
    }

    pub const fn rest_url(self) -> &'static str {
        match self {
            Self::CoreMainnet => "https://mainnet.zklighter.elliot.ai",
            Self::RobinhoodMainnet => "https://api.rh.lighter.xyz",
            Self::CoreTestnet => "https://testnet.zklighter.elliot.ai",
        }
    }

    pub const fn ws_url(self) -> &'static str {
        match self {
            Self::CoreMainnet => "wss://mainnet.zklighter.elliot.ai/stream",
            Self::RobinhoodMainnet => "wss://api.rh.lighter.xyz/stream",
            Self::CoreTestnet => "wss://testnet.zklighter.elliot.ai/stream",
        }
    }

    /// Filesystem-safe domain slug for `/dev/shm` account locks and daily-loss state.
    /// Keeps `"Mainnet"` and `"Testnet"` identical for backward compatibility while
    /// isolating `"Robinhood"` state onto its own files.
    pub const fn lock_slug(self) -> &'static str {
        match self {
            Self::CoreMainnet => "Mainnet",
            Self::RobinhoodMainnet => "Robinhood",
            Self::CoreTestnet => "Testnet",
        }
    }

    pub const fn telemetry_exchange(self) -> &'static str {
        match self {
            Self::CoreMainnet | Self::CoreTestnet => "lighter",
            Self::RobinhoodMainnet => "lighter_robinhood",
        }
    }

    pub const fn valid_api_key_index(chain_id: i32, api_key_index: i64) -> bool {
        api_key_index >= 0 && api_key_index <= 254 && !(chain_id == 466324 && api_key_index == 157)
    }
}

struct ApiKeyCredentials {
    private_key: Zeroizing<String>,
    api_key_index: i64,
}

pub struct LighterCredentials {
    base_url: String,
    chain_id: i32,
    account_index: i64,
    maker_key: ApiKeyCredentials,
    cancel_key: Option<ApiKeyCredentials>,
    taker_key: Option<ApiKeyCredentials>,
}

impl LighterCredentials {
    pub fn account_index(&self) -> i64 {
        self.account_index
    }

    pub fn has_cancel_lane(&self) -> bool {
        self.cancel_key.is_some()
    }

    pub fn has_taker_lane(&self) -> bool {
        self.taker_key.is_some()
    }

    fn parse_api_key(
        private_key_raw: &str,
        api_key_index_raw: &str,
        chain_id: i32,
        label: &str,
    ) -> Result<ApiKeyCredentials> {
        let private_key = private_key_raw
            .strip_prefix("0x")
            .unwrap_or(private_key_raw);
        anyhow::ensure!(
            private_key.len() == 80 && private_key.bytes().all(|c| c.is_ascii_hexdigit()),
            "Lighter {label} API private key must be 40 bytes in hex"
        );
        let api_key_index: i64 = api_key_index_raw.parse()?;
        anyhow::ensure!(
            !(chain_id == 466324 && api_key_index == 157),
            "Lighter {label} API key index 157 is reserved on Robinhood Chain (466324)"
        );
        anyhow::ensure!(
            LighterDomain::valid_api_key_index(chain_id, api_key_index),
            "Invalid Lighter {label} API key index"
        );
        Ok(ApiKeyCredentials {
            private_key: Zeroizing::new(private_key.to_owned()),
            api_key_index,
        })
    }

    fn parse_optional_lane(
        get: &impl Fn(&str) -> Option<String>,
        key_env: &str,
        index_env: &str,
        chain_id: i32,
        label: &str,
    ) -> Result<Option<ApiKeyCredentials>> {
        let key = get(key_env).filter(|s| !s.is_empty()).map(Zeroizing::new);
        let index = get(index_env).filter(|s| !s.is_empty());
        match (key, index) {
            (None, None) => Ok(None),
            (Some(k), Some(idx)) => Ok(Some(Self::parse_api_key(
                k.as_str(),
                &idx,
                chain_id,
                label,
            )?)),
            _ => anyhow::bail!("{key_env} and {index_env} must be configured together"),
        }
    }

    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let required = |key| {
            get(key)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("{key} is required"))
        };
        let chain_id: i32 = get("LIGHTER_CHAIN_ID")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "304".into())
            .parse()?;
        let account_index: i64 = required("LIGHTER_ACCOUNT_INDEX")?.parse()?;
        anyhow::ensure!(account_index > 0, "Invalid Lighter account index");

        let primary_key = Zeroizing::new(
            required("LIGHTER_PRIVATE_KEY").or_else(|_| required("API_KEY_PRIVATE_KEY"))?,
        );
        let primary_index = required("LIGHTER_API_KEY_INDEX")?;
        let maker_key =
            Self::parse_api_key(primary_key.as_str(), &primary_index, chain_id, "maker")?;
        let cancel_key = Self::parse_optional_lane(
            &get,
            "LIGHTER_CANCEL_PRIVATE_KEY",
            "LIGHTER_CANCEL_API_KEY_INDEX",
            chain_id,
            "cancel",
        )?;
        let taker_key = Self::parse_optional_lane(
            &get,
            "LIGHTER_TAKER_PRIVATE_KEY",
            "LIGHTER_TAKER_API_KEY_INDEX",
            chain_id,
            "taker",
        )?;

        if let Some(ref cancel) = cancel_key {
            anyhow::ensure!(
                cancel.api_key_index != maker_key.api_key_index,
                "Lighter cancel lane must use a distinct API key index from maker lane"
            );
        }
        if let Some(ref taker) = taker_key {
            anyhow::ensure!(
                taker.api_key_index != maker_key.api_key_index,
                "Lighter taker lane must use a distinct API key index from maker lane"
            );
            if let Some(ref cancel) = cancel_key {
                anyhow::ensure!(
                    taker.api_key_index != cancel.api_key_index,
                    "Lighter taker lane must use a distinct API key index from cancel lane"
                );
            }
        }

        let default_url = if chain_id == 466324 {
            LighterDomain::RobinhoodMainnet.rest_url()
        } else {
            LighterDomain::CoreMainnet.rest_url()
        };
        Ok(Self {
            base_url: get("LIGHTER_BASE_URL")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| default_url.into())
                .trim_end_matches('/')
                .to_owned(),
            chain_id,
            account_index,
            maker_key,
            cancel_key,
            taker_key,
        })
    }

    pub fn domain(&self) -> Result<LighterDomain> {
        match (self.base_url.as_str(), self.chain_id) {
            ("https://mainnet.zklighter.elliot.ai", 304) => Ok(LighterDomain::CoreMainnet),
            ("https://api.rh.lighter.xyz", 466324) => Ok(LighterDomain::RobinhoodMainnet),
            ("https://testnet.zklighter.elliot.ai", 300) => Ok(LighterDomain::CoreTestnet),
            _ => anyhow::bail!(
                "Lighter REST endpoint and chain ID must identify the same supported network"
            ),
        }
    }

    pub fn network(&self) -> Result<crate::exchanges::hyperliquid::Network> {
        Ok(self.domain()?.network())
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

const UNSET_QUOTA_REMAINING: i64 = i64::MIN;

fn deserialize_optional_quota<'de, D>(deserializer: D) -> std::result::Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value: Option<serde_json::Value> = serde::Deserialize::deserialize(deserializer)?;
    let parsed = match value {
        Some(serde_json::Value::Number(n)) => n.as_i64().or_else(|| {
            n.as_f64()
                .filter(|v| v.is_finite())
                .map(|v| v.floor() as i64)
        }),
        Some(serde_json::Value::String(s)) => {
            let trimmed = s.trim();
            trimmed.parse::<i64>().ok().or_else(|| {
                trimmed
                    .parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite())
                    .map(|v| v.floor() as i64)
            })
        }
        _ => None,
    };
    Ok(parsed.filter(|&q| q >= 0))
}

#[derive(Debug, Deserialize)]
struct QuotaEnvelope {
    #[serde(default, deserialize_with = "deserialize_optional_quota")]
    volume_quota_remaining: Option<i64>,
}

struct QuotaTracker {
    remaining: AtomicI64,
    observed_at_ns: AtomicU64,
}

impl QuotaTracker {
    fn new() -> Self {
        Self {
            remaining: AtomicI64::new(UNSET_QUOTA_REMAINING),
            observed_at_ns: AtomicU64::new(0),
        }
    }

    fn record_from_body(&self, body: &str) {
        if let Ok(env) = serde_json::from_str::<QuotaEnvelope>(body)
            && let Some(q) = env.volume_quota_remaining
        {
            let now_ns = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as u64);
            self.observed_at_ns.store(now_ns, Ordering::Release);
            self.remaining.store(q, Ordering::Release);
        }
    }

    fn snapshot(&self) -> Option<crate::exchange::VolumeQuotaSnapshot> {
        let observed_at_ns = self.observed_at_ns.load(Ordering::Acquire);
        let rem = self.remaining.load(Ordering::Acquire);
        Some(crate::exchange::VolumeQuotaSnapshot {
            remaining: (rem != UNSET_QUOTA_REMAINING).then_some(rem),
            observed_at_ns,
        })
    }
}

/// Per-API-key signing and submission lane.
///
/// Isolates `api_key_index`, monotonic `nonce`, and HTTP submission lock so
/// Lighter's taker speed-bump queue (140-300 ms on IOC/Market orders) cannot
/// head-of-line block 0 ms `PostOnly` maker quotes or emergency cancellations
/// when dedicated API keys are configured.
struct SigningLane {
    signer: Arc<super::ffi::LighterSigner>,
    api_key_index: i64,
    submission_lock: TokioMutex<()>,
    nonce: AtomicI64,
    nonce_init: TokioMutex<bool>,
    quota: Arc<QuotaTracker>,
}

impl SigningLane {
    fn new(
        signer: Arc<super::ffi::LighterSigner>,
        api_key_index: i64,
        quota: Arc<QuotaTracker>,
    ) -> Self {
        Self {
            signer,
            api_key_index,
            submission_lock: TokioMutex::new(()),
            nonce: AtomicI64::new(0),
            nonce_init: TokioMutex::new(false),
            quota,
        }
    }

    async fn fetch_nonce(
        &self,
        client: &Client,
        base_url: &str,
        account_index: i64,
    ) -> Result<i64> {
        let url = format!(
            "{}/api/v1/nextNonce?account_index={}&api_key_index={}",
            base_url, account_index, self.api_key_index
        );
        let resp = client.get(&url).send().await?;
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

    async fn get_nonce(&self, client: &Client, base_url: &str, account_index: i64) -> Result<i64> {
        {
            let mut initialized = self.nonce_init.lock().await;
            if !*initialized {
                let server_nonce = self.fetch_nonce(client, base_url, account_index).await?;
                self.nonce.store(server_nonce, Ordering::Release);
                *initialized = true;
                tracing::info!(
                    api_key_index = self.api_key_index,
                    nonce = server_nonce,
                    "Nonce initialized from server"
                );
                return Ok(server_nonce);
            }
        }
        Ok(self.nonce.load(Ordering::SeqCst))
    }

    fn increment_nonce(&self) {
        self.nonce.fetch_add(1, Ordering::SeqCst);
    }

    async fn send_tx(
        &self,
        client: &Client,
        send_tx_url: &str,
        tx_type: u8,
        tx_info: String,
    ) -> Result<SendTxResponse> {
        let form = reqwest::multipart::Form::new()
            .text("tx_type", tx_type.to_string())
            .text("tx_info", tx_info);

        // Invalidate before the request: cancellation/timeouts can leave a consumed
        // nonce even when no response reaches this process. Remember whether our
        // local nonce was synchronized so definitive pre-application rejections
        // (where Lighter did not consume the nonce) can restore it without burning
        // an extra `/api/v1/nextNonce` HTTP round-trip.
        let was_initialized = std::mem::replace(&mut *self.nonce_init.lock().await, false);
        let resp = client
            .post(send_tx_url)
            .multipart(form)
            .send()
            .await
            .map_err(|e| TradingError::SubmissionUncertain(e.to_string()))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| TradingError::SubmissionUncertain(e.to_string()))?;
        if let Err(failure) = classify_submission_response(status, &body) {
            if was_initialized && failure.preserves_unconsumed_single_tx_nonce() {
                *self.nonce_init.lock().await = true;
            }
            return Err(failure.into_error());
        }
        let parsed: SendTxResponse = serde_json::from_str(&body)
            .map_err(|e| TradingError::SubmissionUncertain(e.to_string()))?;
        if parsed.tx_hash.as_ref().is_none_or(String::is_empty) {
            return Err(
                TradingError::SubmissionUncertain("Missing transaction hash".into()).into(),
            );
        }
        self.quota.record_from_body(&body);
        self.increment_nonce();
        *self.nonce_init.lock().await = true;

        Ok(parsed)
    }

    async fn send_tx_batch(
        &self,
        client: &Client,
        send_tx_batch_url: &str,
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

        // Invalidate before the request: cancellation/timeouts or partial batch
        // acceptance can consume a subset of nonces. Always resync after a failed batch.
        *self.nonce_init.lock().await = false;
        let resp = client
            .post(send_tx_batch_url)
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
        self.quota.record_from_body(&body);
        self.nonce.fetch_add(txs.len() as i64, Ordering::SeqCst);
        *self.nonce_init.lock().await = true;

        Ok(parsed)
    }
}

/// Sentinel bit pattern (`f64` quiet negative NaN) indicating no cap is set yet.
const UNSET_POSITION_CAP_BITS: u64 = u64::MAX;
const AUTH_TOKEN_TTL_SECS: i64 = 600;
const AUTH_TOKEN_REFRESH_BUFFER_SECS: i64 = 60;
const AUTH_TOKEN_CACHE_TTL: Duration =
    Duration::from_secs((AUTH_TOKEN_TTL_SECS - AUTH_TOKEN_REFRESH_BUFFER_SECS) as u64);

#[derive(Clone)]
struct CachedAuthToken {
    token: Zeroizing<String>,
    /// Monotonic refresh cutoff anchored at mint start (`mint_started_at + AUTH_TOKEN_CACHE_TTL`).
    refresh_after: Instant,
    /// Signed UNIX-epoch deadline passed to the signer (`mint_started_wall_secs + AUTH_TOKEN_TTL_SECS`).
    deadline_secs: i64,
}

impl CachedAuthToken {
    fn is_fresh_at(&self, mono_now: Instant, wall_now_secs: i64) -> bool {
        mono_now < self.refresh_after
            && wall_now_secs
                < self
                    .deadline_secs
                    .saturating_sub(AUTH_TOKEN_REFRESH_BUFFER_SECS)
    }
}

#[derive(Default)]
struct AuthTokenCacheState {
    generation: u64,
    entry: Option<CachedAuthToken>,
}

/// Lighter 交易客户端（币种无关，支持 Maker / Cancel / Taker 多 API Key 隔离）
pub struct LighterTrading {
    maker_lane: SigningLane,
    cancel_lane: Option<SigningLane>,
    taker_lane: Option<SigningLane>,
    client: Client,
    base_url: String,
    send_tx_url: String,
    send_tx_batch_url: String,
    account_index: i64,
    market_id: i64,
    market_symbol: String,
    size_multiplier: f64,
    price_multiplier: f64,
    client_order_counter: AtomicI64,
    /// Optional order tracker for per-order state machine (v5.0.0)
    order_tracker: Option<Arc<OrderTracker>>,
    /// Lock-free execution-boundary ceiling on worst-case single-leg position (base units).
    max_worst_case_position_bits: AtomicU64,
    /// Lock-free execution-boundary reference mid price & deviation band (bps) for pre-nonce fat-finger checks.
    ref_mid_bits: AtomicU64,
    max_price_band_bps_bits: AtomicU64,
    /// Shared venue volume quota tracker updated from tx/batch responses (`OPT-05`).
    quota: Arc<QuotaTracker>,
    /// Default order type for limit orders (Limit or LimitPostOnly)
    limit_order_type: OrderType,
    quote_expiry: Option<Duration>,
    auth_token_cache: SyncMutex<AuthTokenCacheState>,
    /// Test builds only: a fixed read-auth token for loopback fixtures, so
    /// REST reads run without the platform signer. Absent in production.
    #[cfg(test)]
    test_auth_token: Option<&'static str>,
}

#[async_trait]
impl Exchange for LighterTrading {
    fn venue_name(&self) -> &'static str {
        "lighter"
    }

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
                    order_index: Some(o.order_index),
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

    async fn reconcile_non_flat_without_feed(&self) -> Result<f64> {
        self.reconcile_non_flat().await
    }

    async fn execute_batch(&self, actions: Vec<BatchAction>) -> Result<BatchResult> {
        self.execute_batch(actions).await
    }

    fn limit_order_type(&self) -> OrderType {
        self.limit_order_type
    }

    fn set_max_worst_case_position(&self, max_position: f64) {
        LighterTrading::set_max_worst_case_position(self, max_position);
    }

    fn volume_quota_snapshot(&self) -> Option<crate::exchange::VolumeQuotaSnapshot> {
        self.quota.snapshot()
    }

    fn set_reference_price_band(&self, reference_mid: f64, max_deviation_bps: f64) {
        LighterTrading::set_reference_price_band(self, reference_mid, max_deviation_bps);
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

    /// Test builds only: trade a different (loopback) Lighter account.
    #[cfg(test)]
    pub(crate) fn with_test_account(mut self, account_index: i64) -> Self {
        self.account_index = account_index;
        self
    }

    #[cfg(test)]
    pub(crate) fn offline_test_client(base_url: &str) -> Self {
        let quota = Arc::new(QuotaTracker::new());
        Self {
            maker_lane: SigningLane::new(
                Arc::new(super::ffi::LighterSigner::offline_for_test()),
                0,
                Arc::clone(&quota),
            ),
            cancel_lane: None,
            taker_lane: None,
            client: Client::builder()
                .timeout(Duration::from_millis(500))
                .no_proxy()
                .build()
                .unwrap(),
            base_url: base_url.to_owned(),
            send_tx_url: format!("{base_url}/api/v1/sendTx"),
            send_tx_batch_url: format!("{base_url}/api/v1/sendTxBatch"),
            account_index: 0,
            market_id: 0,
            market_symbol: "ETH".into(),
            size_multiplier: 1e6,
            price_multiplier: 100.0,
            client_order_counter: AtomicI64::new(43),
            order_tracker: None,
            max_worst_case_position_bits: AtomicU64::new(UNSET_POSITION_CAP_BITS),
            ref_mid_bits: AtomicU64::new(UNSET_POSITION_CAP_BITS),
            max_price_band_bps_bits: AtomicU64::new(UNSET_POSITION_CAP_BITS),
            quota,
            limit_order_type: OrderType::PostOnly,
            quote_expiry: None,
            auth_token_cache: SyncMutex::new(AuthTokenCacheState::default()),
            test_auth_token: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn mock_multi_lane_test_client(
        base_url: &str,
        account_index: i64,
        maker_key_index: i64,
        cancel_key_index: Option<i64>,
        taker_key_index: Option<i64>,
    ) -> Self {
        let quota = Arc::new(QuotaTracker::new());
        let make_lane = |key_idx| {
            SigningLane::new(
                Arc::new(super::ffi::LighterSigner::mock_for_test(
                    key_idx,
                    account_index,
                )),
                key_idx,
                Arc::clone(&quota),
            )
        };
        Self {
            maker_lane: make_lane(maker_key_index),
            cancel_lane: cancel_key_index.map(make_lane),
            taker_lane: taker_key_index.map(make_lane),
            client: Client::builder()
                .timeout(Duration::from_secs(2))
                .no_proxy()
                .build()
                .unwrap(),
            base_url: base_url.to_owned(),
            send_tx_url: format!("{base_url}/api/v1/sendTx"),
            send_tx_batch_url: format!("{base_url}/api/v1/sendTxBatch"),
            account_index,
            market_id: 0,
            market_symbol: "ETH".into(),
            size_multiplier: 1e6,
            price_multiplier: 100.0,
            client_order_counter: AtomicI64::new(1000),
            order_tracker: None,
            max_worst_case_position_bits: AtomicU64::new(UNSET_POSITION_CAP_BITS),
            ref_mid_bits: AtomicU64::new(UNSET_POSITION_CAP_BITS),
            max_price_band_bps_bits: AtomicU64::new(UNSET_POSITION_CAP_BITS),
            quota,
            limit_order_type: OrderType::PostOnly,
            quote_expiry: None,
            auth_token_cache: SyncMutex::new(AuthTokenCacheState::default()),
            test_auth_token: None,
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
            chain_id,
            account_index,
            maker_key,
            cancel_key,
            taker_key,
        } = credentials;

        let quota = Arc::new(QuotaTracker::new());
        let init_lane = |key: ApiKeyCredentials, role: &str| -> Result<SigningLane> {
            let signer = Arc::new(
                super::ffi::LighterSigner::new(
                    &base_url,
                    &key.private_key,
                    chain_id,
                    key.api_key_index,
                    account_index,
                )
                .map_err(|e| anyhow::anyhow!("{role} signer init failed: {e}"))?,
            );
            Ok(SigningLane::new(
                signer,
                key.api_key_index,
                Arc::clone(&quota),
            ))
        };

        let maker_lane = init_lane(maker_key, "Maker")?;
        let cancel_lane = cancel_key.map(|k| init_lane(k, "Cancel")).transpose()?;
        let taker_lane = taker_key.map(|k| init_lane(k, "Taker")).transpose()?;

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
            market_id,
            %market_symbol,
            size_decimals,
            price_decimals,
            maker_api_key = maker_lane.api_key_index,
            cancel_api_key = cancel_lane.as_ref().map(|l| l.api_key_index),
            taker_api_key = taker_lane.as_ref().map(|l| l.api_key_index),
            "Lighter market and signing lanes initialized"
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
            maker_lane,
            cancel_lane,
            taker_lane,
            client,
            base_url,
            send_tx_url,
            send_tx_batch_url,
            account_index,
            market_id,
            market_symbol,
            size_multiplier,
            price_multiplier,
            client_order_counter: AtomicI64::new(counter_start),
            order_tracker: None,
            max_worst_case_position_bits: AtomicU64::new(UNSET_POSITION_CAP_BITS),
            ref_mid_bits: AtomicU64::new(UNSET_POSITION_CAP_BITS),
            max_price_band_bps_bits: AtomicU64::new(UNSET_POSITION_CAP_BITS),
            quota,
            limit_order_type: OrderType::Limit,
            quote_expiry: None,
            auth_token_cache: SyncMutex::new(AuthTokenCacheState::default()),
            #[cfg(test)]
            test_auth_token: None,
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

    // ─── Lane Routing & Nonce 管理 ──────────────────────────────────────────

    fn lane_for_order(&self, order_type: OrderType) -> &SigningLane {
        match order_type {
            OrderType::Ioc | OrderType::Market => {
                self.taker_lane.as_ref().unwrap_or(&self.maker_lane)
            }
            OrderType::Limit | OrderType::PostOnly => &self.maker_lane,
        }
    }

    fn cancel_lane(&self) -> &SigningLane {
        self.cancel_lane.as_ref().unwrap_or(&self.maker_lane)
    }

    fn lane_for_batch(&self, actions: &[BatchAction]) -> &SigningLane {
        if actions.iter().all(|a| matches!(a, BatchAction::Cancel(_))) {
            return self.cancel_lane();
        }
        let any_taker = actions.iter().any(|a| {
            matches!(
                a,
                BatchAction::Place(p)
                    if matches!(p.order_type, OrderType::Ioc | OrderType::Market)
            )
        });
        if any_taker {
            return self.lane_for_order(OrderType::Ioc);
        }
        &self.maker_lane
    }

    /// 生成唯一 client_order_index
    fn next_client_order_index(&self) -> i64 {
        self.client_order_counter.fetch_add(1, Ordering::SeqCst)
    }

    // ─── Auth Token ──────────────────────────────────────────────────────

    /// 创建认证 token（用于查询需要认证的 API）
    /// Go feeder 用 deadline.Unix()（秒级绝对时间戳）；在单调与墙钟双时钟均留出 1 分钟安全余量时复用缓存，并按 generation 防并发失效回写竞态。
    fn unix_now_secs() -> Result<i64> {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| anyhow::anyhow!("System clock error: {}", e))?
            .as_secs();
        i64::try_from(secs).map_err(|_| anyhow::anyhow!("System clock seconds overflow i64"))
    }

    async fn create_auth_token(&self) -> Result<Zeroizing<String>> {
        self.create_auth_token_with_clocks(|| (Instant::now(), Self::unix_now_secs()))
            .await
    }

    async fn create_auth_token_with_clocks<F>(&self, mut clocks: F) -> Result<Zeroizing<String>>
    where
        F: FnMut() -> (Instant, Result<i64>),
    {
        #[cfg(test)]
        if let Some(token) = self.test_auth_token {
            return Ok(Zeroizing::new(token.to_owned()));
        }
        let (mint_started_mono, start_wall_res) = clocks();
        let mint_started_wall_secs = start_wall_res?;
        let start_gen = {
            let cache = self.auth_token_cache.lock();
            if let Some(cached) = cache.entry.as_ref()
                && cached.is_fresh_at(mint_started_mono, mint_started_wall_secs)
            {
                return Ok(cached.token.clone());
            }
            cache.generation
        };
        let deadline_secs = mint_started_wall_secs
            .checked_add(AUTH_TOKEN_TTL_SECS)
            .ok_or_else(|| anyhow::anyhow!("Auth token expiry timestamp overflow"))?;
        // Phase 3: Direct FFI call (<100us, no spawn_blocking needed)
        let token = Zeroizing::new(
            self.maker_lane
                .signer
                .create_auth_token(deadline_secs)
                .map_err(|e| anyhow::anyhow!("Auth token failed: {}", e))?,
        );
        let (after_mono, after_wall_res) = clocks();
        let after_wall_secs = after_wall_res?;
        let hard_deadline_mono = mint_started_mono
            .checked_add(Duration::from_secs(AUTH_TOKEN_TTL_SECS as u64))
            .ok_or_else(|| anyhow::anyhow!("Auth token monotonic deadline overflow"))?;
        anyhow::ensure!(
            after_mono < hard_deadline_mono && after_wall_secs < deadline_secs,
            "Lighter auth token expired during minting"
        );
        let entry = CachedAuthToken {
            token: token.clone(),
            refresh_after: mint_started_mono + AUTH_TOKEN_CACHE_TTL,
            deadline_secs,
        };
        if entry.is_fresh_at(after_mono, after_wall_secs) {
            let mut cache = self.auth_token_cache.lock();
            // Do not repopulate if invalidated concurrently or superseded by a fresher entry.
            if cache.generation == start_gen
                && cache.entry.as_ref().is_none_or(|cur| {
                    !cur.is_fresh_at(after_mono, after_wall_secs)
                        || cur.deadline_secs < entry.deadline_secs
                        || cur.refresh_after < entry.refresh_after
                })
            {
                cache.entry = Some(entry);
            }
        }
        Ok(token)
    }

    pub(super) fn invalidate_auth_token_cache(&self) {
        let mut cache = self.auth_token_cache.lock();
        cache.generation = cache.generation.wrapping_add(1);
        cache.entry = None;
    }

    // ─── 签名 + 发送 ─────────────────────────────────────────────────────

    /// 签名一笔订单（在指定的 signing lane 上使用指定的 nonce）
    async fn sign_order_with_nonce_on_lane(
        &self,
        lane: &SigningLane,
        params: &OrderParams,
        nonce: i64,
    ) -> Result<(u8, String, String, i64)> {
        let client_order_index = self.next_client_order_index();

        // 使用 round() 防止浮点截断: 2085.87 * 100 = 208587.0 而非 208586
        let (price_int, base_amount) = encode_order(
            params.price,
            params.size,
            self.price_multiplier,
            self.size_multiplier,
        )?;

        let (ot, tif, mut expiry) = order_wire_policy(params.order_type);
        if expiry < 0
            && let Some(ttl) = self.quote_expiry
        {
            expiry =
                i64::try_from((SystemTime::now().duration_since(UNIX_EPOCH)? + ttl).as_millis())?;
        }

        let market_id = self.market_id;
        let is_ask = params.side == Side::Sell;

        // Phase 3: Direct FFI call (<100us, no spawn_blocking needed)
        let signed = lane
            .signer
            .sign_create_order(
                market_id,
                client_order_index,
                base_amount,
                price_int,
                is_ask,
                ot,
                tif,
                params.reduce_only,
                0u32,
                expiry,
                nonce,
            )
            .map_err(|e| anyhow::anyhow!("Sign failed: {}", e))?;

        tracing::debug!(
            api_key_index = lane.api_key_index,
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
    async fn sign_order_on_lane(
        &self,
        lane: &SigningLane,
        params: &OrderParams,
    ) -> Result<(u8, String, String, i64)> {
        let nonce = lane
            .get_nonce(&self.client, &self.base_url, self.account_index)
            .await?;
        self.sign_order_with_nonce_on_lane(lane, params, nonce)
            .await
    }

    #[cfg(test)]
    async fn send_tx(&self, tx_type: u8, tx_info: String) -> Result<SendTxResponse> {
        self.maker_lane
            .send_tx(&self.client, &self.send_tx_url, tx_type, tx_info)
            .await
    }

    // ─── 公开交易接口 ─────────────────────────────────────────────────────

    /// Verify a strategy's coarser tick/step is representable at the venue.
    pub fn validate_precision(&self, tick: f64, step: f64) -> Result<()> {
        encode_order(tick, step, self.price_multiplier, self.size_multiplier)?;
        Ok(())
    }

    /// Enforce an execution-boundary hard ceiling on directional worst-case
    /// exposure (`worst_case_long` for buys, `-worst_case_short` for sells)
    /// before any non-reduce-only transaction is dispatched to Lighter.
    /// Accepts `&self` via lock-free atomic bits so USD-notional profiles can
    /// refresh the base-unit cap every quote cycle as mid price moves.
    pub fn set_max_worst_case_position(&self, max_position: f64) {
        if max_position.is_finite() && max_position > 0.0 {
            self.max_worst_case_position_bits
                .store(max_position.to_bits(), Ordering::Release);
        }
    }

    fn max_worst_case_position(&self) -> Option<f64> {
        let bits = self.max_worst_case_position_bits.load(Ordering::Acquire);
        (bits != UNSET_POSITION_CAP_BITS).then(|| f64::from_bits(bits))
    }

    pub fn set_reference_price_band(&self, reference_mid: f64, max_deviation_bps: f64) {
        if reference_mid.is_finite()
            && reference_mid > 0.0
            && max_deviation_bps.is_finite()
            && max_deviation_bps > 0.0
        {
            self.ref_mid_bits
                .store(reference_mid.to_bits(), Ordering::Release);
            self.max_price_band_bps_bits
                .store(max_deviation_bps.to_bits(), Ordering::Release);
        }
    }

    fn reference_price_band(&self) -> Option<(f64, f64)> {
        let mid_bits = self.ref_mid_bits.load(Ordering::Acquire);
        let bps_bits = self.max_price_band_bps_bits.load(Ordering::Acquire);
        (mid_bits != UNSET_POSITION_CAP_BITS && bps_bits != UNSET_POSITION_CAP_BITS)
            .then(|| (f64::from_bits(mid_bits), f64::from_bits(bps_bits)))
    }

    fn ensure_side_exposure_within_cap(&self, side: Side) -> Result<()> {
        if let (Some(tracker), Some(cap)) = (&self.order_tracker, self.max_worst_case_position()) {
            let tol = 0.5 / self.size_multiplier.max(1.0) + 1e-9;
            match side {
                Side::Buy => {
                    let worst_long = tracker.worst_case_long();
                    anyhow::ensure!(
                        worst_long <= cap + tol,
                        "Execution-boundary long cap breached: worst_case_long={worst_long:.6} > cap={cap:.6}"
                    );
                }
                Side::Sell => {
                    let worst_short = tracker.worst_case_short();
                    anyhow::ensure!(
                        -worst_short <= cap + tol,
                        "Execution-boundary short cap breached: worst_case_short={worst_short:.6} < -{cap:.6}"
                    );
                }
            }
        }
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

    fn ensure_reduce_only_valid(
        &self,
        side: Side,
        size: f64,
        verified_position: Option<f64>,
        is_passive: bool,
    ) -> Result<()> {
        self.ensure_reduce_only_valid_with_pending(side, size, verified_position, true, is_passive)
    }

    fn ensure_reduce_only_valid_with_pending(
        &self,
        side: Side,
        size: f64,
        verified_position: Option<f64>,
        include_pending_exposure: bool,
        is_passive: bool,
    ) -> Result<()> {
        let (pos, pending_same_side) = if let Some(pos) = verified_position {
            (pos, 0.0)
        } else {
            let tracker = self.order_tracker.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "Reduce-only order requires OrderTracker or verified REST position proof"
                )
            })?;
            anyhow::ensure!(
                !tracker.needs_reconciliation(),
                "Order state requires reconciliation before unverified reduce-only order"
            );
            if is_passive {
                let (confirmed, short, long) = tracker.exposure_bounds();
                let pending = match side {
                    Side::Buy => (long - confirmed).max(0.0),
                    Side::Sell => (confirmed - short).max(0.0),
                };
                (confirmed, pending)
            } else {
                let tracker_side = match side {
                    Side::Buy => TrackerSide::Buy,
                    Side::Sell => TrackerSide::Sell,
                };
                (
                    tracker.confirmed_position(),
                    tracker.reduce_only_pending_exposure(tracker_side),
                )
            }
        };
        let valid_direction = pos.is_finite()
            && match side {
                Side::Sell => pos > 1e-9,
                Side::Buy => pos < -1e-9,
            };
        let worst_case_reduce = if include_pending_exposure {
            pending_same_side + size
        } else {
            pending_same_side.max(size)
        };
        anyhow::ensure!(
            valid_direction
                && size.is_finite()
                && size > 0.0
                && worst_case_reduce <= pos.abs() * 1.000_001 + 1e-6,
            "Reduce-only {:?} size {:.6} (pending={:.6}) incompatible with confirmed position {:.6}",
            side,
            size,
            pending_same_side,
            pos
        );
        Ok(())
    }

    fn ensure_can_place_with_pending(
        &self,
        params: &OrderParams,
        verified_position: Option<f64>,
        include_pending_exposure: bool,
    ) -> Result<()> {
        if params.reduce_only {
            return self.ensure_reduce_only_valid_with_pending(
                params.side,
                params.size,
                verified_position,
                include_pending_exposure,
                params.order_type != OrderType::Ioc,
            );
        }
        self.ensure_can_place(params, verified_position)
    }

    fn ensure_can_place(&self, params: &OrderParams, verified_position: Option<f64>) -> Result<()> {
        if params.reduce_only {
            return self.ensure_reduce_only_valid(
                params.side,
                params.size,
                verified_position,
                params.order_type != OrderType::Ioc,
            );
        }
        self.ensure_open_risk_allowed()?;
        if let Some((ref_mid, max_bps)) = self.reference_price_band() {
            let dev_bps = ((params.price - ref_mid).abs() / ref_mid) * 10_000.0;
            anyhow::ensure!(
                dev_bps.is_finite() && dev_bps <= max_bps + 1e-6,
                "Execution-boundary price-band guard rejected {:?} @ {:.6} (ref_mid={:.6}, deviation={:.2} bps > max={:.2} bps)",
                params.side,
                params.price,
                ref_mid,
                dev_bps,
                max_bps
            );
        }
        Ok(())
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

    /// 通用下单 (v5.0.0: per-order tracking + multi-key lane isolation)
    pub async fn place_order(&self, params: OrderParams) -> Result<OrderResult> {
        self.place_order_with_proof(params, None).await
    }

    async fn place_order_with_proof(
        &self,
        params: OrderParams,
        verified_position: Option<f64>,
    ) -> Result<OrderResult> {
        let params = self.normalize_order(params)?;
        let lane = self.lane_for_order(params.order_type);
        let _submission = lane.submission_lock.lock().await;
        self.ensure_can_place(&params, verified_position)?;
        let (tx_type, tx_info, tx_hash, client_order_index) =
            self.sign_order_on_lane(lane, &params).await?;
        self.ensure_can_place(&params, verified_position)?;

        // Optimistic accounting: register per-order BEFORE API call
        let tracker_side = match params.side {
            Side::Buy => TrackerSide::Buy,
            Side::Sell => TrackerSide::Sell,
        };
        if let Some(ref tracker) = self.order_tracker {
            anyhow::ensure!(
                tracker.start_tracking_order(
                    client_order_index,
                    tracker_side,
                    params.price,
                    params.size,
                    params.reduce_only,
                ),
                "Tracker refused order reservation"
            );
            // Re-verify reconciliation state and execution-boundary exposure cap
            // after reserving so concurrent lane failures or strategy bugs cannot
            // slip excess exposure across the wire boundary.
            if let Err(err) = self
                .ensure_can_place_with_pending(&params, verified_position, false)
                .and_then(|()| {
                    if params.reduce_only {
                        Ok(())
                    } else {
                        self.ensure_side_exposure_within_cap(params.side)
                    }
                })
            {
                tracker.mark_failed(client_order_index);
                return Err(err);
            }
            tracker.mark_submission_uncertain(client_order_index);
        }

        tracing::info!(
            api_key_index = lane.api_key_index,
            "Signed {} order: price={} size={} type={:?} tx_hash={} coi={}",
            params.side,
            params.price,
            params.size,
            params.order_type,
            tx_hash,
            client_order_index
        );

        let mut guard = SubmissionGuard::new(self.order_tracker.as_deref());
        match lane
            .send_tx(&self.client, &self.send_tx_url, tx_type, tx_info)
            .await
        {
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
        let lane = self.lane_for_order(self.limit_order_type);
        let _submission = lane.submission_lock.lock().await;
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
        self.ensure_can_place(&bid, None)?;
        self.ensure_can_place(&ask, None)?;
        // Get base nonce for batch
        let base_nonce = lane
            .get_nonce(&self.client, &self.base_url, self.account_index)
            .await?;

        // 签名买单 (nonce = base_nonce)
        let (bid_type, bid_info, _bid_hash, bid_coi) = self
            .sign_order_with_nonce_on_lane(lane, &bid, base_nonce)
            .await?;

        // 签名卖单 (nonce = base_nonce + 1)
        let (ask_type, ask_info, _ask_hash, ask_coi) = self
            .sign_order_with_nonce_on_lane(lane, &ask, base_nonce + 1)
            .await?;
        self.ensure_open_risk_allowed()?;

        tracing::info!(
            api_key_index = lane.api_key_index,
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
            if let Err(err) = self
                .ensure_open_risk_allowed()
                .and_then(|()| self.ensure_side_exposure_within_cap(Side::Buy))
                .and_then(|()| self.ensure_side_exposure_within_cap(Side::Sell))
            {
                tracker.mark_failed(bid_coi);
                tracker.mark_failed(ask_coi);
                return Err(err);
            }
        }

        if let Some(tracker) = &self.order_tracker {
            tracker.mark_submission_uncertain(bid_coi);
            tracker.mark_submission_uncertain(ask_coi);
        }
        let mut guard = SubmissionGuard::new(self.order_tracker.as_deref());
        match lane
            .send_tx_batch(
                &self.client,
                &self.send_tx_batch_url,
                &[(bid_type, bid_info), (ask_type, ask_info)],
            )
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
                // A batch may have been partially accepted. Keep every reservation
                // until authoritative order events/reconciliation resolve it.
                Err(e)
            }
        }
    }

    /// 核心：通用批量执行（支持混合 挂单/撤单，纯撤单自动路由至 cancel_lane）
    pub async fn execute_batch(&self, actions: Vec<BatchAction>) -> Result<BatchResult> {
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
        let lane = self.lane_for_batch(&actions);
        let _submission = lane.submission_lock.lock().await;
        let mut txs = Vec::with_capacity(actions.len());
        let mut place_results = Vec::new();
        let market_id = self.market_id;
        let opens_risk = actions
            .iter()
            .any(|action| matches!(action, BatchAction::Place(order) if !order.reduce_only));

        let mut prepared_actions = Vec::with_capacity(actions.len());
        let (mut ro_buy_total, mut ro_sell_total) = (0.0_f64, 0.0_f64);
        let (mut ro_buy_passive, mut ro_sell_passive) = (false, false);
        for action in actions {
            match action {
                BatchAction::Place(params) => {
                    let params = self.normalize_order(params)?;
                    self.ensure_can_place(&params, None)?;
                    if params.reduce_only {
                        let is_passive = params.order_type != OrderType::Ioc;
                        match params.side {
                            Side::Buy => {
                                ro_buy_total += params.size;
                                ro_buy_passive |= is_passive;
                            }
                            Side::Sell => {
                                ro_sell_total += params.size;
                                ro_sell_passive |= is_passive;
                            }
                        }
                    }
                    prepared_actions.push(BatchAction::Place(params));
                }
                BatchAction::Cancel(order_index) => {
                    prepared_actions.push(BatchAction::Cancel(order_index));
                }
            }
        }
        if ro_buy_total > 0.0 {
            self.ensure_reduce_only_valid(Side::Buy, ro_buy_total, None, ro_buy_passive)?;
        }
        if ro_sell_total > 0.0 {
            self.ensure_reduce_only_valid(Side::Sell, ro_sell_total, None, ro_sell_passive)?;
        }

        // 获取起始 Nonce
        let base_nonce = lane
            .get_nonce(&self.client, &self.base_url, self.account_index)
            .await?;

        let mut reduce_only_flags = Vec::with_capacity(prepared_actions.len());
        for (i, action) in prepared_actions.into_iter().enumerate() {
            let nonce = base_nonce + (i as i64);
            match action {
                BatchAction::Place(params) => {
                    let (tx_type, tx_info, _hash, coi) = self
                        .sign_order_with_nonce_on_lane(lane, &params, nonce)
                        .await?;

                    txs.push((tx_type, tx_info));
                    reduce_only_flags
                        .push((params.reduce_only, params.order_type != OrderType::Ioc));
                    place_results.push(PlaceResult {
                        client_order_index: coi,
                        side: params.side,
                        price: params.price,
                        size: params.size,
                    });
                }
                BatchAction::Cancel(order_index) => {
                    let signed = lane
                        .signer
                        .sign_cancel_order(market_id, order_index, nonce)
                        .map_err(|e| anyhow::anyhow!("Sign batch cancel failed: {}", e))?;
                    txs.push((signed.tx_type, signed.tx_info));
                }
            }
        }

        let cancels = txs.len() - place_results.len();
        self.submit_prepared_with_flags(
            txs,
            place_results,
            &reduce_only_flags,
            cancels,
            opens_risk,
            |_, _| std::future::ready(Ok(())),
            |txs| async move {
                lane.send_tx_batch(&self.client, &self.send_tx_batch_url, &txs)
                    .await
            },
        )
        .await
    }

    #[cfg(test)]
    async fn submit_prepared<R, RFut, S, SFut>(
        &self,
        txs: Vec<(u8, String)>,
        place_results: Vec<PlaceResult>,
        cancels: usize,
        opens_risk: bool,
        record: R,
        send: S,
    ) -> Result<BatchResult>
    where
        R: FnOnce(&[PlaceResult], usize) -> RFut,
        RFut: std::future::Future<Output = Result<()>>,
        S: FnOnce(Vec<(u8, String)>) -> SFut,
        SFut: std::future::Future<Output = Result<SendTxBatchResponse>>,
    {
        let flags = vec![(false, false); place_results.len()];
        self.submit_prepared_with_flags(
            txs,
            place_results,
            &flags,
            cancels,
            opens_risk,
            record,
            send,
        )
        .await
    }

    /// The part of `execute_batch` after every action has signed: reserve
    /// risk, then `record` the creates, then `send`. `record` runs only for
    /// batches with creates; until it completes nothing is sent. A record
    /// error releases the reservations because nothing left this process.
    /// Once `record` starts, a dropped future or an uncertain send keeps every
    /// create reserved and requires reconciliation, as for a lost send.
    #[allow(clippy::too_many_arguments)]
    async fn submit_prepared_with_flags<R, RFut, S, SFut>(
        &self,
        txs: Vec<(u8, String)>,
        place_results: Vec<PlaceResult>,
        reduce_only_flags: &[(bool, bool)],
        cancels: usize,
        opens_risk: bool,
        record: R,
        send: S,
    ) -> Result<BatchResult>
    where
        R: FnOnce(&[PlaceResult], usize) -> RFut,
        RFut: std::future::Future<Output = Result<()>>,
        S: FnOnce(Vec<(u8, String)>) -> SFut,
        SFut: std::future::Future<Output = Result<SendTxBatchResponse>>,
    {
        if opens_risk {
            self.ensure_open_risk_allowed()?;
        }
        // Register only after ALL actions have signed successfully. A local signing
        // failure must not leave phantom reservations for an unsent partial batch.
        if let Some(tracker) = &self.order_tracker {
            let (mut ro_buy_reserved, mut ro_sell_reserved) = (0.0_f64, 0.0_f64);
            let (mut ro_buy_passive, mut ro_sell_passive) = (false, false);
            for (idx, order) in place_results.iter().enumerate() {
                let (reduce_only, is_passive) = reduce_only_flags
                    .get(idx)
                    .copied()
                    .unwrap_or((false, false));
                if reduce_only {
                    match order.side {
                        Side::Buy => {
                            ro_buy_reserved += order.size;
                            ro_buy_passive |= is_passive;
                        }
                        Side::Sell => {
                            ro_sell_reserved += order.size;
                            ro_sell_passive |= is_passive;
                        }
                    }
                }
                let side = if order.side == Side::Buy {
                    TrackerSide::Buy
                } else {
                    TrackerSide::Sell
                };
                if !tracker.start_tracking_order(
                    order.client_order_index,
                    side,
                    order.price,
                    order.size,
                    reduce_only,
                ) {
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
            let cap_check = (|| -> Result<()> {
                if ro_buy_reserved > 0.0 {
                    self.ensure_reduce_only_valid_with_pending(
                        Side::Buy,
                        ro_buy_reserved,
                        None,
                        false,
                        ro_buy_passive,
                    )?;
                }
                if ro_sell_reserved > 0.0 {
                    self.ensure_reduce_only_valid_with_pending(
                        Side::Sell,
                        ro_sell_reserved,
                        None,
                        false,
                        ro_sell_passive,
                    )?;
                }
                if opens_risk {
                    self.ensure_open_risk_allowed()?;
                    for order in &place_results {
                        self.ensure_side_exposure_within_cap(order.side)?;
                    }
                }
                Ok(())
            })();
            if let Err(err) = cap_check {
                for order in &place_results {
                    tracker.mark_failed(order.client_order_index);
                }
                return Err(err);
            }
        }

        let mut guard = SubmissionGuard::new(self.order_tracker.as_deref());
        if !place_results.is_empty()
            && let Err(error) = record(&place_results, cancels).await
        {
            // Nothing was sent: release the reservations and the guard.
            if let Some(tracker) = &self.order_tracker {
                for order in &place_results {
                    tracker.mark_failed(order.client_order_index);
                }
            }
            guard.disarm();
            return Err(error.context("Lighter batch record failed; batch not sent"));
        }

        tracing::info!("Submitting batch of {} mixed actions", txs.len());
        match send(txs).await {
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

    /// 撤销单笔订单（支持 cancel_lane 独立队列与 InvalidNonce 单次自愈重同步）
    pub async fn cancel_order(&self, order_index: i64) -> Result<()> {
        let lane = self.cancel_lane();
        let _submission = lane.submission_lock.lock().await;
        let market_id = self.market_id;

        for attempt in 0..2 {
            let nonce = lane
                .get_nonce(&self.client, &self.base_url, self.account_index)
                .await?;
            let signed = lane
                .signer
                .sign_cancel_order(market_id, order_index, nonce)
                .map_err(|e| anyhow::anyhow!("Sign cancel failed: {}", e))?;

            let mut guard = SubmissionGuard::new(self.order_tracker.as_deref());
            match lane
                .send_tx(
                    &self.client,
                    &self.send_tx_url,
                    signed.tx_type,
                    signed.tx_info,
                )
                .await
            {
                Ok(_) => {
                    guard.disarm();
                    tracing::info!(
                        api_key_index = lane.api_key_index,
                        order_index,
                        "Submitted cancellation for order"
                    );
                    return Ok(());
                }
                Err(err) if attempt == 0 && is_invalid_nonce_rejection(&err) => {
                    // Definitive 21104 InvalidNonce: nothing executed and `send_tx`
                    // left `nonce_init = false`, so the retry fetches `nextNonce`.
                    guard.disarm();
                    tracing::warn!(
                        api_key_index = lane.api_key_index,
                        order_index,
                        stale_nonce = nonce,
                        "Cancel rejected with InvalidNonce; resyncing nextNonce once"
                    );
                    continue;
                }
                Err(err) => {
                    if !is_uncertain(&err) {
                        guard.disarm();
                    }
                    return Err(err);
                }
            }
        }
        unreachable!("cancel_order loop returns within 2 attempts")
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
            .header("Authorization", auth.as_str())
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            if matches!(
                status,
                reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
            ) {
                self.invalidate_auth_token_cache();
            }
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
        let (account, _, _) = self.fetch_account().await?;
        let position = unique_position(account.positions, self.market_id)?;
        Ok((position, account.transaction_time))
    }

    async fn fetch_account(&self) -> Result<(AccountInfo, Instant, Instant)> {
        let url = format!(
            "{}/api/v1/account?by=index&value={}",
            self.base_url, self.account_index
        );
        let requested_at = Instant::now();
        let resp = self.client.get(&url).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("get_position HTTP {}", resp.status());
        }
        let acct_resp: AccountsResponse = resp.json().await?;
        let received_at = Instant::now();
        if acct_resp.code != 200 {
            anyhow::bail!("get_position: account response code {}", acct_resp.code);
        }
        Ok((
            unique_account(acct_resp.accounts, self.account_index)?,
            requested_at,
            received_at,
        ))
    }

    pub async fn signed_position(&self) -> Result<rust_decimal::Decimal> {
        Ok(self.position_evidence().await?.position)
    }

    /// The production position read: unique account and market identity,
    /// exact signed size, trade watermark and local request/receipt times.
    pub async fn position_evidence(&self) -> Result<PositionEvidence> {
        let (account, requested_at, received_at) = self.fetch_account().await?;
        let account_index = account.index;
        let (position, transaction_time) =
            account_position(account, self.market_id, &self.market_symbol)?;
        Ok(PositionEvidence {
            account_index,
            market_id: self.market_id,
            position,
            transaction_time,
            requested_at,
            received_at,
        })
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

        self.place_order_with_proof(self.close_order(position, current_price)?, Some(position))
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
        price_units >= 1.0 && price_units <= i32::MAX as f64,
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

#[derive(Debug)]
struct LighterRejectedSubmission {
    code: i32,
    error_code: LighterErrorCode,
    message: Option<String>,
}

impl std::fmt::Display for LighterRejectedSubmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Lighter rejected submission code={}: {}",
            self.code,
            self.message.as_deref().unwrap_or("unknown")
        )
    }
}

impl std::error::Error for LighterRejectedSubmission {}

enum SubmissionFailure {
    Uncertain(String),
    Margin,
    Rejected(LighterRejectedSubmission),
}

impl SubmissionFailure {
    fn preserves_unconsumed_single_tx_nonce(&self) -> bool {
        match self {
            Self::Uncertain(_) => false,
            Self::Margin => true,
            Self::Rejected(rej) => rej.error_code.preserves_unconsumed_single_tx_nonce(),
        }
    }

    fn into_error(self) -> anyhow::Error {
        match self {
            Self::Uncertain(msg) => TradingError::SubmissionUncertain(msg).into(),
            Self::Margin => TradingError::InsufficientMargin.into(),
            Self::Rejected(rej) => rej.into(),
        }
    }
}

fn is_invalid_nonce_rejection(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<LighterRejectedSubmission>(),
        Some(rej) if rej.error_code == LighterErrorCode::InvalidNonce
    )
}

fn classify_submission_response(
    status: reqwest::StatusCode,
    body: &str,
) -> std::result::Result<(), SubmissionFailure> {
    if status.is_server_error() {
        return Err(SubmissionFailure::Uncertain(format!("HTTP {status}")));
    }
    let response: LighterErrorResponse = serde_json::from_str(body)
        .map_err(|e| SubmissionFailure::Uncertain(format!("HTTP {status}: {e}")))?;
    if response.code == 200 && status.is_success() {
        return Ok(());
    }
    if response.code == 200 {
        return Err(SubmissionFailure::Uncertain(format!(
            "Conflicting HTTP {status}"
        )));
    }
    let error_code = response.error_code();
    if error_code.is_margin_error() {
        return Err(SubmissionFailure::Margin);
    }
    Err(SubmissionFailure::Rejected(LighterRejectedSubmission {
        code: response.code,
        error_code,
        message: response.message,
    }))
}

fn validate_submission_response(status: reqwest::StatusCode, body: &str) -> Result<()> {
    classify_submission_response(status, body).map_err(SubmissionFailure::into_error)
}

/// Exactly one account with `index`; an empty, foreign or duplicated list
/// is not evidence of anything, even when the duplicates agree.
fn unique_account(accounts: Vec<AccountInfo>, index: i64) -> Result<AccountInfo> {
    let mut matching = accounts
        .into_iter()
        .filter(|account| account.index == index);
    let account = matching
        .next()
        .ok_or_else(|| anyhow::anyhow!("Position response does not match the Lighter account"))?;
    anyhow::ensure!(
        matching.next().is_none(),
        "Position response lists the Lighter account more than once"
    );
    Ok(account)
}

/// At most one position for `market_id`; other markets keep the existing
/// account scope and are ignored.
fn unique_position(positions: Vec<Position>, market_id: i64) -> Result<Option<Position>> {
    let mut matching = positions.into_iter().filter(|p| p.market_id == market_id);
    let position = matching.next();
    anyhow::ensure!(
        matching.next().is_none(),
        "Position response lists the Lighter market more than once"
    );
    Ok(position)
}

/// Exact signed position and trade watermark for `market_id` in one account,
/// by the production rules: unique market entry, matching symbol, valid sign
/// and a non-negative size. A missing entry is a zero position; a 0
/// watermark is unknown.
fn account_position(
    account: AccountInfo,
    market_id: i64,
    symbol: &str,
) -> Result<(rust_decimal::Decimal, Option<u64>)> {
    use rust_decimal::Decimal;
    let transaction_time = (account.transaction_time > 0).then_some(account.transaction_time);
    let position = match unique_position(account.positions, market_id)? {
        None => Decimal::ZERO,
        Some(position) => {
            let size = Decimal::from_str_exact(&position.position)?;
            anyhow::ensure!(
                position.symbol == symbol
                    && size >= Decimal::ZERO
                    && (matches!(position.sign, -1 | 1) || (position.sign == 0 && size.is_zero())),
                "Invalid Lighter position snapshot"
            );
            size * Decimal::from(position.sign)
        }
    };
    Ok((position, transaction_time))
}

/// Offline form of `position_evidence` for a frozen raw
/// `/api/v1/account?by=index` response: the same code check, unique account,
/// unique market and position rules. Returns the signed position and the
/// account trade watermark.
pub(crate) fn account_response_position(
    response: &serde_json::Value,
    account_index: i64,
    market_id: i64,
    symbol: &str,
) -> Result<(rust_decimal::Decimal, Option<u64>)> {
    let response = AccountsResponse::deserialize(response)?;
    anyhow::ensure!(
        response.code == 200,
        "get_position: account response code {}",
        response.code
    );
    account_position(
        unique_account(response.accounts, account_index)?,
        market_id,
        symbol,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// Serve each `(status, body)` once per request on a loopback port.
    async fn account_server(
        responses: Vec<(u16, String)>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0; 4096];
                let read = socket.read(&mut request).await.unwrap();
                let line = String::from_utf8_lossy(&request[..read]);
                assert!(
                    line.starts_with("GET /api/v1/account?by=index&value=0 "),
                    "{line}"
                );
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        (url, server)
    }

    fn position_json(market: i64, symbol: &str, sign: i64, size: &str) -> serde_json::Value {
        serde_json::json!({"market_id": market, "symbol": symbol, "sign": sign,
            "position": size, "avg_entry_price": "2000", "unrealized_pnl": "0",
            "liquidation_price": "0"})
    }

    fn accounts_json(accounts: &[serde_json::Value]) -> String {
        serde_json::json!({"code": 200, "accounts": accounts}).to_string()
    }

    fn account_json(
        index: i64,
        time: Option<u64>,
        positions: &[serde_json::Value],
    ) -> serde_json::Value {
        let mut account = serde_json::json!({"index": index, "positions": positions});
        if let Some(time) = time {
            account["transaction_time"] = time.into();
        }
        account
    }

    /// The real client parses each response into exact, uniquely identified
    /// evidence or rejects it; nothing ambiguous or missing becomes flat.
    #[tokio::test]
    async fn position_evidence_requires_unique_identity_and_exact_values() {
        use rust_decimal::Decimal;
        let eth = |sign, size| position_json(0, "ETH", sign, size);
        let other = position_json(1, "BTC", 1, "0.5");
        let one = |time, positions: &[serde_json::Value]| {
            (200, accounts_json(&[account_json(0, time, positions)]))
        };
        let valid: Vec<((u16, String), Decimal, Option<u64>)> = vec![
            (
                one(Some(30), &[eth(1, "0.0300")]),
                Decimal::new(3, 2),
                Some(30),
            ),
            (
                one(Some(31), &[other.clone(), eth(-1, "0.02")]),
                Decimal::new(-2, 2),
                Some(31),
            ),
            (one(Some(32), &[eth(0, "0")]), Decimal::ZERO, Some(32)),
            // Unique account without this market: flat, other markets ignored.
            (
                one(Some(33), std::slice::from_ref(&other)),
                Decimal::ZERO,
                Some(33),
            ),
            (one(None, &[eth(1, "0.01")]), Decimal::new(1, 2), None),
            (one(Some(0), &[]), Decimal::ZERO, None),
        ];
        let duplicate_account = accounts_json(&[
            account_json(0, Some(30), &[eth(1, "0.03")]),
            account_json(0, Some(30), &[eth(1, "0.03")]),
        ]);
        let conflicting_accounts = |first_flat: bool| {
            let flat = account_json(0, Some(30), &[]);
            let long = account_json(0, Some(30), &[eth(1, "0.03")]);
            accounts_json(&if first_flat {
                [flat, long]
            } else {
                [long, flat]
            })
        };
        let invalid: Vec<(u16, String)> = vec![
            (500, "{}".into()),
            (
                200,
                serde_json::json!({"code": 21100, "accounts": []}).to_string(),
            ),
            (200, accounts_json(&[])),
            (
                200,
                accounts_json(&[account_json(7, Some(30), &[eth(1, "0.03")])]),
            ),
            (
                200,
                serde_json::json!({"code": 200, "accounts": [{"index": 0}]}).to_string(),
            ),
            (200, "truncated".into()),
            (200, duplicate_account),
            (200, conflicting_accounts(true)),
            (200, conflicting_accounts(false)),
            one(Some(30), &[eth(1, "0.03"), eth(1, "0.03")]),
            one(Some(30), &[eth(0, "0"), eth(1, "0.03")]),
            one(Some(30), &[eth(1, "0.03"), eth(0, "0")]),
            one(Some(30), &[position_json(0, "BTC", 1, "0.03")]),
            one(Some(30), &[eth(2, "0.03")]),
            one(Some(30), &[eth(0, "0.03")]),
            one(Some(30), &[eth(1, "-0.03")]),
            one(Some(30), &[eth(1, "0.00000000000000000000000000001")]),
            one(Some(30), &[eth(1, "1e400")]),
            one(Some(30), &[eth(1, "not-a-number")]),
        ];
        let responses = valid
            .iter()
            .map(|(response, _, _)| response.clone())
            .chain(invalid.iter().cloned())
            .collect();
        let (url, server) = account_server(responses).await;
        let trading = LighterTrading::offline_test_client(&url);
        for (response, position, watermark) in &valid {
            let before = Instant::now();
            let evidence = trading.position_evidence().await.unwrap();
            assert_eq!(
                (
                    evidence.account_index,
                    evidence.market_id,
                    evidence.position,
                    evidence.transaction_time
                ),
                (0, 0, *position, *watermark),
                "{}",
                response.1
            );
            assert!(
                before <= evidence.requested_at && evidence.requested_at <= evidence.received_at
            );
        }
        for (status, body) in &invalid {
            assert!(
                trading.position_evidence().await.is_err(),
                "accepted {status} {body}"
            );
        }
        server.await.unwrap();
    }

    #[test]
    fn fill_watermark_coverage_never_uses_unknown_or_receipt_time() {
        let now = Instant::now();
        let evidence = |transaction_time| PositionEvidence {
            account_index: 0,
            market_id: 0,
            position: rust_decimal::Decimal::ZERO,
            transaction_time,
            requested_at: now,
            received_at: now,
        };
        assert!(evidence(None).covers(0), "no fills to cover");
        assert!(!evidence(None).covers(30));
        assert!(!evidence(Some(29)).covers(30));
        assert!(evidence(Some(30)).covers(30));
        assert!(evidence(Some(31)).covers(30));
    }

    #[tokio::test]
    async fn transport_failure_invalidates_nonce_without_releasing_reservation() {
        let tracker = Arc::new(OrderTracker::new());
        assert!(tracker.start_tracking(42, TrackerSide::Buy, 100.0, 1.0));
        tracker.mark_submission_uncertain(42);
        let mut trading = LighterTrading::offline_test_client("http://127.0.0.1:1");
        trading.maker_lane.nonce.store(9, Ordering::Release);
        *trading.maker_lane.nonce_init.lock().await = true;
        trading.set_order_tracker(Arc::clone(&tracker));
        let error = trading.send_tx(14, "{}".into()).await.err().unwrap();
        assert!(is_uncertain(&error));
        assert!(!*trading.maker_lane.nonce_init.lock().await);
        assert_eq!(trading.maker_lane.nonce.load(Ordering::Acquire), 9);
        assert_eq!(tracker.worst_case_long(), 1.0);
    }

    #[test]
    fn robinhood_domain_resolves_466324_and_rejects_reserved_api_key_157() {
        use crate::exchanges::hyperliquid::Network;
        let hex_a = "01".repeat(40);
        let hex_b = "02".repeat(40);
        let hex_c = "03".repeat(40);
        let creds = LighterCredentials::from_env(|key| match key {
            "LIGHTER_PRIVATE_KEY" => Some(hex_a.clone()),
            "LIGHTER_ACCOUNT_INDEX" => Some("42".into()),
            "LIGHTER_API_KEY_INDEX" => Some("7".into()),
            "LIGHTER_CHAIN_ID" => Some("466324".into()),
            "LIGHTER_BASE_URL" => Some("https://api.rh.lighter.xyz/".into()),
            "LIGHTER_CANCEL_PRIVATE_KEY" => Some(hex_b.clone()),
            "LIGHTER_CANCEL_API_KEY_INDEX" => Some("8".into()),
            "LIGHTER_TAKER_PRIVATE_KEY" => Some(hex_c.clone()),
            "LIGHTER_TAKER_API_KEY_INDEX" => Some("9".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(creds.domain().unwrap(), LighterDomain::RobinhoodMainnet);
        assert_eq!(creds.network().unwrap(), Network::Mainnet);
        assert_eq!(LighterDomain::RobinhoodMainnet.lock_slug(), "Robinhood");
        assert_eq!(
            LighterDomain::RobinhoodMainnet.telemetry_exchange(),
            "lighter_robinhood"
        );
        assert!(creds.has_cancel_lane() && creds.has_taker_lane());

        // Reserved key 157 on Robinhood Chain (466324) must fail closed on every lane.
        for (idx_key, idx_val) in [
            ("LIGHTER_API_KEY_INDEX", "157"),
            ("LIGHTER_CANCEL_API_KEY_INDEX", "157"),
            ("LIGHTER_TAKER_API_KEY_INDEX", "157"),
        ] {
            let err = LighterCredentials::from_env(|key| match key {
                k if k == idx_key => Some(idx_val.into()),
                "LIGHTER_PRIVATE_KEY" => Some(hex_a.clone()),
                "LIGHTER_ACCOUNT_INDEX" => Some("42".into()),
                "LIGHTER_API_KEY_INDEX" => Some("7".into()),
                "LIGHTER_CHAIN_ID" => Some("466324".into()),
                "LIGHTER_CANCEL_PRIVATE_KEY" => Some(hex_b.clone()),
                "LIGHTER_CANCEL_API_KEY_INDEX" => Some("8".into()),
                "LIGHTER_TAKER_PRIVATE_KEY" => Some(hex_c.clone()),
                "LIGHTER_TAKER_API_KEY_INDEX" => Some("9".into()),
                _ => None,
            })
            .err()
            .expect("reserved key 157 must fail");
            assert!(err.to_string().contains("157"), "{err}");
        }

        // Duplicate API key index across lanes must fail closed.
        let dup = LighterCredentials::from_env(|key| match key {
            "LIGHTER_PRIVATE_KEY" => Some(hex_a.clone()),
            "LIGHTER_ACCOUNT_INDEX" => Some("42".into()),
            "LIGHTER_API_KEY_INDEX" => Some("7".into()),
            "LIGHTER_CANCEL_PRIVATE_KEY" => Some(hex_b.clone()),
            "LIGHTER_CANCEL_API_KEY_INDEX" => Some("7".into()),
            _ => None,
        });
        assert!(dup.is_err());

        // Cross-domain URL / chain_id mismatch must fail closed.
        let mismatch = LighterCredentials::from_env(|key| match key {
            "LIGHTER_PRIVATE_KEY" => Some(hex_a.clone()),
            "LIGHTER_ACCOUNT_INDEX" => Some("42".into()),
            "LIGHTER_API_KEY_INDEX" => Some("7".into()),
            "LIGHTER_CHAIN_ID" => Some("466324".into()),
            "LIGHTER_BASE_URL" => Some("https://mainnet.zklighter.elliot.ai".into()),
            _ => None,
        })
        .unwrap();
        assert!(mismatch.domain().is_err());
    }

    #[tokio::test]
    async fn pre_application_rejection_preserves_cached_nonce_while_invalid_nonce_resyncs() {
        use std::sync::atomic::AtomicUsize;
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let nonce_fetches = Arc::new(AtomicUsize::new(0));
        let fetches = Arc::clone(&nonce_fetches);
        let send_calls = Arc::new(AtomicUsize::new(0));
        let sends = Arc::clone(&send_calls);

        let server = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let fetches = Arc::clone(&fetches);
                let sends = Arc::clone(&sends);
                tokio::spawn(async move {
                    let mut socket = BufReader::new(socket);
                    let mut req_line = String::new();
                    socket.read_line(&mut req_line).await.unwrap();
                    let path = req_line.split_whitespace().nth(1).unwrap().to_owned();
                    let mut len = 0usize;
                    loop {
                        let mut hdr = String::new();
                        socket.read_line(&mut hdr).await.unwrap();
                        if hdr.trim().is_empty() {
                            break;
                        }
                        if let Some((k, v)) = hdr.split_once(':')
                            && k.eq_ignore_ascii_case("content-length")
                        {
                            len = v.trim().parse().unwrap();
                        }
                    }
                    let mut body = vec![0u8; len];
                    socket.read_exact(&mut body).await.unwrap();
                    let body_str = String::from_utf8_lossy(&body);

                    let (status, resp) = match path.split('?').next().unwrap() {
                        "/api/v1/nextNonce" => {
                            let n = fetches.fetch_add(1, Ordering::AcqRel);
                            let server_nonce = if n == 0 { 50 } else { 88 };
                            (200, serde_json::json!({"nonce": server_nonce}).to_string())
                        }
                        "/api/v1/sendTx" => {
                            let step = sends.fetch_add(1, Ordering::AcqRel);
                            let tx_line =
                                body_str.lines().find(|line| line.starts_with('{')).unwrap();
                            let tx: serde_json::Value = serde_json::from_str(tx_line).unwrap();
                            match step {
                                // Step 0: InsufficientMargin (21301) at nonce 50 -> unconsumed!
                                0 => {
                                    assert_eq!(tx["Nonce"], 50);
                                    (
                                        200,
                                        serde_json::json!({"code": 21301, "message": "margin"})
                                            .to_string(),
                                    )
                                }
                                // Step 1: RateLimited (23000) HTTP 429 at SAME nonce 50 -> unconsumed!
                                1 => {
                                    assert_eq!(tx["Nonce"], 50);
                                    (
                                        429,
                                        serde_json::json!({"code": 23000, "message": "rate limit"})
                                            .to_string(),
                                    )
                                }
                                // Step 2: Succeeds at SAME cached nonce 50 without any extra nextNonce GET!
                                2 => {
                                    assert_eq!(tx["Nonce"], 50);
                                    (
                                        200,
                                        serde_json::json!({"code": 200, "tx_hash": "ok-50"})
                                            .to_string(),
                                    )
                                }
                                // Step 3: InvalidNonce (21104) at nonce 51 -> invalidates cache!
                                3 => {
                                    assert_eq!(tx["Nonce"], 51);
                                    (
                                        200,
                                        serde_json::json!({"code": 21104, "message": "bad nonce"})
                                            .to_string(),
                                    )
                                }
                                // Step 4: Resynced via second nextNonce GET -> nonce 88!
                                4 => {
                                    assert_eq!(tx["Nonce"], 88);
                                    (
                                        200,
                                        serde_json::json!({"code": 200, "tx_hash": "ok-88"})
                                            .to_string(),
                                    )
                                }
                                other => panic!("unexpected send step {other}"),
                            }
                        }
                        other => panic!("unexpected endpoint {other}"),
                    };
                    socket
                        .get_mut()
                        .write_all(
                            format!(
                                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\n\
                                 Content-Length: {}\r\nConnection: close\r\n\r\n{resp}",
                                resp.len()
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                });
            }
        });

        let mut trading = LighterTrading::mock_multi_lane_test_client(&url, 10, 7, None, None);
        let tracker = Arc::new(OrderTracker::new());
        trading.set_order_tracker(Arc::clone(&tracker));

        let order = OrderParams {
            size: 0.01,
            price: 2000.0,
            side: Side::Buy,
            order_type: OrderType::PostOnly,
            reduce_only: false,
        };
        // 1. Margin error releases reservation AND preserves cached nonce 50.
        let e1 = trading.place_order(order.clone()).await.unwrap_err();
        assert!(matches!(
            e1.downcast_ref::<TradingError>(),
            Some(TradingError::InsufficientMargin)
        ));
        assert_eq!(nonce_fetches.load(Ordering::Acquire), 1);
        assert!(!tracker.needs_reconciliation());
        assert_eq!(tracker.worst_case_long(), 0.0);

        // 2. Rate-limit error preserves cached nonce 50 with zero extra nextNonce GETs.
        assert!(trading.place_order(order.clone()).await.is_err());
        assert_eq!(nonce_fetches.load(Ordering::Acquire), 1);

        // 3. Next order succeeds immediately at cached nonce 50 (still only 1 nextNonce GET!).
        let ok1 = trading.place_order(order.clone()).await.unwrap();
        assert_eq!(nonce_fetches.load(Ordering::Acquire), 1);
        tracker.mark_failed(ok1.client_order_index);

        // 4. InvalidNonce invalidates local cache, so order #5 fetches nextNonce=88 and succeeds.
        assert!(trading.place_order(order.clone()).await.is_err());
        assert!(!*trading.maker_lane.nonce_init.lock().await);
        let ok2 = trading.place_order(order).await.unwrap();
        assert_eq!(nonce_fetches.load(Ordering::Acquire), 2);
        assert_eq!(trading.maker_lane.nonce.load(Ordering::Acquire), 89);
        tracker.mark_failed(ok2.client_order_index);
        server.abort();
    }

    #[tokio::test]
    async fn multi_key_lanes_isolate_taker_speed_bump_and_auto_heal_cancel_nonce() {
        use std::sync::atomic::AtomicUsize;
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let cancel_attempts = Arc::new(AtomicUsize::new(0));
        let cancels = Arc::clone(&cancel_attempts);

        let server = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                let cancels = Arc::clone(&cancels);
                tokio::spawn(async move {
                    let mut socket = BufReader::new(socket);
                    let mut req_line = String::new();
                    socket.read_line(&mut req_line).await.unwrap();
                    let path = req_line.split_whitespace().nth(1).unwrap().to_owned();
                    let mut len = 0usize;
                    loop {
                        let mut hdr = String::new();
                        socket.read_line(&mut hdr).await.unwrap();
                        if hdr.trim().is_empty() {
                            break;
                        }
                        if let Some((k, v)) = hdr.split_once(':')
                            && k.eq_ignore_ascii_case("content-length")
                        {
                            len = v.trim().parse().unwrap();
                        }
                    }
                    let mut body = vec![0u8; len];
                    socket.read_exact(&mut body).await.unwrap();
                    let body_str = String::from_utf8_lossy(&body).into_owned();

                    let resp = match path.split('?').next().unwrap() {
                        "/api/v1/nextNonce" => {
                            let nonce = if path.contains("api_key_index=7") {
                                100
                            } else if path.contains("api_key_index=8") {
                                if cancels.load(Ordering::Acquire) == 0 {
                                    200
                                } else {
                                    205
                                }
                            } else if path.contains("api_key_index=9") {
                                300
                            } else {
                                panic!("unexpected nonce query: {path}");
                            };
                            serde_json::json!({"nonce": nonce}).to_string()
                        }
                        "/api/v1/sendTx" => {
                            let tx_line =
                                body_str.lines().find(|line| line.starts_with('{')).unwrap();
                            let tx: serde_json::Value = serde_json::from_str(tx_line).unwrap();
                            let key = tx["ApiKeyIndex"].as_i64().unwrap();
                            if key == 9 {
                                // Simulate Lighter's 200ms taker speed-bump on api_key_index=9.
                                assert_eq!(tx["Nonce"], 300);
                                assert_eq!(tx["TimeInForce"], 0);
                                tokio::time::sleep(Duration::from_millis(200)).await;
                                serde_json::json!({"code": 200, "tx_hash": "taker-300"}).to_string()
                            } else if key == 7 {
                                assert_eq!(tx["Nonce"], 100);
                                assert_eq!(tx["TimeInForce"], 2);
                                serde_json::json!({"code": 200, "tx_hash": "maker-100"}).to_string()
                            } else if key == 8 {
                                let attempt = cancels.fetch_add(1, Ordering::AcqRel);
                                if attempt == 0 {
                                    assert_eq!(tx["Nonce"], 200);
                                    serde_json::json!({"code": 21104, "message": "drifted"})
                                        .to_string()
                                } else {
                                    assert_eq!(tx["Nonce"], 205);
                                    serde_json::json!({"code": 200, "tx_hash": "cancel-205"})
                                        .to_string()
                                }
                            } else {
                                panic!("unexpected api_key_index {key}");
                            }
                        }
                        "/api/v1/sendTxBatch" => {
                            // Pure cancel batch must route to cancel_lane (api_key_index=8, nonce=206).
                            assert!(body_str.contains(r#"\"ApiKeyIndex\":8"#), "{body_str}");
                            assert!(body_str.contains(r#"\"Nonce\":206"#), "{body_str}");
                            serde_json::json!({"code": 200, "tx_hash": ["batch-cancel-206"]})
                                .to_string()
                        }
                        other => panic!("unexpected endpoint {other}"),
                    };
                    socket
                        .get_mut()
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                                 Content-Length: {}\r\nConnection: close\r\n\r\n{resp}",
                                resp.len()
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                });
            }
        });

        let mut trading_inner =
            LighterTrading::mock_multi_lane_test_client(&url, 10, 7, Some(8), Some(9));
        let tracker = Arc::new(OrderTracker::new());
        assert!(tracker.confirm_cleanup(tracker.reconciliation_epoch(), -0.05));
        trading_inner.set_order_tracker(tracker);
        let trading = Arc::new(trading_inner);

        let start = Instant::now();
        let t_taker = {
            let trading = Arc::clone(&trading);
            tokio::spawn(async move {
                trading
                    .place_order(OrderParams {
                        size: 0.01,
                        price: 2010.0,
                        side: Side::Buy,
                        order_type: OrderType::Ioc,
                        reduce_only: true,
                    })
                    .await
                    .unwrap();
                start.elapsed()
            })
        };
        // Give taker IOC 15ms head-start so it holds taker_lane's submission_lock in-flight.
        tokio::time::sleep(Duration::from_millis(15)).await;

        let maker_start = Instant::now();
        trading
            .place_order(OrderParams {
                size: 0.01,
                price: 1990.0,
                side: Side::Buy,
                order_type: OrderType::PostOnly,
                reduce_only: false,
            })
            .await
            .unwrap();
        let maker_elapsed = maker_start.elapsed();

        let cancel_start = Instant::now();
        // First cancel attempt gets 21104 at nonce 200, auto-resyncs to 205, and succeeds!
        trading.cancel_order(777).await.unwrap();
        // Pure-cancel batch also routes to cancel_lane (nonce 206) without waiting on taker!
        trading
            .execute_batch(vec![BatchAction::Cancel(778)])
            .await
            .unwrap();
        let cancel_elapsed = cancel_start.elapsed();

        let taker_elapsed = t_taker.await.unwrap();
        assert!(
            taker_elapsed >= Duration::from_millis(190),
            "taker={taker_elapsed:?}"
        );
        assert!(
            maker_elapsed < Duration::from_millis(120),
            "maker blocked by taker speed-bump: {maker_elapsed:?}"
        );
        assert!(
            cancel_elapsed < Duration::from_millis(120),
            "cancel blocked by taker speed-bump: {cancel_elapsed:?}"
        );
        assert_eq!(cancel_attempts.load(Ordering::Acquire), 2);
        server.abort();
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
        assert!(encode_order(i32::MAX as f64 + 1.0, 1.0, 1.0, 1e6).is_err());
        let signer = crate::lighter_ffi::LighterSigner::mock_for_test(0, 1);
        assert!(
            signer
                .sign_create_order(
                    0i64,
                    1,
                    100,
                    i32::MAX as u32 + 1,
                    false,
                    0,
                    0,
                    false,
                    0,
                    0,
                    1
                )
                .is_err()
        );
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

    #[tokio::test]
    async fn execution_boundary_position_cap_blocks_excess_and_allows_flattening() {
        let tracker = Arc::new(OrderTracker::new());
        let mut trading = LighterTrading::offline_test_client("http://127.0.0.1:1");
        trading.set_order_tracker(Arc::clone(&tracker));
        trading.set_max_worst_case_position(0.20);

        // Simulate being long +0.21 (1 step above cap): new buys must be blocked
        // and rolled back before touching the network, while a flattening sell of
        // 0.05 (leaving worst_case_short = +0.16) passes the directional cap check.
        assert!(tracker.confirm_cleanup(tracker.reconciliation_epoch(), 0.21));

        let blocked_buy = vec![PlaceResult {
            client_order_index: 501,
            side: Side::Buy,
            price: 2000.0,
            size: 0.02,
        }];
        let err = trading
            .submit_prepared(
                vec![(14, "{}".into())],
                blocked_buy,
                0,
                true,
                |_, _| async { Ok(()) },
                |_| async { panic!("wire send must not be called when cap breached") },
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("Execution-boundary long cap breached")
        );
        assert!(!tracker.needs_reconciliation());
        assert!((tracker.worst_case_long() - 0.21).abs() < 1e-9);

        let allowed_flatten_sell = vec![PlaceResult {
            client_order_index: 502,
            side: Side::Sell,
            price: 2001.0,
            size: 0.05,
        }];
        let res = trading
            .submit_prepared(
                vec![(14, "{}".into())],
                allowed_flatten_sell,
                0,
                true,
                |_, _| async { Ok(()) },
                |_| async {
                    Ok(SendTxBatchResponse {
                        tx_hash: Some(vec!["0xok".into()]),
                    })
                },
            )
            .await
            .unwrap();
        assert_eq!(res.place_results.len(), 1);

        // Dynamic USD-notional cap updates through `&dyn Exchange` tighten the
        // base-unit ceiling without requiring mutable ownership.
        let shared = Arc::new(trading);
        let as_exchange: Arc<dyn Exchange> = Arc::clone(&shared) as Arc<dyn Exchange>;
        as_exchange.set_max_worst_case_position(0.10);
        // Confirming +0.10 flat and trying to buy +0.02 now breaches the tightened 0.10 cap.
        tracker
            .apply_event(&crate::types::ShmPrivateEventV2::order_canceled(
                0, 2, 1, 10, 502, 10, 0.05, 0,
            ))
            .unwrap();
        assert!(tracker.confirm_cleanup(tracker.reconciliation_epoch(), 0.10));
        let tightened_err = shared
            .submit_prepared(
                vec![(14, "{}".into())],
                vec![PlaceResult {
                    client_order_index: 503,
                    side: Side::Buy,
                    price: 2000.0,
                    size: 0.02,
                }],
                0,
                true,
                |_, _| async { Ok(()) },
                |_| async { panic!("wire send must not be called after dynamic cap tighten") },
            )
            .await
            .unwrap_err();
        assert!(
            tightened_err
                .to_string()
                .contains("Execution-boundary long cap breached")
        );

        // Execution-boundary reduce-only checks reject wrong side, flat state, and oversizing.
        assert!(
            shared
                .ensure_can_place(
                    &OrderParams {
                        side: Side::Buy,
                        price: 2000.0,
                        size: 0.05,
                        order_type: OrderType::Ioc,
                        reduce_only: true,
                    },
                    None
                )
                .is_err()
        );
        assert!(
            shared
                .ensure_can_place(
                    &OrderParams {
                        side: Side::Sell,
                        price: 2000.0,
                        size: 0.25,
                        order_type: OrderType::Ioc,
                        reduce_only: true,
                    },
                    None
                )
                .is_err()
        );
        assert!(
            shared
                .ensure_can_place(
                    &OrderParams {
                        side: Side::Sell,
                        price: 2000.0,
                        size: 0.10,
                        order_type: OrderType::Ioc,
                        reduce_only: true,
                    },
                    None
                )
                .is_ok()
        );
    }

    #[tokio::test]
    async fn unverified_reduce_only_rejects_pre_nonce_and_pre_sign_without_http_requests() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let http_requests = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&http_requests);
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let seen = Arc::clone(&seen);
                tokio::spawn(async move {
                    seen.fetch_add(1, Ordering::AcqRel);
                    let mut buf = [0u8; 4096];
                    let n = tokio::io::AsyncReadExt::read(&mut socket, &mut buf)
                        .await
                        .unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let body = if req.contains("sendTxBatch") {
                        serde_json::json!({"code": 200, "tx_hash": ["ok-proof"]}).to_string()
                    } else {
                        serde_json::json!({"code": 200, "nonce": 77, "tx_hash": "ok-proof"})
                            .to_string()
                    };
                    let _ = socket
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                });
            }
        });

        let sell_close = OrderParams {
            side: Side::Sell,
            price: 2000.0,
            size: 0.05,
            order_type: OrderType::Ioc,
            reduce_only: true,
        };

        // 1. Missing OrderTracker fails closed pre-nonce/pre-sign on both single and batch paths.
        let mut trading = LighterTrading::mock_multi_lane_test_client(&url, 10, 7, None, None);
        assert!(trading.place_order(sell_close.clone()).await.is_err());
        assert!(
            trading
                .execute_batch(vec![BatchAction::Place(sell_close.clone())])
                .await
                .is_err()
        );
        assert_eq!(http_requests.load(Ordering::Acquire), 0);

        // 2. Stale / reconciling OrderTracker also fails closed pre-nonce for unverified callers.
        let tracker = Arc::new(OrderTracker::new());
        assert!(tracker.confirm_cleanup(tracker.reconciliation_epoch(), 0.10));
        tracker.require_reconciliation();
        trading.set_order_tracker(Arc::clone(&tracker));
        assert!(trading.place_order(sell_close.clone()).await.is_err());
        assert!(
            trading
                .execute_batch(vec![BatchAction::Place(sell_close.clone())])
                .await
                .is_err()
        );
        assert_eq!(http_requests.load(Ordering::Acquire), 0);

        // 3. Cumulative reduce-only orders in a single batch exceeding confirmed position reject pre-nonce.
        assert!(tracker.confirm_cleanup(tracker.reconciliation_epoch(), 0.08));
        assert!(
            trading
                .execute_batch(vec![
                    BatchAction::Place(sell_close.clone()),
                    BatchAction::Place(sell_close.clone()),
                ])
                .await
                .is_err()
        );
        assert_eq!(http_requests.load(Ordering::Acquire), 0);

        // 4. Verified REST proof rejects wrong size pre-sign, and permits emergency recovery close.
        assert!(
            trading
                .place_order_with_proof(
                    OrderParams {
                        size: 0.20,
                        ..sell_close.clone()
                    },
                    Some(0.05)
                )
                .await
                .is_err()
        );
        assert_eq!(http_requests.load(Ordering::Acquire), 0);

        tracker.require_reconciliation();
        trading
            .place_order_with_proof(sell_close.clone(), Some(0.05))
            .await
            .unwrap();
        assert!(http_requests.load(Ordering::Acquire) >= 1);

        // 5. Existing open/pending-cancel exposure on the same side counts toward the reduce-only ceiling.
        let pending_tracker = Arc::new(OrderTracker::new());
        assert!(pending_tracker.confirm_cleanup(pending_tracker.reconciliation_epoch(), 0.10));
        assert!(pending_tracker.start_tracking_order(701, TrackerSide::Sell, 2001.0, 0.06, true));
        pending_tracker.mark_pending_cancel(701);
        trading.set_order_tracker(Arc::clone(&pending_tracker));
        let reqs_before_pending = http_requests.load(Ordering::Acquire);
        assert!(
            trading
                .execute_batch(vec![BatchAction::Place(OrderParams {
                    size: 0.06,
                    ..sell_close.clone()
                })])
                .await
                .is_err()
        );
        assert_eq!(http_requests.load(Ordering::Acquire), reqs_before_pending);
        trading
            .execute_batch(vec![BatchAction::Place(OrderParams {
                size: 0.04,
                ..sell_close
            })])
            .await
            .unwrap();
        assert!(http_requests.load(Ordering::Acquire) > reqs_before_pending);

        server.abort();
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn native_signer_preserves_mainnet_robinhood_and_testnet_market_ids_offline() {
        for (chain, account, key_idx, markets) in [
            (304, 1001, 7, [0, 1]),
            (466324, 1003, 8, [0, 104]),
            (300, 1002, 9, [4095, 4096]),
        ] {
            let signer = crate::lighter_ffi::LighterSigner::new(
                "http://127.0.0.1:1",
                &"01".repeat(40),
                chain,
                key_idx,
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
                assert_eq!(tx["ApiKeyIndex"], key_idx);
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

    #[tokio::test]
    async fn volume_quota_and_batch_price_band_guard_enforce_pre_nonce_safety() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let http_requests = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&http_requests);
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let seen = Arc::clone(&seen);
                tokio::spawn(async move {
                    let count = seen.fetch_add(1, Ordering::AcqRel) + 1;
                    let mut buf = [0u8; 4096];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let body = if req.contains("/api/v1/nextNonce") {
                        serde_json::json!({"code": 200, "nonce": 77 + count}).to_string()
                    } else if count <= 2 {
                        serde_json::json!({
                            "code": 200,
                            "tx_hash": ["0x1", "0x2"],
                            "volume_quota_remaining": 42
                        })
                        .to_string()
                    } else {
                        // Response without `volume_quota_remaining` must retain last known `Some(42)`.
                        serde_json::json!({"code": 200, "tx_hash": "0x3"}).to_string()
                    };
                    let _ = socket
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                });
            }
        });

        let mut trading = LighterTrading::mock_multi_lane_test_client(&url, 1, 10, None, None);
        let tracker = Arc::new(OrderTracker::new());
        assert!(tracker.confirm_cleanup(tracker.reconciliation_epoch(), 0.05));
        trading.set_order_tracker(Arc::clone(&tracker));

        assert_eq!(trading.volume_quota_snapshot().unwrap().remaining, None);
        trading.set_reference_price_band(2000.0, 50.0); // +/- 50 bps = [1990.0, 2010.0]

        // Pre-nonce rejection on both `execute_batch` and `place_batch` with zero HTTP traffic.
        let bad_ask = OrderParams {
            side: Side::Sell,
            price: 2050.0, // +250 bps > 50 bps
            size: 0.02,
            order_type: OrderType::PostOnly,
            reduce_only: false,
        };
        assert!(
            trading
                .execute_batch(vec![BatchAction::Place(bad_ask)])
                .await
                .is_err()
        );
        assert!(
            trading
                .place_batch(BatchOrderParams {
                    bid_price: 1998.0,
                    ask_price: 2050.0,
                    bid_size: 0.02,
                    ask_size: 0.02,
                })
                .await
                .is_err()
        );
        assert_eq!(http_requests.load(Ordering::Acquire), 0);

        // Valid batch within band hits mock server, parses `volume_quota_remaining = 42`.
        trading
            .place_batch(BatchOrderParams {
                bid_price: 1998.0,
                ask_price: 2002.0,
                bid_size: 0.02,
                ask_size: 0.02,
            })
            .await
            .unwrap();
        let snap1 = trading.volume_quota_snapshot().unwrap();
        assert_eq!(snap1.remaining, Some(42));
        assert!(snap1.observed_at_ns > 0);

        // Emergency `reduce_only` flatten bypasses opening price band and preserves quota `42`
        // when subsequent response omits quota.
        trading
            .place_order(OrderParams {
                side: Side::Sell,
                price: 1970.0, // -150 bps from ref_mid, emergency close of confirmed +0.05
                size: 0.05,
                order_type: OrderType::Ioc,
                reduce_only: true,
            })
            .await
            .unwrap();
        assert_eq!(trading.volume_quota_snapshot().unwrap().remaining, Some(42));

        server.abort();
    }

    #[test]
    fn lighter_credentials_errors_never_leak_private_key() {
        let non_hex_sentinel =
            "zz-secret-sentinel-key-do-not-leak-0123456789abcdef0123456789abcdef0123456789";
        let short_sentinel = "secret-sentinel-too-short";
        let valid_hex = "01".repeat(40);

        for (priv_key, idx, chain_id) in [
            (non_hex_sentinel, "7", "304"),
            (short_sentinel, "7", "304"),
            (valid_hex.as_str(), "255", "304"),
            (valid_hex.as_str(), "157", "466324"),
        ] {
            let err = LighterCredentials::from_env(|key| match key {
                "LIGHTER_PRIVATE_KEY" => Some(priv_key.to_owned()),
                "LIGHTER_ACCOUNT_INDEX" => Some("42".into()),
                "LIGHTER_API_KEY_INDEX" => Some(idx.to_owned()),
                "LIGHTER_CHAIN_ID" => Some(chain_id.to_owned()),
                _ => None,
            })
            .err()
            .expect("invalid credentials must fail");
            let msg = format!("{err:#}");
            assert!(!msg.contains(priv_key), "error leaked private key: {msg}");
        }

        // Optional lane mismatch/invalid format must not leak secret material either.
        let lane_err = LighterCredentials::from_env(|key| match key {
            "LIGHTER_PRIVATE_KEY" => Some(valid_hex.clone()),
            "LIGHTER_ACCOUNT_INDEX" => Some("42".into()),
            "LIGHTER_API_KEY_INDEX" => Some("7".into()),
            "LIGHTER_CANCEL_PRIVATE_KEY" => Some(non_hex_sentinel.to_owned()),
            "LIGHTER_CANCEL_API_KEY_INDEX" => Some("8".into()),
            _ => None,
        })
        .err()
        .expect("invalid cancel key must fail");
        assert!(!format!("{lane_err:#}").contains(non_hex_sentinel));
    }

    #[tokio::test]
    async fn auth_token_cache_reuses_token_and_clears_on_401_or_invalidation() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let seen_auth: Arc<SyncMutex<Option<String>>> = Arc::new(SyncMutex::new(None));
        let seen_auth_srv = Arc::clone(&seen_auth);
        let server = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let seen_auth_srv = Arc::clone(&seen_auth_srv);
                tokio::spawn(async move {
                    let (reader, mut writer) = socket.into_split();
                    let mut lines = BufReader::new(reader);
                    let mut line = String::new();
                    while let Ok(n) = lines.read_line(&mut line).await {
                        if n == 0 || line == "\r\n" || line == "\n" {
                            break;
                        }
                        if let Some(val) = line
                            .strip_prefix("authorization:")
                            .or_else(|| line.strip_prefix("Authorization:"))
                        {
                            *seen_auth_srv.lock() = Some(val.trim().to_owned());
                        }
                        line.clear();
                    }
                    let body = "unauthorized";
                    let _ = writer
                        .write_all(
                            format!(
                                "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                });
            }
        });

        let trading = LighterTrading::mock_multi_lane_test_client(&url, 1, 10, None, None);
        assert!(trading.auth_token_cache.lock().entry.is_none());

        let t1 = trading.create_auth_token().await.unwrap();
        assert_eq!(t1.as_str().matches(":1:10:mock").count(), 1);
        let first_refresh = trading
            .auth_token_cache
            .lock()
            .entry
            .as_ref()
            .expect("cache populated")
            .refresh_after;

        let t2 = trading.create_auth_token().await.unwrap();
        assert_eq!(t2.as_str(), t1.as_str());
        assert_eq!(
            trading
                .auth_token_cache
                .lock()
                .entry
                .as_ref()
                .unwrap()
                .refresh_after,
            first_refresh
        );

        trading.invalidate_auth_token_cache();
        assert!(trading.auth_token_cache.lock().entry.is_none());
        assert_eq!(trading.auth_token_cache.lock().generation, 1);

        let t3 = trading.create_auth_token().await.unwrap();
        assert!(trading.auth_token_cache.lock().entry.is_some());

        let err = trading.get_active_orders().await.unwrap_err();
        assert!(err.to_string().contains("401"), "{err}");
        assert_eq!(seen_auth.lock().as_deref(), Some(t3.as_str()));
        assert!(trading.auth_token_cache.lock().entry.is_none());
        assert_eq!(trading.auth_token_cache.lock().generation, 2);

        server.abort();
    }

    #[tokio::test]
    async fn fresh_auth_token_requires_a_valid_deadline_after_minting() {
        let trading =
            LighterTrading::mock_multi_lane_test_client("http://127.0.0.1:9", 1, 10, None, None);
        let start = Instant::now();
        let unix_start = 10_000;

        for (elapsed, wall_elapsed, returns_token, caches_token) in [
            (539, Some(539), true, true),
            (540, Some(540), true, false),
            (599, Some(599), true, false),
            (600, Some(600), false, false),
            (601, Some(601), false, false),
            (100, Some(600), false, false),
            (600, Some(500), false, false),
            (0, None, false, false),
        ] {
            trading.invalidate_auth_token_cache();
            let after_wall = wall_elapsed
                .map(|secs| unix_start + secs)
                .ok_or_else(|| anyhow::anyhow!("synthetic clock unavailable"));
            let mut clocks = [
                (start, Ok(unix_start)),
                (start + Duration::from_secs(elapsed), after_wall),
            ]
            .into_iter();
            let result = trading
                .create_auth_token_with_clocks(|| clocks.next().expect("two clock samples"))
                .await;

            assert_eq!(result.is_ok(), returns_token, "{elapsed}, {wall_elapsed:?}");
            assert_eq!(
                trading.auth_token_cache.lock().entry.is_some(),
                caches_token,
                "{elapsed}, {wall_elapsed:?}"
            );
        }
    }

    #[tokio::test]
    async fn auth_token_cache_enforces_dual_clocks_mint_start_and_generation_guard() {
        let trading =
            LighterTrading::mock_multi_lane_test_client("http://127.0.0.1:9", 1, 10, None, None);
        let base_mono = Instant::now();
        let start_wall = 1_000_i64;

        // Mint at (mono=0, wall=1000) -> signed deadline = 1600, refresh_after = +540s.
        let t1 = trading
            .create_auth_token_with_clocks(|| (base_mono, Ok(start_wall)))
            .await
            .unwrap();
        assert_eq!(t1.as_str(), "1600:1:10:mock");

        // At +539s mono and wall=1539 (< 1600 - 60): cached token is reused.
        let hit = trading
            .create_auth_token_with_clocks(|| {
                (base_mono + Duration::from_secs(539), Ok(start_wall + 539))
            })
            .await
            .unwrap();
        assert_eq!(hit.as_str(), t1.as_str());

        // Forward UTC clock correction at +530s mono (wall=1605 > 1540) must NOT reuse expired deadline.
        let wall_refreshed = trading
            .create_auth_token_with_clocks(|| {
                (base_mono + Duration::from_secs(530), Ok(start_wall + 605))
            })
            .await
            .unwrap();
        assert_eq!(wall_refreshed.as_str(), "2205:1:10:mock");

        // Backward wall-clock jump at +1070s mono (540s since second mint at +530s) still expires on monotonic bound.
        let mono_refreshed = trading
            .create_auth_token_with_clocks(|| {
                (base_mono + Duration::from_secs(1070), Ok(start_wall + 100))
            })
            .await
            .unwrap();
        assert_eq!(mono_refreshed.as_str(), "1700:1:10:mock");

        // Slow mint/pause (90s between mint start and store): refresh_after stays anchored at mint start, not store time.
        trading.invalidate_auth_token_cache();
        let mut step = 0usize;
        let slow_mint = trading
            .create_auth_token_with_clocks(|| {
                let tick = step;
                step += 1;
                if tick == 0 {
                    (base_mono + Duration::from_secs(2_000), Ok(2_000))
                } else {
                    (base_mono + Duration::from_secs(2_090), Ok(2_090))
                }
            })
            .await
            .unwrap();
        assert_eq!(slow_mint.as_str(), "2600:1:10:mock");
        // Still valid at +539s from mint start (2539s), rejected at +540s from mint start (2540s).
        let slow_hit = trading
            .create_auth_token_with_clocks(|| (base_mono + Duration::from_secs(2_539), Ok(2_539)))
            .await
            .unwrap();
        assert_eq!(slow_hit.as_str(), slow_mint.as_str());
        let slow_expired = trading
            .create_auth_token_with_clocks(|| (base_mono + Duration::from_secs(2_540), Ok(2_539)))
            .await
            .unwrap();
        assert_eq!(slow_expired.as_str(), "3139:1:10:mock");

        // Stale mint stalled >= 540s returns the token once without caching it.
        trading.invalidate_auth_token_cache();
        let mut stall_step = 0usize;
        let _ = trading
            .create_auth_token_with_clocks(|| {
                let tick = stall_step;
                stall_step += 1;
                if tick == 0 {
                    (base_mono + Duration::from_secs(4_000), Ok(4_000))
                } else {
                    (base_mono + Duration::from_secs(4_540), Ok(4_540))
                }
            })
            .await
            .unwrap();
        assert!(trading.auth_token_cache.lock().entry.is_none());

        // Concurrent invalidation between mint start and commit blocks stale cache repopulation.
        let gen_before = trading.auth_token_cache.lock().generation;
        let mut race_step = 0usize;
        let _ = trading
            .create_auth_token_with_clocks(|| {
                let tick = race_step;
                race_step += 1;
                if tick == 1 {
                    trading.invalidate_auth_token_cache();
                }
                (base_mono + Duration::from_secs(5_000), Ok(5_000))
            })
            .await
            .unwrap();
        assert_eq!(trading.auth_token_cache.lock().generation, gen_before + 1);
        assert!(trading.auth_token_cache.lock().entry.is_none());
    }
}

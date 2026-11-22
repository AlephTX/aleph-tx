use super::model::*;
use super::signing::{HyperliquidSigner, L1Signature};
use anyhow::{Context, Result, ensure};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct HyperliquidInfo {
    client: Client,
    network: Network,
    base_url: String,
}

impl HyperliquidInfo {
    pub fn new(network: Network) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .user_agent("AlephTX/Hyperliquid")
                .build()?,
            network,
            base_url: network.base_url().to_owned(),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_test_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    pub(crate) async fn info<T: DeserializeOwned>(&self, query: serde_json::Value) -> Result<T> {
        Ok(self
            .client
            .post(format!("{}/info", self.base_url))
            .json(&query)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    pub async fn perpetual_dexs(&self) -> Result<Vec<Option<PerpDex>>> {
        self.info(json!({"type": "perpDexs"})).await
    }

    /// Native core or a named HIP-3 deployer (e.g. Entropy's `io`).
    pub async fn perpetual_markets(&self, dex: Option<&str>) -> Result<Vec<PerpMarket>> {
        let dex = dex.unwrap_or("");
        let offset = if dex.is_empty() {
            0
        } else {
            resolve_perp_offset(dex, &self.perpetual_dexs().await?)?
        };
        let (meta, contexts): (PerpMeta, Vec<AssetContext>) = self
            .info(json!({"type": "metaAndAssetCtxs", "dex": dex}))
            .await?;
        parse_markets_for_dex(meta, contexts, dex, offset)
    }

    pub async fn l2_book(&self, coin: &str) -> Result<ValidatedBook> {
        ensure!(!coin.is_empty(), "Coin must not be empty");
        let book: L2Book = self.info(json!({"type": "l2Book", "coin": coin})).await?;
        ensure!(
            book.coin == coin,
            "Returned book belongs to a different coin"
        );
        book.validated_book()
    }

    pub async fn bbo(&self, coin: &str) -> Result<BestBidAsk> {
        Ok(self.l2_book(coin).await?.bbo)
    }

    /// Lightweight read to keep the HTTP/TLS connection pool warm across sparse opportunities.
    pub async fn warm_http(&self) -> Result<()> {
        let _status: serde_json::Value = self.info(json!({"type": "exchangeStatus"})).await?;
        Ok(())
    }

    /// Master/subaccount address, never substitute an approved agent's address.
    pub async fn account_state(&self, account_address: &str) -> Result<AccountState> {
        self.account_state_for_dex(account_address, "").await
    }

    pub async fn validate_agent(&self, signer: &HyperliquidSigner, account: &str) -> Result<()> {
        validate_hex(account, 20)?;
        let role: serde_json::Value = self
            .info(json!({"type": "userRole", "user": signer.address()}))
            .await?;
        ensure!(
            role["role"] == "agent"
                && role["data"]["user"]
                    .as_str()
                    .is_some_and(|user| user.eq_ignore_ascii_case(account)),
            "Hyperliquid signing key is not an approved agent for the configured account"
        );
        Ok(())
    }

    pub async fn account_state_for_dex(
        &self,
        account_address: &str,
        dex: &str,
    ) -> Result<AccountState> {
        validate_hex(account_address, 20)?;
        self.info(json!({"type": "clearinghouseState", "user": account_address, "dex": dex}))
            .await
    }

    pub async fn open_orders(&self, account_address: &str) -> Result<Vec<OpenOrder>> {
        self.open_orders_for_dex(account_address, "").await
    }

    pub async fn open_orders_for_dex(
        &self,
        account_address: &str,
        dex: &str,
    ) -> Result<Vec<OpenOrder>> {
        validate_hex(account_address, 20)?;
        self.info(json!({"type": "openOrders", "user": account_address, "dex": dex}))
            .await
    }

    /// Stable client IDs allow lookup after a response timeout without resubmission.
    pub async fn order_by_client_id(
        &self,
        account_address: &str,
        client_order_id: &str,
    ) -> Result<OrderLookup> {
        validate_hex(account_address, 20)?;
        validate_hex(client_order_id, 16)?;
        let response: serde_json::Value = self
            .info(json!({"type": "orderStatus", "user": account_address, "oid": client_order_id}))
            .await?;
        OrderLookup::from_response(response)
    }
}

impl OrderLookup {
    /// Interpret a raw `orderStatus` info response. Shared by the live client
    /// and offline evidence files so both read the same wire format.
    pub fn from_response(response: serde_json::Value) -> Result<Self> {
        let response: OrderLookupResponse = serde_json::from_value(response)?;
        match response.status.as_str() {
            "unknownOid" => Ok(Self::Unknown),
            "order" => Ok(Self::Found(
                response.order.context("Missing order lookup data")?,
            )),
            _ => anyhow::bail!("Unrecognized order lookup response"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountState {
    pub margin_summary: MarginSummary,
    pub asset_positions: Vec<AssetPosition>,
    pub withdrawable: String,
    pub time: u64,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarginSummary {
    pub account_value: String,
    pub total_ntl_pos: String,
    pub total_margin_used: String,
}
#[derive(Debug, Clone, Deserialize)]
pub struct AssetPosition {
    pub position: Position,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub coin: String,
    pub szi: String,
    pub entry_px: Option<String>,
    pub unrealized_pnl: String,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenOrder {
    pub coin: String,
    pub side: String,
    pub limit_px: String,
    pub sz: String,
    pub oid: u64,
    pub timestamp: u64,
    #[serde(default)]
    pub cloid: Option<String>,
    #[serde(default, rename = "origSz")]
    pub original_size: Option<String>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocatedOrder {
    pub order: OpenOrder,
    pub status: String,
    pub status_timestamp: u64,
}
#[derive(Debug, Clone)]
pub enum OrderLookup {
    Unknown,
    Found(LocatedOrder),
}
#[derive(Deserialize)]
struct OrderLookupResponse {
    status: String,
    order: Option<LocatedOrder>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SignedRequest<'a, T> {
    action: &'a T,
    nonce: u64,
    signature: L1Signature,
    #[serde(skip_serializing_if = "Option::is_none")]
    vault_address: Option<&'a str>,
    expires_after: u64,
}

/// Shared monotonic nonce and submission serializer for a single agent signing
/// key used across multiple Hyperliquid DEXes (e.g. native core, `io`, `xyz`).
#[derive(Clone, Default)]
pub struct SharedNoncePool {
    nonce: Arc<AtomicU64>,
    submission: Arc<Mutex<()>>,
}

impl SharedNoncePool {
    pub fn next_nonce_now(&self, now_ms: u64) -> Result<u64> {
        next_nonce(&self.nonce, now_ms)
    }
}

/// Deduplicates Core collateral when aggregating isolated HIP-3 DEX states.
pub fn aggregate_multi_dex_equity(
    core_state: &AccountState,
    dex_states: &[(&str, &AccountState)],
) -> Result<Decimal> {
    let mut total: Decimal = core_state.margin_summary.account_value.parse()?;
    let mut seen = HashSet::new();
    for (dex, state) in dex_states {
        let key = dex.trim().to_ascii_lowercase();
        if key.is_empty() || key == "core" || !seen.insert(key) {
            continue;
        }
        total += state.margin_summary.account_value.parse::<Decimal>()?;
    }
    Ok(total)
}

/// A client instance owns or shares one approved API key's nonce sequence. Do
/// not share the same key with external processes; Hyperliquid tracks nonce
/// usage by signer. Requests are never retried automatically, including
/// transport failures.
pub struct HyperliquidTrading {
    info: HyperliquidInfo,
    signer: ActionSigner,
    vault_address: Option<String>,
    nonce_pool: SharedNoncePool,
    expires_after_rejects: Arc<AtomicU64>,
}

/// A serialized, locally signed request. The nonce lane remains reserved until
/// the venue responds or the caller discards this unsubmitted request.
pub struct PreparedSubmission<'a> {
    client: &'a HyperliquidTrading,
    request: reqwest::Request,
    response_type: &'static str,
    expected: usize,
    _lane: tokio::sync::MutexGuard<'a, ()>,
}

/// Production always signs; the unsigned variant exists only in test builds.
enum ActionSigner {
    Key(HyperliquidSigner),
    #[cfg(test)]
    UnsignedTest,
}

impl HyperliquidTrading {
    pub fn new(
        info: HyperliquidInfo,
        signer: HyperliquidSigner,
        vault_address: Option<String>,
    ) -> Result<Self> {
        Self::with_shared_nonce(info, signer, vault_address, SharedNoncePool::default())
    }

    pub fn with_shared_nonce(
        info: HyperliquidInfo,
        signer: HyperliquidSigner,
        vault_address: Option<String>,
        nonce_pool: SharedNoncePool,
    ) -> Result<Self> {
        if let Some(address) = &vault_address {
            validate_hex(address, 20)?;
        }
        Ok(Self {
            info,
            signer: ActionSigner::Key(signer),
            vault_address,
            nonce_pool,
            expires_after_rejects: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn expires_after_reject_count(&self) -> u64 {
        self.expires_after_rejects.load(Ordering::Relaxed)
    }

    /// Test client that skips only the L1 signature (a fixed placeholder is
    /// sent); validation, nonce, journaling callers and response parsing run
    /// as in production. Never constructible outside tests.
    #[cfg(test)]
    pub(crate) fn unsigned_for_test(info: HyperliquidInfo) -> Self {
        Self {
            info,
            signer: ActionSigner::UnsignedTest,
            vault_address: None,
            nonce_pool: SharedNoncePool::default(),
            expires_after_rejects: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Caller must durably reserve exposure and persist IDs before awaiting this.
    /// A partial batch is represented by one status for each input order.
    pub async fn submit_orders(&self, action: &OrderAction) -> Result<Vec<ExecutionStatus>> {
        self.submit_orders_with_expiry(action, None).await
    }

    /// Submit orders with an explicit caller-journaled `expiresAfter` deadline (ms).
    pub async fn submit_orders_with_expiry(
        &self,
        action: &OrderAction,
        explicit_expires_after: Option<u64>,
    ) -> Result<Vec<ExecutionStatus>> {
        self.prepare_orders(action, explicit_expires_after)
            .await?
            .send()
            .await
    }

    pub async fn prepare_orders(
        &self,
        action: &OrderAction,
        explicit_expires_after: Option<u64>,
    ) -> Result<PreparedSubmission<'_>> {
        ensure!(
            !action.orders.is_empty() && action.orders.len() <= 100,
            "Order batch must contain 1..=100 orders"
        );
        ensure!(
            action.orders.iter().all(|order| order.c.is_some()),
            "Live submission requires a persistent client order ID for every order"
        );
        let ids: std::collections::HashSet<_> = action
            .orders
            .iter()
            .filter_map(|order| order.c.as_deref())
            .collect();
        ensure!(
            ids.len() == action.orders.len(),
            "Duplicate client order ID within batch"
        );
        self.prepare(action, "order", action.orders.len(), explicit_expires_after)
            .await
    }

    pub async fn cancel_orders(&self, action: &CancelAction) -> Result<Vec<ExecutionStatus>> {
        ensure!(
            !action.cancels.is_empty() && action.cancels.len() <= 100,
            "Cancel batch must contain 1..=100 orders"
        );
        self.prepare(action, "cancel", action.cancels.len(), None)
            .await?
            .send()
            .await
    }

    async fn prepare<T: Serialize>(
        &self,
        action: &T,
        response_type: &'static str,
        expected: usize,
        explicit_expires_after: Option<u64>,
    ) -> Result<PreparedSubmission<'_>> {
        let lane = self.nonce_pool.submission.lock().await;
        let now = unix_ms()?;
        let nonce = self.nonce_pool.next_nonce_now(now)?;
        let expires_after = match explicit_expires_after {
            Some(exp) => exp,
            None => now.checked_add(5_000).context("Expiry overflow")?,
        };
        // Guard against runaway local nonce generation or clock rollback.
        ensure!(
            nonce < expires_after,
            "Nonce clock drift exceeds request lifetime"
        );
        let signature = match &self.signer {
            ActionSigner::Key(signer) => signer.sign_action(
                action,
                self.info.network,
                nonce,
                self.vault_address.as_deref(),
                Some(expires_after),
            )?,
            #[cfg(test)]
            ActionSigner::UnsignedTest => super::signing::L1Signature {
                r: "0x0".into(),
                s: "0x0".into(),
                v: 0,
            },
        };
        let request = SignedRequest {
            action,
            nonce,
            signature,
            vault_address: self.vault_address.as_deref(),
            expires_after,
        };
        let request = self
            .info
            .client
            .post(format!("{}/exchange", self.info.base_url))
            .json(&request)
            .build()?;
        Ok(PreparedSubmission {
            client: self,
            request,
            response_type,
            expected,
            _lane: lane,
        })
    }
}

impl PreparedSubmission<'_> {
    pub async fn send(self) -> Result<Vec<ExecutionStatus>> {
        let response = self
            .client
            .info
            .client
            .execute(self.request)
            .await
            .map_err(|err| SubmissionError::Uncertain(err.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| SubmissionError::Uncertain(err.to_string()))?;
        // Non-2xx proxies/server responses cannot reliably prove a transaction was rejected.
        if !status.is_success() {
            return Err(SubmissionError::Uncertain(format!("HTTP {status}")).into());
        }
        match parse_execution_response(&body, self.response_type, self.expected) {
            Ok(statuses) => {
                let expired = statuses.iter().any(|s| {
                    matches!(
                        s,
                        ExecutionStatus::Rejected { reason }
                            if reason.to_ascii_lowercase().contains("expire")
                    )
                });
                if expired {
                    self.client
                        .expires_after_rejects
                        .fetch_add(1, Ordering::Relaxed);
                }
                Ok(statuses)
            }
            Err(err) => {
                if let SubmissionError::Rejected(reason) = &err
                    && reason.to_ascii_lowercase().contains("expire")
                {
                    self.client
                        .expires_after_rejects
                        .fetch_add(1, Ordering::Relaxed);
                }
                Err(err.into())
            }
        }
    }
}

fn unix_ms() -> Result<u64> {
    Ok(u64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
    )?)
}

pub(super) fn next_nonce(counter: &AtomicU64, now: u64) -> Result<u64> {
    loop {
        let previous = counter.load(Ordering::Acquire);
        let nonce = now.max(previous.checked_add(1).context("Nonce exhausted")?);
        if counter
            .compare_exchange_weak(previous, nonce, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Ok(nonce);
        }
    }
}

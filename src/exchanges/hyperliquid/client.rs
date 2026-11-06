use super::model::*;
use super::signing::{HyperliquidSigner, L1Signature};
use anyhow::{Context, Result, ensure};
use reqwest::Client;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct HyperliquidInfo {
    client: Client,
    network: Network,
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
        })
    }

    async fn info<T: DeserializeOwned>(&self, query: serde_json::Value) -> Result<T> {
        Ok(self
            .client
            .post(format!("{}/info", self.network.base_url()))
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

    pub async fn bbo(&self, coin: &str) -> Result<BestBidAsk> {
        ensure!(!coin.is_empty(), "Coin must not be empty");
        let book: L2Book = self.info(json!({"type": "l2Book", "coin": coin})).await?;
        ensure!(
            book.coin == coin,
            "Returned book belongs to a different coin"
        );
        book.best_bid_ask()
    }

    /// Master/subaccount address, never substitute an approved agent's address.
    pub async fn account_state(&self, account_address: &str) -> Result<AccountState> {
        self.account_state_for_dex(account_address, "").await
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
        let response: OrderLookupResponse = self
            .info(json!({"type": "orderStatus", "user": account_address, "oid": client_order_id}))
            .await?;
        match response.status.as_str() {
            "unknownOid" => Ok(OrderLookup::Unknown),
            "order" => Ok(OrderLookup::Found(
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

/// A client instance owns one approved API key's nonce sequence. Do not share the
/// same key with other processes; Hyperliquid tracks nonce usage by signer.
/// Requests are never retried automatically, including transport failures.
pub struct HyperliquidTrading {
    info: HyperliquidInfo,
    signer: HyperliquidSigner,
    vault_address: Option<String>,
    nonce: AtomicU64,
    submission: Mutex<()>,
}

impl HyperliquidTrading {
    pub fn new(
        info: HyperliquidInfo,
        signer: HyperliquidSigner,
        vault_address: Option<String>,
    ) -> Result<Self> {
        if let Some(address) = &vault_address {
            validate_hex(address, 20)?;
        }
        Ok(Self {
            info,
            signer,
            vault_address,
            nonce: AtomicU64::new(0),
            submission: Mutex::new(()),
        })
    }

    /// Caller must durably reserve exposure and persist IDs before awaiting this.
    /// A partial batch is represented by one status for each input order.
    pub async fn submit_orders(&self, action: &OrderAction) -> Result<Vec<ExecutionStatus>> {
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
        self.submit(action, "order", action.orders.len()).await
    }

    pub async fn cancel_orders(&self, action: &CancelAction) -> Result<Vec<ExecutionStatus>> {
        ensure!(
            !action.cancels.is_empty() && action.cancels.len() <= 100,
            "Cancel batch must contain 1..=100 orders"
        );
        self.submit(action, "cancel", action.cancels.len()).await
    }

    async fn submit<T: Serialize>(
        &self,
        action: &T,
        response_type: &str,
        expected: usize,
    ) -> Result<Vec<ExecutionStatus>> {
        let _submission = self.submission.lock().await;
        let now = unix_ms()?;
        let nonce = next_nonce(&self.nonce, now)?;
        let expires_after = now.checked_add(5_000).context("Expiry overflow")?;
        // Guard against runaway local nonce generation or clock rollback.
        ensure!(
            nonce < expires_after,
            "Nonce clock drift exceeds request lifetime"
        );
        let signature = self.signer.sign_action(
            action,
            self.info.network,
            nonce,
            self.vault_address.as_deref(),
            Some(expires_after),
        )?;
        let request = SignedRequest {
            action,
            nonce,
            signature,
            vault_address: self.vault_address.as_deref(),
            expires_after,
        };
        let response = self
            .info
            .client
            .post(format!("{}/exchange", self.info.network.base_url()))
            .json(&request)
            .send()
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
        Ok(parse_execution_response(&body, response_type, expected)?)
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

use super::{
    Account, ArcusInfo, ArcusSigner, CancelLease, LimitOrder, Market, Network,
    ScheduledCancelReceipt, Side, SignedRequest,
};
use anyhow::{Context, Result, ensure};
use reqwest::{RequestBuilder, StatusCode, Url};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

/// A submission receipt. An HTTP 202 confirms receipt by the gateway.
/// Read the order and fills before changing inventory based on a submission.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderReceipt {
    #[serde(skip_deserializing)]
    pub http_status: u16,
    pub status: String,
    pub order_id: Option<String>,
    pub client_id: Option<String>,
    pub rejection_reason: Option<String>,
}

impl OrderReceipt {
    fn confirm_cancel(&self) -> Result<()> {
        ensure!(
            self.http_status == 200
                && self.rejection_reason.is_none()
                && matches!(
                    self.status.to_ascii_lowercase().as_str(),
                    "success" | "cancelled" | "canceled"
                ),
            "Arcus cancel-all lacks a confirmed success receipt"
        );
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExecutionError {
    #[error("Arcus rejected {path} with HTTP {status}: {message}")]
    Rejected {
        path: &'static str,
        status: StatusCode,
        message: String,
    },
    #[error("Unknown Arcus outcome for {path}: {message}. Reconcile the order before retrying")]
    Uncertain { path: &'static str, message: String },
}

pub struct PreparedLimit(SignedRequest);

pub struct ArcusTrading {
    info: ArcusInfo,
    signer: ArcusSigner,
    last_timestamp: AtomicU64,
    #[cfg(test)]
    wall_clock_override: Option<Box<dyn Fn() -> u64 + Send + Sync>>,
}

// An async timeout/cancellation drops the future without returning an error
// through the normal path. Keep that outcome as unsafe as a transport error.
struct CancelLeaseAttempt<'a> {
    lease: &'a mut CancelLease,
    confirmed: bool,
}

impl Drop for CancelLeaseAttempt<'_> {
    fn drop(&mut self) {
        if !self.confirmed {
            self.lease.invalidate();
        }
    }
}

impl ArcusTrading {
    pub fn new(network: Network, signer: ArcusSigner) -> Result<Self> {
        Ok(Self {
            info: ArcusInfo::new(network)?,
            signer,
            last_timestamp: AtomicU64::new(0),
            #[cfg(test)]
            wall_clock_override: None,
        })
    }

    /// Warm the dedicated order-execution HTTP/TLS connection pool before first submission.
    pub async fn warm_http(&self) -> Result<()> {
        self.info.warm_http().await
    }

    pub async fn place_limit(
        &self,
        market: &Market,
        order: LimitOrder<'_>,
    ) -> Result<OrderReceipt> {
        self.submit_limit(self.prepare_limit(market, order)?).await
    }

    pub fn prepare_limit(&self, market: &Market, order: LimitOrder<'_>) -> Result<PreparedLimit> {
        Ok(PreparedLimit(self.signer.place_limit(
            market,
            order,
            self.timestamp()?,
        )?))
    }

    pub async fn submit_limit(&self, order: PreparedLimit) -> Result<OrderReceipt> {
        self.submit(order.0).await
    }

    pub async fn cancel_by_order_id(&self, market_id: u16, order_id: &str) -> Result<OrderReceipt> {
        let request = self
            .signer
            .cancel_by_order_id(market_id, order_id, self.timestamp()?)?;
        self.submit(request).await
    }

    pub async fn cancel_by_client_id(
        &self,
        market_id: u16,
        client_id: &str,
    ) -> Result<OrderReceipt> {
        let request = self
            .signer
            .cancel_by_client_id(market_id, client_id, self.timestamp()?)?;
        self.submit(request).await
    }

    pub async fn cancel_all(&self, market_id: Option<u16>) -> Result<OrderReceipt> {
        let request = self.signer.cancel_all(market_id, self.timestamp()?)?;
        self.submit(request).await
    }

    pub async fn cancel_all_confirmed(&self, market_id: Option<u16>) -> Result<()> {
        let receipt = self.cancel_all(market_id).await?;
        receipt.confirm_cancel()
    }

    /// One explicit renewal attempt. This starts no background task and never
    /// submits/disarms orders. Failure invalidates local protection; the caller
    /// must also supervise unresolved exposure and its durable recovery barrier.
    pub async fn renew_cancel_lease(&self, lease: &mut CancelLease) -> Result<()> {
        let mut attempt = CancelLeaseAttempt {
            lease,
            confirmed: false,
        };
        ensure!(
            attempt.lease.matches_account(&self.signer.account),
            "Arcus lease account mismatch"
        );
        let sent_mono = Instant::now();
        let request = attempt
            .lease
            .begin_renewal(self.wall_clock_ns()?, sent_mono)?;
        let (receipt, received_wall, received_mono) = self
            .schedule_cancel_observed(request.market_id(), Some(request.deadline_us()))
            .await?;
        attempt
            .lease
            .confirm_renewal(&receipt, received_wall, received_mono)?;
        attempt.confirmed = true;
        Ok(())
    }

    /// Arm/refresh/disarm exactly one cancellation scope. No retry or automatic
    /// disarm: uncertainty must stop new risk until protection is re-established.
    /// Callers still reconcile orders and fills before releasing exposure.
    pub async fn schedule_cancel(
        &self,
        market_id: Option<u16>,
        deadline_us: Option<u64>,
    ) -> Result<ScheduledCancelReceipt> {
        let (receipt, _, _) = self
            .schedule_cancel_observed(market_id, deadline_us)
            .await?;
        Ok(receipt)
    }

    async fn schedule_cancel_observed(
        &self,
        market_id: Option<u16>,
        deadline_us: Option<u64>,
    ) -> Result<(ScheduledCancelReceipt, u64, Instant)> {
        let request = self
            .signer
            .schedule_cancel(market_id, deadline_us, self.timestamp()?)?;
        let path = request.path;
        let uncertain = |message| ExecutionError::Uncertain { path, message };
        let response = self
            .info
            .client
            .post(format!("{}{path}", self.info.base_url))
            .query(&[("address", &self.signer.account.address)])
            .header("X-API-Key", request.api_key)
            .header("X-Timestamp", request.timestamp_ns.to_string())
            .header("X-Signature", request.signature)
            .json(&request.body)
            .send()
            .await
            .map_err(|_| uncertain("schedule transport failed".into()))?;
        let status = response.status();
        if status == StatusCode::OK {
            let receipt: ScheduledCancelReceipt = response
                .json()
                .await
                .map_err(|_| uncertain("invalid schedule response".into()))?;
            let received_mono = Instant::now();
            let received_ns = self
                .wall_clock_ns()
                .map_err(|_| uncertain("local clock unavailable".into()))?;
            receipt
                .validate(&self.signer.account, market_id, deadline_us, received_ns)
                .map_err(|_| uncertain("uncorrelated or expired schedule receipt".into()))?;
            return Ok((receipt, received_ns, received_mono));
        }
        // DMS 429 is a documented quota/switch-cap rejection. It is not a
        // successful renewal, and never cancels or extends an earlier lease.
        if matches!(
            status,
            StatusCode::BAD_REQUEST
                | StatusCode::UNAUTHORIZED
                | StatusCode::FORBIDDEN
                | StatusCode::TOO_MANY_REQUESTS
        ) {
            return Err(ExecutionError::Rejected {
                path,
                status,
                message: "cancel schedule rejected".into(),
            }
            .into());
        }
        // In particular, an order-style 202 is not a confirmed DMS schedule.
        Err(uncertain(format!("HTTP {status}")).into())
    }

    fn wall_clock_ns(&self) -> Result<u64> {
        #[cfg(test)]
        if let Some(clock) = &self.wall_clock_override {
            return Ok(clock());
        }
        Ok(u64::try_from(
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
        )?)
    }

    fn timestamp(&self) -> Result<u64> {
        // Signing timestamps must advance even when the wall clock moves.
        // Lease validation compares the raw clock with monotonic elapsed time.
        let now = self.wall_clock_ns()?;
        let next = |last: u64| last.checked_add(1).map(|value| value.max(now));
        let previous = self
            .last_timestamp
            .try_update(Ordering::Relaxed, Ordering::Relaxed, next)
            .map_err(|_| anyhow::anyhow!("Arcus timestamp exhausted"))?;
        next(previous).context("Arcus timestamp exhausted")
    }

    async fn submit(&self, request: SignedRequest) -> Result<OrderReceipt> {
        let path = request.path;
        let uncertain = |message| ExecutionError::Uncertain { path, message };
        let response = self
            .info
            .client
            .post(format!("{}{path}", self.info.base_url))
            .query(&[("address", &self.signer.account.address)])
            .header("X-API-Key", request.api_key)
            .header("X-Timestamp", request.timestamp_ns.to_string())
            .header("X-Signature", request.signature)
            .json(&request.body)
            .send()
            .await
            .map_err(|error| uncertain(error.to_string()))?;
        let status = response.status();
        if matches!(status, StatusCode::OK | StatusCode::ACCEPTED) {
            let mut receipt: OrderReceipt = response
                .json()
                .await
                .map_err(|error| uncertain(error.to_string()))?;
            receipt.http_status = status.as_u16();
            let matches_id = |field: &str, observed: Option<&str>| {
                request.body[field]
                    .as_str()
                    .is_none_or(|expected| observed == Some(expected))
            };
            if receipt.status.is_empty()
                || !matches_id("clientId", receipt.client_id.as_deref())
                || !matches_id("orderId", receipt.order_id.as_deref())
                || (path == "/v1/placeOrder"
                    && receipt.order_id.as_deref().is_none_or(str::is_empty))
            {
                return Err(uncertain("invalid or uncorrelated receipt".into()).into());
            }
            return Ok(receipt);
        }
        // Only documented gateway validation/auth failures prove this request
        // was rejected. A duplicate ID still requires reconciling the original.
        if matches!(
            status,
            StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) {
            #[derive(Deserialize)]
            struct ErrorResponse {
                error: String,
            }
            let message = response
                .json::<ErrorResponse>()
                .await
                .map(|body| body.error.chars().take(240).collect())
                .unwrap_or_else(|_| "no exchange error detail".into());
            return Err(ExecutionError::Rejected {
                path,
                status,
                message,
            }
            .into());
        }
        Err(uncertain(format!("HTTP {status}")).into())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Order {
    pub order_id: String,
    pub client_id: Option<String>,
    pub market_id: u16,
    pub side: Side,
    pub status: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub original_size: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub remaining_size: Decimal,
    pub created_at: Option<u64>,
    pub updated_at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fill {
    pub trade_id: String,
    pub order_id: String,
    pub client_id: Option<String>,
    pub market_id: u16,
    pub side: Side,
    #[serde(with = "rust_decimal::serde::str")]
    pub size: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub fee: Decimal,
    pub role: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSummary {
    pub account_index: u8,
    pub address: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub net_quote_balance: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub equity: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub free_collateral: Decimal,
    pub sequence_number: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Position {
    pub market_id: u16,
    pub market_display_name: String,
    pub side: String,
    /// Arcus reports a signed size, positive for long and negative for short.
    #[serde(with = "rust_decimal::serde::str")]
    pub size: Decimal,
}

/// Inclusive Unix-microsecond bounds for one page. An empty page is only a
/// snapshot; it does not prove that a recent submission failed.
#[derive(Debug, Default, Serialize)]
pub struct HistoryWindow {
    #[serde(rename = "from", skip_serializing_if = "Option::is_none")]
    pub from_us: Option<u64>,
    #[serde(rename = "to", skip_serializing_if = "Option::is_none")]
    pub to_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u16>,
}

impl ArcusInfo {
    pub async fn order(&self, account: &Account, order_id: &str) -> Result<Option<Order>> {
        super::validate_id(order_id)?;
        ensure!(
            order_id != "." && order_id != "..",
            "Invalid Arcus order ID"
        );
        let mut url = Url::parse(&format!("{}/v1/order/", self.base_url))?;
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("Invalid Arcus base URL"))?
            .pop_if_empty()
            .push(order_id);
        let response = self
            .account_query(self.client.get(url), account)
            .send()
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(response.error_for_status()?.json().await?))
    }

    pub async fn open_orders(
        &self,
        account: &Account,
        window: &HistoryWindow,
    ) -> Result<Vec<Order>> {
        self.orders("/v1/openOrders", account, window).await
    }

    pub async fn order_history(
        &self,
        account: &Account,
        window: &HistoryWindow,
    ) -> Result<Vec<Order>> {
        self.orders("/v1/orders", account, window).await
    }

    async fn orders(
        &self,
        path: &str,
        account: &Account,
        window: &HistoryWindow,
    ) -> Result<Vec<Order>> {
        #[derive(Deserialize)]
        struct Response {
            orders: Vec<Order>,
        }
        Ok(self
            .account_get(path, account)
            .query(window)
            .send()
            .await?
            .error_for_status()?
            .json::<Response>()
            .await?
            .orders)
    }

    pub async fn fills(&self, account: &Account, window: &HistoryWindow) -> Result<Vec<Fill>> {
        #[derive(Deserialize)]
        struct Response {
            fills: Vec<Fill>,
        }
        Ok(self
            .account_get("/v1/fills", account)
            .query(window)
            .send()
            .await?
            .error_for_status()?
            .json::<Response>()
            .await?
            .fills)
    }

    pub async fn account(&self, account: &Account) -> Result<AccountSummary> {
        let summary: AccountSummary = self
            .account_get("/v1/account", account)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            summary.account_index == account.index
                && summary.address.eq_ignore_ascii_case(&account.address),
            "Arcus account summary identity mismatch"
        );
        Ok(summary)
    }

    pub async fn positions(&self, account: &Account) -> Result<BTreeMap<u16, Position>> {
        #[derive(Deserialize)]
        struct Response {
            positions: BTreeMap<u16, Position>,
        }
        Ok(self
            .account_get("/v1/positions", account)
            .send()
            .await?
            .error_for_status()?
            .json::<Response>()
            .await?
            .positions)
    }

    fn account_get(&self, path: &str, account: &Account) -> RequestBuilder {
        self.account_query(self.client.get(format!("{}{path}", self.base_url)), account)
    }

    fn account_query(&self, request: RequestBuilder, account: &Account) -> RequestBuilder {
        request
            .query(&[("address", account.address.as_str())])
            .query(&[("accountIndex", account.index)])
    }
}

#[cfg(test)]
mod tests {
    mod cancel_lease_tests;
    mod scheduled_cancel_tests;
    use super::*;
    use crate::exchanges::arcus::TimeInForce;
    use ed25519_dalek::{Signature, SigningKey, Verifier};
    use serde_json::{Value, json};
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::{TcpListener, TcpStream},
    };

    const ADDRESS: &str = "0x11111111111111111111111111111111111111ab";
    const KEY: [u8; 32] = [7; 32];

    #[test]
    fn cancel_receipt_must_confirm_business_success() {
        for (http, status, reason, ok) in [
            (200, "canceled", None, true),
            (200, "rejected", None, false),
            (200, "success", Some("denied"), false),
            (202, "accepted", None, false),
            (200, "unrecognized", None, false),
        ] {
            let receipt = OrderReceipt {
                http_status: http,
                status: status.into(),
                order_id: None,
                client_id: None,
                rejection_reason: reason.map(str::to_owned),
            };
            let confirmed = receipt.confirm_cancel().is_ok();
            assert_eq!(confirmed, ok);
            use crate::strategy::arcus_xyz_basis::{ShutdownDisposition, classify_shutdown};
            assert_eq!(
                matches!(
                    classify_shutdown(confirmed, Ok((0, 0)), Ok((Decimal::ZERO, Decimal::ZERO))),
                    ShutdownDisposition::Clean
                ),
                ok
            );
        }
    }

    async fn read_request(
        socket: TcpStream,
    ) -> (TcpStream, String, BTreeMap<String, String>, Vec<u8>) {
        let mut reader = BufReader::new(socket);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let mut headers = BTreeMap::new();
        loop {
            let mut header = String::new();
            assert!(reader.read_line(&mut header).await.unwrap() > 0);
            if header == "\r\n" {
                break;
            }
            let (name, value) = header.split_once(':').unwrap();
            headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
        }
        let mut body = vec![
            0;
            headers
                .get("content-length")
                .map(|n| n.parse().unwrap())
                .unwrap_or(0)
        ];
        reader.read_exact(&mut body).await.unwrap();
        (reader.into_inner(), line, headers, body)
    }

    async fn respond(mut socket: TcpStream, status: &str, json: Value) {
        let body = json.to_string();
        socket.write_all(format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len(),
        ).as_bytes()).await.unwrap();
    }

    fn local_client(listener: &TcpListener) -> ArcusTrading {
        let signer = ArcusSigner::new(SigningKey::from_bytes(&KEY), ADDRESS, 2).unwrap();
        let mut client = ArcusTrading::new(Network::Testnet, signer).unwrap();
        client.info.base_url = format!("http://{}", listener.local_addr().unwrap());
        client
    }

    #[tokio::test]
    async fn place_ack_is_correlated_and_order_is_read_separately() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let trading = local_client(&listener);
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let (socket, line, headers, body) = read_request(socket).await;
            assert_eq!(
                line,
                format!("POST /v1/placeOrder?address={ADDRESS} HTTP/1.1\r\n")
            );
            let body: Value = serde_json::from_slice(&body).unwrap();
            let timestamp: u64 = headers["x-timestamp"].parse().unwrap();
            assert_eq!(body["timestamp"], timestamp);
            assert_eq!(body["accountIndex"], 2);
            assert_eq!(body["clientId"], "Mixed-Case");
            assert_eq!(body["quantity"], "0.002");
            assert_eq!(body["price"], "60000.5");
            let expiry = body["goodTilTime"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .unwrap()
                * 1_000;
            let payload = format!(
                r#"{{"ad":"{ADDRESS}","ai":2,"c":"Mixed-Case","ct":{timestamp},"g":{expiry},"m":1,"op":1,"p":600005,"q":2,"r":0,"s":0,"t":3,"v":1}}"#,
            );
            let public = SigningKey::from_bytes(&KEY).verifying_key();
            assert_eq!(headers["x-api-key"], hex::encode(public.to_bytes()));
            let signature =
                Signature::from_slice(&hex::decode(&headers["x-signature"]).unwrap()).unwrap();
            public.verify(payload.as_bytes(), &signature).unwrap();
            respond(
                socket,
                "202 Accepted",
                json!({"orderId":"ab12","clientId":"Mixed-Case","status":"ACK"}),
            )
            .await;

            let (socket, _) = listener.accept().await.unwrap();
            let (socket, line, headers, _) = read_request(socket).await;
            assert_eq!(
                line,
                format!("GET /v1/order/ab12?address={ADDRESS}&accountIndex=2 HTTP/1.1\r\n")
            );
            assert!(!headers.contains_key("x-signature"));
            respond(
                socket,
                "200 OK",
                json!({"orderId":"ab12","clientId":"Mixed-Case","marketId":1,
                "side":"BUY","status":"FILLED","price":"60000.5","originalSize":"0.002",
                "remainingSize":"0","updatedAt":1800000000000000_u64}),
            )
            .await;
        });
        let market = serde_json::from_value(json!({"marketId":1,"marketDisplayName":"BTC-USD","status":"ONLINE",
            "type":"PERPETUAL","baseAsset":"BTC","quoteAsset":"USD","tickSize":"0.1","stepSize":"0.001",
            "minOrderSize":"0.001","maxOrderSize":"10","minOrderNotional":"5"})).unwrap();
        let receipt = trading
            .place_limit(
                &market,
                LimitOrder {
                    side: Side::Buy,
                    quantity: Decimal::new(2, 3),
                    price: Decimal::new(600005, 1),
                    time_in_force: TimeInForce::Alo,
                    reduce_only: false,
                    client_id: "Mixed-Case",
                },
            )
            .await
            .unwrap();
        assert_eq!(receipt.http_status, 202);
        assert_eq!(receipt.status, "ACK");
        let order = trading
            .info
            .order(
                &trading.signer.account,
                receipt.order_id.as_deref().unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(order.status, "FILLED");
        assert_eq!(order.remaining_size, Decimal::ZERO);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn gateway_failure_is_uncertain_but_invalid_signature_is_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let trading = local_client(&listener);
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            respond(
                read_request(socket).await.0,
                "502 Bad Gateway",
                json!({"error":"unknown transmission state"}),
            )
            .await;
            let (socket, _) = listener.accept().await.unwrap();
            respond(
                read_request(socket).await.0,
                "401 Unauthorized",
                json!({"error":"invalid signature"}),
            )
            .await;
        });
        let error = trading.cancel_by_client_id(1, "first").await.unwrap_err();
        assert!(matches!(
            error.downcast_ref::<ExecutionError>(),
            Some(ExecutionError::Uncertain { .. })
        ));
        let error = trading.cancel_by_client_id(1, "second").await.unwrap_err();
        assert!(matches!(
            error.downcast_ref::<ExecutionError>(),
            Some(ExecutionError::Rejected {
                status: StatusCode::UNAUTHORIZED,
                ..
            })
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn account_summary_parses_balances_and_rejects_mismatch() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let trading = local_client(&listener);
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let (socket, line, _, _) = read_request(socket).await;
            assert_eq!(
                line,
                format!("GET /v1/account?address={ADDRESS}&accountIndex=2 HTTP/1.1\r\n")
            );
            respond(
                socket,
                "200 OK",
                json!({
                    "accountIndex": 2,
                    "address": ADDRESS,
                    "netQuoteBalance": "125.50",
                    "equity": "126.10",
                    "freeCollateral": "100.25",
                    "sequenceNumber": 42
                }),
            )
            .await;

            let (socket, _) = listener.accept().await.unwrap();
            let (socket, _, _, _) = read_request(socket).await;
            respond(
                socket,
                "200 OK",
                json!({
                    "accountIndex": 0,
                    "address": ADDRESS,
                    "netQuoteBalance": "125.50",
                    "equity": "126.10",
                    "freeCollateral": "100.25",
                    "sequenceNumber": 42
                }),
            )
            .await;
        });

        let summary = trading.info.account(&trading.signer.account).await.unwrap();
        assert_eq!(summary.account_index, 2);
        assert_eq!(summary.equity, Decimal::new(12610, 2));
        assert_eq!(summary.free_collateral, Decimal::new(10025, 2));
        assert_eq!(summary.net_quote_balance, Decimal::new(12550, 2));
        assert_eq!(summary.sequence_number, 42);

        assert!(trading.info.account(&trading.signer.account).await.is_err());
        server.await.unwrap();
    }
}

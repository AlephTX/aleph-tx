use super::{Account, ArcusInfo, ArcusSigner, LimitOrder, Market, Network, Side, SignedRequest};
use anyhow::{Context, Result, ensure};
use reqwest::{RequestBuilder, StatusCode, Url};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
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

pub struct ArcusTrading {
    info: ArcusInfo,
    signer: ArcusSigner,
    last_timestamp: AtomicU64,
}

impl ArcusTrading {
    pub fn new(network: Network, signer: ArcusSigner) -> Result<Self> {
        Ok(Self {
            info: ArcusInfo::new(network)?,
            signer,
            last_timestamp: AtomicU64::new(0),
        })
    }

    pub async fn place_limit(
        &self,
        market: &Market,
        order: LimitOrder<'_>,
    ) -> Result<OrderReceipt> {
        let request = self.signer.place_limit(market, order, self.timestamp()?)?;
        self.submit(request).await
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

    fn timestamp(&self) -> Result<u64> {
        let now = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos())?;
        let next = |last: u64| last.checked_add(1).map(|value| value.max(now));
        let previous = self
            .last_timestamp
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, next)
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
}

//! REST-only shutdown after the private feed becomes unavailable.

use super::*;
use anyhow::{Context, ensure};
use rust_decimal::Decimal;
use std::collections::BTreeSet;

#[derive(Deserialize)]
struct OrderHistory {
    code: i32,
    orders: Vec<HistoricalOrder>,
}

/// One `accountOrders` entry, as recovery and the offline audit read it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub(crate) struct HistoricalOrder {
    pub(crate) client_order_index: i64,
    pub(crate) market_index: i64,
    pub(crate) owner_account_index: i64,
    pub(crate) status: String,
    pub(crate) filled_base_amount: Decimal,
    pub(crate) transaction_time: u64,
}

/// Where one known client order stands in an order history.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum OrderState<'a> {
    /// Not in the history: neither terminal nor absent is proven.
    Missing,
    /// Listed but not terminal.
    Live(&'a HistoricalOrder),
    /// Terminal; `fill_watermark` is the order's `transaction_time` when it
    /// filled anything and 0 when it filled nothing.
    Terminal {
        order: &'a HistoricalOrder,
        fill_watermark: u64,
    },
}

/// Why a history entry for a known client order contradicts recovery's
/// identity and quantity rules; never a pending state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OrderContradiction {
    Duplicate,
    ForeignAccount,
    ForeignMarket,
    NegativeFill,
    FillWithoutWatermark,
    FilledWithoutQuantity,
}

impl OrderContradiction {
    pub(crate) fn kind(self) -> &'static str {
        match self {
            Self::Duplicate => "duplicate",
            Self::ForeignAccount => "foreign_account",
            Self::ForeignMarket => "foreign_market",
            Self::NegativeFill => "negative_fill",
            Self::FillWithoutWatermark => "fill_without_watermark",
            Self::FilledWithoutQuantity => "filled_without_quantity",
        }
    }
}

impl std::fmt::Display for OrderContradiction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Duplicate => "Lighter historical order is listed more than once",
            Self::ForeignAccount => "Lighter historical order belongs to another account",
            Self::ForeignMarket => "Lighter historical order is for another market",
            Self::NegativeFill => "Lighter historical order reports a negative fill",
            Self::FillWithoutWatermark => "Lighter fill has no transaction watermark",
            Self::FilledWithoutQuantity => "Lighter filled order reports zero quantity",
        })
    }
}

impl std::error::Error for OrderContradiction {}

/// Offline form of the `accountOrders` read: the same code check and
/// entries as recovery parses them.
pub(crate) fn order_history(response: &serde_json::Value) -> Result<Vec<HistoricalOrder>> {
    let response = OrderHistory::deserialize(response)?;
    ensure!(
        response.code == 200,
        "Lighter order reconciliation failed with code {}",
        response.code
    );
    Ok(response.orders)
}

impl LighterTrading {
    pub(super) async fn recover_flat(&self) -> Result<()> {
        let tracker = self
            .order_tracker
            .as_ref()
            .context("Shutdown recovery requires the order tracker")?;
        tracker.require_reconciliation();
        let mut orders = tracker.active_cois().into_iter().collect::<BTreeSet<_>>();
        let mut canceled = BTreeSet::new();
        let mut closes = 0;
        for _ in 0..10 {
            let open = self.get_active_orders().await?;
            if !open.is_empty() {
                orders.extend(open.iter().map(|order| order.client_order_index));
                let actions: Vec<_> = open
                    .iter()
                    .filter(|order| canceled.insert(order.order_index))
                    .map(|order| BatchAction::Cancel(order.order_index))
                    .collect();
                for chunk in actions.chunks(50) {
                    self.execute_batch(chunk.to_vec()).await?;
                }
            } else if let Some(fill_time) = self.terminal_fill_time(&orders).await? {
                let account = self.position_evidence().await?;
                let position = account.position;
                if account.covers(fill_time) {
                    if position.is_zero() {
                        tracing::info!(
                            "Lighter REST recovery confirmed zero position and terminal orders"
                        );
                        return Ok(());
                    }
                    ensure!(closes < 3, "Lighter REST recovery left residual inventory");
                    let position: f64 = position.try_into()?;
                    let touch = self.recovery_touch(position > 0.0).await?;
                    let close = self.place_order(self.close_order(position, touch)?).await?;
                    orders.insert(close.client_order_index);
                    closes += 1;
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        anyhow::bail!("Lighter REST recovery could not prove zero position and terminal orders")
    }

    // The absence of an order from either REST endpoint is insufficient. For
    // fills, require the account snapshot to be at least as recent as the order.
    async fn terminal_fill_time(&self, orders: &BTreeSet<i64>) -> Result<Option<u64>> {
        if orders.is_empty() {
            return Ok(Some(0));
        }
        ensure!(
            orders.iter().all(|&id| id > 0),
            "Lighter recovery found an order without a client ID"
        );
        let auth = self.create_auth_token().await?;
        let orders: Vec<_> = orders.iter().copied().collect();
        let mut watermark = 0;
        for chunk in orders.chunks(20) {
            let ids = chunk
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let response: OrderHistory = self
                .client
                .get(format!("{}/api/v1/accountOrders", self.base_url))
                .query(&[
                    ("account_index", self.account_index.to_string()),
                    ("client_order_indexes", ids),
                ])
                .header("Authorization", &auth)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            ensure!(
                response.code == 200,
                "Lighter order reconciliation failed with code {}",
                response.code
            );
            let Some(chunk_time) =
                terminal_fill_time(&response.orders, chunk, self.account_index, self.market_id)?
            else {
                return Ok(None);
            };
            watermark = watermark.max(chunk_time);
        }
        Ok(Some(watermark))
    }

    async fn recovery_touch(&self, sell: bool) -> Result<f64> {
        #[derive(Deserialize)]
        struct Level {
            price: String,
            remaining_base_amount: String,
        }
        #[derive(Deserialize)]
        struct Book {
            code: i32,
            bids: Vec<Level>,
            asks: Vec<Level>,
        }

        let book: Book = self
            .client
            .get(format!("{}/api/v1/orderBookOrders", self.base_url))
            .query(&[("market_id", self.market_id), ("limit", 1)])
            .header(reqwest::header::CACHE_CONTROL, "no-cache")
            .timeout(Duration::from_secs(2))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        ensure!(
            book.code == 200,
            "Lighter recovery book returned code {}",
            book.code
        );
        let touch = if sell {
            book.bids.first()
        } else {
            book.asks.first()
        }
        .context("Lighter recovery has no executable touch")?;
        let price = touch.price.parse::<f64>()?;
        let size = touch.remaining_base_amount.parse::<f64>()?;
        ensure!(
            price.is_finite() && price > 0.0 && size.is_finite() && size > 0.0,
            "Invalid Lighter recovery quote"
        );
        Ok(price)
    }
}

fn terminal_fill_time(
    records: &[HistoricalOrder],
    client_ids: &[i64],
    account_index: i64,
    market_id: i64,
) -> Result<Option<u64>> {
    let mut watermark = 0;
    for id in client_ids {
        match order_state(records, *id, account_index, market_id)? {
            OrderState::Missing | OrderState::Live(_) => return Ok(None),
            OrderState::Terminal { fill_watermark, .. } => {
                watermark = watermark.max(fill_watermark);
            }
        }
    }
    Ok(Some(watermark))
}

/// The recovery rule for one client order, shared with the offline audit:
/// a unique entry for the exact account and market with a non-negative fill,
/// a terminal status, and a trade watermark on any fill.
pub(crate) fn order_state<'a>(
    records: &'a [HistoricalOrder],
    id: i64,
    account_index: i64,
    market_id: i64,
) -> std::result::Result<OrderState<'a>, OrderContradiction> {
    use OrderContradiction as C;
    let mut matched = records
        .iter()
        .filter(|order| order.client_order_index == id);
    let Some(order) = matched.next() else {
        return Ok(OrderState::Missing);
    };
    let check = |ok: bool, contradiction| if ok { Ok(()) } else { Err(contradiction) };
    check(matched.next().is_none(), C::Duplicate)?;
    check(
        order.owner_account_index == account_index,
        C::ForeignAccount,
    )?;
    check(order.market_index == market_id, C::ForeignMarket)?;
    check(order.filled_base_amount >= Decimal::ZERO, C::NegativeFill)?;
    if !(matches!(
        order.status.as_str(),
        "filled" | "canceled" | "rejected" | "expired"
    ) || order.status.starts_with("canceled-"))
    {
        return Ok(OrderState::Live(order));
    }
    let fill_watermark = if order.filled_base_amount > Decimal::ZERO {
        check(order.transaction_time > 0, C::FillWithoutWatermark)?;
        order.transaction_time
    } else {
        check(order.status != "filled", C::FilledWithoutQuantity)?;
        0
    };
    Ok(OrderState::Terminal {
        order,
        fill_watermark,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_requires_terminal_orders_for_the_exact_account_and_market() {
        let mut order = HistoricalOrder {
            client_order_index: 123,
            market_index: 4095,
            owner_account_index: 7,
            status: "open".into(),
            filled_base_amount: Decimal::new(2, 2),
            transaction_time: 30,
        };
        assert_eq!(terminal_fill_time(&[], &[123], 7, 4095).unwrap(), None);
        assert_eq!(
            terminal_fill_time(std::slice::from_ref(&order), &[123], 7, 4095).unwrap(),
            None
        );
        order.status = "canceled-ioc".into();
        assert_eq!(
            terminal_fill_time(std::slice::from_ref(&order), &[123], 7, 4095).unwrap(),
            Some(30)
        );
        assert!(terminal_fill_time(std::slice::from_ref(&order), &[123], 7, 4096).is_err());
        assert!(terminal_fill_time(std::slice::from_ref(&order), &[123], 8, 4095).is_err());
        order.transaction_time = 0;
        assert!(terminal_fill_time(&[order], &[123], 7, 4095).is_err());
    }

    /// Loopback Lighter for the real recover_flat loop: no open orders, one
    /// terminal maker order (client id 123) filled at watermark 30, and a
    /// scripted sequence of account responses. Returns the account request
    /// count when the client gives up or succeeds.
    async fn recovery_venue(
        accounts: Vec<serde_json::Value>,
    ) -> (
        String,
        Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        use serde_json::json;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let served = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&served);
        let server = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                let mut request = String::new();
                socket.read_line(&mut request).await.unwrap();
                let mut authorized = false;
                loop {
                    let mut header = String::new();
                    socket.read_line(&mut header).await.unwrap();
                    if header.trim().is_empty() {
                        break;
                    }
                    authorized |=
                        header.eq_ignore_ascii_case("authorization: fixture-read-auth\r\n");
                }
                let path = request.split_whitespace().nth(1).unwrap().to_owned();
                let response = match path.split('?').next().unwrap() {
                    "/api/v1/accountActiveOrders" => {
                        assert!(authorized && path.contains("account_index=0&market_id=0"));
                        json!({"code":200,"orders":[]})
                    }
                    "/api/v1/accountOrders" => {
                        assert!(authorized && path.contains("client_order_indexes=123"));
                        json!({"code":200,"orders":[{"client_order_index":123,"market_index":0,
                            "owner_account_index":0,"status":"canceled","filled_base_amount":"0.02",
                            "transaction_time":30}]})
                    }
                    "/api/v1/account" => {
                        let index = count.fetch_add(1, Ordering::AcqRel);
                        accounts[index.min(accounts.len() - 1)].clone()
                    }
                    other => panic!("recovery must not sign or send: {other}"),
                }
                .to_string();
                socket
                    .get_mut()
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{response}",
                            response.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            }
        });
        (url, served, server)
    }

    fn account(time: Option<u64>, size: &str) -> serde_json::Value {
        let mut account = serde_json::json!({"index":0,"positions":[{"market_id":0,
            "symbol":"ETH","sign":1,"position":size,"avg_entry_price":"2000",
            "unrealized_pnl":"0","liquidation_price":"0"}]});
        if let Some(time) = time {
            account["transaction_time"] = time.into();
        }
        serde_json::json!({"code":200,"accounts":[account]})
    }

    async fn recover(accounts: Vec<serde_json::Value>) -> (Result<()>, usize, bool) {
        let (url, served, server) = recovery_venue(accounts).await;
        let mut trading = LighterTrading::offline_test_client(&url);
        trading.test_auth_token = Some("fixture-read-auth");
        let tracker = Arc::new(OrderTracker::new());
        assert!(tracker.start_tracking(123, TrackerSide::Buy, 2000.0, 0.02));
        trading.set_order_tracker(Arc::clone(&tracker));
        let result = trading.recover_flat().await;
        server.abort();
        (
            result,
            served.load(std::sync::atomic::Ordering::Acquire),
            tracker.needs_reconciliation(),
        )
    }

    /// The real recovery loop, without signing: a flat account older than
    /// the fill watermark, or one without a watermark, cannot end recovery;
    /// it keeps polling until a covering snapshot arrives. Real clock: the
    /// HTTP client timeout must not fire on a paused one (~6 s total).
    #[tokio::test]
    async fn recovery_waits_for_an_account_watermark_covering_the_fill() {
        let (result, requests, _) = recover(vec![
            account(Some(20), "0"),
            account(None, "0"),
            account(Some(29), "0"),
            account(Some(30), "0"),
        ])
        .await;
        result.unwrap();
        assert_eq!(requests, 4, "succeeded before the covering snapshot");
        // Never covered: recovery gives up after its 10 rounds and the
        // tracker still requires reconciliation.
        let (result, requests, unreconciled) =
            recover(vec![account(Some(20), "0"), account(None, "0")]).await;
        assert!(format!("{:#}", result.unwrap_err()).contains("could not prove zero position"),);
        assert_eq!(requests, 10);
        assert!(unreconciled);
        // Ambiguous account evidence fails outright, never as flat.
        let duplicate = serde_json::json!({"code":200,"accounts":[
            account(Some(40), "0")["accounts"][0].clone(),
            account(Some(40), "0")["accounts"][0].clone(),
        ]});
        let (result, requests, unreconciled) = recover(vec![duplicate]).await;
        assert!(format!("{:#}", result.unwrap_err()).contains("more than once"));
        assert_eq!(requests, 1);
        assert!(unreconciled);
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[tokio::test]
    async fn signed_maker_partial_fill_can_be_canceled_and_closed_without_the_feeder() {
        use serde_json::{Value, json};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let post_close_snapshots = Arc::new(AtomicUsize::new(0));
        let snapshots = Arc::clone(&post_close_snapshots);
        let server = tokio::spawn(async move {
            let (mut maker, mut close) = (None, None);
            let mut canceled = false;
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                let mut request = String::new();
                socket.read_line(&mut request).await.unwrap();
                let path = request.split_whitespace().nth(1).unwrap();
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    socket.read_line(&mut header).await.unwrap();
                    if header.trim().is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':')
                        && name.eq_ignore_ascii_case("Content-Length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                socket.read_exact(&mut body).await.unwrap();
                let body = String::from_utf8(body).unwrap();
                let response = match path.split('?').next().unwrap() {
                    "/api/v1/orderBookDetails" => {
                        assert!(path.ends_with("?market_id=4095"));
                        json!({"order_book_details":[{"symbol":"ETH","market_type":"perp",
                            "status":"active","market_id":4095,"size_decimals":4,"price_decimals":2}]})
                    }
                    "/api/v1/nextNonce" => json!({"nonce":7}),
                    "/api/v1/sendTx" => {
                        let tx = body.lines().find(|line| line.starts_with('{')).unwrap();
                        let tx: Value = serde_json::from_str(tx).unwrap();
                        assert_eq!(tx["MarketIndex"], 4095);
                        let id = tx["ClientOrderIndex"].as_i64().unwrap();
                        if maker.is_none() {
                            assert_eq!(tx["TimeInForce"], 2);
                            assert_eq!(tx["ReduceOnly"], 0);
                            assert_eq!(tx["Nonce"], 7);
                            maker = Some(id);
                        } else {
                            assert!(canceled && close.is_none());
                            assert_eq!(tx["TimeInForce"], 0);
                            assert_eq!(tx["ReduceOnly"], 1);
                            assert_eq!(tx["IsAsk"], 1);
                            assert_eq!(tx["BaseAmount"], 200);
                            assert_eq!(tx["Nonce"], 9);
                            close = Some(id);
                        }
                        json!({"code":200,"tx_hash":"fixture-order-hash"})
                    }
                    "/api/v1/sendTxBatch" => {
                        assert!(maker.is_some() && !canceled);
                        assert!(body.contains(r#"\"MarketIndex\":4095"#));
                        assert!(body.contains(r#"\"Nonce\":8"#));
                        canceled = true;
                        json!({"code":200,"tx_hash":["fixture-cancel-hash"]})
                    }
                    "/api/v1/accountActiveOrders" => {
                        let orders = if !canceled && let Some(id) = maker {
                            vec![json!({"order_index":281474976710677_i64,"client_order_index":id,
                                "order_id":"fixture-order","market_index":4095,"owner_account_index":1801,
                                "initial_base_amount":"0.03","price":"2000.00","nonce":7,
                                "remaining_base_amount":"0.01","is_ask":false,"base_size":300,"base_price":200000,
                                "filled_base_amount":"0.02","filled_quote_amount":"40.00","side":"buy","type":"limit",
                                "time_in_force":"post-only","reduce_only":false,"trigger_price":"0",
                                "order_expiry":0,"status":"open","trigger_status":"n/a","block_height":1,
                                "timestamp":10,"created_at":10,"updated_at":20})]
                        } else {
                            vec![]
                        };
                        json!({"code":200,"orders":orders})
                    }
                    "/api/v1/accountOrders" => {
                        assert!(canceled);
                        let mut orders = vec![json!({"client_order_index":maker.unwrap(),"market_index":4095,
                            "owner_account_index":1801,"status":"canceled","filled_base_amount":"0.02","transaction_time":20})];
                        if let Some(id) = close {
                            orders.push(json!({"client_order_index":id,"market_index":4095,
                                "owner_account_index":1801,"status":"filled","filled_base_amount":"0.02","transaction_time":30}));
                        }
                        json!({"code":200,"orders":orders})
                    }
                    "/api/v1/account" => {
                        let (size, time) = if close.is_none() {
                            ("0.02", 20)
                        } else if snapshots.fetch_add(1, Ordering::AcqRel) == 0 {
                            ("0", 20) // An apparently flat but stale account snapshot.
                        } else {
                            ("0", 30)
                        };
                        json!({"code":200,"accounts":[{"index":1801,"transaction_time":time,"positions":[{
                            "market_id":4095,"symbol":"ETH","sign":1,"position":size,
                            "avg_entry_price":"2000","unrealized_pnl":"0","liquidation_price":"0"
                        }]}]})
                    }
                    "/api/v1/orderBookOrders" => json!({"code":200,
                        "bids":[{"price":"2000","remaining_base_amount":"0.1"}],"asks":[]}),
                    other => panic!("unexpected Lighter recovery endpoint {other}"),
                }.to_string();
                socket.get_mut().write_all(format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                ).as_bytes()).await.unwrap();
            }
        });
        let credentials = LighterCredentials::from_env(|key| match key {
            "LIGHTER_PRIVATE_KEY" => Some("01".repeat(40)),
            "LIGHTER_BASE_URL" => Some(url.clone()),
            "LIGHTER_CHAIN_ID" => Some("300".into()),
            "LIGHTER_ACCOUNT_INDEX" => Some("1801".into()),
            "LIGHTER_API_KEY_INDEX" => Some("7".into()),
            _ => None,
        })
        .unwrap();
        let mut trading = LighterTrading::with_credentials(4095, credentials)
            .await
            .unwrap();
        trading.set_order_tracker(Arc::new(OrderTracker::new()));
        trading
            .place_order(OrderParams {
                size: 0.03,
                price: 2000.0,
                side: Side::Buy,
                order_type: OrderType::PostOnly,
                reduce_only: false,
            })
            .await
            .unwrap();
        trading.recover_flat().await.unwrap();
        assert_eq!(post_close_snapshots.load(Ordering::Acquire), 2);
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}

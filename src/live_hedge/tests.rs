use super::*;
use crate::exchange::{OrderParams, OrderType};
use crate::exchanges::hyperliquid::PerpAsset;
use std::str::FromStr;

fn d(value: &str) -> Decimal {
    Decimal::from_str(value).unwrap()
}

fn market() -> PerpMarket {
    PerpMarket {
        asset_id: 1,
        asset: PerpAsset {
            name: "ETH".into(),
            sz_decimals: 4,
            max_leverage: 20,
            is_delisted: false,
        },
        collateral_token: None,
        mark_price: d("2000"),
        oracle_price: d("2000"),
        hourly_funding: Decimal::ZERO,
        open_interest: d("100"),
    }
}

fn limits() -> HedgeLimits {
    HedgeLimits {
        max_unhedged_base: d("0.05"),
        deadband_base: d("0.001"),
        max_order_notional_usd: d("30"),
        min_available_usd: d("60"),
        slippage_bps: 10,
        taker_fee_bps: 4.5,
    }
}

fn observation(primary: &str, hedge: &str) -> Observation {
    Observation {
        primary: d(primary),
        hedge: d(hedge),
        book: BestBidAsk {
            coin: "ETH".into(),
            timestamp_ms: 100,
            bid: d("2000"),
            bid_size: d("0.08"),
            ask: d("2001"),
            ask_size: d("0.12"),
        },
    }
}

#[test]
fn hedge_reserves_the_net_cap_without_releasing_pending_cancels() {
    let place = |side, size| {
        BatchAction::Place(OrderParams {
            side,
            size,
            price: 2_000.0,
            order_type: OrderType::PostOnly,
            reduce_only: false,
        })
    };
    let allowance = QuoteAllowance {
        primary: 0.02,
        hedge: -0.02,
        max_unhedged: 0.05,
        expires: Instant::now(),
    };
    let mut batch = vec![
        BatchAction::Cancel(77),
        place(Side::Buy, 0.02),
        place(Side::Sell, 0.025),
        place(Side::Sell, 0.01),
    ];
    allowance.cap_batch(&mut batch, 0.0, 0.04, 0.001, |_| 0.007);
    assert!(
        matches!(&batch[..], [BatchAction::Cancel(77), BatchAction::Place(bid), BatchAction::Place(ask)]
            if bid.side == Side::Buy && (0.007..=0.01).contains(&bid.size)
            && ask.side == Side::Sell && ask.size == 0.025)
    );
}

#[test]
fn hedge_ioc_caps_notional_and_visible_size_and_never_flips_a_reduce_only_order() {
    let market = market();
    let limits = limits();
    let mut long = observation("0.03", "0");
    let sell = plan_ioc(&market, &limits, &long, false).unwrap().unwrap();
    assert!(!sell.is_buy && !sell.reduce_only);
    assert_eq!(sell.time_in_force, TimeInForce::Ioc);
    assert!(sell.size < long.primary);
    assert!(sell.size * long.book.bid <= limits.max_order_notional_usd);
    assert!(sell.limit_price >= long.book.bid * d("0.999"));
    long.book.bid_size = d("0.006");
    let thin = plan_ioc(&market, &limits, &long, false).unwrap().unwrap();
    assert_eq!(thin.size, long.book.bid_size);

    let sign_flip = observation("-0.02", "-0.004");
    let close = plan_ioc(&market, &limits, &sign_flip, false)
        .unwrap()
        .unwrap();
    assert!(close.is_buy && close.reduce_only);
    assert_eq!(close.size, d("0.004"));
    assert!(close.limit_price <= sign_flip.book.ask * d("1.001"));
    assert!(
        plan_ioc(&market, &limits, &observation("0.001", "0"), false)
            .unwrap()
            .is_none()
    );
    assert!(
        plan_ioc(&market, &limits, &observation("0", "-0.001"), true)
            .unwrap()
            .unwrap()
            .reduce_only
    );
}

#[test]
fn ack_and_timeout_reserve_exposure_until_terminal_and_account_agree() {
    let order = plan_ioc(&market(), &limits(), &observation("0.03", "0"), false)
        .unwrap()
        .unwrap();
    let mut pending = PendingIoc {
        id: "test".into(),
        before: Decimal::ZERO,
        order,
        sent_at_ms: 100,
        terminal: None,
        cancel_sent: false,
    };
    assert_eq!(pending.confirmed_fill(d("-0.007"), 125).unwrap(), None);
    pending.terminal = Some(Terminal {
        at_ms: 124,
        filled: d("0.007"),
        price: Some(d("1999")),
    });
    assert_eq!(pending.confirmed_fill(d("-0.007"), 123).unwrap(), None);
    assert_eq!(pending.confirmed_fill(Decimal::ZERO, 125).unwrap(), None);
    assert_eq!(
        pending.confirmed_fill(d("-0.007"), 125).unwrap(),
        Some(d("0.007"))
    );
    assert!(pending.confirmed_fill(d("-0.03"), 125).is_err());
    assert!(pending.confirmed_fill(d("0.007"), 125).is_err());

    let residual = observation("0.03", "-0.007");
    let next = plan_ioc(&market(), &limits(), &residual, false)
        .unwrap()
        .unwrap();
    assert!(!next.is_buy && next.size <= d("0.023"));
}

#[tokio::test]
async fn signed_ioc_is_journaled_before_dispatch_and_a_lost_ack_blocks_another_order() {
    use std::fs::OpenOptions;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let journal_path =
        std::env::temp_dir().join(format!("aleph-hedge-{}.jsonl", uuid::Uuid::new_v4()));
    let journal = OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(&journal_path)
        .unwrap();
    let captured = Arc::new(AtomicUsize::new(0));
    let orders = Arc::clone(&captured);
    let server_journal = journal_path.clone();
    let server = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            let mut request = String::new();
            socket.read_line(&mut request).await.unwrap();
            let mut size = 0;
            loop {
                let mut header = String::new();
                socket.read_line(&mut header).await.unwrap();
                if header.trim().is_empty() {
                    break;
                }
                if let Some((name, value)) = header.split_once(':')
                    && name.eq_ignore_ascii_case("Content-Length")
                {
                    size = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; size];
            socket.read_exact(&mut body).await.unwrap();
            let response = if request.starts_with("POST /exchange ") {
                let action: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let wire = &action["action"]["orders"][0];
                let id = wire["c"].as_str().unwrap();
                let persisted = std::fs::read_to_string(&server_journal).unwrap();
                assert!(persisted.contains(id));
                assert_eq!(wire["t"]["limit"]["tif"], "Ioc");
                assert_eq!(wire["b"], false);
                assert!(action["signature"]["r"].is_string());
                if orders.fetch_add(1, Ordering::AcqRel) > 0 {
                    // The POST reached the venue, but no acknowledgment reached the engine.
                    continue;
                }
                json!({"status":"ok","response":{"type":"order","data":{"statuses":[
                    {"filled":{"oid":7,"totalSz":"0.007","avgPx":"2000"}}
                ]}}})
            } else if request.starts_with("GET /api/v1/account?") {
                json!({"code":200,"accounts":[{"index":0,"positions":[{
                    "market_id":0,"symbol":"ETH","sign":1,"position":"0.03",
                    "avg_entry_price":"2000","unrealized_pnl":"0","liquidation_price":"0"
                }]}]})
            } else {
                let now = unix_ms().unwrap();
                let query: serde_json::Value = serde_json::from_slice(&body).unwrap();
                match query["type"].as_str().unwrap() {
                    "clearinghouseState" => json!({"time":now,"withdrawable":"100","marginSummary":{
                        "accountValue":"100","totalNtlPos":"14","totalMarginUsed":"1"},
                        "assetPositions":[{"position":{"coin":"ETH","szi":"-0.007","unrealizedPnl":"0"}}]}),
                    "l2Book" => json!({"coin":"ETH","time":now,
                        "levels":[[{"px":"2000","sz":"0.08"}],[{"px":"2001","sz":"0.12"}]]}),
                    "orderStatus" => json!({"status":"unknownOid"}),
                    kind => panic!("unexpected info request {kind}"),
                }
            }.to_string();
            socket.get_mut().write_all(format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            ).as_bytes()).await.unwrap();
        }
    });

    let info = HyperliquidInfo::new(Network::Testnet)
        .unwrap()
        .with_test_url(base_url.clone());
    let signer = HyperliquidSigner::from_private_key(&format!("{:064x}", 1)).unwrap();
    let trading = HyperliquidTrading::new(info.clone(), signer, None).unwrap();
    let mut hedge = LiveInventoryHedge {
        limits: limits(),
        primary: Arc::new(LighterTrading::offline_test_client(&base_url)),
        info,
        trading,
        market: market(),
        account: format!("0x{:040x}", 2),
        start_equity: d("100"),
        confirmed: Decimal::ZERO,
        pending: None,
        journal,
        last_poll: None,
        last_rebalance: None,
        allowance: None,
        blocked_since: None,
    };
    let mut observed = observation("0.03", "0");
    observed.book.timestamp_ms = unix_ms().unwrap();
    hedge.execute(observed, false).await.unwrap();
    assert_eq!(hedge.confirmed, Decimal::ZERO);
    let observed = hedge.observe(false).await.unwrap().unwrap();
    assert_eq!(observed.hedge, d("-0.007"));
    assert!(hedge.pending.is_none());
    hedge.execute(observed, false).await.unwrap();
    let unresolved = hedge.pending.as_ref().unwrap().id.clone();
    assert!(hedge.observe(false).await.unwrap().is_none());
    hedge.rebalance().await.unwrap();
    assert_eq!(hedge.pending.as_ref().unwrap().id, unresolved);
    assert_eq!(captured.load(Ordering::Acquire), 2);
    let log = std::fs::read_to_string(&journal_path).unwrap();
    assert_eq!(log.matches("hedge_intent").count(), 2);
    assert_eq!(log.matches("hedge_terminal").count(), 1);
    drop(hedge);
    server.abort();
    let error = server.await.unwrap_err();
    assert!(error.is_cancelled());
    std::fs::remove_file(journal_path).unwrap();
}

use super::*;
use crate::exchange::{OrderParams, OrderType};
use crate::exchanges::hyperliquid::PerpAsset;
use serde_json::json;
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
            growth_mode: None,
            deployer_fee_scale: None,
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
        primary_requested_at: Instant::now(),
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
        sell_proceeds: 1997.0,
        buy_cost: 2004.0,
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
        source: "response",
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let journal_path =
        std::env::temp_dir().join(format!("aleph-hedge-{}.jsonl", uuid::Uuid::new_v4()));
    let mut journal = OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(&journal_path)
        .unwrap();
    let hedge_account = format!("0x{:040x}", 2);
    journal
        .write_all(
            &JournalRecord::Start(journal::StartRecord {
                session_id: "loopback".into(),
                network: "Testnet".into(),
                // LighterCredentials only accepts positive account indices.
                lighter_account_index: 1,
                hyperliquid_account: Some(hedge_account.clone()),
                symbol: "ETH".into(),
                journal_schema: Some(journal::JOURNAL_SCHEMA),
            })
            .to_line()
            .unwrap(),
        )
        .unwrap();
    // Offline audit of the file alone, as after a crash at this point.
    let audit_now = |evidence: Option<&audit::Evidence>| {
        audit::audit(&std::fs::read(&journal_path).unwrap(), evidence)
    };
    let captured = Arc::new(AtomicUsize::new(0));
    let orders = Arc::clone(&captured);
    let lookup_ready = Arc::new(AtomicBool::new(false));
    let server_lookup_ready = Arc::clone(&lookup_ready);
    let server_journal = journal_path.clone();
    let server = tokio::spawn(async move {
        let mut lost_order: Option<serde_json::Value> = None;
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
                    lost_order = Some(wire.clone());
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
                let recovered = server_lookup_ready.load(Ordering::Acquire);
                match query["type"].as_str().unwrap() {
                    "clearinghouseState" => json!({"time":now,"withdrawable":"100","marginSummary":{
                        "accountValue":"100","totalNtlPos":"14","totalMarginUsed":"1"},
                        "assetPositions":[{"position":{"coin":"ETH","szi":if recovered {"-0.011"} else {"-0.007"},"unrealizedPnl":"0"}}]}),
                    "l2Book" => json!({"coin":"ETH","time":now,
                        "levels":[[{"px":"2000","sz":"0.08"}],[{"px":"2001","sz":"0.12"}]]}),
                    "orderStatus" if recovered => {
                        let order = lost_order.as_ref().unwrap();
                        let remaining = d(order["s"].as_str().unwrap()) - d("0.004");
                        json!({"status":"order", "order":{
                            "status":"canceled", "statusTimestamp":now,
                            "order":{"coin":"ETH", "cloid":order["c"], "side":"A",
                                "limitPx":order["p"], "origSz":order["s"],
                                "sz":remaining.to_string(), "oid":8, "timestamp":now}
                        }})
                    }
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
        account: hedge_account.clone(),
        start_equity: d("100"),
        confirmed: Decimal::ZERO,
        pending: None,
        journal,
        last_poll: None,
        last_rebalance: None,
        allowance: None,
        blocked_since: None,
        telemetry: None,
        max_primary_age: Duration::from_millis(MAX_ACCOUNT_AGE_MS),
    };
    let telemetry_dir =
        std::env::temp_dir().join(format!("aleph-hedge-telemetry-{}", uuid::Uuid::new_v4()));
    let sink = crate::telemetry_sink::TelemetrySink::open(
        &telemetry_dir,
        &crate::telemetry_sink::SessionInfo {
            session_id: "hedge".into(),
            network: "Testnet".into(),
            exchange: "lighter".into(),
            symbol: "ETH".into(),
            market_id: 0,
            account: 1,
            hedge_account: None,
        },
    )
    .unwrap();
    hedge.set_telemetry(Some(sink.clone()));
    let mut observed = observation("0.03", "0");
    observed.book.timestamp_ms = unix_ms().unwrap();
    hedge.execute(observed, false).await.unwrap();
    assert_eq!(hedge.confirmed, Decimal::ZERO);
    // Crash after the intent is durable but before any terminal is known.
    let crashed = audit_now(None);
    assert_eq!(crashed.status, audit::AuditStatus::Unresolved);
    assert_eq!(crashed.orders[0].status, audit::OrderStatus::Pending);
    let observed = hedge.observe(false).await.unwrap().unwrap();
    assert_eq!(observed.hedge, d("-0.007"));
    assert!(hedge.pending.is_none());
    // Crash after the terminal is durable.
    let recorded = audit_now(None);
    assert_eq!(recorded.status, audit::AuditStatus::JournalRecorded);
    assert_eq!(recorded.hedge_position.journal_confirmed, d("-0.007"));
    hedge.execute(observed, false).await.unwrap();
    let unresolved = hedge.pending.as_ref().unwrap().id.clone();
    // Crash after the lost ACK: the second IOC may have filled up to its size.
    let lost_ack = audit_now(None);
    assert_eq!(lost_ack.status, audit::AuditStatus::Unresolved);
    let open = &lost_ack.orders[1];
    assert_eq!(
        (open.cloid.as_str(), open.status),
        (unresolved.as_str(), audit::OrderStatus::Pending)
    );
    assert_eq!(open.before_position, d("-0.007"));
    assert_eq!(open.possible_position_max, Some(d("-0.007")));
    assert_eq!(open.possible_position_min, Some(d("-0.007") - open.size));
    // Frozen venue evidence in the raw wire format, as an operator captures it.
    let (size, limit) = (open.size, open.limit_price);
    let now = unix_ms().unwrap();
    let frozen = |lookup: serde_json::Value, position: Option<&str>, at: u64| {
        audit::Evidence {
        network: "Testnet".into(),
        hyperliquid_account: hedge_account.clone(),
        coin: "ETH".into(),
        lookups: vec![audit::LookupEvidence {
            cloid: unresolved.clone(),
            observed_at_ms: at + 5,
            response: lookup,
        }],
        account_snapshot: position.map(|szi| audit::SnapshotEvidence {
            observed_at_ms: at + 5,
            state: json!({"time": at, "withdrawable": "100", "marginSummary": {"accountValue": "100",
                "totalNtlPos": "14", "totalMarginUsed": "1"},
                "assetPositions": [{"position": {"coin": "ETH", "szi": szi, "unrealizedPnl": "0"}}]}),
        }),
    }
    };
    let unknown = frozen(json!({"status": "unknownOid"}), Some("-0.007"), now);
    assert_eq!(
        audit_now(Some(&unknown)).orders[1].status,
        audit::OrderStatus::Pending
    );
    let partial = json!({"status": "order", "order": {"status": "canceled", "statusTimestamp": now,
        "order": {"coin": "ETH", "cloid": unresolved, "side": "A", "limitPx": limit.to_string(),
            "origSz": size.to_string(), "sz": (size - d("0.004")).to_string(), "oid": 8, "timestamp": now}}});
    // Partial fill known at the venue, account snapshot not yet updated.
    let lagging = frozen(partial.clone(), Some("-0.007"), now);
    assert_eq!(
        audit_now(Some(&lagging)).orders[1].status,
        audit::OrderStatus::Pending
    );
    let unexplained = frozen(partial.clone(), Some("-0.012"), now);
    assert_eq!(
        audit_now(Some(&unexplained)).status,
        audit::AuditStatus::Invalid
    );
    let older = frozen(partial.clone(), Some("-0.011"), now - 1);
    assert_eq!(
        audit_now(Some(&older)).orders[1].status,
        audit::OrderStatus::Pending
    );
    let matched = audit_now(Some(&frozen(partial.clone(), Some("-0.011"), now)));
    assert_eq!(matched.status, audit::AuditStatus::EvidenceMatched);
    assert_eq!(matched.orders[1].filled, Some(d("0.004")));
    assert!(!matched.restart_permitted);
    // Torn tail: a terminal cut mid-write leaves the IOC pending.
    let mut torn = std::fs::read(&journal_path).unwrap();
    torn.extend_from_slice(br#"{"event":"hedge_terminal","cloid":"#);
    assert_eq!(
        audit::audit(&torn, None).orders[1].status,
        audit::OrderStatus::Pending
    );
    // Intent time precedes journal fsync and signing. Even a much older
    // intent cannot turn an unknown lookup into proof of a zero fill.
    hedge.pending.as_mut().unwrap().sent_at_ms -= 60_000;
    assert!(hedge.observe(false).await.unwrap().is_none());
    hedge.rebalance().await.unwrap();
    assert_eq!(hedge.pending.as_ref().unwrap().id, unresolved);
    assert_eq!(captured.load(Ordering::Acquire), 2);
    // A delayed terminal response must still be queried and its partial fill
    // matched to the account before releasing the existing reservation.
    lookup_ready.store(true, Ordering::Release);
    let recovered = hedge.observe(false).await.unwrap().unwrap();
    assert_eq!(recovered.hedge, d("-0.011"));
    assert!(hedge.pending.is_none());
    assert_eq!(captured.load(Ordering::Acquire), 2);
    let log = std::fs::read_to_string(&journal_path).unwrap();
    assert_eq!(log.matches("hedge_intent").count(), 2);
    assert_eq!(log.matches("hedge_terminal").count(), 2);
    // The production writer's records parse with the shared journal types
    // and carry the terminal/account times the offline audit relies on.
    let records: Vec<_> = log
        .lines()
        .map(|line| JournalRecord::parse(line.as_bytes()).unwrap())
        .collect();
    let sources: Vec<_> = records
        .iter()
        .filter_map(|r| match r {
            JournalRecord::Terminal(t) => {
                assert!(t.account_time_ms >= t.terminal_at_ms && t.account_time_ms.is_some());
                t.terminal_source.clone()
            }
            _ => None,
        })
        .collect();
    assert_eq!(sources, ["response", "lookup"]);
    // After recovery the journal alone explains both IOCs.
    let done = audit_now(None);
    assert_eq!(done.status, audit::AuditStatus::JournalRecorded);
    assert_eq!(done.hedge_position.journal_confirmed, d("-0.011"));
    assert_eq!(done.orders[1].filled, Some(d("0.004")));
    // Both reconciled fills are exported as Hyperliquid sells, after journaling.
    sink.stopped().await.unwrap();
    let exported: Vec<serde_json::Value> =
        std::fs::read_to_string(telemetry_dir.join("hedge.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .filter(|line: &serde_json::Value| line["type"] == "hedge_fill")
            .collect();
    assert_eq!(exported.len(), 2);
    assert!(exported.iter().all(|f| f["venue"] == "hyperliquid"
        && f["side"] == "sell"
        && f["size"].as_f64().unwrap() > 0.0
        && f["fee"].as_f64().unwrap() > 0.0));
    assert_eq!(exported[1]["position_after"], -0.011);
    std::fs::remove_dir_all(telemetry_dir).unwrap();
    drop(hedge);
    server.abort();
    let error = server.await.unwrap_err();
    assert!(error.is_cancelled());
    std::fs::remove_file(journal_path).unwrap();
}

#[test]
fn freshness_tolerates_bounded_future_skew_only() {
    assert!(fresh(10_000, 10_500, 3_000));
    assert!(fresh(10_000, 10_000 + CLOCK_TOLERANCE_MS, 3_000));
    assert!(!fresh(10_000, 10_001 + CLOCK_TOLERANCE_MS, 3_000));
    assert!(!fresh(10_000, 6_999, 3_000));
    assert!(!fresh(10_000, 0, 3_000));
}

#[test]
fn partial_ioc_cancel_reports_the_filled_quantity() {
    assert_eq!(
        terminal_fill("canceled", d("0.015"), d("0.004")).unwrap(),
        d("0.011")
    );
    assert_eq!(
        terminal_fill("filled", d("0.015"), Decimal::ZERO).unwrap(),
        d("0.015")
    );
    assert_eq!(
        terminal_fill("canceled", d("0.015"), d("0.015")).unwrap(),
        Decimal::ZERO
    );
    assert!(terminal_fill("canceled", d("0.015"), d("0.016")).is_err());
    assert!(terminal_fill("canceled", d("0.015"), d("-0.001")).is_err());
}

#[test]
fn configured_limits_bound_each_leg_and_net_exposure() {
    let limits = limits();
    assert!(within_limits(&limits, d("0.05"), d("-0.05")));
    assert!(within_limits(&limits, d("0.03"), d("0.02")));
    assert!(!within_limits(&limits, d("0.051"), d("-0.05")));
    assert!(!within_limits(&limits, d("0.05"), d("-0.051")));
    assert!(!within_limits(&limits, d("0.03"), d("0.03")));
    assert!(!within_limits(&limits, d("-0.03"), d("-0.03")));

    let order = plan_ioc(&market(), &limits, &observation("0.04", "-0.02"), false)
        .unwrap()
        .unwrap();
    assert!(!order.is_buy && !order.reduce_only);
    assert!(order.size > Decimal::ZERO && order.size <= d("0.02"));
}

/// Loopback venue for the stale-primary scenarios. Each Lighter account and
/// Hyperliquid info reply waits for its configured delay; order posts are
/// counted and answered as an empty fill.
struct DelayedVenue {
    url: String,
    lighter_delay_ms: Arc<std::sync::atomic::AtomicU64>,
    hyperliquid_delay_ms: Arc<std::sync::atomic::AtomicU64>,
    lighter_position: Arc<parking_lot::Mutex<String>>,
    /// When the server received the latest Lighter account request; the
    /// client's request start is no later than this.
    lighter_seen: Arc<parking_lot::Mutex<Option<Instant>>>,
    orders: Arc<std::sync::atomic::AtomicUsize>,
    server: tokio::task::JoinHandle<()>,
}

impl DelayedVenue {
    async fn start() -> Self {
        use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let lighter_delay_ms = Arc::new(AtomicU64::new(0));
        let hyperliquid_delay_ms = Arc::new(AtomicU64::new(0));
        let lighter_position = Arc::new(parking_lot::Mutex::new("0".to_owned()));
        let orders = Arc::new(AtomicUsize::new(0));
        let lighter_seen = Arc::new(parking_lot::Mutex::new(None));
        let (ld, hd, lp, counted, seen) = (
            Arc::clone(&lighter_delay_ms),
            Arc::clone(&hyperliquid_delay_ms),
            Arc::clone(&lighter_position),
            Arc::clone(&orders),
            Arc::clone(&lighter_seen),
        );
        let server = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let (ld, hd, lp, counted, seen) = (
                    Arc::clone(&ld),
                    Arc::clone(&hd),
                    Arc::clone(&lp),
                    Arc::clone(&counted),
                    Arc::clone(&seen),
                );
                tokio::spawn(async move {
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
                    let (delay, response) = if request.starts_with("GET /api/v1/account?") {
                        *seen.lock() = Some(Instant::now());
                        let size = lp.lock().clone();
                        (
                            ld.load(Ordering::Acquire),
                            json!({"code":200,"accounts":[{
                            "index":0,"transaction_time":30,"positions":[{
                            "market_id":0,"symbol":"ETH","sign":1,"position":size,
                            "avg_entry_price":"2000","unrealized_pnl":"0","liquidation_price":"0"}]}]}),
                        )
                    } else if request.starts_with("POST /exchange ") {
                        let action: serde_json::Value = serde_json::from_slice(&body).unwrap();
                        assert_eq!(action["signature"]["r"], "0x0", "test submits unsigned");
                        assert_eq!(action["action"]["orders"][0]["t"]["limit"]["tif"], "Ioc");
                        counted.fetch_add(1, Ordering::AcqRel);
                        (
                            0,
                            json!({"status":"ok","response":{"type":"order","data":{"statuses":[
                            {"error":"fixture rejects every order"}]}}}),
                        )
                    } else {
                        let query: serde_json::Value = serde_json::from_slice(&body).unwrap();
                        let delay = hd.load(Ordering::Acquire);
                        tokio::time::sleep(Duration::from_millis(delay)).await;
                        // Hyperliquid times are taken after its own delay, so
                        // its snapshots stay fresh however slow the reply is.
                        let now = unix_ms().unwrap();
                        let reply = match query["type"].as_str().unwrap() {
                            "clearinghouseState" => json!({"time":now,"withdrawable":"100",
                                "marginSummary":{"accountValue":"100","totalNtlPos":"0","totalMarginUsed":"0"},
                                "assetPositions":[]}),
                            "l2Book" => json!({"coin":"ETH","time":now,
                                "levels":[[{"px":"2000","sz":"0.08"}],[{"px":"2001","sz":"0.12"}]]}),
                            kind => panic!("unexpected info request {kind}"),
                        };
                        (0, reply)
                    };
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    let response = response.to_string();
                    let _ = socket.get_mut().write_all(format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                        response.len()
                    ).as_bytes()).await;
                });
            }
        });
        Self {
            url,
            lighter_delay_ms,
            hyperliquid_delay_ms,
            lighter_position,
            lighter_seen,
            orders,
            server,
        }
    }

    fn hedge(&self, max_primary_age: Duration) -> (LiveInventoryHedge, std::path::PathBuf) {
        use std::fs::OpenOptions;
        let journal_path =
            std::env::temp_dir().join(format!("aleph-stale-{}.jsonl", uuid::Uuid::new_v4()));
        let journal = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&journal_path)
            .unwrap();
        let info = HyperliquidInfo::new(Network::Testnet)
            .unwrap()
            .with_test_url(self.url.clone());
        // No signing key: only the L1 signature is replaced, the production
        // execute/journal/submit path and response parsing still run.
        let trading = HyperliquidTrading::unsigned_for_test(info.clone());
        let hedge = LiveInventoryHedge {
            limits: limits(),
            primary: Arc::new(LighterTrading::offline_test_client(&self.url)),
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
            telemetry: None,
            max_primary_age,
        };
        (hedge, journal_path)
    }

    fn delays(&self, lighter_ms: u64, hyperliquid_ms: u64) {
        use std::sync::atomic::Ordering;
        self.lighter_delay_ms.store(lighter_ms, Ordering::Release);
        self.hyperliquid_delay_ms
            .store(hyperliquid_ms, Ordering::Release);
    }
}

/// The real poll/rebalance/execute path with a 200 ms primary age limit
/// (production: MAX_ACCOUNT_AGE_MS). Hyperliquid data is always fresh here,
/// so only the Lighter observation age can block.
#[tokio::test]
async fn stale_primary_observation_blocks_allowance_and_new_intents() {
    use std::sync::atomic::Ordering;
    let venue = DelayedVenue::start().await;
    let (mut hedge, journal_path) = venue.hedge(Duration::from_millis(200));
    let intents = || {
        std::fs::read_to_string(&journal_path)
            .unwrap()
            .matches("hedge_intent")
            .count()
    };
    // Fresh flat control: allowance granted and bounded by the primary age.
    // Hyperliquid answers after 100 ms, so a deadline taken after the joined
    // reply (or after poll) would be ~100 ms later than the primary's.
    let limit = Duration::from_millis(200);
    venue.delays(0, 100);
    let before = Instant::now();
    hedge.poll().await.unwrap();
    let after = Instant::now();
    let seen = venue.lighter_seen.lock().expect("Lighter request served");
    let allowance = hedge
        .allowance_at(seen)
        .expect("fresh flat observation grants an allowance");
    // Deadline = primary request start + limit; start lies in [before, seen].
    assert!(before + limit <= allowance.expires && allowance.expires <= seen + limit);
    assert!(
        allowance.expires < after + limit,
        "not measured from poll end"
    );
    // Exactly at the deadline it is unusable; just inside it is usable.
    assert!(hedge.allowance_at(allowance.expires).is_none());
    assert!(
        hedge
            .allowance_at(allowance.expires - Duration::from_nanos(1))
            .is_some()
    );
    // Lighter answers at once but Hyperliquid is slow: the joined primary
    // value is older than the limit when consumed.
    venue.delays(0, 300);
    hedge.last_poll = None;
    hedge.poll().await.unwrap();
    assert!(hedge.allowance().is_none(), "slow Hyperliquid reply");
    // observe itself yields nothing, also when closing: finish() must not
    // read a stale zero as a flat Lighter leg.
    for closing in [false, true] {
        assert!(
            hedge.observe(closing).await.unwrap().is_none(),
            "closing={closing}"
        );
    }
    // Lighter's own request is slow: request start, not receipt, counts.
    venue.delays(300, 0);
    hedge.last_poll = None;
    hedge.poll().await.unwrap();
    assert!(hedge.allowance().is_none(), "slow Lighter request");
    // Just inside the limit still works.
    venue.delays(50, 50);
    hedge.last_poll = None;
    hedge.poll().await.unwrap();
    assert!(hedge.allowance().is_some(), "within the limit");
    // An allowance never outlives its primary evidence.
    tokio::time::sleep(Duration::from_millis(220)).await;
    assert!(
        hedge.allowance().is_none(),
        "allowance expired with the evidence"
    );
    // Unhedged Lighter inventory: a stale observation must not journal or
    // submit a new IOC; a fresh one does.
    *venue.lighter_position.lock() = "0.03".into();
    venue.delays(0, 300);
    hedge.rebalance().await.unwrap();
    assert_eq!((intents(), venue.orders.load(Ordering::Acquire)), (0, 0));
    venue.delays(300, 0);
    hedge.last_rebalance = None;
    hedge.rebalance().await.unwrap();
    assert_eq!((intents(), venue.orders.load(Ordering::Acquire)), (0, 0));
    // Observation fresh, but consumed after the limit: execute re-checks.
    venue.delays(0, 0);
    let observed = hedge
        .observe(false)
        .await
        .unwrap()
        .expect("fresh observation");
    tokio::time::sleep(Duration::from_millis(220)).await;
    hedge.execute(observed, false).await.unwrap();
    assert_eq!((intents(), venue.orders.load(Ordering::Acquire)), (0, 0));
    assert!(hedge.pending.is_none());
    // Fresh control: the same inventory now journals and submits one IOC.
    hedge.last_rebalance = None;
    hedge.rebalance().await.unwrap();
    assert_eq!((intents(), venue.orders.load(Ordering::Acquire)), (1, 1));
    drop(hedge);
    venue.server.abort();
    std::fs::remove_file(journal_path).unwrap();
}

use super::*;
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::json;
use std::str::FromStr;
use std::sync::atomic::AtomicU64;

// Public, deliberately insecure SDK test fixture; never use this key for funds.
// https://github.com/hyperliquid-dex/hyperliquid-python-sdk/blob/master/tests/signing_test.py
const SDK_TEST_KEY: &str = "0x0123456789012345678901234567890123456789012345678901234567890123";
const CLOID: &str = "0x00000000000000000000000000000001";

#[test]
fn approved_agent_lookup_uses_the_ethereum_address() {
    let key = format!("{:064x}", 1);
    assert_eq!(
        HyperliquidSigner::from_private_key(&key).unwrap().address(),
        "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf"
    );
}

fn decimal(value: &str) -> Decimal {
    Decimal::from_str(value).unwrap()
}
fn asset(sz_decimals: u32) -> PerpAsset {
    PerpAsset {
        name: "ETH".into(),
        sz_decimals,
        max_leverage: 20,
        is_delisted: false,
    }
}
fn order(price: &str, size: &str) -> PerpOrder {
    PerpOrder {
        is_buy: true,
        limit_price: decimal(price),
        size: decimal(size),
        reduce_only: false,
        time_in_force: TimeInForce::Gtc,
        client_order_id: None,
    }
}
fn sdk_order(with_cloid: bool) -> OrderAction {
    let mut order = order("100", "100");
    if with_cloid {
        order.client_order_id = Some(CLOID.into());
    }
    OrderAction::new(vec![OrderWire::new(1, &asset(4), &order).unwrap()]).unwrap()
}

#[test]
fn action_hash_matches_official_sdk_production_vector() {
    let mut request = order("1670.1", "0.0147");
    request.time_in_force = TimeInForce::Ioc;
    let action = OrderAction::new(vec![OrderWire::new(4, &asset(4), &request).unwrap()]).unwrap();
    assert_eq!(
        hex::encode(action_hash(&action, None, 1_677_777_606_040, None).unwrap()),
        "0fcbeda5ae3c4950a548021552a4fea2226858c4453571bf3f24ba017eac2908"
    );
}

#[test]
fn signatures_match_official_sdk_network_and_cloid_vectors() {
    let signer = HyperliquidSigner::from_private_key(SDK_TEST_KEY).unwrap();
    let vectors = [
        (
            Network::Mainnet,
            false,
            "0xd65369825a9df5d80099e513cce430311d7d26ddf477f5b3a33d2806b100d78e",
            "0x2b54116ff64054968aa237c20ca9ff68000f977c93289157748a3162b6ea940e",
            28,
        ),
        (
            Network::Testnet,
            false,
            "0x82b2ba28e76b3d761093aaded1b1cdad4960b3af30212b343fb2e6cdfa4e3d54",
            "0x6b53878fc99d26047f4d7e8c90eb98955a109f44209163f52d8dc4278cbbd9f5",
            27,
        ),
        (
            Network::Mainnet,
            true,
            "0x41ae18e8239a56cacbc5dad94d45d0b747e5da11ad564077fcac71277a946e3",
            "0x3c61f667e747404fe7eea8f90ab0e76cc12ce60270438b2058324681a00116da",
            27,
        ),
        (
            Network::Testnet,
            true,
            "0xeba0664bed2676fc4e5a743bf89e5c7501aa6d870bdb9446e122c9466c5cd16d",
            "0x7f3e74825c9114bc59086f1eebea2928c190fdfbfde144827cb02b85bbe90988",
            28,
        ),
    ];
    for (network, with_cloid, r, s, v) in vectors {
        assert_eq!(
            signer
                .sign_action(&sdk_order(with_cloid), network, 0, None, None)
                .unwrap(),
            L1Signature {
                r: r.into(),
                s: s.into(),
                v
            }
        );
    }
}

#[test]
fn signatures_match_official_sdk_vault_vectors() {
    #[derive(Serialize)]
    struct Dummy {
        r#type: &'static str,
        num: u64,
    }
    let action = Dummy {
        r#type: "dummy",
        num: 100_000_000_000,
    };
    let signer = HyperliquidSigner::from_private_key(SDK_TEST_KEY).unwrap();
    let vault = "0x1719884eb866cb12b2287399b15f7db5e7d775ea";
    let signature = signer
        .sign_action(&action, Network::Mainnet, 0, Some(vault), None)
        .unwrap();
    assert_eq!(
        signature,
        L1Signature {
            r: "0x3c548db75e479f8012acf3000ca3a6b05606bc2ec0c29c50c515066a326239".into(),
            s: "0x4d402be7396ce74fbba3795769cda45aec00dc3125a984f2a9f23177b190da2c".into(),
            v: 28
        }
    );
}

#[test]
fn expiry_nonce_reduce_only_and_vault_are_committed_to_hash() {
    let action = sdk_order(true);
    let base = action_hash(&action, None, 100, None).unwrap();
    assert_ne!(base, action_hash(&action, None, 101, None).unwrap());
    assert_ne!(base, action_hash(&action, None, 100, Some(5_100)).unwrap());
    assert_ne!(
        base,
        action_hash(
            &action,
            Some("0x0000000000000000000000000000000000000001"),
            100,
            None
        )
        .unwrap()
    );
    let mut reduce = action.clone();
    reduce.orders[0].r = true;
    assert_ne!(base, action_hash(&reduce, None, 100, None).unwrap());
    assert!(action_hash(&action, Some("0x01"), 100, None).is_err());
}

#[test]
fn invalid_signing_keys_fail_without_exposing_input() {
    let secret = "secret-not-hex";
    let error = HyperliquidSigner::from_private_key(secret).err().unwrap();
    assert!(!format!("{error:#}").contains(secret));
    assert!(HyperliquidSigner::from_private_key(&"00".repeat(32)).is_err());
}

#[test]
fn price_and_size_precision_match_exchange_rules() {
    let eth = asset(4);
    assert!(eth.validate_order(&order("1234.5", "0.0100")).is_ok());
    assert!(eth.validate_order(&order("123456", "1")).is_ok()); // Integer exception.
    assert!(eth.validate_order(&order("1234.56", "1")).is_err()); // Six significant digits.
    assert!(eth.validate_order(&order("12.345", "1")).is_err()); // Too many decimals.
    assert!(eth.validate_order(&order("100", "0.00001")).is_err());
    assert!(eth.validate_order(&order("100", "0")).is_err());
    assert!(eth.validate_order(&order("-1", "1")).is_err());
    assert!(asset(0).validate_order(&order("0.0012345", "1")).is_err()); // Six-decimal ceiling.
    assert!(asset(0).validate_order(&order("0.012345", "1")).is_ok());
}

#[test]
fn normalization_never_increases_size_or_crosses_limit() {
    for (price, expected_buy, expected_sell) in [
        ("1234.56789", "1234.5", "1234.6"),
        ("9999.999", "9999.9", "10000"),
        ("12.3456", "12.34", "12.35"),
        ("123456.1", "123456", "123457"),
    ] {
        let requested = order(price, "1.23456");
        let buy = asset(4).normalize_order(requested.clone()).unwrap();
        let sell = asset(4)
            .normalize_order(PerpOrder {
                is_buy: false,
                ..requested.clone()
            })
            .unwrap();
        assert_eq!(buy.limit_price, decimal(expected_buy));
        assert_eq!(sell.limit_price, decimal(expected_sell));
        assert!(
            buy.limit_price <= requested.limit_price && sell.limit_price >= requested.limit_price
        );
        assert_eq!(buy.size, decimal("1.2345"));
    }
    let small = asset(0)
        .normalize_order(order("0.012345678", "1.9"))
        .unwrap();
    assert_eq!(small.limit_price, decimal("0.012345"));
    assert_eq!(small.size, Decimal::ONE);
    assert!(asset(4).normalize_order(order("100", "0.00001")).is_err());
}

#[test]
fn wire_serialization_preserves_ioc_reduce_only_and_canonical_decimals() {
    let request = PerpOrder {
        reduce_only: true,
        time_in_force: TimeInForce::Ioc,
        client_order_id: Some(CLOID.into()),
        ..order("100.00", "1.2300")
    };
    let wire = OrderWire::new(120_002, &asset(4), &request).unwrap();
    assert_eq!(
        serde_json::to_value(wire).unwrap(),
        json!({"a":120002,"b":true,"p":"100","s":"1.23","r":true,"t":{"limit":{"tif":"Ioc"}},"c":CLOID})
    );
    assert!(OrderAction::new(vec![]).is_err());
    assert!(CancelAction::new(vec![]).is_err());
    for id in [
        "0x01",
        "00000000000000000000000000000001",
        "0x0000000000000000000000000000000z",
    ] {
        assert!(
            asset(4)
                .validate_order(&PerpOrder {
                    client_order_id: Some(id.into()),
                    ..request.clone()
                })
                .is_err()
        );
    }
}

fn context() -> AssetContext {
    AssetContext {
        funding: "-0.0000125".into(),
        mark_px: "100.5".into(),
        oracle_px: "100".into(),
        open_interest: "10".into(),
    }
}

#[test]
fn hip3_indices_are_resolved_from_dex_registry_and_metadata_order() {
    let dexs = vec![
        None,
        Some(PerpDex { name: "xyz".into() }),
        Some(PerpDex { name: "io".into() }),
    ];
    assert_eq!(resolve_perp_offset("", &dexs).unwrap(), 0);
    assert_eq!(resolve_perp_offset("io", &dexs).unwrap(), 120_000);
    let meta = PerpMeta {
        universe: vec![
            PerpAsset {
                name: "io:BTC".into(),
                ..asset(4)
            },
            PerpAsset {
                name: "io:ETH".into(),
                ..asset(4)
            },
        ],
        collateral_token: Some(7),
    };
    let markets = parse_markets_for_dex(meta, vec![context(), context()], "io", 120_000).unwrap();
    assert_eq!(markets[0].asset_id, 120_000);
    assert_eq!(markets[1].asset_id, 120_001);
    assert_eq!(markets[0].collateral_token, Some(7));
    assert_eq!(markets[0].hourly_funding, decimal("-0.0000125"));
    assert!(resolve_perp_offset("absent", &dexs).is_err());
    assert!(resolve_perp_offset("io", &[Some(PerpDex { name: "io".into() })]).is_err());
    assert!(
        resolve_perp_offset(
            "io",
            &[
                None,
                Some(PerpDex { name: "io".into() }),
                Some(PerpDex { name: "io".into() })
            ]
        )
        .is_err()
    );
}

#[test]
fn metadata_rejects_namespace_mismatches_unresolved_ids_and_bad_contexts() {
    let meta = PerpMeta {
        universe: vec![asset(4)],
        collateral_token: None,
    };
    assert!(parse_markets_for_dex(meta.clone(), vec![context()], "io", 120_000).is_err());
    assert!(parse_markets_for_dex(meta.clone(), vec![context()], "", 120_000).is_err());
    assert!(parse_markets_for_dex(meta.clone(), vec![context()], "io", 0).is_err());
    assert!(parse_markets(meta.clone(), vec![]).is_err());
    assert!(
        parse_markets(
            meta.clone(),
            vec![AssetContext {
                mark_px: "NaN".into(),
                ..context()
            }]
        )
        .is_err()
    );
    assert!(
        parse_markets(
            meta.clone(),
            vec![AssetContext {
                open_interest: "-1".into(),
                ..context()
            }]
        )
        .is_err()
    );
    assert_eq!(parse_markets(meta, vec![context()]).unwrap()[0].asset_id, 0);
}

#[test]
fn bbo_requires_two_valid_uncrossed_sides_and_timestamp() {
    let book: L2Book = serde_json::from_value(json!({"coin":"io:BTC","time":1234,"levels":[[{"px":"100","sz":"2","n":3}],[{"px":"101","sz":"3","n":4}]]})).unwrap();
    assert_eq!(book.best_bid_ask().unwrap().ask, decimal("101"));
    let mut bad = book.clone();
    bad.levels[1].clear();
    assert!(bad.best_bid_ask().is_err());
    let mut bad = book.clone();
    bad.levels[1][0].px = "99".into();
    assert!(bad.best_bid_ask().is_err());
    let mut bad = book;
    bad.time = 0;
    assert!(bad.best_bid_ask().is_err());
}

#[test]
fn mixed_batch_response_preserves_actual_fill_and_per_order_rejection() {
    let body = json!({"status":"ok","response":{"type":"order","data":{"statuses":[
        {"filled":{"oid":1,"totalSz":"0.25","avgPx":"100.1"}},
        {"error":"Order could not immediately match"}, {"resting":{"oid":3}}
    ]}}})
    .to_string();
    assert_eq!(
        parse_execution_response(&body, "order", 3).unwrap(),
        vec![
            ExecutionStatus::Filled {
                order_id: 1,
                total_size: decimal("0.25"),
                average_price: decimal("100.1")
            },
            ExecutionStatus::Rejected {
                reason: "Order could not immediately match".into()
            },
            ExecutionStatus::Resting { order_id: 3 }
        ]
    );
}

#[test]
fn malformed_or_incomplete_responses_require_reconciliation() {
    for body in ["gateway timeout".to_owned(),
        json!({"status":"ok","response":{"type":"order","data":{"statuses":[]}}}).to_string(),
        json!({"status":"ok","response":{"type":"order","data":{"statuses":["success"]}}}).to_string(),
        json!({"status":"ok","response":{"type":"order","data":{"statuses":[{"filled":{"oid":1,"totalSz":"NaN","avgPx":"100"}}]}}}).to_string(),
        json!({"status":"ok","response":{"type":"order","data":{"statuses":[{"filled":{"oid":1,"totalSz":"1","avgPx":"0"}}]}}}).to_string(),
    ] { assert!(matches!(parse_execution_response(&body, "order", 1), Err(SubmissionError::Uncertain(_)))); }
    assert!(matches!(
        parse_execution_response(
            r#"{"status":"err","response":"Invalid signature"}"#,
            "order",
            1
        ),
        Err(SubmissionError::Rejected(_))
    ));
    let canceled = r#"{"status":"ok","response":{"type":"cancel","data":{"statuses":["success",{"error":"not found"}]}}}"#;
    assert_eq!(
        parse_execution_response(canceled, "cancel", 2).unwrap()[0],
        ExecutionStatus::Canceled
    );
}

#[test]
fn nonce_is_monotonic_through_clock_rollback_and_concurrent_calls() {
    let counter = AtomicU64::new(0);
    assert_eq!(super::client::next_nonce(&counter, 1000).unwrap(), 1000);
    assert_eq!(super::client::next_nonce(&counter, 999).unwrap(), 1001);
    let mut results = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    (0..100)
                        .map(|_| super::client::next_nonce(&counter, 1000).unwrap())
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        threads
            .into_iter()
            .flat_map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>()
    });
    results.sort_unstable();
    assert_eq!(results, (1002..1802).collect::<Vec<_>>());
    assert!(super::client::next_nonce(&AtomicU64::new(u64::MAX), 1000).is_err());
}

#[tokio::test]
async fn live_submission_preconditions_fail_before_network_access() {
    let client = HyperliquidTrading::new(
        HyperliquidInfo::new(Network::Testnet).unwrap(),
        HyperliquidSigner::from_private_key(SDK_TEST_KEY).unwrap(),
        None,
    )
    .unwrap();
    assert!(client.submit_orders(&sdk_order(false)).await.is_err());
    let mut duplicate = sdk_order(true);
    duplicate.orders.push(duplicate.orders[0].clone());
    assert!(client.submit_orders(&duplicate).await.is_err());
    let mut empty = sdk_order(true);
    empty.orders.clear();
    assert!(client.submit_orders(&empty).await.is_err());
}

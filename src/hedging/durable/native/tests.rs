use super::*;
use crate::hedging::{
    OrderIntent,
    durable::{ExecutionJournal, LegLimits, PositionEvidence, Reconciliation, VenuePriceRules},
};
use serde_json::{Value, json};
fn d(v: &str) -> Decimal {
    Decimal::from_str_exact(v).unwrap()
}
const US: u64 = 1_795_161_600_000_000;
const MS: u64 = US / 1000;
fn spec() -> SessionSpec {
    SessionSpec {
        session_id: uuid::Uuid::new_v4().to_string(),
        legs: [
            LegLimits {
                identity: LegIdentity {
                    venue: "arcus".into(),
                    network: "testnet".into(),
                    account: format!("0x{}", "1".repeat(40)),
                    subaccount: Some(2),
                    market: "ANT-USD".into(),
                    market_id: 88,
                },
                quote_asset: "USDG".into(),
                quantity_step: d("0.001"),
                max_position_base: d("5"),
                max_order_notional: d("1000"),
                price_rules: VenuePriceRules::Arcus {
                    tick_size: d("0.01"),
                    tick_tiers: vec![],
                },
                quote_budget: d("2000"),
                max_fee_rate: d("0.001"),
            },
            LegLimits {
                identity: LegIdentity {
                    venue: "trade_xyz".into(),
                    network: "testnet".into(),
                    account: format!("0x{}", "2".repeat(40)),
                    subaccount: None,
                    market: "xyz:ANT".into(),
                    market_id: 110088,
                },
                quote_asset: "USDC".into(),
                quantity_step: d("0.001"),
                max_position_base: d("5"),
                max_order_notional: d("1000"),
                price_rules: VenuePriceRules::TradeXyz { sz_decimals: 3 },
                quote_budget: d("2000"),
                max_fee_rate: d("0.001"),
            },
        ],
        max_unhedged_base: d("2"),
        max_observation_age_us: 1_000_000,
    }
}
fn context(spec: &SessionSpec, leg: Leg) -> RequestContext {
    RequestContext {
        identity: spec.legs[index(leg)].identity.clone(),
        request_started_us: US + 1,
        received_at_us: US + 100,
    }
}
fn adapter(leg: Leg) -> NativeEvidence {
    let s = spec();
    NativeEvidence::new(&s, leg, context(&s, leg), US + 101).unwrap()
}
fn prepared(leg: Leg) -> PreparedOrder {
    PreparedOrder {
        intent: OrderIntent {
            client_order_id: "logical-1".into(),
            venue: if leg == Leg::Primary {
                "arcus"
            } else {
                "trade_xyz"
            }
            .into(),
            instrument: "ANT".into(),
            leg,
            side: HedgeSide::Buy,
            base_quantity: d("1"),
            limit_price: d("100"),
            immediate_or_cancel: leg == Leg::Hedge,
        },
        wire_client_id: format!(
            "{}{}",
            if leg == Leg::Primary { "a" } else { "0x" },
            "a".repeat(32)
        ),
        reduce_only: false,
        max_fill_price: d("100"),
    }
}
fn bytes(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).unwrap()
}
fn arcus_order() -> Value {
    json!({"orderId":"order-1","clientId":prepared(Leg::Primary).wire_client_id,"address":spec().legs[0].identity.account,"accountIndex":2,"marketId":88,"marketDisplayName":"ANT-USD","side":"BUY","status":"CANCELED","price":"100","originalSize":"1","remainingSize":"0.6","updatedAt":US})
}
fn hl_order() -> Value {
    json!({"status":"order","order":{"order":{"oid":7,"cloid":prepared(Leg::Hedge).wire_client_id,"coin":"xyz:ANT","side":"B","limitPx":"100","origSz":"1","sz":"0"},"status":"filled","statusTimestamp":MS}})
}
fn arcus_fill() -> Value {
    json!({"tradeId":"trade-1","orderId":"order-1","address":spec().legs[0].identity.account,"accountIndex":2,"marketId":88,"marketDisplayName":"ANT-USD","side":"BUY","originalSize":"1","size":"0.4","price":"99","fee":"-0.01","role":"MAKER","createdAt":US})
}
fn hl_fill() -> Value {
    json!({"tid":9,"oid":7,"coin":"xyz:ANT","side":"B","sz":"1","px":"99","time":MS,"fee":"0.02","builderFee":"0.01","feeToken":"USDC"})
}
fn query(leg: Leg) -> HistoryQuery {
    let t = if leg == Leg::Primary { US } else { MS };
    HistoryQuery {
        range: HistoryRange {
            from: t,
            to: t + 10,
        },
        limit: None,
        aggregate_by_time: false,
    }
}
#[test]
fn native_scope_and_request_clock_are_required() {
    let s = spec();
    for case in 0..6 {
        let mut c = context(&s, Leg::Primary);
        match case {
            0 => c.identity.account = s.legs[1].identity.account.clone(),
            1 => c.identity.network = "mainnet".into(),
            2 => c.identity.subaccount = Some(1),
            3 => c.identity.market_id = 89,
            4 => c.request_started_us = US - 2_000_000,
            _ => c.received_at_us = US + 102,
        };
        assert!(NativeEvidence::new(&s, Leg::Primary, c, US + 101).is_err());
    }
}
#[test]
fn arcus_cancel_maps_exact_remaining_without_releasing_inventory() {
    let a = adapter(Leg::Primary);
    let o = a
        .order(&bytes(&arcus_order()), &prepared(Leg::Primary))
        .unwrap()
        .unwrap();
    let t = o.terminal.unwrap();
    assert_eq!(t.status, OrderStatus::Canceled);
    assert_eq!(t.cumulative_filled, d("0.4"));
    assert_eq!(t.venue_at_us, US);
}
#[test]
fn arcus_order_identity_size_price_client_and_side_must_match() {
    let a = adapter(Leg::Primary);
    for (key, value) in [
        ("address", json!(spec().legs[1].identity.account)),
        ("accountIndex", json!(0)),
        ("marketId", json!(89)),
        ("marketDisplayName", json!("BTC-USD")),
        ("clientId", Value::Null),
        ("side", json!("SELL")),
        ("price", json!("101")),
        ("originalSize", json!("2")),
        ("remainingSize", json!("1.1")),
    ] {
        let mut v = arcus_order();
        v[key] = value;
        assert!(a.order(&bytes(&v), &prepared(Leg::Primary)).is_err());
    }
}
#[test]
fn missing_order_is_unknown_and_cancel_ack_is_not_fill_evidence() {
    let a = adapter(Leg::Hedge);
    assert!(
        a.order(br#"{"status":"unknownOid"}"#, &prepared(Leg::Hedge))
            .unwrap()
            .is_none()
    );
    let mut v = hl_order();
    v["order"]["status"] = json!("scheduledCancel");
    let o = a.order(&bytes(&v), &prepared(Leg::Hedge)).unwrap().unwrap();
    assert!(o.terminal.is_none());
    assert!(o.canceled_quantity_unproven);
}
#[test]
fn hl_order_uses_exact_namespace_cloid_and_millisecond_conversion() {
    let a = adapter(Leg::Hedge);
    let o = a
        .order(&bytes(&hl_order()), &prepared(Leg::Hedge))
        .unwrap()
        .unwrap();
    assert_eq!(o.terminal.unwrap().venue_at_us, US);
    for (key, v) in [
        ("coin", json!("ANT")),
        ("cloid", Value::Null),
        ("origSz", json!("0.9")),
        ("side", json!("A")),
    ] {
        let mut raw = hl_order();
        raw["order"]["order"][key] = v;
        assert!(a.order(&bytes(&raw), &prepared(Leg::Hedge)).is_err());
    }
}
#[test]
fn unknown_status_and_contradictory_terminal_fail_closed() {
    let a = adapter(Leg::Primary);
    for (status, remainder) in [
        ("NEW_CANCEL_KIND", "1"),
        ("FILLED", "0.1"),
        ("REJECTED", "0.6"),
        ("CANCELED", "-1"),
    ] {
        let mut v = arcus_order();
        v["status"] = json!(status);
        v["remainingSize"] = json!(remainder);
        assert!(a.order(&bytes(&v), &prepared(Leg::Primary)).is_err());
    }
}
#[test]
fn arcus_missing_fee_asset_stays_unknown_even_with_exact_fill_binding() {
    let a = adapter(Leg::Primary);
    let page = a
        .fills(
            &bytes(&json!({"fills":[arcus_fill()],"total":1})),
            query(Leg::Primary),
        )
        .unwrap();
    assert_eq!(page.fills[0].fee, d("-0.01"));
    assert!(page.fills[0].fee_asset.is_none());
    assert!(page.complete_through_venue_us.is_none());
    let ack = a
        .order(&bytes(&arcus_order()), &prepared(Leg::Primary))
        .unwrap()
        .unwrap()
        .acknowledgement;
    assert!(
        page.fills[0]
            .execution_fill(&prepared(Leg::Primary), &ack)
            .is_err()
    );
}
#[test]
fn hl_fees_include_builder_once_and_oid_binds_missing_cloid() {
    let a = adapter(Leg::Hedge);
    let page = a
        .fills(&bytes(&json!([hl_fill()])), query(Leg::Hedge))
        .unwrap();
    let p = prepared(Leg::Hedge);
    let mut ack = a
        .order(&bytes(&hl_order()), &p)
        .unwrap()
        .unwrap()
        .acknowledgement;
    let fill = page.fills[0].execution_fill(&p, &ack).unwrap();
    assert_eq!(fill.fee, d("0.02"));
    assert_eq!(fill.fee_asset, "USDC");
    assert_eq!(fill.venue_at_us, US);
    ack.exchange_order_id = "8".into();
    assert!(page.fills[0].execution_fill(&p, &ack).is_err());
    ack.exchange_order_id = "7".into();
    ack.identity.account = spec().legs[0].identity.account.clone();
    assert!(page.fills[0].execution_fill(&p, &ack).is_err());
}
#[test]
fn native_decimals_never_round_or_echo_private_values() {
    let a = adapter(Leg::Hedge);
    for value in [
        json!(0.1),
        json!("0.123456789012345678901234567890123456789"),
        json!("private-account-secret"),
    ] {
        let mut v = hl_fill();
        v["sz"] = value;
        let err = a
            .fills(&bytes(&json!([v])), query(Leg::Hedge))
            .err()
            .unwrap();
        assert_eq!(err.to_string(), "Malformed native response");
    }
}
#[test]
fn raw_duplicate_fields_and_oversized_responses_are_rejected() {
    let a = adapter(Leg::Hedge);
    let raw = bytes(&json!([hl_fill()]));
    let text = String::from_utf8(raw)
        .unwrap()
        .replace("\"oid\":7", "\"oid\":7,\"oid\":8");
    assert!(a.fills(text.as_bytes(), query(Leg::Hedge)).is_err());
    assert!(a.positions(&vec![b' '; MAX_RESPONSE_BYTES + 1]).is_err());
}
#[test]
fn duplicate_fills_are_idempotent_but_conflicts_stop_the_page() {
    let a = adapter(Leg::Hedge);
    let f = hl_fill();
    assert_eq!(
        a.fills(&bytes(&json!([f, f])), query(Leg::Hedge))
            .unwrap()
            .fills
            .len(),
        1
    );
    let mut bad = f.clone();
    bad["fee"] = json!("0.03");
    assert!(
        a.fills(&bytes(&json!([f, bad])), query(Leg::Hedge))
            .is_err()
    );
}
#[test]
fn history_bounds_units_aggregation_and_native_caps_are_checked() {
    let a = adapter(Leg::Primary);
    let mut q = query(Leg::Primary);
    q.range.from = MS;
    assert!(a.fills(br#"{"fills":[]}"#, q).is_err());
    for limit in [0, 1001] {
        let mut q = query(Leg::Primary);
        q.limit = Some(limit);
        assert!(a.fills(br#"{"fills":[]}"#, q).is_err());
    }
    let a = adapter(Leg::Hedge);
    let mut q = query(Leg::Hedge);
    q.aggregate_by_time = true;
    assert!(a.fills(b"[]", q).is_err());
    q.aggregate_by_time = false;
    q.limit = Some(100);
    assert!(a.fills(b"[]", q).is_err());
    q.limit = None;
    q.range.to = u64::MAX;
    assert!(a.fills(b"[]", q).is_err());
    let mut f = hl_fill();
    f["time"] = json!(MS - 1);
    assert!(a.fills(&bytes(&json!([f])), query(Leg::Hedge)).is_err());
}
#[test]
fn full_arcus_page_splits_integer_interval_without_skipping_ties() {
    let a = adapter(Leg::Primary);
    let mut q = query(Leg::Primary);
    q.limit = Some(1);
    let page = a
        .fills(&bytes(&json!({"fills":[arcus_fill()],"total":1})), q)
        .unwrap();
    assert_eq!(
        page.pagination,
        Pagination::Split([
            HistoryRange {
                from: US,
                to: US + 5
            },
            HistoryRange {
                from: US + 6,
                to: US + 10
            }
        ])
    );
    q.range.to = US;
    let page = a
        .fills(&bytes(&json!({"fills":[arcus_fill()]})), q)
        .unwrap();
    assert_eq!(page.pagination, Pagination::SaturatedTimestamp);
    assert!(page.complete_through_venue_us.is_none());
}
#[test]
fn full_hl_timestamp_bucket_cannot_be_paginated_away() {
    let a = adapter(Leg::Hedge);
    let rows: Vec<_> = (0..2000)
        .map(|i| {
            let mut f = hl_fill();
            f["tid"] = json!(i);
            f
        })
        .collect();
    let mut q = query(Leg::Hedge);
    q.range.to = MS;
    let page = a.fills(&bytes(&json!(rows)), q).unwrap();
    assert_eq!(page.pagination, Pagination::SaturatedTimestamp);
    assert_eq!(page.fills.len(), 2000);
}
#[test]
fn empty_short_page_never_proves_engine_watermark_or_retention() {
    for leg in [Leg::Primary, Leg::Hedge] {
        let a = adapter(leg);
        let raw = if leg == Leg::Primary {
            br#"{"fills":[],"total":0}"#.as_slice()
        } else {
            b"[]"
        };
        let p = a.fills(raw, query(leg)).unwrap();
        assert_eq!(p.pagination, Pagination::BelowCap);
        assert!(p.complete_through_venue_us.is_none());
    }
}
#[test]
fn arcus_positions_keep_signed_size_and_unknown_source_time() {
    let a = adapter(Leg::Primary);
    let p=a.positions(&bytes(&json!({"positions":{"88":{"address":spec().legs[0].identity.account,"accountIndex":2,"marketId":88,"marketDisplayName":"ANT-USD","side":"SHORT","size":"-0.4"}},"total":1}))).unwrap();
    assert_eq!(p.position_base, d("-0.4"));
    assert!(p.position_venue_at_us.is_none());
    assert!(p.fills_complete_through_venue_us.is_none());
    let p = a.positions(br#"{"positions":{},"total":0}"#).unwrap();
    assert!(p.position_base.is_zero());
    assert!(p.position_venue_at_us.is_none());
}
#[test]
fn positions_cannot_hide_identity_sign_duplicate_or_source_time_errors() {
    let a = adapter(Leg::Primary);
    for (key, val) in [
        ("address", json!(spec().legs[1].identity.account)),
        ("accountIndex", json!(0)),
        ("marketId", json!(89)),
        ("size", json!("-1")),
    ] {
        let mut v = json!({"address":spec().legs[0].identity.account,"accountIndex":2,"marketId":88,"marketDisplayName":"ANT-USD","side":"LONG","size":"1"});
        v[key] = val;
        assert!(a.positions(&bytes(&json!({"positions":{"88":v}}))).is_err());
    }
    let a = adapter(Leg::Hedge);
    for time in [0, u64::MAX] {
        assert!(
            a.positions(&bytes(&json!({"assetPositions":[],"time":time})))
                .is_err()
        );
    }
    assert!(a.positions(&bytes(&json!({"assetPositions":[{"position":{"coin":"xyz:ANT","szi":"1"}},{"position":{"coin":"xyz:ANT","szi":"0"}}],"time":MS}))).is_err());
}
#[test]
fn hl_snapshot_time_is_source_time_not_fill_coverage() {
    let a = adapter(Leg::Hedge);
    let p=a.positions(&bytes(&json!({"assetPositions":[{"position":{"coin":"xyz:ANT","szi":"1"}},{"position":{"coin":"xyz:TSLA","szi":"-2"}}],"time":MS}))).unwrap();
    assert_eq!(p.position_venue_at_us, Some(US));
    assert!(p.fills_complete_through_venue_us.is_none());
    assert_eq!(p.other_nonzero_positions, 1);
}
#[test]
fn normalized_hl_fill_reaches_durable_ledger_without_synthetic_ack_fills() {
    let s = spec();
    let path =
        std::env::temp_dir().join(format!("aleph-native-synthetic-{}", uuid::Uuid::new_v4()));
    let mut j = ExecutionJournal::create(&path, s.clone(), US - 1000).unwrap();
    let proof = Reconciliation {
        observed_sequence: j.sequence(),
        legs: s.legs.clone().map(|l| PositionEvidence {
            identity: l.identity,
            request_started_us: US - 900,
            received_at_us: US - 800,
            position_venue_at_us: US - 1000,
            fills_complete_through_venue_us: US - 1000,
            covered_trade_ids: vec![],
            open_order_ids: vec![],
            position_base: Decimal::ZERO,
        }),
    };
    j.reconcile(proof, US - 700).unwrap();
    let p = j
        .prepare_intent(prepared(Leg::Hedge).intent, false, None, US - 600)
        .unwrap();
    let a = NativeEvidence::new(&s, Leg::Hedge, context(&s, Leg::Hedge), US + 101).unwrap();
    let mut raw = hl_order();
    raw["order"]["order"]["cloid"] = json!(p.wire_client_id);
    let order = a.order(&bytes(&raw), &p).unwrap().unwrap();
    j.acknowledge(order.acknowledgement.clone(), US + 102)
        .unwrap();
    assert_eq!(j.positions(), [Decimal::ZERO; 2]);
    let page = a
        .fills(&bytes(&json!([hl_fill()])), query(Leg::Hedge))
        .unwrap();
    j.record_fill(
        page.fills[0]
            .execution_fill(&p, &order.acknowledgement)
            .unwrap(),
        US + 103,
    )
    .unwrap();
    j.record_terminal(order.terminal.unwrap(), US + 104)
        .unwrap();
    assert_eq!(j.positions(), [Decimal::ZERO, Decimal::ONE]);
    assert_eq!(j.fees_by_asset().unwrap()[&(1, "USDC".into())], d("0.02"));
    drop(j);
    std::fs::remove_dir_all(path).unwrap();
}

#[test]
fn arcus_conflicting_cumulative_and_duplicate_position_keys_are_rejected() {
    let a = adapter(Leg::Primary);
    let mut order = arcus_order();
    order["filledSize"] = json!("0.5");
    assert!(a.order(&bytes(&order), &prepared(Leg::Primary)).is_err());
    let p=json!({"address":spec().legs[0].identity.account,"accountIndex":2,"marketId":88,"marketDisplayName":"ANT-USD","side":"LONG","size":"1"}).to_string();
    let raw = format!("{{\"positions\":{{\"88\":{p},\"88\":{p}}}}}");
    assert!(a.positions(raw.as_bytes()).is_err());
}

#[test]
fn pending_cancel_is_an_acknowledgement_not_terminal_evidence() {
    let a = adapter(Leg::Primary);
    let mut raw = arcus_order();
    raw["status"] = json!("CANCEL_PENDING");
    let observed = a
        .order(&bytes(&raw), &prepared(Leg::Primary))
        .unwrap()
        .unwrap();
    assert!(observed.terminal.is_none());
}

#[test]
fn normalized_same_ms_hl_multi_fill_ioc_does_not_halt_session() {
    let s = spec();
    let path = std::env::temp_dir().join(format!("aleph-native-same-ms-{}", uuid::Uuid::new_v4()));
    let mut j = ExecutionJournal::create(&path, s.clone(), US - 1000).unwrap();
    let proof = Reconciliation {
        observed_sequence: j.sequence(),
        legs: s.legs.clone().map(|l| PositionEvidence {
            identity: l.identity,
            request_started_us: US - 900,
            received_at_us: US - 800,
            position_venue_at_us: US - 1000,
            fills_complete_through_venue_us: US - 1000,
            covered_trade_ids: vec![],
            open_order_ids: vec![],
            position_base: Decimal::ZERO,
        }),
    };
    j.reconcile(proof, US - 700).unwrap();
    let p = j
        .prepare_intent(prepared(Leg::Hedge).intent, false, None, US - 600)
        .unwrap();
    let a = NativeEvidence::new(&s, Leg::Hedge, context(&s, Leg::Hedge), US + 101).unwrap();
    let mut raw = hl_order();
    raw["order"]["order"]["cloid"] = json!(p.wire_client_id);
    let order = a.order(&bytes(&raw), &p).unwrap().unwrap();
    j.acknowledge(order.acknowledgement.clone(), US + 102)
        .unwrap();
    let mut f1 = hl_fill();
    f1["tid"] = json!(9001);
    f1["sz"] = json!("0.4");
    f1["fee"] = json!("0.008");
    let mut f2 = hl_fill();
    f2["tid"] = json!(9002);
    f2["sz"] = json!("0.6");
    f2["fee"] = json!("0.012");
    let page = a
        .fills(&bytes(&json!([f1, f2])), query(Leg::Hedge))
        .unwrap();
    assert_eq!(page.fills.len(), 2);
    let e1 = j
        .record_fill(
            page.fills[0]
                .execution_fill(&p, &order.acknowledgement)
                .unwrap(),
            US + 103,
        )
        .unwrap()
        .unwrap();
    assert!(e1.opened_base.is_some());
    let e2 = j
        .record_fill(
            page.fills[1]
                .execution_fill(&p, &order.acknowledgement)
                .unwrap(),
            US + 104,
        )
        .unwrap()
        .unwrap();
    assert_eq!(e2.opened_base, None);
    assert_eq!(e2.closed_base, None);
    assert_eq!(e2.crossed_zero, None);
    assert!(!j.requires_reconciliation());
    j.record_terminal(order.terminal.unwrap(), US + 105)
        .unwrap();
    assert_eq!(j.positions(), [Decimal::ZERO, Decimal::ONE]);
    assert_eq!(j.fees_by_asset().unwrap()[&(1, "USDC".into())], d("0.02"));
    // Can still submit subsequent hedge/primary intents mid-session without halting.
    let mut next_intent = prepared(Leg::Primary).intent;
    next_intent.client_order_id = "logical-2".into();
    j.prepare_intent(next_intent, false, None, US + 106)
        .unwrap();
    drop(j);
    std::fs::remove_dir_all(path).unwrap();
}

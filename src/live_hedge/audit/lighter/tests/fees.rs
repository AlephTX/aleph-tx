//! Declared Lighter fee rates through the real evidence parser and audit.

use super::super::super::{AuditStatus, Report};
use super::{account, d, kinds, order, v2};
use serde_json::{Value, json};

/// Order 11 (order_index 110) bought 0.01 at the history's terms.
fn filled(id: i64, size: &str) -> Value {
    let mut order = order(id, "filled", size, 40);
    order["is_ask"] = json!(false);
    order
}

/// One trade: account 7's order 110 on the bid, account 99's 990 on the
/// ask. `maker_ask` true makes account 7 the taker.
fn trade(id: u64, size: &str, maker_ask: bool) -> Value {
    json!({"trade_id": id, "tx_hash": "00", "type": "trade", "market_id": 4095,
        "size": size, "price": "2000.5", "usd_amount": "0", "ask_id": 990, "bid_id": 110,
        "ask_account_id": 99, "bid_account_id": 7, "ask_client_id": 0, "bid_client_id": 11,
        "is_maker_ask": maker_ask, "block_height": 1, "timestamp": 1,
        "maker_fee": 20, "taker_fee": 400})
}

fn with_trades(orders: Vec<Value>, captures: Vec<Vec<Value>>) -> Report {
    let ids: Vec<i64> = orders
        .iter()
        .map(|o| o["client_order_index"].as_i64().unwrap())
        .collect();
    let mut lighter = super::lighter(json!(ids), orders, Some(account(41, None)));
    lighter["trades"] = captures
        .into_iter()
        .enumerate()
        .map(|(i, trades)| {
            json!({"observed_at_ms": 3_000 + i as u64,
                "response": {"code": 200, "trades": trades}})
        })
        .collect();
    v2(lighter)
}

fn one(trade: Value) -> Report {
    with_trades(vec![filled(11, "0.01")], vec![vec![trade]])
}

fn fee(report: &Report) -> &crate::live_hedge::audit::LighterFeeReport {
    &report.primary_leg.orders[0].fee
}

/// Bound maker and taker: the role's own field is used, never the other
/// side's higher rate; the estimate is exact; the leg verdict is unchanged.
#[test]
fn bound_trade_derives_the_estimate_from_the_roles_own_rate() {
    // Account 7 bids; maker_ask=false makes the bid the maker.
    let maker = one(trade(501, "0.01", false));
    let fee = fee(&maker);
    assert_eq!(fee.status, "rate_derived", "{:?}", fee.reasons);
    assert_eq!(fee.source, "frozen_trades");
    assert_eq!((fee.order_index, fee.trade_id), (Some(110), Some(501)));
    assert_eq!(
        (fee.side, fee.role, fee.rate_field),
        (Some("bid"), Some("maker"), Some("maker_fee"))
    );
    assert_eq!((fee.rate_state, fee.rate_ppm), (Some("explicit"), Some(20)));
    // 20 / 1e6 * 2000.5 * 0.01 = 0.0004001
    assert_eq!(fee.estimated_fee, Some(d("0.0004001")));
    assert_eq!((fee.price, fee.size), (Some(d("2000.5")), Some(d("0.01"))));
    assert_eq!(maker.primary_leg.status, "explained");
    assert_eq!(
        maker.status,
        AuditStatus::Unresolved,
        "HL IOC still pending"
    );

    let taker = one(trade(502, "0.01", true));
    let fee = &taker.primary_leg.orders[0].fee;
    assert_eq!(
        (fee.role, fee.rate_field, fee.rate_ppm),
        (Some("taker"), Some("taker_fee"), Some(400))
    );
    // 400 / 1e6 * 2000.5 * 0.01 = 0.008002
    assert_eq!(fee.estimated_fee, Some(d("0.008002")));

    // Serialized as exact strings, as the CLI prints them.
    let json = serde_json::to_value(&taker.primary_leg.orders[0]).unwrap();
    assert_eq!(json["fee"]["estimated_fee"], "0.008002");
    assert_eq!(json["fee"]["rate_ppm"], 400);
}

/// Explicit zero and a rebate are rate-derived; an absent field (Lighter
/// omits zero or empty) and null stay unknown with no amount.
#[test]
fn explicit_zero_and_rebates_differ_from_absent_and_null_rates() {
    let mut zero = trade(503, "0.01", false);
    zero["maker_fee"] = json!(0);
    let zero = one(zero);
    let fee = fee(&zero);
    assert_eq!(
        (fee.status, fee.rate_state, fee.rate_ppm),
        ("rate_derived", Some("explicit"), Some(0))
    );
    assert_eq!(fee.estimated_fee, Some(d("0")));
    assert_eq!(
        serde_json::to_value(&zero.primary_leg.orders[0]).unwrap()["fee"]["estimated_fee"],
        "0"
    );

    let mut rebate = trade(504, "0.01", false);
    rebate["maker_fee"] = json!(-25);
    let rebate = one(rebate);
    assert_eq!(fee_of(&rebate).estimated_fee, Some(d("-0.00050012500")));

    let mut absent = trade(505, "0.01", false);
    absent.as_object_mut().unwrap().remove("maker_fee");
    let mut null = trade(506, "0.01", false);
    null["maker_fee"] = Value::Null;
    for (case, state) in [(absent, "absent"), (null, "null")] {
        let report = one(case);
        let fee = fee_of(&report);
        assert_eq!((fee.status, fee.rate_state), ("unknown", Some(state)));
        assert_eq!((fee.rate_ppm, fee.estimated_fee), (None, None));
        assert_eq!(fee.role, Some("maker"), "{state}: role still shown");
        // Unknown is not a contradiction: the audit stays as it was.
        assert!(!kinds(&report).contains(&"lighter_invalid_fee_evidence"));
        assert_eq!(report.primary_leg.status, "explained");
    }
    // The other side's rate never fills in a missing role rate.
    let mut other_side_only = trade(507, "0.01", false);
    other_side_only.as_object_mut().unwrap().remove("maker_fee");
    assert_eq!(fee_of(&one(other_side_only)).rate_ppm, None);
}

fn fee_of(report: &Report) -> &crate::live_hedge::audit::LighterFeeReport {
    &report.primary_leg.orders[0].fee
}

/// Wrong account, market, client order or side for the bound order index
/// is a contradiction: invalid, never an estimate. The order state stays.
#[test]
fn identity_contradictions_are_invalid_not_estimated() {
    let cases: [(&str, Value); 5] = [
        ("account", json!({"bid_account_id": 8})),
        ("market", json!({"market_id": 4096})),
        ("client order", json!({"bid_client_id": 12})),
        ("both sides", json!({"ask_id": 110})),
        (
            "side",
            json!({"ask_id": 110, "bid_id": 990, "ask_account_id": 7,
            "bid_account_id": 99, "ask_client_id": 11, "bid_client_id": 0}),
        ),
    ];
    for (case, patch) in cases {
        let mut bad = trade(510, "0.01", false);
        for (key, value) in patch.as_object().unwrap() {
            bad[key] = value.clone();
        }
        let report = one(bad);
        let fee = fee_of(&report);
        assert_eq!(fee.status, "invalid", "{case}: {:?}", fee.reasons);
        assert_eq!(fee.estimated_fee, None, "{case}");
        assert!(
            kinds(&report).contains(&"lighter_invalid_fee_evidence"),
            "{case}"
        );
        assert_eq!(report.status, AuditStatus::Invalid, "{case}");
        assert_eq!(report.primary_leg.orders[0].state, "terminal", "{case}");
        assert!(!report.restart_permitted);
    }
    // Absent account on our side: the trade does not confirm ownership.
    let mut unowned = trade(511, "0.01", false);
    unowned.as_object_mut().unwrap().remove("bid_account_id");
    let fee = fee_of(&one(unowned)).clone();
    assert_eq!((fee.status, fee.estimated_fee), ("unknown", None));
}

/// No order index, no naming trade, a short or split fill and liquidation
/// trades stay unknown; an excess fill or a trade on an unfilled order is
/// invalid. A repeated identical trade adds no coverage.
#[test]
fn coverage_must_be_one_exact_trade() {
    let mut no_index = filled(11, "0.01");
    no_index.as_object_mut().unwrap().remove("order_index");
    let report = with_trades(vec![no_index], vec![vec![trade(520, "0.01", false)]]);
    assert_eq!(fee_of(&report).status, "unknown");
    assert_eq!(fee_of(&report).order_index, None);

    let mut elsewhere = trade(521, "0.01", false);
    elsewhere["bid_id"] = json!(111);
    let fee = fee_of(&one(elsewhere)).clone();
    assert_eq!((fee.status, fee.trade_id), ("unknown", None));

    let short = one(trade(522, "0.004", false));
    assert_eq!(fee_of(&short).status, "unknown");
    assert!(fee_of(&short).reasons[0].contains("cover 0.004"));

    let split = with_trades(
        vec![filled(11, "0.01")],
        vec![vec![trade(523, "0.004", false), trade(524, "0.006", false)]],
    );
    assert_eq!(fee_of(&split).status, "unknown");
    assert!(fee_of(&split).reasons[0].contains("split over 2 trades"));

    let excess = with_trades(
        vec![filled(11, "0.01")],
        vec![vec![trade(525, "0.01", false), trade(526, "0.004", false)]],
    );
    assert_eq!(fee_of(&excess).status, "invalid");
    assert_eq!(excess.status, AuditStatus::Invalid);

    // The same trade captured twice is one trade: still exactly covered.
    let repeated = with_trades(
        vec![filled(11, "0.01")],
        vec![
            vec![trade(527, "0.005", false)],
            vec![trade(527, "0.005", false)],
        ],
    );
    assert_eq!(fee_of(&repeated).status, "unknown");
    assert!(fee_of(&repeated).reasons[0].contains("cover 0.005"));

    let mut liquidation = trade(528, "0.01", false);
    liquidation["type"] = json!("liquidation");
    let fee = fee_of(&one(liquidation)).clone();
    assert_eq!(
        (fee.status, fee.role, fee.estimated_fee),
        ("unknown", None, None)
    );

    let unfilled = with_trades(
        vec![order(11, "canceled", "0", 0)],
        vec![vec![trade(529, "0.01", false)]],
    );
    assert_eq!(fee_of(&unfilled).status, "invalid");
    let nothing = with_trades(vec![order(11, "canceled", "0", 0)], vec![vec![]]);
    assert_eq!(fee_of(&nothing).status, "no_fill");
    assert_eq!(nothing.status, AuditStatus::Unresolved);
}

/// Conflicting repeats (any raw field, read or not), malformed captures,
/// out-of-range or non-integer rates and unrepresentable amounts are
/// invalid; none yields an estimate.
#[test]
fn contradictory_or_unrepresentable_trades_are_invalid() {
    let base = trade(530, "0.01", false);
    let mut fee_changed = base.clone();
    fee_changed["maker_fee"] = json!(21);
    let mut unread_changed = base.clone();
    unread_changed["tx_hash"] = json!("ff");
    for (case, other) in [("fee", fee_changed), ("unread field", unread_changed)] {
        let report = with_trades(
            vec![filled(11, "0.01")],
            vec![vec![base.clone()], vec![other]],
        );
        assert_eq!(fee_of(&report).status, "invalid", "{case}");
        assert!(kinds(&report).contains(&"lighter_invalid_trades"), "{case}");
        assert!(
            fee_of(&report).reasons[0].contains("conflicting fields"),
            "{case}"
        );
        assert_eq!(report.status, AuditStatus::Invalid, "{case}");
    }

    let mut rates = Vec::new();
    for rate in [
        json!(1_000_001),
        json!(-1_000_001),
        json!(i64::MIN),
        json!(2.5),
        json!("20"),
    ] {
        let mut bad = trade(531, "0.01", false);
        bad["maker_fee"] = rate.clone();
        let report = one(bad);
        let fee = fee_of(&report);
        assert_eq!(
            (fee.status, fee.rate_state),
            ("invalid", Some("invalid")),
            "{rate}"
        );
        assert_eq!(fee.estimated_fee, None, "{rate}");
        rates.push(report.status);
    }
    assert!(rates.iter().all(|s| *s == AuditStatus::Invalid));
    // The full ±FeeTick range is accepted.
    let mut full = trade(532, "0.01", false);
    full["maker_fee"] = json!(-1_000_000);
    assert_eq!(fee_of(&one(full)).estimated_fee, Some(d("-20.005")));

    // Excess precision in the trade or the history fill never rounds.
    let mut long_size = trade(533, "0.01000000000000000000000000001", false);
    long_size["size"] = json!("0.01000000000000000000000000001");
    let report = one(long_size);
    assert_eq!(fee_of(&report).status, "invalid");
    assert!(kinds(&report).contains(&"lighter_invalid_trades"));
    let history = with_trades(
        vec![filled(11, "0.01000000000000000000000000001")],
        vec![vec![trade(534, "0.01", false)]],
    );
    assert_eq!(
        fee_of(&history).status,
        "invalid",
        "{:?}",
        fee_of(&history).reasons
    );
    // A product that no Decimal holds exactly is refused, not rounded.
    let mut wide = trade(535, "0.0000000000001", false);
    wide["price"] = json!("1234567.0000000000001");
    wide["maker_fee"] = json!(7);
    let report = with_trades(vec![filled(11, "0.0000000000001")], vec![vec![wide]]);
    let fee = fee_of(&report);
    assert_eq!(fee.status, "invalid", "{:?}", fee.reasons);
    assert!(fee.reasons[0].contains("not exactly representable"));

    let malformed = super::lighter(
        json!([11]),
        vec![filled(11, "0.01")],
        Some(account(41, None)),
    );
    for response in [
        json!({"code": 429, "trades": []}),
        json!({"code": 200}),
        json!([]),
    ] {
        let mut evidence = malformed.clone();
        evidence["trades"] = json!([{"observed_at_ms": 3_000, "response": response}]);
        let report = v2(evidence);
        assert!(
            kinds(&report).contains(&"lighter_invalid_trades"),
            "{response}"
        );
        assert_eq!(fee_of(&report).status, "invalid");
    }
}

/// Live and missing orders get no fee view; an order index listed for two
/// client orders binds neither; without trades nothing changes.
#[test]
fn only_uniquely_indexed_terminal_orders_are_replayed_and_old_inputs_still_run() {
    let report = with_trades(
        vec![filled(11, "0.01"), order(12, "open", "0", 0)],
        vec![vec![trade(540, "0.01", false)]],
    );
    let fees: Vec<_> = report
        .primary_leg
        .orders
        .iter()
        .map(|o| o.fee.status)
        .collect();
    assert_eq!(fees, ["rate_derived", "unknown"]);

    let mut twin = order(12, "filled", "0.01", 40);
    twin["order_index"] = json!(110);
    let shared = with_trades(
        vec![filled(11, "0.01"), twin],
        vec![vec![trade(541, "0.01", false)]],
    );
    for order in &shared.primary_leg.orders {
        assert_eq!(order.fee.status, "invalid", "{}", order.client_order_id);
    }

    // Old evidence without `trades`: fee views are `not_provided`, the
    // serialized order keeps every earlier field, verdicts unchanged.
    let legacy = v2(super::lighter(
        json!([11]),
        vec![filled(11, "0.01")],
        Some(account(41, None)),
    ));
    let order = &legacy.primary_leg.orders[0];
    assert_eq!(
        (order.fee.status, order.fee.source),
        ("not_provided", "none")
    );
    assert_eq!(legacy.primary_leg.status, "explained");
    assert!(
        !kinds(&legacy)
            .iter()
            .any(|k| k.contains("fee") || k.contains("trades"))
    );
    let json = serde_json::to_value(order).unwrap();
    for key in [
        "client_order_id",
        "source",
        "journal_line",
        "state",
        "venue_status",
        "filled",
        "fill_watermark",
        "reason",
        "fee",
    ] {
        assert!(json.get(key).is_some(), "{key}");
    }
}

/// The order's time in force cross-checks the role the trade implies; a
/// declared integrator fee leaves the rate-only estimate unknown.
#[test]
fn time_in_force_and_integrator_fees_bound_the_estimate() {
    let with_tif = |tif: &str| {
        let mut order = filled(11, "0.01");
        order["time_in_force"] = json!(tif);
        order
    };
    // Post-only maker and IOC taker agree with their roles.
    for (tif, maker_ask, role) in [
        ("post-only", false, "maker"),
        ("immediate-or-cancel", true, "taker"),
        ("good-till-time", true, "taker"),
    ] {
        let report = with_trades(
            vec![with_tif(tif)],
            vec![vec![trade(550, "0.01", maker_ask)]],
        );
        let fee = fee_of(&report);
        assert_eq!(
            (fee.status, fee.role),
            ("rate_derived", Some(role)),
            "{tif}"
        );
    }
    // A post-only taker or an IOC maker contradicts the order.
    for (tif, maker_ask) in [("post-only", true), ("immediate-or-cancel", false)] {
        let report = with_trades(
            vec![with_tif(tif)],
            vec![vec![trade(551, "0.01", maker_ask)]],
        );
        let fee = fee_of(&report);
        assert_eq!((fee.status, fee.estimated_fee), ("invalid", None), "{tif}");
        assert_eq!(report.status, AuditStatus::Invalid, "{tif}");
    }
    let mut bad_tif = filled(11, "0.01");
    bad_tif["time_in_force"] = json!(2);
    let report = with_trades(vec![bad_tif], vec![vec![trade(552, "0.01", false)]]);
    assert_eq!(fee_of(&report).status, "invalid");

    // Integrator fee for our role: unknown; zero or the other role's: no
    // effect.
    let mut ours = trade(553, "0.01", false);
    ours["integrator_maker_fee"] = json!(5);
    let fee = fee_of(&one(ours)).clone();
    assert_eq!(
        (fee.status, fee.rate_ppm, fee.estimated_fee),
        ("unknown", None, None)
    );
    assert!(fee.reasons[0].contains("integrator_maker_fee 5"));
    let mut zero = trade(554, "0.01", false);
    zero["integrator_maker_fee"] = json!(0);
    zero["integrator_taker_fee"] = json!(9);
    assert_eq!(fee_of(&one(zero)).status, "rate_derived");
}

/// Trades in evidence for another network are never replayed: the leg
/// stops at the identity check, before any order or fee view.
#[test]
fn trades_for_another_network_are_never_replayed() {
    let mut lighter = super::lighter(
        json!([11]),
        vec![filled(11, "0.01")],
        Some(account(41, None)),
    );
    lighter["trades"] = json!([{"observed_at_ms": 3_000,
        "response": {"code": 200, "trades": [trade(560, "0.01", false)]}}]);
    let evidence = serde_json::from_value(json!({"network": "Mainnet",
        "hyperliquid_account": super::HL, "coin": "ETH", "lighter": lighter}))
    .unwrap();
    let journal = super::journal(&[super::start(2, Some(4095)), super::pending_intent()]);
    let report = super::super::super::audit(&journal, Some(&evidence));
    assert_eq!(report.primary_leg.status, "invalid");
    assert!(report.primary_leg.orders.is_empty());
    assert!(kinds(&report).contains(&"evidence_identity"));
    assert_eq!(report.status, AuditStatus::Invalid);
}

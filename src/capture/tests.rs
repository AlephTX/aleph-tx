use super::*;
use serde_json::{Value, json};

const T: u64 = 1_800_000_000_000;

fn config() -> CaptureConfig {
    let mut config: CaptureConfig =
        serde_json::from_str(include_str!("../../examples/eth_capture.json")).unwrap();
    config.sample_ms = 100;
    config
}

fn row(source: &str, at: u64, payload: Value) -> Value {
    json!({"source":source,"event":"message","received_at_ms":at,"available_at_ms":at,"payload":payload})
}

fn footer(at: u64) -> Value {
    json!({"source":"collector","event":"end","received_at_ms":at,"available_at_ms":at})
}

fn lighter(at: u64, source_at: u64, nonce: u64, previous: u64, snapshot: bool) -> Value {
    row(
        "lighter",
        at,
        json!({
            "type": if snapshot { "subscribed/order_book" } else { "update/order_book" },
            "channel":"order_book:0",
            "order_book":{"nonce":nonce,"begin_nonce":previous,"last_updated_at":source_at*1000,
                "bids":[{"price":"1999.9","size":"4"}],"asks":[{"price":"2000.1","size":"5"}]}
        }),
    )
}

fn hl(source: &str, at: u64) -> Value {
    row(
        source,
        at,
        json!({"channel":"l2Book","data":{
            "coin":"ETH","time":at-1,"levels":[[{"px":"1999.9","sz":"4"}],[{"px":"2000.1","sz":"5"}]]
        }}),
    )
}

fn text(rows: &[Value]) -> String {
    rows.iter().map(|row| format!("{row}\n")).collect()
}

fn replay(config: &CaptureConfig, rows: &[Value]) -> (Coverage, Vec<Value>) {
    let mut out = Vec::new();
    let coverage = compile(
        config,
        text(rows).as_bytes(),
        &mut out,
        &VenueRegistry::default(),
    )
    .unwrap();
    let frames = String::from_utf8(out)
        .unwrap()
        .lines()
        .skip(1)
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    (coverage, frames)
}

#[test]
fn nonce_gaps_require_snapshot_but_clock_rejections_preserve_continuity() {
    let rows = [
        lighter(T, T - 1, 10, 0, true),
        hl("hyperliquid", T),
        lighter(T + 100, T + 120, 11, 10, false),
        lighter(T + 200, T + 199, 12, 11, false),
        lighter(T + 300, T + 299, 14, 13, false),
        lighter(T + 400, T + 399, 15, 14, false),
        lighter(T + 500, T + 499, 16, 0, true),
        footer(T + 600),
    ];
    let (coverage, frames) = replay(&config(), &rows);
    assert_eq!(coverage.lighter_nonce_gaps, [1, 0]);
    assert_eq!(coverage.clock_rejections, [1, 0]);
    assert_eq!(coverage.max_rejected_clock_lead_ms, [20, 0]);
    assert_eq!(coverage.leg_book_updates, [3, 1]);
    let tradeable: Vec<_> = frames
        .iter()
        .map(|f| f["legs"][0]["book"]["tradable"].as_bool().unwrap())
        .collect();
    assert_eq!(tradeable, [true, false, true, false, false, true]);
}

#[test]
fn l2_deletions_and_receipt_time_bounds_preserve_exact_levels() {
    let mut config = config();
    config.legs[0].source_clock_ahead_bound_ms = 50;
    let mut initial = lighter(T, T + 20, 10, 0, true);
    initial["payload"]["order_book"]["bids"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "price":"1999.85","size":"1.2345"
        }));
    let mut delta = lighter(T + 100, T + 120, 11, 10, false);
    delta["payload"]["order_book"]["bids"][0]["size"] = json!("0");
    let (coverage, frames) = replay(&config, &[initial, delta, footer(T + 200)]);
    assert_eq!(coverage.clock_rejections, [0, 0]);
    assert_eq!(frames[0]["legs"][0]["book"]["timestamp_ms"], T - 30);
    assert_eq!(frames[1]["legs"][0]["book"]["timestamp_ms"], T + 70);
    assert_eq!(frames[1]["legs"][0]["book"]["bids"][0]["price"], "1999.85");
    assert_eq!(
        frames[1]["legs"][0]["book"]["bids"][0]["contracts"],
        "1.2345"
    );
}

#[test]
fn receipt_order_and_context_policy_prevent_reference_lookahead() {
    let mut config = config();
    config.reference = ReferenceFeed::External {
        observations: vec![ReferenceObservation {
            available_at_ms: T + 100,
            oracle: Oracle {
                price: Decimal::from(2000),
                timestamp_ms: T - 10,
                mode: OracleMode::LiveExternal,
            },
        }],
    };
    config.legs[1].accept_receipt_time_for_oracle = false;
    let context = row(
        "hyperliquid",
        T,
        json!({"channel":"activeAssetCtx","data":{
            "coin":"ETH","ctx":{"oraclePx":"2000","funding":"0.0000125"}
        }}),
    );
    let (_, frames) = replay(
        &config,
        &[
            context,
            hl("hyperliquid", T),
            hl("hyperliquid", T + 100),
            footer(T + 200),
        ],
    );
    assert_eq!(frames[0]["reference"]["timestamp_ms"], 0);
    assert_eq!(frames[1]["reference"]["timestamp_ms"], T - 10);
    assert_eq!(frames[1]["legs"][1]["oracle"]["mode"], "internal");
    assert_eq!(frames[1]["legs"][1]["oracle"]["timestamp_ms"], 0);
}

#[test]
fn forecast_expires_at_the_payment_and_never_reads_settlement_as_prediction() {
    let mut config = config();
    config.legs[0].funding = Some(FundingRule {
        raw_rate_scale: "0.01".parse().unwrap(),
        uncertainty_bps: Decimal::ONE,
    });
    config.settlements[0].push(FundingSettlement {
        at_ms: T,
        rate: "0.00003".parse().unwrap(),
        oracle_price: Decimal::from(2000),
    });
    let stats = row(
        "lighter",
        T - 200,
        json!({"type":"update/market_stats","channel":"market_stats:0","timestamp":T-201,
        "market_stats":{"market_id":0,"symbol":"ETH","index_price":"2000","mark_price":"1999.5","current_funding_rate":"0.001","funding_rate":"0.40","funding_timestamp":T-3_600_000+1}
        }),
    );
    let stale_republication = row(
        "lighter",
        T + 100,
        json!({"type":"update/market_stats","channel":"market_stats:0","timestamp":T+99,
            "market_stats":{"market_id":0,"symbol":"ETH","index_price":"2000","mark_price":"1999.5","current_funding_rate":"0.001","funding_timestamp":T-3_600_000+1}
        }),
    );
    let (coverage, frames) = replay(
        &config,
        &[
            stats,
            hl("hyperliquid", T),
            stale_republication,
            footer(T + 200),
        ],
    );
    assert_eq!(frames[0]["legs"][0]["forecast"]["payments"][0]["at_ms"], T);
    assert_eq!(
        frames[0]["legs"][0]["forecast"]["payments"][0]["rate"],
        "0.00001"
    );
    assert_eq!(
        frames[0]["legs"][0]["forecast"]["payments"][0]["oracle_price"],
        "1999.5"
    );
    assert!(
        frames[0]["legs"][0]["settled"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(frames[1]["legs"][0]["forecast"].is_null());
    assert!(frames[2]["legs"][0]["forecast"].is_null());
    assert_eq!(frames[1]["legs"][0]["settled"][0]["rate"], "0.00003");
    assert_eq!(coverage.settlement_records_written, [1, 0]);
}

#[test]
fn snapshot_selection_does_not_mix_transports_or_drop_the_rest_book_on_ws_gap() {
    let mut config = config();
    config.legs[1].use_hyperliquid_snapshots = true;
    let rows = [
        hl("hyperliquid", T),
        hl("hyperliquid_snapshot", T + 100),
        json!({"source":"hyperliquid","event":"disconnect","received_at_ms":T+200,"available_at_ms":T+200}),
        footer(T + 300),
    ];
    let (coverage, frames) = replay(&config, &rows);
    assert_eq!(coverage.leg_book_updates, [0, 1]);
    assert_eq!(frames[0]["legs"][1]["book"]["tradable"], false);
    assert_eq!(frames[2]["legs"][1]["book"]["timestamp_ms"], T + 99);
}

#[test]
fn freshest_book_keeps_source_time_monotonic_across_transport_failures() {
    let mut config = config();
    config.legs[1].hyperliquid_book_mode = Some(HyperliquidBookMode::Freshest);
    let snapshot = "hyperliquid_snapshot";
    let ws = "hyperliquid";
    let stale = |source, at, time| {
        let mut event = hl(source, at);
        event["payload"]["data"]["time"] = json!(time);
        event
    };
    let disconnect = |source, at| json!({"source":source,"event":"disconnect","received_at_ms":at,"available_at_ms":at});
    let rows = [
        lighter(T, T - 1, 10, 0, true),
        hl(ws, T),
        stale(snapshot, T + 100, T - 1),
        hl(snapshot, T + 200),
        disconnect(ws, T + 300),
        stale(ws, T + 400, T + 100),
        disconnect(snapshot, T + 500),
        stale(ws, T + 600, T + 199),
        stale(ws, T + 700, T + 650),
        stale(snapshot, T + 800, T + 900),
        footer(T + 900),
    ];
    let (coverage, frames) = replay(&config, &rows);
    let times: Vec<_> = frames
        .iter()
        .map(|f| f["legs"][1]["book"]["timestamp_ms"].as_u64().unwrap())
        .collect();
    assert_eq!(
        times,
        [
            T - 1,
            T - 1,
            T + 199,
            T + 199,
            T + 199,
            0,
            0,
            T + 650,
            T + 650
        ]
    );
    assert_eq!(coverage.leg_book_updates, [1, 3]);
    assert_eq!(coverage.leg_snapshot_updates, [0, 1]);
    assert_eq!(coverage.leg_fresh_book_frames, [9, 7]);
    assert_eq!(coverage.clock_rejections, [0, 1]);
    assert_eq!(coverage.book_age_at_availability_ms[1].observations, 3);
    assert_eq!(
        coverage.book_age_at_availability_ms[1].p50_upper_ms,
        Some(1)
    );
    assert_eq!(
        coverage.book_age_at_availability_ms[1].p95_upper_ms,
        Some(50)
    );
    assert_eq!(coverage.fresh_book_skew_ms.observations, 7);
    assert_eq!(coverage.fresh_book_skew_ms.p50_upper_ms, Some(200));
}

#[test]
fn rejects_truncated_input_and_equity_entries_without_the_underlying_feed() {
    let input = text(&[hl("hyperliquid", T)]);
    let err = compile(
        &config(),
        input.as_bytes(),
        Vec::new(),
        &VenueRegistry::default(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("capture incomplete"));
    let mut config: CaptureConfig =
        serde_json::from_str(include_str!("../../examples/nvda_capture.json")).unwrap();
    config.paper_entries = true;
    let input = text(&[hl("hyperliquid", T), footer(T + 100)]);
    let err = compile(
        &config,
        input.as_bytes(),
        Vec::new(),
        &VenueRegistry::default(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("external reference and session"));
}

use super::*;

const SLOT: u64 = 1_793_991_600_000;
const LLP: u64 = 281_474_976_710_654;

fn evidence() -> (Policy, RawEvidence) {
    let policy: Policy = serde_json::from_str(include_str!(
        "../../../examples/eth_lighter_funding_evidence.json"
    ))
    .unwrap();
    let raw: RawEvidence = serde_json::from_value(json!({
        "first_slot_ms": SLOT, "last_slot_ms": SLOT, "market_id": 0, "symbol": "ETH",
        "market_history": {"code":200,"resolution":"1h","fundings":[{
            "timestamp":SLOT/1000,"value":"0.02677332","rate":"0.0012","direction":"long"
        }]},
        "public_ledger": {(LLP.to_string()): {"code":200,"position_fundings":[{
            "timestamp":SLOT/1000,"market_id":0,"funding_id":1819291,"change":"10.557427",
            "rate":"0.000012","position_size":"394.3264","position_side":"short","discount":"0.000000"
        }]}}
    })).unwrap();
    (policy, raw)
}

#[test]
fn market_percentage_and_unit_value_agree_with_pool_receipt() {
    let (policy, raw) = evidence();
    let audit = reconcile(&policy, &raw).unwrap();
    assert_eq!(audit.slots[0].witnesses, 1);
    let payment = audit.witnessed_settlements[0];
    assert_eq!(payment.rate, "0.000012".parse::<Decimal>().unwrap());
    assert!(payment.valid(SCHEDULE, policy.funding_guard_ms));
    assert!(payment.long_cash_low_per_base <= "-0.02677332".parse::<Decimal>().unwrap());
    assert!(payment.long_cash_high_per_base >= "-0.02677332".parse::<Decimal>().unwrap());
}

#[test]
fn discount_and_market_cash_mismatch_cannot_validate_a_slot() {
    let (policy, mut raw) = evidence();
    raw.public_ledger.get_mut(&LLP).unwrap().position_fundings[0].discount = Decimal::ONE;
    assert_eq!(
        reconcile(&policy, &raw).unwrap().slots[0].rejection,
        Some(Rejection::TooFewWitnesses)
    );

    let (policy, mut raw) = evidence();
    raw.market_history.fundings[0].value *= Decimal::TWO;
    assert_eq!(
        reconcile(&policy, &raw).unwrap().slots[0].rejection,
        Some(Rejection::InconsistentCashBounds)
    );
}

#[test]
fn short_payer_proof_credits_the_long_unit() {
    let (policy, mut raw) = evidence();
    let market = &mut raw.market_history.fundings[0];
    market.rate = "-0.0025".parse().unwrap();
    market.value = "0.05".parse().unwrap();
    market.direction = "short".into();
    let receipt = &mut raw.public_ledger.get_mut(&LLP).unwrap().position_fundings[0];
    receipt.rate = "-0.000025".parse().unwrap();
    receipt.position_size = Decimal::TWO;
    receipt.change = "-0.1".parse().unwrap();
    let expected_rate = receipt.rate;
    let audit = reconcile(&policy, &raw).unwrap();
    let payment = audit.witnessed_settlements[0];
    assert_eq!(payment.rate, expected_rate);
    assert!(payment.long_cash_low_per_base > Decimal::ZERO);
    assert!(payment.valid(SCHEDULE, policy.funding_guard_ms));
}

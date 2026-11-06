use super::*;

const SLOT: u64 = 1_793_991_600_000;
const A: &str = "0x010461c14e146ac35fe42271bdc1134ee31c703a";
const B: &str = "0x31ca8395cf837de08b24da3f660e77761dfb974b";

fn decimal(value: &str) -> Decimal {
    value.parse().unwrap()
}

fn evidence() -> (Policy, RawEvidence) {
    let policy = Policy {
        coin: "ETH".into(),
        schedule: FundingSchedule {
            interval_ms: 3_600_000,
            offset_ms: 0,
        },
        funding_guard_ms: 1_000,
        cash_tolerance_quote: decimal("0.000001"),
        min_witnesses: 2,
        public_witness_addresses: vec![A.into(), B.into()],
    };
    let time = SLOT + 28;
    let funding_rate = decimal("0.0000125");
    let event = |usdc, szi| LedgerEvent {
        time,
        hash: None,
        delta: LedgerDelta {
            kind: "funding".into(),
            coin: "ETH".into(),
            usdc: decimal(usdc),
            szi: decimal(szi),
            funding_rate,
            n_samples: None,
        },
    };
    let raw = RawEvidence {
        first_slot_ms: SLOT,
        last_slot_ms: SLOT,
        market_rates: vec![MarketRate {
            coin: "ETH".into(),
            time,
            funding_rate,
            premium: None,
        }],
        public_ledger: BTreeMap::from([
            (A.into(), vec![event("-0.041615", "1.492")]),
            (B.into(), vec![event("0.052036", "-1.8656")]),
        ]),
    };
    (policy, raw)
}

#[test]
fn opposite_positions_constrain_same_settlement_cashflow() {
    let (policy, raw) = evidence();
    let audit = reconcile(&policy, &raw).unwrap();
    assert_eq!(audit.slots[0].witnesses, 2);
    assert_eq!(audit.slots[0].rejection, None);
    let settlement = audit.witnessed_settlements[0];
    assert_eq!(settlement.at_ms, SLOT);
    assert_eq!(settlement.exchange_time_ms, SLOT + 28);
    assert!(settlement.valid(policy.schedule, policy.funding_guard_ms));
    // For each public position, the receipt intersects the common unit
    // cash interval within the allowed reporting tolerance.
    for row in raw.public_ledger.values().flatten() {
        let a = settlement.long_cash_low_per_base * row.delta.szi;
        let b = settlement.long_cash_high_per_base * row.delta.szi;
        let (low, high) = (a.min(b), a.max(b));
        assert!(row.delta.usdc + policy.cash_tolerance_quote >= low);
        assert!(row.delta.usdc - policy.cash_tolerance_quote <= high);
    }
    assert!(
        settlement.long_cash_high_per_base - settlement.long_cash_low_per_base
            < decimal("0.000001")
    );
}

#[test]
fn timestamp_and_rate_conflicts_do_not_pass_as_coverage() {
    let (policy, mut raw) = evidence();
    raw.public_ledger.get_mut(B).unwrap()[0].time += 1;
    let audit = reconcile(&policy, &raw).unwrap();
    assert_eq!(audit.slots[0].rejection, Some(Rejection::TooFewWitnesses));
    assert!(audit.witnessed_settlements.is_empty());

    let (policy, mut raw) = evidence();
    raw.public_ledger.get_mut(B).unwrap()[0].delta.funding_rate = decimal("0.00005");
    assert_eq!(
        reconcile(&policy, &raw).unwrap().slots[0].rejection,
        Some(Rejection::InconsistentLedgerPayment)
    );
}

#[test]
fn disjoint_cash_intervals_and_duplicate_rows_are_rejected() {
    let (policy, mut raw) = evidence();
    raw.public_ledger.get_mut(B).unwrap()[0].delta.usdc = decimal("0.09");
    assert_eq!(
        reconcile(&policy, &raw).unwrap().slots[0].rejection,
        Some(Rejection::InconsistentCashBounds)
    );
    let (policy, mut raw) = evidence();
    let rows = raw.public_ledger.get_mut(B).unwrap();
    rows.push(rows[0].clone());
    assert_eq!(
        reconcile(&policy, &raw).unwrap().slots[0].rejection,
        Some(Rejection::DuplicateLedgerPayment)
    );
}

#[test]
fn rate_gaps_are_explicit_and_a_witness_must_be_distinct() {
    let (mut policy, mut raw) = evidence();
    raw.last_slot_ms += 3_600_000;
    let audit = reconcile(&policy, &raw).unwrap();
    assert_eq!(audit.slots[1].rejection, Some(Rejection::MissingMarketRate));
    policy.public_witness_addresses[1] = A.into();
    assert!(reconcile(&policy, &raw).is_err());
}

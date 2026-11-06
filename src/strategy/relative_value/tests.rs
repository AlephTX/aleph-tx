use super::*;

fn d(value: &str) -> Decimal {
    value.parse().unwrap()
}

fn crypto() -> PaperScenario {
    serde_json::from_str(include_str!("../../../examples/crypto_funding_paper.json")).unwrap()
}

fn equity() -> PaperScenario {
    serde_json::from_str(include_str!("../../../examples/equity_basis_paper.json")).unwrap()
}

fn first_plan(scenario: &PaperScenario) -> Decision<Plan> {
    let mut engine = RelativeValue::new(scenario.pair.clone(), &VenueRegistry::default()).unwrap();
    for frame in &scenario.frames[..3] {
        assert!(matches!(
            engine.observe(frame).unwrap(),
            Err(Rejection::Warmup)
        ));
    }
    engine.observe(&scenario.frames[3]).unwrap()
}

#[test]
fn crypto_carry_uses_explicit_funding_and_settles_four_fills() {
    let scenario = crypto();
    let plan = first_plan(&scenario).unwrap();
    assert_eq!(plan.buy_leg, 0);
    assert_eq!(plan.hedge.target_base, d("0.04"));
    assert_eq!(plan.modeled_convergence, Decimal::ZERO);
    assert_eq!(plan.funding_lower_bound, d("0.1104"));
    let report = run_paper(&scenario, &VenueRegistry::default()).unwrap();
    assert_eq!(report.trades.len(), 1);
    assert_eq!(report.ioc_filled, 4);
    assert_eq!(report.trades[0].exit_reason, ExitReason::HoldingTime);
    assert_eq!(report.funding, d("0.1032"));
    assert_eq!(report.fees, d("0.064"));
    assert_eq!(report.realized_pnl, d("0.0232"));
    assert_eq!(report.final_liquidation_pnl, Some(report.realized_pnl));
    assert_eq!(report.open_positions_base, [Decimal::ZERO; 2]);
    assert_eq!(report.max_unhedged_ms, 100);
}

#[test]
fn equity_uses_prior_basis_and_picks_the_net_profitable_depth_band() {
    let scenario = equity();
    let plan = first_plan(&scenario).unwrap();
    assert_eq!(plan.basis.prior_samples, 3);
    assert_eq!(plan.basis.mean_bps, 0.0);
    assert_eq!(plan.basis.observed_bps, 20.0);
    assert_eq!(plan.hedge.target_base, d("0.5"));
    // 10 Entropy contracts × 0.05 shares offsets 0.5 shares on Lighter.
    assert_eq!(plan.hedge.hedge.quantity_step, d("0.05"));
    let report = run_paper(&scenario, &VenueRegistry::default()).unwrap();
    assert_eq!(report.trades.len(), 1);
    assert_eq!(report.trades[0].exit_reason, ExitReason::Reverted);
    assert_eq!(report.trades[0].base_opened, d("0.5"));
    assert_eq!(report.realized_pnl, d("0.06"));
    assert_eq!(report.open_positions_base, [Decimal::ZERO; 2]);
}

#[test]
fn unused_depth_cannot_raise_the_notional_reserved_for_a_shallower_band() {
    let mut scenario = equity();
    scenario.pair.planner.max_unhedged_notional_usd = 50.06;
    let asks = &mut scenario.frames[3].legs[1].book.asks;
    asks[0].contracts = d("10");
    asks.push(Level {
        price: d("100.25"),
        contracts: d("100"),
    });
    let plan = first_plan(&scenario).unwrap();
    assert_eq!(plan.hedge.target_base, d("0.5"));
}

#[test]
fn funding_does_not_credit_near_entry_receipts_or_missing_future_points() {
    let mut scenario = crypto();
    let now = scenario.frames[3].now_ms;
    let near = now + 400;
    for i in 0..2 {
        let leg = &mut scenario.pair.legs[i];
        leg.funding.offset_ms = near % leg.funding.interval_ms;
        scenario.frames[3].legs[i]
            .forecast
            .as_mut()
            .unwrap()
            .payments[0]
            .at_ms = near;
    }
    assert!(matches!(first_plan(&scenario), Err(Rejection::Edge)));

    let mut scenario = crypto();
    // The short-side schedule no longer agrees with the forecast.
    scenario.pair.legs[1].funding.offset_ms = 200;
    assert!(matches!(first_plan(&scenario), Err(Rejection::Funding)));

    let mut scenario = crypto();
    scenario.frames[3].legs[1]
        .forecast
        .as_mut()
        .unwrap()
        .observed_at_ms = scenario.frames[3].now_ms + 1;
    assert!(matches!(first_plan(&scenario), Err(Rejection::Funding)));
}

#[test]
fn equity_rejects_session_deadlines_and_internal_oracle() {
    let mut scenario = equity();
    let now = scenario.frames[3].now_ms;
    scenario.frames[3]
        .equity_session
        .as_mut()
        .unwrap()
        .clear_until_ms = now + 3_100;
    assert!(matches!(first_plan(&scenario), Err(Rejection::Session)));

    let mut scenario = equity();
    scenario.frames[3].legs[1].oracle.mode = OracleMode::LastClose;
    assert!(matches!(first_plan(&scenario), Err(Rejection::Oracle)));

    let mut scenario = equity();
    scenario.frames[3].legs[1].oracle.price = d("102");
    assert!(matches!(first_plan(&scenario), Err(Rejection::Oracle)));

    let mut scenario = equity();
    scenario.frames[3].equity_session.as_mut().unwrap().halted = true;
    assert!(matches!(first_plan(&scenario), Err(Rejection::Session)));
}

#[test]
fn mismatched_economic_payoffs_and_shared_risk_are_rejected() {
    let mut scenario = equity();
    let registry = VenueRegistry::default();
    scenario.pair.legs[1].payoff_id = "total return".into();
    assert!(RelativeValue::new(scenario.pair, &registry).is_err());
    let mut scenario = crypto();
    scenario.pair.legs[0].venue = "entropy".into();
    scenario.pair.legs[0].market = "io:ETH".into();
    assert!(RelativeValue::new(scenario.pair.clone(), &registry).is_err());
    scenario.pair.allow_shared_risk_domain = true;
    assert!(RelativeValue::new(scenario.pair, &registry).is_ok());
}

#[test]
fn duplicate_or_regressed_books_do_not_train_a_pair() {
    let scenario = equity();
    let mut engine = RelativeValue::new(scenario.pair.clone(), &VenueRegistry::default()).unwrap();
    for frame in &scenario.frames[..3] {
        assert!(matches!(
            engine.observe(frame).unwrap(),
            Err(Rejection::Warmup)
        ));
    }
    let mut stale = scenario.frames[3].clone();
    stale.legs[1].book.timestamp_ms = scenario.frames[2].now_ms;
    stale.legs[0].book.timestamp_ms = scenario.frames[2].now_ms;
    assert!(matches!(
        engine.observe(&stale).unwrap(),
        Err(Rejection::SampleCadence)
    ));
    let mut regressed = scenario.frames[4].clone();
    for leg in &mut regressed.legs {
        leg.book.timestamp_ms = scenario.frames[1].now_ms;
    }
    assert!(matches!(
        engine.observe(&regressed).unwrap(),
        Err(Rejection::Book)
    ));
    assert!(matches!(
        engine.observe(&scenario.frames[5]).unwrap(),
        Err(Rejection::Warmup)
    ));
}

#[test]
fn short_book_skew_rejects_the_frame_without_starving_the_estimator() {
    let scenario = equity();
    let mut engine = RelativeValue::new(scenario.pair.clone(), &VenueRegistry::default()).unwrap();
    for frame in &scenario.frames[..3] {
        assert!(matches!(
            engine.observe(frame).unwrap(),
            Err(Rejection::Warmup)
        ));
    }
    let mut skewed = scenario.frames[3].clone();
    skewed.legs[0].book.timestamp_ms -= 60;
    assert!(matches!(
        engine.observe(&skewed).unwrap(),
        Err(Rejection::Book)
    ));
    let plan = engine.observe(&scenario.frames[4]).unwrap().unwrap();
    assert_eq!(plan.basis.prior_samples, 3);
}

#[test]
fn paper_does_not_fill_using_a_book_generated_before_arrival() {
    let mut scenario = crypto();
    scenario.frames.truncate(5);
    for leg in &mut scenario.frames[4].legs {
        leg.book.timestamp_ms -= 1;
    }
    let report = run_paper(&scenario, &VenueRegistry::default()).unwrap();
    assert_eq!(report.entries_submitted, 1);
    assert_eq!(report.ioc_submitted, 1);
    assert_eq!(report.ioc_filled, 0);
    assert_eq!(report.open_phase, Some("opening"));
    assert_eq!(report.open_positions_base, [Decimal::ZERO; 2]);
}

#[test]
fn primary_can_fill_when_hedge_book_is_unavailable_and_exposure_is_reported() {
    let mut scenario = crypto();
    scenario.frames.truncate(7);
    for frame in &mut scenario.frames[4..] {
        frame.legs[1].book.tradable = false;
    }
    let report = run_paper(&scenario, &VenueRegistry::default()).unwrap();
    assert_eq!(report.ioc_filled, 1);
    assert_eq!(report.ioc_expired_without_book, 1);
    assert!(report.halted.is_some());
    assert!(report.trades.is_empty());
    assert_eq!(report.open_positions_base, [d("0.04"), Decimal::ZERO]);
    assert_eq!(report.max_unhedged_base, d("0.04"));
    assert_eq!(report.max_unhedged_ms, 800);
    assert_eq!(report.realized_pnl, Decimal::ZERO);
}

#[test]
fn partial_entry_hedges_only_the_confirmed_amount_then_can_complete() {
    let mut scenario = crypto();
    scenario.frames[4].legs[0].book.asks[0].contracts = d("0.02");
    let mut sixth = scenario.frames[5].clone();
    sixth.now_ms += 100;
    for leg in &mut sixth.legs {
        leg.book.timestamp_ms = sixth.now_ms;
        leg.oracle.timestamp_ms = sixth.now_ms;
    }
    sixth.reference.timestamp_ms = sixth.now_ms;
    let mut seventh = sixth.clone();
    seventh.now_ms += 100;
    for leg in &mut seventh.legs {
        leg.book.timestamp_ms = seventh.now_ms;
        leg.oracle.timestamp_ms = seventh.now_ms;
    }
    seventh.reference.timestamp_ms = seventh.now_ms;
    scenario.frames.insert(6, sixth);
    scenario.frames.insert(7, seventh);
    let report = run_paper(&scenario, &VenueRegistry::default()).unwrap();
    assert_eq!(report.ioc_partial, 1);
    assert_eq!(report.max_unhedged_base, d("0.04")); // Unwind of the full pair.
    assert_eq!(report.trades[0].base_opened, d("0.04"));
    assert_eq!(report.realized_pnl, d("0.0232"));
}

#[test]
fn missing_realized_funding_fails_the_accounting_instead_of_imputing_zero() {
    let mut scenario = crypto();
    scenario.frames[7].legs[1].settled.clear();
    let err = run_paper(&scenario, &VenueRegistry::default()).unwrap_err();
    assert!(
        err.to_string()
            .contains("missing realized funding on hyperliquid")
    );
}

#[test]
fn training_frames_fit_the_estimator_without_creating_fills() {
    let mut scenario = crypto();
    assert!(first_plan(&scenario).is_ok());
    // Start the evaluation once the funding event has passed. The otherwise
    // eligible carry during the training period must not enter the PnL.
    scenario.trade_from_ms = Some(scenario.frames[7].now_ms);
    let report = run_paper(&scenario, &VenueRegistry::default()).unwrap();
    assert_eq!(report.training_frames, 7);
    assert_eq!(report.entries_submitted, 0);
    assert_eq!(report.rejected.get(&Rejection::Warmup), None);
    assert_eq!(report.realized_pnl, Decimal::ZERO);
}

#[test]
#[ignore = "release microbenchmark"]
fn benchmark_relative_value_l2_plan() {
    use std::{hint::black_box, time::Instant};
    let mut scenario = equity();
    let frame = &mut scenario.frames[3];
    for leg in &mut frame.legs {
        let bid = leg.book.bids[0].price;
        let ask = leg.book.asks[0].price;
        leg.book.bids = (0..20)
            .map(|i| Level {
                price: bid - Decimal::new(i, 2),
                contracts: d("0.25"),
            })
            .collect();
        leg.book.asks = (0..20)
            .map(|i| Level {
                price: ask + Decimal::new(i, 2),
                contracts: d("0.25"),
            })
            .collect();
    }
    // Make every displayed band tradable at the respective lot step.
    for level in frame.legs[1]
        .book
        .bids
        .iter_mut()
        .chain(&mut frame.legs[1].book.asks)
    {
        level.contracts = d("5");
    }
    let engine = RelativeValue::new(scenario.pair, &VenueRegistry::default()).unwrap();
    let signal = BasisSignal {
        observed_bps: 20.0,
        mean_bps: 0.0,
        sigma_bps: 4.0,
        z: 5.0,
        prior_samples: 30,
    };
    let cutoff = engine.cutoff(frame).unwrap();
    let trials = 20_000;
    let start = Instant::now();
    for _ in 0..trials {
        black_box(
            engine
                .plan(black_box(frame), black_box(signal), black_box(cutoff))
                .unwrap(),
        );
    }
    eprintln!(
        "relative value, 4×20 depth levels: {:.0} ns/plan ({trials} evaluations)",
        start.elapsed().as_nanos() as f64 / f64::from(trials)
    );
}

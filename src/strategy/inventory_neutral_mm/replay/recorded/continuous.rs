//! One production strategy instance across settlement boundaries. This is an
//! offline flat-at-funding experiment, not a funding-carry model.
use super::*;

fn flat(venue: &SimVenue) -> bool {
    let matching = venue.recorded.as_ref().expect("recorded matching");
    matching.prune_cancelled(now_ms());
    venue.net_position() == 0.0
        && venue.book.lock().is_empty()
        && matching.market.lock().cancelled.is_empty()
        && venue.tracker.active_orders_snapshot().is_empty()
        && !venue.tracker.needs_reconciliation()
        && venue.tracker.exposure_bounds() == (0.0, 0.0, 0.0)
}

fn healthy(matching: &Matching, schedule: &FlatAtFunding) -> bool {
    let market = matching.market.lock();
    market.trade_feed_online == Some(true)
        && market.book.fresh(now_ms(), matching.max_age_ms)
        && now_ms().saturating_sub(market.last_event_ms) <= schedule.max_event_gap_ms
}

async fn sleep_to(at: u64) {
    tokio::time::sleep(Duration::from_millis(at.saturating_sub(now_ms()).max(1))).await;
}

/// Never call `run`/startup for each hour: that would reset session state,
/// signals and the daily loss guard. Only cleanup is repeated between slots.
pub(super) async fn run(
    strategy: &mut InventoryNeutralMM,
    config: &RecordedMakerConfig,
    venue: &SimVenue,
    stop: watch::Receiver<bool>,
) -> (Result<()>, u64) {
    let schedule = config.flat_at_funding.as_ref().expect("funding schedule");
    let matching = venue.recorded.as_ref().expect("recorded matching");
    let mut verified = std::collections::BTreeSet::new();
    let mut expected = std::collections::BTreeSet::new();
    let mut slot = schedule.next_slot(config.start_ms);
    while slot <= config.drain_until_ms {
        expected.insert(slot);
        slot += schedule.interval_ms;
    }
    let mut prepared = None;
    let mut shutdown = false;
    let mut offline_cleaned = false;
    let result: Result<()> = async {
        strategy.perform_startup_initialization().await?;
        while now_ms() < config.drain_until_ms {
            ensure!(
                expected
                    .iter()
                    .all(|slot| now_ms() < slot + schedule.guard_ms || verified.contains(slot)),
                "funding guard was skipped by an in-flight operation"
            );
            ensure!(
                !*stop.borrow() || now_ms() >= config.end_ms,
                "continuous replay input stopped before decision interval ended"
            );
            if now_ms() >= config.end_ms && !shutdown {
                strategy.perform_graceful_shutdown().await?;
                shutdown = true;
                ensure!(
                    now_ms() <= config.drain_until_ms,
                    "final cleanup exceeded recorded drain coverage"
                );
                let next = schedule.next_slot(now_ms());
                if !matches!(schedule.phase(now_ms()), FundingPhase::Guard(_))
                    && now_ms() <= next - schedule.guard_ms
                {
                    // The final close can precede a settlement in the tail.
                    // Preserve its proof while no new risk is permitted.
                    prepared = Some(next);
                }
            }
            let at = now_ms();
            match schedule.phase(at) {
                FundingPhase::Trading if !shutdown => {
                    if !healthy(matching, schedule) {
                        if !offline_cleaned {
                            strategy.clean_exchange_state("research data pause").await?;
                            offline_cleaned = true;
                        }
                        // A stale feed cannot be turned into ten hours of
                        // valid no-trade observations by advancing the clock.
                        ensure!(
                            now_ms().saturating_sub(matching.market.lock().last_event_ms)
                                <= schedule.max_event_gap_ms,
                            "continuous replay event gap exceeds bound"
                        );
                        sleep_to((at + 100).min(config.end_ms)).await;
                        continue;
                    }
                    offline_cleaned = false;
                    prepared = None;
                    ensure!(
                        strategy
                            .is_running
                            .load(std::sync::atomic::Ordering::SeqCst),
                        "strategy stopped before continuous decision interval ended"
                    );
                    let outcome = strategy.quote_cycle(Some(&stop)).await?;
                    // A batch can await across a phase boundary. Matching
                    // checks the actual arrival time before admitting risk.
                    let boundary =
                        schedule.next_slot(at) - schedule.guard_ms - schedule.drain_before_ms;
                    let wake = boundary.min(config.end_ms);
                    match outcome {
                        CycleOutcome::Stop => continue,
                        CycleOutcome::Backoff(delay) => {
                            tokio::select! {
                                _ = tokio::time::sleep(delay) => {},
                                _ = sleep_to(wake) => {},
                            }
                        }
                        CycleOutcome::Quoted => {
                            tokio::select! {
                                _ = strategy.wait_for_next_cycle(Some(&stop)) => {},
                                _ = sleep_to(wake) => {},
                            }
                        }
                    }
                }
                FundingPhase::Drain(slot) => {
                    if prepared != Some(slot) {
                        strategy
                            .clean_exchange_state("research funding drain")
                            .await?;
                        ensure!(
                            now_ms() <= slot - schedule.guard_ms,
                            "funding cleanup missed flat guard deadline"
                        );
                        ensure!(flat(venue), "funding cleanup left residual risk");
                        prepared = Some(slot);
                    }
                    ensure!(flat(venue), "late exposure appeared after funding drain");
                    sleep_to(
                        (now_ms() + 100)
                            .min(slot - schedule.guard_ms)
                            .min(config.drain_until_ms),
                    )
                    .await;
                }
                FundingPhase::Guard(slot) => {
                    // An initial guard is safe only for the known empty
                    // initial state. Every later guard must have drained.
                    ensure!(
                        prepared == Some(slot)
                            || config.start_ms >= slot.saturating_sub(schedule.guard_ms),
                        "funding guard reached without completed drain"
                    );
                    ensure!(flat(venue), "funding guard has unsettled risk");
                    ensure!(
                        healthy(matching, schedule),
                        "funding guard lacks fresh book or online trade-feed evidence"
                    );
                    if at >= slot && slot > config.start_ms {
                        verified.insert(slot);
                    }
                    sleep_to(
                        (at + 100)
                            .min(if at < slot { slot } else { u64::MAX })
                            .min(slot + schedule.guard_ms)
                            .min(config.drain_until_ms),
                    )
                    .await;
                }
                FundingPhase::Trading => {
                    ensure!(flat(venue), "final drain has residual risk");
                    sleep_to((at + 100).min(config.drain_until_ms)).await;
                }
            }
        }
        ensure!(
            shutdown && flat(venue),
            "continuous replay did not finish flat"
        );
        ensure!(
            verified == expected,
            "continuous replay lacks verification for an expected funding boundary"
        );
        Ok(())
    }
    .await;
    let cleanup = if shutdown {
        Ok(())
    } else {
        strategy.perform_graceful_shutdown().await
    };
    let result = match (result, cleanup) {
        (Ok(()), result) | (result, Ok(())) => result,
        (Err(run), Err(cleanup)) => {
            Err(run.context(format!("Research cleanup also failed: {cleanup:#}")))
        }
    };
    (result, verified.len() as u64)
}

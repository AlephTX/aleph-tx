//! Cross-venue replay, market discovery, and explicit Arcus REST operations.
#[path = "cross_venue/arcus_cli.rs"]
mod arcus_cli;
use aleph_tx::{
    capture::{CaptureConfig, compile as compile_capture},
    exchanges::{arcus, bulk, hyperliquid, variational},
    funding_evidence,
    hedging::{HedgeAction, HedgeMachine, HedgePlan, OrderUpdate},
    strategy::cross_venue::{
        PlanKind, PlanRejection, PlannerConfig, TradePlan, VenueSnapshot, evaluate_pair,
    },
    strategy::relative_value::{
        Frame as PerpFrame, PairConfig, PaperReport, PaperScenario, run_paper, run_paper_frames,
    },
    venues::VenueRegistry,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use std::{
    collections::HashSet,
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, Read},
    path::Path,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Replay {
    #[serde(default)]
    config: PlannerConfig,
    frames: Vec<Frame>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Frame {
    now_ms: u64,
    snapshots: Vec<VenueSnapshot>,
}

#[derive(Serialize)]
struct Candidate {
    buy: String,
    sell: String,
    kind: PlanKind,
    shares_risk_domain: bool,
    result: std::result::Result<TradePlan, PlanRejection>,
}

fn evaluate(
    frame: &Frame,
    config: &PlannerConfig,
    registry: &VenueRegistry,
) -> Result<Vec<Candidate>> {
    ensure!(
        (2..=32).contains(&frame.snapshots.len()),
        "Each frame needs 2 to 32 snapshots"
    );
    let mut identities = HashSet::new();
    let venues = frame
        .snapshots
        .iter()
        .map(|snapshot| {
            ensure!(
                identities.insert((&snapshot.venue, &snapshot.instrument.base)),
                "Duplicate venue/base in replay frame"
            );
            registry.validate_snapshot(snapshot)
        })
        .collect::<Result<Vec<_>>>()?;
    let mut candidates = Vec::new();
    for (i, buy) in frame.snapshots.iter().enumerate() {
        for (j, sell) in frame.snapshots.iter().enumerate() {
            if i == j {
                continue;
            }
            for kind in [PlanKind::SpreadCapture, PlanKind::FundingCarry] {
                candidates.push(Candidate {
                    buy: buy.venue.clone(),
                    sell: sell.venue.clone(),
                    kind,
                    shares_risk_domain: venues[i].risk_domain == venues[j].risk_domain,
                    result: evaluate_pair(buy, sell, config, frame.now_ms, kind),
                });
            }
        }
    }
    Ok(candidates)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HedgeReplay {
    plan: HedgePlan,
    events: Vec<HedgeEvent>,
}

#[derive(Deserialize)]
struct HedgeEvent {
    now_ms: u64,
    #[serde(flatten)]
    event: Event,
}

#[derive(Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
enum Event {
    Poll,
    Update { update: OrderUpdate },
    Uncertain { client_order_id: String },
    Reconcile { update: OrderUpdate },
}

fn replay_hedge(replay: HedgeReplay, registry: &VenueRegistry) -> Result<serde_json::Value> {
    let primary = registry.get(&replay.plan.primary.venue)?;
    let hedge = registry.get(&replay.plan.hedge.venue)?;
    ensure!(
        primary.liquidity_domain != hedge.liquidity_domain,
        "Hedge legs share a book"
    );
    for venue in [primary, hedge] {
        ensure!(
            !matches!(
                venue.order_support,
                aleph_tx::venues::OrderSupport::ReadOnly
            ),
            "{} cannot provide executable fills",
            venue.id
        );
    }
    ensure!(!replay.events.is_empty(), "Hedge replay is empty");
    let mut machine = HedgeMachine::new(replay.plan)?;
    let mut now = 0;
    let mut actions: Vec<HedgeAction> = Vec::new();
    for (index, frame) in replay.events.into_iter().enumerate() {
        ensure!(
            frame.now_ms >= now && frame.now_ms > 0,
            "Hedge event times must be positive and monotonic"
        );
        now = frame.now_ms;
        let result = match frame.event {
            Event::Poll => machine.next_action(now).map(|action| actions.push(action)),
            Event::Update { update } => machine.apply_update(update).map(|_| ()),
            Event::Uncertain { client_order_id } => machine.mark_uncertain(&client_order_id),
            Event::Reconcile { update } => machine.reconcile_order(update).map(|_| ()),
        };
        result.with_context(|| format!("Hedge event {}", index + 1))?;
    }
    Ok(json!({
        "mode": "offline",
        "actions": actions,
        "residual_base": machine.residual_base()?.to_string(),
        "checkpoint": machine.snapshot(),
    }))
}

fn read_replay<T: DeserializeOwned>(path: &str) -> Result<T> {
    const LIMIT: u64 = 16 * 1024 * 1024;
    let mut bytes = Vec::new();
    File::open(path)?.take(LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= LIMIT, "Replay exceeds 16 MiB");
    Ok(serde_json::from_slice(&bytes)?)
}

fn stream_paper(mut reader: impl BufRead, registry: &VenueRegistry) -> Result<PaperReport> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Header {
        pair: PairConfig,
        one_way_latency_ms: u64,
        trade_from_ms: Option<u64>,
        #[serde(default)]
        funding_guard_ms: u64,
    }
    let mut bytes = Vec::new();
    let header: Header =
        read_record(&mut reader, &mut bytes)?.context("Missing paper replay header")?;
    let mut line = 1;
    let frames = std::iter::from_fn(|| {
        line += 1;
        read_record::<PerpFrame>(&mut reader, &mut bytes)
            .with_context(|| format!("Invalid replay frame on line {line}"))
            .transpose()
    });
    run_paper_frames(
        &header.pair,
        header.one_way_latency_ms,
        header.trade_from_ms,
        header.funding_guard_ms,
        frames,
        registry,
    )
}

fn read_record<T: DeserializeOwned>(
    reader: &mut impl BufRead,
    bytes: &mut Vec<u8>,
) -> Result<Option<T>> {
    const LIMIT: u64 = 1024 * 1024;
    bytes.clear();
    if reader.take(LIMIT + 1).read_until(b'\n', bytes)? == 0 {
        return Ok(None);
    }
    ensure!(bytes.len() as u64 <= LIMIT, "Replay record exceeds 1 MiB");
    Ok(Some(serde_json::from_slice(bytes)?))
}

fn print(value: &impl Serialize) -> Result<()> {
    serde_json::to_writer_pretty(std::io::stdout().lock(), value)?;
    println!();
    Ok(())
}

#[derive(Deserialize)]
struct ArchivedProof<P, R> {
    policy: P,
    raw: R,
}

type HyperliquidProof = ArchivedProof<funding_evidence::Policy, funding_evidence::RawEvidence>;
type LighterProof =
    ArchivedProof<funding_evidence::lighter::Policy, funding_evidence::lighter::RawEvidence>;

fn attach_funding_proofs(
    config: &mut CaptureConfig,
    lighter: &str,
    hyperliquid: &str,
) -> Result<()> {
    if lighter != "-" {
        let archive: LighterProof = read_replay(lighter)?;
        let i = config
            .pair
            .legs
            .iter()
            .position(|leg| leg.venue == "lighter")
            .context("capture has no Lighter leg")?;
        let leg = &config.pair.legs[i];
        ensure!(
            config.legs[i].lighter_market_id == Some(archive.policy.market_id)
                && leg.market == archive.policy.symbol
                && leg.funding.interval_ms == 3_600_000
                && leg.funding.offset_ms == 0
                && leg.base_per_contract == rust_decimal::Decimal::ONE
                && leg.settlement == "USDC",
            "Lighter proof does not match the replay contract or schedule"
        );
        ensure!(
            config.witnessed_settlements[i].is_empty(),
            "Lighter witnesses already configured"
        );
        let audit = funding_evidence::lighter::reconcile(&archive.policy, &archive.raw)?;
        ensure!(
            !audit.witnessed_settlements.is_empty(),
            "no audited Lighter funding settlement to attach"
        );
        config.witnessed_settlements[i] = audit.witnessed_settlements;
        config.funding_guard_ms = config.funding_guard_ms.max(archive.policy.funding_guard_ms);
    }
    if hyperliquid != "-" {
        let archive: HyperliquidProof = read_replay(hyperliquid)?;
        let i = config
            .pair
            .legs
            .iter()
            .position(|leg| leg.venue == "hyperliquid")
            .context("capture has no Hyperliquid core leg")?;
        let leg = &config.pair.legs[i];
        ensure!(
            leg.market == archive.policy.coin
                && leg.funding.interval_ms == archive.policy.schedule.interval_ms
                && leg.funding.offset_ms == archive.policy.schedule.offset_ms
                && leg.base_per_contract == rust_decimal::Decimal::ONE
                && leg.settlement == "USDC",
            "Hyperliquid proof does not match the replay contract or schedule"
        );
        ensure!(
            config.witnessed_settlements[i].is_empty(),
            "Hyperliquid witnesses already configured"
        );
        let audit = funding_evidence::reconcile(&archive.policy, &archive.raw)?;
        ensure!(
            !audit.witnessed_settlements.is_empty(),
            "no audited Hyperliquid funding settlement to attach"
        );
        config.witnessed_settlements[i] = audit.witnessed_settlements;
        config.funding_guard_ms = config.funding_guard_ms.max(archive.policy.funding_guard_ms);
    }
    Ok(())
}

fn capture_to_replay(
    config: &CaptureConfig,
    input: &str,
    output: &str,
    registry: &VenueRegistry,
) -> Result<()> {
    let input = BufReader::new(File::open(input)?);
    let output = Path::new(output);
    ensure!(
        output.extension().is_some_and(|suffix| suffix == "ndjson"),
        "output must have the .ndjson extension"
    );
    let temporary = output.with_extension(format!("{}.partial", uuid::Uuid::new_v4()));
    let result: Result<_> = (|| {
        let file = File::create_new(&temporary)?;
        let coverage = compile_capture(config, input, BufWriter::new(&file), registry)?;
        file.sync_all()?;
        // Publish a complete replay and never overwrite an earlier dataset.
        fs::hard_link(&temporary, output).context("publish replay without overwriting")?;
        Ok(coverage)
    })();
    if temporary.exists() {
        fs::remove_file(&temporary)?;
    }
    print(&result?)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let registry = VenueRegistry::default();
    match args.as_slice() {
        [venue, network, command @ ..] if venue == "arcus" => {
            arcus_cli::run(network, command).await
        }
        [command] if command == "venues" => print(&registry.iter().collect::<Vec<_>>()),
        [command, journal, evidence @ ..]
            if command == "hedge-journal-audit" && evidence.len() <= 1 =>
        {
            // Offline and read-only: no client, credential or network access.
            let report = aleph_tx::live_hedge::audit::audit_files(
                Path::new(journal),
                evidence.first().map(Path::new),
            )?;
            print(&report)?;
            std::process::exit(report.status.exit_code());
        }
        [command, file] if command == "replay" => {
            let replay: Replay = read_replay(file)?;
            replay.config.validate()?;
            ensure!(!replay.frames.is_empty(), "Market replay is empty");
            let mut now = 0;
            // Stream one JSON record per frame so long replays need no result buffer.
            for frame in replay.frames {
                ensure!(frame.now_ms > now, "Replay frame timestamps must increase");
                now = frame.now_ms;
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "mode": "offline",
                        "now_ms": now,
                        "candidates": evaluate(&frame, &replay.config, &registry)?,
                    }))?
                );
            }
            Ok(())
        }
        [command, file] if command == "hedge-replay" => {
            print(&replay_hedge(read_replay(file)?, &registry)?)
        }
        [command, file] if command == "perp-replay" => {
            let report = if file.ends_with(".ndjson") {
                stream_paper(BufReader::new(File::open(file)?), &registry)?
            } else {
                let scenario: PaperScenario = read_replay(file)?;
                run_paper(&scenario, &registry)?
            };
            print(&report)
        }
        [command, config, input, output] if command == "perp-compile" => {
            capture_to_replay(&read_replay(config)?, input, output, &registry)
        }
        [command, config, input, output, lighter, hyperliquid] if command == "perp-compile" => {
            let mut config = read_replay(config)?;
            attach_funding_proofs(&mut config, lighter, hyperliquid)?;
            capture_to_replay(&config, input, output, &registry)
        }
        [command, venue, policy, first, last] if command == "perp-funding-proof" => {
            let (first, last) = (first.parse()?, last.parse()?);
            match venue.as_str() {
                "lighter" => print(
                    &funding_evidence::lighter::collect(&read_replay(policy)?, first, last).await?,
                ),
                "hyperliquid" => {
                    print(&funding_evidence::collect(&read_replay(policy)?, first, last).await?)
                }
                _ => anyhow::bail!("funding proofs support lighter and hyperliquid"),
            }
        }
        [command, venue, file] if command == "perp-funding-audit" => match venue.as_str() {
            "lighter" => {
                let archive: LighterProof = read_replay(file)?;
                print(&funding_evidence::lighter::reconcile(
                    &archive.policy,
                    &archive.raw,
                )?)
            }
            "hyperliquid" => {
                let archive: HyperliquidProof = read_replay(file)?;
                print(&funding_evidence::reconcile(&archive.policy, &archive.raw)?)
            }
            _ => anyhow::bail!("funding audits support lighter and hyperliquid"),
        },
        [command, venue, market] if command == "bbo" => match venue.as_str() {
            "arcus" => print(
                &arcus::ArcusInfo::new(arcus::Network::Mainnet)?
                    .bbo(market)
                    .await?,
            ),
            "bulk" => print(&bulk::BulkInfo::new()?.bbo(market).await?),
            "hyperliquid" | "entropy" | "trade_xyz" => {
                let prefix = registry
                    .get(venue)?
                    .hyperliquid_dex
                    .as_deref()
                    .unwrap_or_default();
                if prefix.is_empty() {
                    ensure!(
                        !market.contains(':'),
                        "Use the named HIP-3 venue for namespaced coins"
                    );
                } else {
                    ensure!(
                        market
                            .split_once(':')
                            .is_some_and(|(dex, coin)| dex == prefix && !coin.is_empty()),
                        "Coin must use the {prefix}: namespace"
                    );
                }
                let book = hyperliquid::HyperliquidInfo::new(hyperliquid::Network::Mainnet)?
                    .bbo(market)
                    .await?;
                print(&json!({
                    "coin": book.coin, "timestamp_ms": book.timestamp_ms,
                    "bid": book.bid.to_string(), "ask": book.ask.to_string(),
                    "bid_size": book.bid_size.to_string(), "ask_size": book.ask_size.to_string(),
                }))
            }
            _ => anyhow::bail!(
                "Public BBO available for arcus, bulk, hyperliquid, entropy and trade_xyz"
            ),
        },
        [command, venue] if command == "markets" => match registry.get(venue)? {
            venue if venue.hyperliquid_dex.is_some() => {
                let info = hyperliquid::HyperliquidInfo::new(hyperliquid::Network::Mainnet)?;
                let markets = info
                    .perpetual_markets(venue.hyperliquid_dex.as_deref())
                    .await?;
                let rows: Vec<_> = markets
                    .iter()
                    .map(|m| {
                        json!({
                            "coin": m.asset.name,
                            "asset_id": m.asset_id,
                            "size_decimals": m.asset.sz_decimals,
                            "is_delisted": m.asset.is_delisted,
                            "growth_mode": m.asset.growth_mode,
                            "deployer_fee_scale": m.asset.deployer_fee_scale,
                            "collateral_token": m.collateral_token,
                            "oracle_price": m.oracle_price.to_string(),
                            "hourly_funding": m.hourly_funding.to_string(),
                        })
                    })
                    .collect();
                print(&rows)
            }
            venue if venue.id == "arcus" => print(
                &arcus::ArcusInfo::new(arcus::Network::Mainnet)?
                    .markets()
                    .await?,
            ),
            venue if venue.id == "bulk" => print(&bulk::BulkInfo::new()?.markets().await?),
            venue if venue.id == "variational" => {
                print(&variational::VariationalInfo::new()?.stats().await?)
            }
            _ => {
                anyhow::bail!("Lighter discovery is provided by the existing feeder configuration")
            }
        },
        _ => anyhow::bail!(
            "Usage: cross_venue venues | hedge-journal-audit JOURNAL [EVIDENCE] | replay FILE | hedge-replay FILE | perp-replay FILE | perp-compile CONFIG INPUT OUTPUT [LIGHTER_PROOF HYPERLIQUID_PROOF] | perp-funding-proof VENUE POLICY FIRST_SLOT_MS LAST_SLOT_MS | perp-funding-audit VENUE PROOF | markets VENUE | bbo VENUE MARKET | arcus <testnet|mainnet> COMMAND"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aleph_tx::hedging::Leg;
    use rust_decimal::Decimal;

    #[test]
    fn streamed_perp_recording_has_the_same_pnl_as_the_json_fixture() {
        let fixture = include_str!("../../examples/crypto_funding_paper.json");
        let mut header: serde_json::Value = serde_json::from_str(fixture).unwrap();
        let frames = header.as_object_mut().unwrap().remove("frames").unwrap();
        let mut ndjson = format!("{header}\n");
        for frame in frames.as_array().unwrap() {
            ndjson.push_str(&format!("{frame}\n"));
        }
        let registry = VenueRegistry::default();
        let streamed = stream_paper(std::io::Cursor::new(ndjson), &registry).unwrap();
        let batch = run_paper(&serde_json::from_str(fixture).unwrap(), &registry).unwrap();
        assert_eq!(
            serde_json::to_value(streamed).unwrap(),
            serde_json::to_value(batch).unwrap()
        );
    }

    #[test]
    fn replay_finds_only_the_fresh_net_profitable_executable_pair() {
        let replay: Replay =
            serde_json::from_str(include_str!("../../examples/cross_venue_replay.json")).unwrap();
        let registry = VenueRegistry::default();
        let result =
            |index: usize| evaluate(&replay.frames[index], &replay.config, &registry).unwrap();
        let fresh = result(0);
        let spread: Vec<_> = fresh
            .iter()
            .filter_map(|c| c.result.as_ref().ok())
            .filter(|p| p.kind == PlanKind::SpreadCapture)
            .collect();
        assert_eq!(spread.len(), 1);
        let plan = spread[0];
        assert_eq!(
            (&*plan.buy.venue, &*plan.sell.venue),
            ("lighter", "hyperliquid")
        );
        assert_eq!(plan.base_quantity, 0.02);
        assert!(plan.net_edge_usd > 0.0 && plan.entry_cost_usd > 0.0 && plan.exit_cost_usd > 0.0);
        assert!(
            fresh
                .iter()
                .any(|c| c.result == Err(PlanRejection::IndicativeQuote))
        );
        assert!(
            result(1)
                .iter()
                .all(|c| c.result == Err(PlanRejection::InsufficientEdge))
        );
        assert!(
            result(2)
                .iter()
                .all(|c| c.result == Err(PlanRejection::StaleQuotes))
        );
    }

    #[test]
    fn replay_reconciles_an_uncertain_partial_hedge_and_finishes_flat() {
        let replay: HedgeReplay =
            serde_json::from_str(include_str!("../../examples/hedge_replay.json")).unwrap();
        let market_replay: Replay =
            serde_json::from_str(include_str!("../../examples/cross_venue_replay.json")).unwrap();
        let registry = VenueRegistry::default();
        let candidates =
            evaluate(&market_replay.frames[0], &market_replay.config, &registry).unwrap();
        let spread = candidates
            .iter()
            .find_map(|c| {
                c.result
                    .as_ref()
                    .ok()
                    .filter(|p| p.kind == PlanKind::SpreadCapture)
            })
            .unwrap();
        assert_eq!(
            replay.plan.target_base,
            spread.base_quantity.to_string().parse::<Decimal>().unwrap()
        );
        assert_eq!(
            replay.plan.primary.limit_price,
            spread
                .buy
                .limit_price
                .to_string()
                .parse::<Decimal>()
                .unwrap()
        );
        assert_eq!(
            replay.plan.hedge.limit_price,
            spread
                .sell
                .limit_price
                .to_string()
                .parse::<Decimal>()
                .unwrap()
        );

        let report = replay_hedge(replay, &registry).unwrap();
        let actions: Vec<HedgeAction> = serde_json::from_value(report["actions"].clone()).unwrap();
        assert!(matches!(&actions[1], HedgeAction::Await { .. }));
        assert!(
            matches!(&actions[2], HedgeAction::Submit { order } if order.leg == Leg::Hedge && order.base_quantity == Decimal::new(8, 3))
        );
        assert!(
            matches!(&actions[3], HedgeAction::Reconcile { client_order_ids } if client_order_ids == &["replay-eth:hedge:1"])
        );
        assert_eq!(actions.last(), Some(&HedgeAction::Complete));
        assert_eq!(
            report["residual_base"]
                .as_str()
                .unwrap()
                .parse::<Decimal>()
                .unwrap(),
            Decimal::ZERO
        );
    }
}

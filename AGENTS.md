# AlephTX engineering guide

Applies repository-wide. Start with the [README](README.md) for current
capabilities and priorities. Use the linked runbooks and current code for
operational details.

## Sensitive data and Git

**Hard stop.** Never stage, commit, or push sensitive material. This rule
applies to GitHub and other Git remotes, including private repositories,
testnet work, and every branch, tag, and commit in a proposed push.

- Keep keys, mnemonics, tokens, credential files, private profiles, signed
  requests and signatures, account identifiers tied to private activity,
  authenticated responses, and raw order, fill, balance, telemetry,
  recovery, and debug records outside Git and tool output. Treat copies,
  screenshots, encodings, and exports the same way.
- Use synthetic or irreversibly redacted fixtures. Treat any artifact whose
  contents or provenance are uncertain as sensitive until reviewed.
- Review paths and contents before staging. Stage an explicit path list,
  then verify staged blobs with a redacting review or scanner. Before
  pushing, review every outgoing commit, including intermediate commits.
  Keep findings to paths, line numbers, categories, and commit IDs.
- If sensitive material is staged, unstage it and preserve the local
  evidence. If an outgoing commit contains it, stop the push and report
  the path and commit ID without reproducing the material.

## Working boundaries

- Default to offline fixtures, replay, and local test services. Trading on
  either network, sending cancellations, deploying, restarting services, and
  changing account or recovery state require authorization for that action.
  `make lighter-up` starts live trading by default. Some binaries and examples
  also submit orders. Read the [trading runbook](docs/LIVE_TRADING.md) before
  operating them.
- Preserve local changes and runtime evidence.
- Commit and push when requested for the current change. Verify the
  destination and use a Conventional Commit subject.

## Entry points

| Work | Start here |
| --- | --- |
| Lighter session and config | [src/bin/lighter_inventory_mm.rs](src/bin/lighter_inventory_mm.rs), [session supervisor](src/bin/lighter_inventory_mm/session.rs), [example profile](config.example.toml) |
| Quoting and order accounting | [inventory strategy](src/strategy/inventory_neutral_mm.rs), [order tracker](src/order_tracker.rs), [Lighter execution](src/exchanges/lighter/trading.rs) |
| Hedge and recovery audit | [live hedge](src/live_hedge.rs), [journal](src/live_hedge/journal.rs), [audit](src/live_hedge/audit.rs) |
| Go/Rust IPC | [Go writers](feeder/shm/), [Rust BBO reader](src/shm_reader.rs), [private events](src/types/events.rs) |
| Telemetry and console | [Rust writer](src/telemetry_sink.rs), [Python monitor and store](deploy/oasis/), [React dashboard](dashboard/) |
| Capture and research | [cross-venue CLI](src/bin/cross_venue.rs), [recording guide](docs/PERP_RECORDING.md), [paper replay guide](docs/PERP_RELATIVE_VALUE.md) |

## Strategy research contract

- Research quant profit, hedge arbitrage and incentive turnover independently:
  separate configurations, execution state, capital and loss budgets, ledgers
  and reports. Shared data or execution primitives do not permit cross-subsidy.
- Use continuous **10-hour validation units**. Record any pre-window warm-up
  and terminal drain separately. Do not present summed short replays with
  inventory, equity or budget resets as a continuous 10-hour result. Preserve
  shorter runs as diagnostics and report missing coverage explicitly.
- Maintain the durable [data-source registry](docs/RESEARCH_DATA_SOURCES.md)
  and an append-only campaign inventory outside Git. Record every consumed
  source, domain, market, time range, hash, availability rule, data gap and
  transformation, including funding and fees. Preserve previous versions and
  failures; check the inventory before claiming data is unavailable or unused.
- Freeze strategy parameters and split boundaries before validation; distinguish
  historical regression, selection and untouched holdout periods. Report all
  tested variants and incomplete results. A promising mechanism is a research
  hypothesis, not evidence or a promise of profitable execution.

## Verification

Use the [Makefile](Makefile) entry points and CI toolchain. Test the affected
boundary, including both sides when a wire format changes.

| Change | Checks |
| --- | --- |
| Rust or Go | `make fmt-check`, `make check`, `make test`, `make build` |
| Profile or launch validation | `make trade MODE=check CONFIG=config.example.toml` and the relevant offline profile cases |
| Telemetry or monitor | `make test-python`; also run Rust checks for writer changes and `make dashboard-build` for API contract changes |
| Dashboard | `make dashboard-build` |
| Documentation | Check links, command examples, and `git diff --check`. Run changed offline examples when applicable. |

Report checks that ran and any platform or environment limits. Local
benchmarks are opt-in via `make benchmark`. Keep regressions reproducible with
synthetic data, a controlled clock, or a local fake venue.

## Code Review Rules

- Preserve the Go/Rust ABI as a shared contract. Check size, alignment,
  offsets, and publication order in both languages. The live private event
  path uses 128-byte V2 events. Keep `symbol_id`, exchange ID, and venue
  `market_id` distinct.
- Reserve worst-case exposure before dispatch and preserve it through pending
  cancels, ambiguous acknowledgments, and stream gaps. Require authoritative
  terminal evidence and the applicable position freshness or fill watermark
  before releasing risk. A timeout or an empty open-order list alone is
  insufficient. Check reduce-only direction, tick and lot rounding, and
  single-leg and net limits at the execution boundary.
- Preserve intent durability and restart barriers. Reconcile both venues
  before clearing an active journal. The audit is read-only and cannot
  authorize restart. Keep schema 3 maker journaling and offline trials out
  of live execution unless a change explicitly promotes them.
- Keep source times, receipt times, and journal or telemetry write times
  distinct. Match evidence to account, market, and order identity. Preserve
  exact order and fee values, and attribute fees by asset only with exact
  reconciled fill coverage.
- Keep the UTC daily loss baseline and alert threshold sourced from the
  strategy guard across restarts. Preserve unknown telemetry as unknown.
  Update the writer, reader, API types, and fixtures together when their
  contract changes. Keep reported and estimated valuation inputs distinct.

Prefer small changes along existing boundaries. Keep the quote path free of
avoidable allocations and blocking work. Explain safety invariants at the
boundary they protect, and update the owning runbook when behavior changes.

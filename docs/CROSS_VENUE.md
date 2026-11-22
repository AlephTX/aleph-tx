# Cross-venue research and execution primitives

The cross-venue CLI discovers markets, inspects public books and replays
two-leg order events. For funding carry and basis reversion on matched crypto
or equity perpetuals, use the depth-aware [paper replay](PERP_RELATIVE_VALUE.md).

## Venue capabilities

| Venue | Data in AlephTX | Order transport | Operating scope |
| --- | --- | --- | --- |
| Lighter Core | Go BBO, depth and private events plus public L2 capture | Native signer on Linux x86-64 | Primary BTC or ETH maker |
| Lighter Robinhood Chain | Separate public L2, market statistics and trade capture | Read-only integration | Domain-specific offline research |
| Hyperliquid | Go BBO plus public WebSocket and REST L2 capture | Native REST limit orders, cancellation and order status | Native BTC or ETH inventory hedge |
| Entropy | HIP-3 metadata and `io:SYMBOL` books via both Hyperliquid transports | Hyperliquid REST with resolved HIP-3 asset IDs | Offline pair replay |
| TradeXYZ | HIP-3 metadata and `xyz:SYMBOL` books via both Hyperliquid transports | Hyperliquid REST with resolved HIP-3 asset IDs | Offline pair replay |
| Arcus | Public REST metadata and BBO, REST and WebSocket L2 capture | Standalone signed Ed25519 REST, order status and fills | Offline pair replay |
| BULK | Public REST metadata and BBO, REST and WebSocket L2 capture | Read-only integration | Offline pair replay |
| Extended | Public CLOB metadata, sequenced WebSocket L2 and public trades | Read-only capture | Offline maker-first replay |
| Variational | Public v2 perpetual and swap statistics with source timestamps | Read-only indicative data | Market research |

A liquidity domain identifies a matching book. Two frontends using the same
book are one liquidity domain and cannot form a pair. A risk domain
identifies shared infrastructure. Native Hyperliquid, Entropy and TradeXYZ
have distinct books and share that risk domain. Entropy × TradeXYZ
requires `allow_shared_risk_domain: true` on the pair.
[Hyperliquid HIP-3](https://hyperliquid.gitbook.io/hyperliquid-docs/hyperliquid-improvement-proposals-hips/hip-3-builder-deployed-perpetuals).

`lighter` remains the Core identifier. `lighter_robinhood` identifies the
Robinhood Chain instance: independent contracts, sequencer and liquidity,
with public API base `https://api.rh.lighter.xyz/`. Equal market IDs on the
two instances do not identify the same book. The registry conservatively
assigns both to the `lighter` risk domain, so a Core/Robinhood research pair
requires the shared-risk opt-in **and** verified matching economics.
Robinhood's USDG deposit documentation must not be treated as a USDC
settlement guarantee. Core signing chain IDs and account settings are not
reused for Robinhood; live order support is not enabled.
[Official domain specifications](https://docs.robinhood.com/chain/lighter-domains/) ·
[Public domain capture](PERP_RECORDING.md#lighter-core-and-robinhood-chain)

A single recording can support all six pairings among Entropy, Arcus,
TradeXYZ and BULK. Each replay has its own limits and evaluates one pair
in isolation. Joint allocation and live reconciliation need a coordinator
that reserves exposure across all routes. Additional HIP-3 deployers use
`VenueRegistry::register_hip3`. An independent venue adds its own adapter
and registry entry. Research capture runs outside the live shared-memory path. [Four-venue recording](PERP_RECORDING.md#four-venue-pair-recordings).

## Evaluate and replay

```sh
make build
target/release/cross_venue venues
target/release/cross_venue replay examples/cross_venue_replay.json
target/release/cross_venue hedge-replay examples/hedge_replay.json
```

The synthetic market fixture contains an eligible Lighter/Hyperliquid pair, a
more attractive indicative quote, a fee increase and stale books. Each frame
prints candidate directions and their cost breakdown or rejection reason. The
hedge fixture starts at the first frame's approved limits and quantity. It
exercises an ACK without a fill, a partial IOC fill, an uncertain hedge
submission, reconciliation and completion with zero residual base exposure.
Both fixtures run in `make test`.

```sh
target/release/cross_venue markets hyperliquid
target/release/cross_venue markets entropy
target/release/cross_venue markets trade_xyz
target/release/cross_venue markets bulk
target/release/cross_venue markets variational
target/release/cross_venue bbo arcus BTC-USD
target/release/cross_venue bbo bulk NVDA-USD
```

### Pair admission

Verify canonical underlying, payoff adjustments, settlement asset and base
units per contract against the venue specifications. Configure native tick,
lot and minimums, the account's applicable fees and capacity, and each
payment schedule. A settlement or payoff mismatch excludes a pair. FX,
inverse contracts, spot-perpetual pairs and asymmetric swap funding require
separate models. The included examples use synthetic terms.

The SHM scanner accepts explicit economics and instrument metadata through
`configure_market`. Without that configuration it collects BBOs only.

## Arcus REST

Use `cross_venue arcus testnet` for the testnet or `cross_venue arcus mainnet`
for the mainnet. Market discovery needs no credentials. Account reads take
`ARCUS_ACCOUNT_ADDRESS` and `ARCUS_ACCOUNT_INDEX` (default `0`). Order writes
also require `ARCUS_API_PRIVATE_KEY`, the 32-byte Ed25519 signing key registered
for that address and subaccount. See the [Arcus REST guide](https://docs.arcus.xyz/guides/rest-trading).

```sh
target/release/cross_venue arcus testnet markets
target/release/cross_venue arcus testnet bbo BTC-USD
target/release/cross_venue arcus testnet open-orders 100
target/release/cross_venue arcus testnet positions
```

`place-limit MARKET SIDE SIZE PRICE TIF CLIENT_ID [--reduce-only]` sends a
limit order with `alo`, `ioc`, `fok`, or `gtt`. Set the price and size to the
market's tick and step. Use a unique client ID for each intent and save it
before submitting. HTTP 202 confirms receipt. Use `order ORDER_ID` and
`fills [LIMIT]` to confirm fills. If the response is lost, reconcile the
client ID through `open-orders`, `orders` and subsequent fill and position
evidence before another submission. Allow for delayed visibility after
the gateway responds.

`cancel-order MARKET_ID ORDER_ID`, `cancel-client MARKET_ID CLIENT_ID`, and
`cancel-all MARKET_ID` submit cancellations. Use `cancel-all all` for the whole
subaccount. Verify the outcome through order and fill queries before releasing
reserved exposure.

### Arcus scheduled cancellation backstop

The Rust library exposes `ArcusSigner::schedule_cancel` and
`ArcusTrading::schedule_cancel(market_id, deadline_us)`. The deadline is absolute
Unix **microseconds**, 5 seconds to 5 minutes ahead; signing uses nanoseconds.
`None` deadline disarms exactly the selected scope. Account-wide and per-market
switches are independent. A refresh must send a later deadline before expiry.
There is no automatic retry, disarm, CLI command or live supervisor integration.

Only HTTP 200 with a matching account, market scope, deadline and status confirms
the schedule; 202 is insufficient. Expired or uncorrelated receipts and transport
failures are uncertain. The caller must stop new exposure on renewal failure,
re-establish protection, and reconcile authoritative orders/fills/positions before
releasing reserves. Scheduling or eventual auto-cancellation is not fill-terminal
evidence. Respect daily trigger and active-switch quotas; see the
[official DMS contract](https://docs.arcus.xyz/api-reference/exchange/schedule-cancel-all-dead-mans-switch.md).

`CancelLease` now models one account/scope with an explicit lifetime, refresh
interval, safety margin and allowed clock-step tolerance. `renew_cancel_lease`
performs one attempt and validates the client account before any request. It
starts no loop and grants no order authorization. Use `allows_new_exposure`
only as one conjunct of the supervisor's admission checks, immediately before
dispatch. Protection expires against a monotonic deadline anchored at request
start; response latency never extends it. Pending renewal closes the gate.

Failure, a mismatched/late receipt, timeout or cancellation of the polled async
future sets a sticky `requires_reconciliation` boundary. Expiry, or receiving a
renewal after the previous deadline, also requires reconciliation: the old switch
may have fired. A new ACK cannot clear this condition in the same guard. Complete
both-venue order/fill/position reconciliation through the durable supervisor,
then create a fresh unprotected guard and obtain a new confirmed schedule.
Do not clear this boundary from an empty open-order snapshot or a timer alone.
The gate is intentionally not serializable, never releases exposure, and does
not implement that durable supervisor. Absolute venue-clock synchronization and
order/session/fee checks remain separate requirements. No live order path has
been connected to it yet.

The [ANT research and readiness plan](ANT_CAMPAIGN_REVIEW_AND_48H_PLAN_2026_11_20.md)
includes a public-only snapshot auditor. It deliberately never grants live
readiness; fees, session bands and account eligibility still need execution-boundary
integration and current evidence.

### ANT durable execution ledger (offline boundary)

`hedging::durable::ExecutionJournal` adds an independent Arcus ANT / TradeXYZ
`xyz:ANT` execution ledger. It reuses `OrderIntent`, `Leg`, `HedgeSide` and
`OrderStatus`; the existing IOC `HedgeMachine` and Lighter journal schema are
unchanged. There is no live coordinator, signer, dispatch loop or CLI promotion.
`PreparedOrder` is durable data, not an authorization token or an instruction to
resubmit after recovery.

- Start from independently budgeted, flat accounts. `SessionSpec` binds both
  accounts/subaccounts, network, markets, resolved market IDs, lot sizes and
  explicit base/notional limits. A new session needs fresh dual-leg reconciliation
  before its first intent. Example limits and test identities are synthetic.
- `prepare_intent` validates lots, order notional, reduce-only direction and the
  worst-case remaining quantities of **all** unsettled orders. It persists the
  logical-to-wire client ID mapping and fsyncs both journal and committed markers
  before returning. Opposing pending legs do not cancel each other's reserve.
  An unresolved submission blocks further preparation until authoritative evidence.
- An ACK binds identity only. Exact fills update inventory and retain fees by
  venue and asset. Duplicate trade IDs are idempotent only if every field agrees;
  conflicting duplicates stop preparation. A confirmed partial maker fill can
  support a separately reserved hedge while the maker remainder stays reserved.
- Cancel requests, timeouts, terminal status and empty open-order lists alone do
  not release remaining exposure. Explicit reconciliation requires each order's
  terminal evidence, exact cumulative-fill coverage, both empty order snapshots,
  matching positions, source fill watermarks and fresh receipt/query boundaries.
  Each venue must report a positive fill watermark even for a flat account.
  Its position timestamp and fill watermark must each reach that venue's own
  highest previously accepted value. Equal values allow an unchanged venue state
  to be confirmed by a fresh query. Recovery reconstructs these two maxima from
  all checkpoints, including those written by older schema 1 writers.
  Evidence is tied to the journal revision. A failed check retains all reserves.
- Fill effects distinguish closing, opening and crossing zero. These are provisional
  quantity attributions, not realized PnL. Late or tied source timestamps retain
  positions and fees but withhold cycle attribution and require reconciliation.

All evidence structs are **normalized adapter contracts**. This module neither
fetches nor authenticates raw venue responses. The adapters still must verify
side, account/market/order identity, pagination completeness, source timestamps,
native units, fee asset and response provenance. Missing source watermarks must
remain unknown; a local receipt timestamp cannot replace one. It cannot yet
perform a real-account reconciliation.

Use one canonical, owner-only state directory outside Git for both accounts on
the execution host. Journal and marker files are private activity, not research
fixtures; do not print, commit or transfer them. Account locks span markets and
session UUIDs within that directory, but are not distributed locks across hosts
or different directories. Directory permissions must be 0700, files 0600;
symlinks and multiply linked files are rejected. The advisory lock inode is
separate from the atomically replaced active marker.

Each journal record has a sequence and chained checksum. Two durable account
markers retain the session and committed sequence/checksum, detecting even loss
of a complete last record. Recovery accepts an intact WAL ahead of a marker as
possibly dispatched work and reserves it; a WAL behind a marker is rejected.
Write, fsync or marker-publication failure poisons the writer. Recovery never
truncates, repairs, resends or resumes automatically. A checksum detects damage,
not malicious rewriting of all state.

`recover` always starts behind a fresh two-venue reconciliation barrier.
`audit_journal` is read-only and always returns `restart_allowed: false`.
`close_session` requires a fresh reconciled checkpoint with **each** leg flat;
net-flat hedged positions are insufficient. It fsyncs `Closed` before removing
active markers and preserves the journal. Partial marker cleanup can finish from
that closed record. Do not delete markers or invent a new UUID to bypass recovery.

This implementation clones ledger state and performs blocking disk I/O. Run it
on a dedicated execution writer, outside the quote path. The 64 MiB journal and
1 MiB record bounds fail closed; there is no automatic compaction or demonstrated
production throughput. Next integration must add the raw response adapters,
single-owner asynchronous coordinator, cancellation/drop handling, maker/IOC
transport mapping and immediate pre-dispatch conjunction of the DMS lease,
fresh books, session bands, fees, loss/capital limits and durable reservations.
Neither this ledger nor the DMS lease supplies that complete admission policy.

Offline fault-injection checks:

```sh
cargo test --offline --locked --lib hedging::durable
```

### Native ANT REST evidence normalization (offline)

`hedging::durable::native::NativeEvidence` consumes bounded raw REST response
bytes and a collector-supplied `RequestContext`. It makes no network request.
The collector must bind the actual account/subaccount, network, HIP-3 dex and
request/receipt clocks; caller-supplied context is not authentication. A parser
constructed for one response is not a reusable live freshness permit.

Order rows must match the durable client ID, market, side, original quantity and
limit. Arcus REST account fields are checked as well. Native decimal strings
must fit exactly; rounding, numeric JSON amounts, conflicting IDs, duplicate
position keys and malformed evidence are rejected with errors that omit raw
values. Added fields are ignored while the original private receipt remains
the collector's responsibility. The existing general REST types and trading
transport remain unchanged; those older types alone do not supply this boundary.

`unknownOid` stays unknown. Arcus `CANCEL_PENDING` is not terminal. Arcus terminal
quantity uses the documented original-minus-remaining fields, checking
`filledSize` if present. Hyperliquid cancellation status is retained as
`canceled_quantity_unproven`; it does not manufacture an exact terminal quantity
from an empty orders list or a cancel ACK. Native terminal observations still
require the ledger's independent fill and position reconciliation.
[Arcus order schema](https://docs.arcus.xyz/api-reference/public/get-order-history.md) ·
[Hyperliquid info schema](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint).

Fill pages retain exact amounts, venue IDs and source times. Hyperliquid
milliseconds are checked before conversion to microseconds; Arcus uses native
microseconds. Fills without a client ID require an already verified order-ID
binding before `execution_fill` conversion. Hyperliquid `fee` includes
`builderFee`, so the latter is not added again. Arcus REST does not name the fee
asset: its `NativeFill.fee_asset` stays `None`, and conversion is withheld until
a separate evidence-backed accounting design resolves it. Do not assign USDG,
USDC or the market's display quote currency by assumption.
[Arcus fills schema](https://docs.arcus.xyz/api-reference/public/get-fills.md).

`HistoryQuery` records inclusive native-unit bounds and the actual page limit.
Arcus permits up to 1,000 rows; Hyperliquid returns up to 2,000 and retains only
the latest 10,000 fills. Aggregation is rejected. A full page requests two
disjoint integer time intervals; a full single-timestamp bucket returns
`SaturatedTimestamp`, never a cursor that skips tied fills. Execute all requested
subranges with overlap/conflict checks and a bounded work budget in the future
collector. Below-cap and empty pages **never** establish retention completeness,
indexing catch-up or `complete_through_venue_us`; that field remains unknown.

Arcus positions preserve signed size, explicit identity and other-position
counts, but have no source timestamp in the reviewed REST schema. Hyperliquid
positions preserve `clearinghouseState.time`; it is not a fill-stream watermark.
These observations cannot yet construct a complete `PositionEvidence` or clear
the restart barrier. There is no asynchronous collection/pagination runner,
source completeness protocol, automatic reconciliation or live promotion.
The next task must resolve those source gaps before wiring the supervisor.
[Arcus positions schema](https://docs.arcus.xyz/api-reference/public/get-positions.md) ·
[Hyperliquid account state](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint/perpetuals).

## Risk and interpretation

- The planner computes executable entry limits, sizes both legs to a common
  tradable lot and reserves entry and exit fees, slippage and closing basis.
  It limits depth use, per-leg notional and single-leg exposure. Candidates are
  ranked scenarios and may compete for the same funds. Portfolio limits and
  actual available margin belong to the caller.
- Funding carry uses per-payment rates, exchange payment schedules and oracle
  notionals, with an explicit uncertainty allowance. The projection holds the
  forecast fixed over the horizon. Use recorded rates and order book events for
  historical performance measurement. The sample fixture measures behavior.
- Firm RFQs require a quote ID, exact size and sufficient validity. The current
  Variational endpoint supplies indicative prices with a source timestamp.
  Perpetual interval rates and asymmetric swap rates are parsed separately.
- The hedge machine follows confirmed cumulative fills and reserves each intent
  before returning it. On uncertain submission, query order status before
  issuing another intent. An ACK alone cannot release a reservation. A restored
  checkpoint requires reconciliation and stays halted. Persist venue/client ID
  mappings, add an order journal and supervise unresolved exposure before
  connecting a live coordinator. The replay commands never submit orders.
- Arcus signing uses the top-level tick as the signed integer denominator and
  checks coarser price tiers separately. Its order commands are explicit REST
  operations. The supervised inventory strategy uses Lighter and Hyperliquid.

### Dated settlement and combination orders

The offline `matched-settlement` command screens two linear cash-settled
contracts with a common underlying, fixing index, fixing rule, expiry and
settlement currency. The fixing rule includes the index source, averaging
window and fallback methodology. Both legs must have terminal-only cash
flows. The screen converts native quantities to a common base lot and
subtracts supplied fee and holding-cost bounds from the terminal spread.
Entry limits are expressed per base unit and apply at the supplied capacity.
The return denominator is supplied capital across both accounts and any
external reserve. The cash bound depends on matched fills, collateral through
expiry and fulfillment of both contracts. Record venue minimums and verify
capacity and settlement specifications before supplying an input.

```sh
target/release/cross_venue matched-settlement examples/matched_settlement.json
```

OKX Nitro Spreads and Deribit Combo Books execute their supported legs as a
combination. Paradigm reports clearing status for all legs at the underlying
settlement venue. Keep order matching and final clearing evidence distinct.
The current AlephTX cross-venue simulator submits independent IOC legs and
models partial execution and recovery.
[OKX Nitro Spreads](https://www.okx.com/en-gb/help/nitro-spreads-introduction) ·
[Deribit Combo Books](https://support.deribit.com/hc/en-us/articles/31424954956061-Combo-Books) ·
[Paradigm clearing](https://docs.paradigm.co/unified-markets/summary)

Deribit linear futures deliver in USDC using a 30-minute Deribit index
average. OKX's USDT expiry product specifies USDT and a final-hour index
average. Both venues also describe daily PnL settlement. A matching BTC
ticker and delivery timestamp leave these contracts outside the current
terminal-only screen.
[Deribit linear futures](https://support.deribit.com/hc/en-us/articles/31424954805405-Linear-Futures) ·
[Deribit daily settlement](https://support.deribit.com/hc/en-us/articles/29734325712413-Settlement) ·
[OKX expiry futures](https://www.okx.com/en-us/help/expiry-futures)

The public Deribit Box probe checks the signed legs of each open four-leg
combination for common USDC settlement, fixing, expiry and multiplier.
A long Box has a fixed gross expiry value equal to quantity times strike
width. The probe calculates a depth-weighted debit and uses the native
fill-or-kill limit to bound cash outflow. A positive net cash-flow bound
also requires bounded delivery, financing and trading costs. Account
margin remains unknown from public endpoints.

```sh
python3 scripts/probe_deribit_box.py
```

At 11:18:02 UTC on 2026-11-13, the probe checked 36 active USDC combinations,
including four with four legs and complete leg metadata, against 3,162
open USDC linear options. It found zero native Box candidates. The
documented combo creation types omit Box. Block RFQ has a separate minimum
size, quote and margin workflow. No account quote or margin evidence was
collected.
[Deribit combo types](https://support.deribit.com/hc/en-us/articles/31424954956061-Combo-Books) ·
[Combo order types](https://support.deribit.com/hc/en-us/articles/25944794271261-Option-Combo-Order) ·
[Linear USDC options](https://support.deribit.com/hc/en-us/articles/31424932728093-Linear-USDC-Options) ·
[Fees](https://support.deribit.com/hc/en-us/articles/25944746248989-Fees) ·
[Block RFQ](https://support.deribit.com/hc/en-us/articles/25951371614621-Deribit-Block-RFQ)

### Extended execution research

Oasis records Extended BTC-USD and ETH-USD active CLOB perpetuals with
Lighter on one receipt clock. The initial sealed sample included
sequenced depth and public trades for both markets.
[Capture and replay](PERP_RECORDING.md#extended--lighter-synchronized-capture)

Before using CLOB queue simulation, check `isRfq` and verify the public depth
sequence, trade IDs, timestamps, fee tier and settlement currency. RFQ market
depth on the standard stream is indicative. The dedicated RFQ stream carries
resting orders that an opposing IOC may pick up at the maker fee. The RFQ
order endpoint has a one-request-per-second limit. Venue credits require
separate account evidence.
[Extended API and RFQ documentation](https://api.docs.extended.exchange/#rfq-markets)

Extended's published ordinary crypto rates are 0 bps maker and 2.5 bps
taker. Its RWA taker rate is 1 bp. Ordinary crypto maker rebates of 0.2,
0.4, 0.6 and 1.0 bps depend on trailing 30-day maker market share and a
$10 minimum daily payout. Lighter Standard regular orders are 0 bps on
both sides, with 300 ms added to taker and cancel/modify actions. Plus
uses 0.5 bps on both sides with the same delays. Treat these as venue
schedule inputs and confirm the actual account tier for execution research.
[Extended fee schedule](https://docs.extended.exchange/extended-resources/trading/trading-fees-and-rebates.md) ·
[Lighter fee and latency schedule](https://docs.lighter.xyz/trading/trading-fees)

Design references: [Hummingbot fill-driven hedge architecture](https://hummingbot.org/strategies/v1-strategies/cross-exchange-market-making/),
[Hyperliquid perpetual metadata](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint/perpetuals),
[Hyperliquid funding](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/funding),
[Hyperliquid signing vectors](https://github.com/hyperliquid-dex/hyperliquid-python-sdk/blob/master/tests/signing_test.py),
[Entropy HIP-3 deployment](https://docs.entropy.io/about-entropy/readme.md),
[Arcus typed signing](https://docs.arcus.xyz/api-reference/authentication),
[Variational API](https://docs.variational.io/technical-documentation/api).

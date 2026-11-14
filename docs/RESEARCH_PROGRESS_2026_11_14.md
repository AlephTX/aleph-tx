# Independent strategy research — November 14, 2026

The public recordings are shared evidence for three independent programs:
quant profit, hedge arbitrage and incentive turnover. The main shared source
has sealed and the first new continuous ledgers have completed. The original
quant baselines remain negative; a fully Core-only fair-value control is
positive on the inspected window and now has a separate prospective test.
Capture duration, a quoted spread and a funding forecast are not realized
revenue. Earlier checkpoints below retain their original scope and timing.

## Three-program snapshot

All three programs share public archives while retaining independent
configuration, cash, capital/loss limits and execution state. The current
results are offline simulations. Historical variants on the same window
are selection evidence, not independent profit validation.

| Program | Current mechanism | Continuous ten-hour evidence | Decision |
| --- | --- | --- | --- |
| Quant profit | Inventory-aware Lighter Core Maker quoting; compare external reference, Core-only fair value and a local reversal gate | ETH Core-only Standard: +0.127003 / +0.418129 net at $500/$1,000 capital labels, on 988.73 / 2,085.70 gross turnover. Premium: −0.000782 / −0.139330; TTL 15/30 s does not improve Standard; Core-only reversal gate has no fills | Core-only Standard remains a candidate. All 15 and 18 Maker fills have the cancel-pending label at receipt; source-time cohorts remain unaudited. The frozen 13:00–23:00 prospective window is still required |
| Hedge arbitrage | Matched basis/funding positions; require executable edge after both legs' fees, depth, delay, exit costs and funding evidence | Indexed Extended/Core BTC and ETH: both flat, zero entries/volume; best evaluated net edges −5.421 / −6.144 bps versus +2 bps admission | No qualifying entry. RWA/funding candidates still need complete execution and settlement evidence |
| Incentive turnover | RH Maker entry/close with Core IOC hedges; independent neutral/long-first/short-first ledgers and 0/0.5/1/2/4/8 bps admission budgets | Conservative cancellation sensitivity: 47 complete cash ledgers out of 72; 32 zero-volume, 15 nonzero and all negative. BTC 4/8 bps complete arms reach 4,946–9,968 target volume at 1.336–1.651 bps cost, with 52–54 cycles | A modeled cost-volume curve, not profit or confirmed eligible volume. Keep 21 feed-gap failures, four funding failures and one incomplete price attribution visible |

Quant net includes the uncalibrated 0.25 bps allowance on gross turnover.
RH/Core monetary comparisons retain unverified USDG/USDC parity and
domain execution assumptions; reward value remains zero. More collected
hours do not establish counterparty tier, effective account fees or actual
venue matching. The detailed tables below preserve every tested variant.

## Imported execution-clock and cash-report audit

The November 14 review patch is integrated locally. The original
`cancel_pending_fills` label describes cancellation state when a trade is
processed. It does not establish that execution occurred after the cancel
request. New diagnostic cohorts retain unknown source-clock calibration;
the historical 15/18 counts and cash ledgers have not been rerun or replaced.

A synthetic conflict reproduced the cost-reporting bug: an exact net of
−0.4 and a legacy feedback net of +0.4 produced a falsely negative cost.
The imported fix rejects conflicting cash evidence and uses reconciled exact
totals for accepted reports. Historical cost curves above remain archived
selection results pending regeneration; this reproduction does not show that
those archived results contain the same conflict.

The current official account-types page specifies 200 ms additional
Maker/Cancel latency for RH Standard. This newly checked rule does not
rewrite the older 300 ms research assumption or the frozen prospective run.
Binance A/B requires separate receipt/availability evidence and a new frozen
window. See the [execution audit](EXECUTION_CLOCK_REVIEW_2026_11_14.md) and
[priority venue design](PRIORITY_VENUE_RESEARCH_DESIGN_2026_11_14.md).

## Execution and incentive review

The latest external review supplied useful hypotheses, but several claims
needed separate evidence. Public rules, code inspection and new simulations
are distinguished below. The eight quant sensitivities and 84 cancellation
arms in `execution-measurement-20261114T053515Z` use the already inspected
window; none is an untouched holdout or an account-tier recommendation.

### Reward eligibility changes the denominator

The public RH points rule excludes **Standard-to-Standard** activity for
both point types. Public trade prints do not identify the counterparty's
account tier or prove program qualification. The reports now keep target
turnover, rule-window candidate turnover and confirmed eligible turnover
separate; the last remains unknown. The 0%, 20%, 50%, 80% and 100%
eligibility cases change only the denominator of the existing cash ledger.
Zero eligible volume has undefined cost per eligible volume, not zero cost.
Premium status alone does not establish an award, and points retain zero
cash value. [Official RH points rules](https://docs.lighter.xyz/programs/lighter-on-robinhood-chain-points)

For the historical BTC $500 neutral case, $169.60 target turnover costs
$0.013667: **0.806 bps** per target turnover becomes **4.029 / 1.612 /
1.007 / 0.806 bps** at 20% / 50% / 80% / 100% assumed eligibility. The
cash loss is identical in every case. This review does not rerun orders or
change admission budgets based on an unobserved counterparty tier.

The public W13/W14 Hedge Boost is a dated program rule, not a cash return
or permission to pool independent programs' budgets. A single hedged
strategy can account for both its own legs in its own ledger. The LPP
describes eligible resting quotes and sampled liquidity, not generic
open-interest hours. Funding received or paid belongs in full-cycle net
cash even when reported as a separate attribution component.
[Liquidity Partner Program](https://docs.lighter.xyz/programs/liquidity-partner-program)

Published Core Standard/Premium delays and fees were checked directly.
The RH-specific **300 ms** assumption was not independently confirmed by
the retrieved domain documentation; a local configuration is not external
verification. The rule snapshots, receipt times and hashes are archived in
`public-rules-review-20261114T052857Z`; failed follow-up lookups are retained.
[Lighter fees and latency](https://docs.lighter.xyz/trading/trading-fees) ·
[Robinhood domain overview](https://docs.robinhood.com/chain/lighter-domains/)

### Premium and longer quote lifetimes did not improve the control

All eight new ETH quant ledgers finish flat with verified feed state. The
two Standard controls reproduce the earlier Core-only ledgers. Each cell
below is net quote PnL, including the unchanged 0.25 bps model allowance.

| Frozen execution case | $500 / $50 | $1,000 / $100 |
| --- | ---: | ---: |
| Standard control | +0.127003 | +0.418129 |
| Premium fee/latency counterfactual | −0.000782 | −0.139330 |
| Standard, 15 s replacement lifetime | +0.112065 | +0.335907 |
| Standard, 30 s replacement lifetime | +0.112065 | +0.324142 |

The Premium counterfactual uses the public zero-stake 0.4/2.8 bps
Maker/Taker fees, 0 ms additional cancellation delay and 140 ms additional
Taker delay. It retains **250 ms transport**. All five Maker fills at $500
and all seven at $1,000 have the cancel-pending label at receipt. The Standard
controls have 15 and 18 Maker fills respectively, with the same receipt-state
label. These counts alone cannot locate execution inside the cancel window.
The new fill path and fees must be considered together; this is not a
fixed-ledger fee subtraction.

The existing lifetime knob applies to ordinary quote replacement. The
strategy's edge-floor withdrawal check runs before that replacement plan;
risk withdrawals, funding drains and exits remain active. Cancellation
requests change only slightly in the TTL cases: 20,991 to 20,981 at $500,
and 20,861 to 20,857/20,860 at $1,000. No reason-level frequency was
measured, so the edge-floor path is a code-confirmed bypass, not a measured
attribution of every cancellation. No hard minimum-life risk exemption was
introduced.

### Cancellation overlap changes fills and validity

All **84 arms** finished: 72 with the optional conservative cancellation
guard and 12 strict 2 bps controls. Every strict control reproduces its
parent ledger. With the same 100 ms clock guard, the new mode includes
the uncertain interval from 100 ms before to 100 ms after cancellation
effectiveness. It preserves queue, delivery, funding and risk checks;
neither mode is a calibrated bound on real PnL.

After the exact-cash review below, the 72 conservative arms contain
**47 complete cash ledgers: 32 with no volume and 15 with volume, all
negative after overhead**. The other 25 are invalidated by 21 trade-feed
gap exposures and four unresolved funding boundaries.

| Admission budget | BTC: complete cash-ledger observations | ETH: complete cash-ledger observations |
| --- | --- | --- |
| 0 / 0.5 bps | Each budget: all six arms have zero volume | Each budget: all six arms have zero volume |
| 1 bps | Four arms: 99.44–198.88 target volume, 0.119–0.121 bps cost, one cycle each; two arms zero | All six arms zero |
| 2 bps | Four arms: 369.51–1,059.08 volume, 0.656–1.269 bps cost, 3–7 cycles; two funding failures | Four arms: 21.30–24.00 volume, 2.360–2.462 bps cost, one cycle each; two funding failures |
| 4 bps | One arm: 9,803.10 volume, 1.651 bps cost, 54 cycles; five gap failures | All six arms fail at a gap |
| 8 bps | Two arms: 4,946.34–9,967.92 volume, 1.336–1.538 bps cost, 52–54 cycles; four gap failures | All six arms fail at a gap |

Volume units retain the unverified USDG/USDC parity assumption. The BTC
8 bps $500 long-first case has a complete cash ledger but four fills with
missing fresh midpoint labels; its full price-component decomposition is
unknown. Consequently **46**, rather than 47, conservative arms have both
cash and price attribution fully reconciled. Known cash cost and missing
benchmark attribution are reported separately.

At 2 bps, BTC $1,000 neutral changes from three to seven cycles and from
469.59 to 1,059.08 target volume. Total loss increases from 0.060691 to
0.098170, while cost per volume falls from 1.292 to 0.927 bps. The two
formerly tiny positive ETH long-first cases now fail the funding boundary;
they cannot be presented as complete profit observations under this mode.
The other four ETH 2 bps arms exceed their admission budget in realized
cost. Admission is an estimate, not a guarantee of realized execution cost.

Some BTC 4/8 bps paths now finish without exposure at a recorded gap. The
underlying gap remains in the tape; the new fill path did not repair source
continuity. These are separate model sensitivities on one inspected window,
not independent samples or production capacity estimates. No posture is
promoted from these results.

The original guard report SHA-256 is
`043cb901dce0230319b92f954d8b0e699ce96e56535b0fbf0cad0ff3d09022eb`;
the quant report is
`a802e7f2601fbb5e4b982d7fa015c49f428ba5a9496f933e1f61d26d0436766f`.
`execution-review-20261114T060430Z` preserves the first aggregate review.

### Exact full-ledger cash proof

One new BTC short-first ledger was initially rejected only by the Python
aggregate check: all three cycles reconciled exactly in Rust, but the
legacy float JSON total differed from the decimal cycle sum by `1e-17`,
exceeding the existing ULP bound. That rejection remains in the original
report. The tolerance was not enlarged.

New diagnostics serialize the complete ledger's before/after-overhead net
as decimal strings and verify both cycle sums against the exact engine
cash. Python requires that proof when present. In
`exact-cash-proof-20261114T055810Z`, the affected arm and one strict control
were replayed with identical parameters. Every original non-diagnostic
report field and every cycle record matched exactly. The append-only
review corrects that one cash-attribution classification, retains the
separate missing-midpoint case and all 25 execution failures, and preserves
all 12 strict control matches. Its report SHA-256 is
`40f6deffb51871031b4effd2d2c42a4a0afaa42ac1d88f04ed3ca98c1fdec54b`.

### Forced close is not a universal dominant cost

The unchanged 72-arm RH ledger review is archived separately as
`reward-cost-review-20261114T053743Z`, report SHA-256
`16e2d41208ff0ad954c28d2730d264d6d853855daea3c2358aeba64a5746b2a4`.
Among the eight complete nonzero arms, only BTC $1,000 neutral and
short-first contain forced-close fills: two affected cycles and four fills
each. Their signed forced-close execution cost is 0.018032 quote units,
or **29.7% / 31.5%** of their respective positive full-cycle costs. The
other six have no forced-close fills.

This ratio compares execution cost against net cost including price drift,
fees and overhead. Offsetting components can make it negative or above
100%; it is not a bounded loss allocation or a reason to relax a risk
limit. Entry markouts, hedge submission slippage and additive cash
decomposition remain separate diagnostics.

The earlier prospective **13:00–23:00 Beijing** test retains its original
six configurations, policy and executable hashes. These new results do
not change that window, its parameters or its definition of completion.

Final verification covers 204 matching source files in the latest isolated
Oasis snapshot, plus unchanged identities for the preceding matrix, its 72
parent ledgers, 84 guard configurations and eight quant configurations.
Local checks pass formatting, Clippy, Go vet, **549 Rust tests** (six ignored),
Go race tests, **85 Python tests** and Rust/Go release builds. Oasis passes
Clippy, **551 Rust tests** (seven ignored), **85 Python tests** and the
research release build. The local rerun used a writable temporary Go cache
and permission for loopback fake-venue tests after sandbox denials; those
initial environment failures remain in the local logs. Documentation links,
diff whitespace and the 24-path sensitive-pattern review pass. At this
verification checkpoint, the changes were still uncommitted.

At **14:08 Beijing**, `execution-final-checkpoint-20261114T060808Z`
binds the corrected curve summary, both source verifications, original
reports, exact-cash review, public rules and prospective policy. Its
inventory SHA-256 is
`0a1284abc1acae274aedf3d35cb6ebd419a331e88b777f290af258f4485ab595`.
The prospective collector was writing normally with its policy,
configurations and executable unchanged; disk reserve was 18.34 GiB.

## Cost-curve and Core-only follow-up

The external comments prompted two separate changes: prioritize RH/Core's
budget-versus-volume curve, and remove external books from the quant
fair-value path itself. Lower explicit fees are not the assumed remedy for
adverse fills. Parameters remain frozen before each comparison, and the
original ten-hour window remains selection data.

### RH/Core cost curve: sparse fills and explicit gaps

`rh-turnover-cost-curve-20261114T043334Z` finished all **72 frozen arms**:
BTC/ETH, two capital/order sizes, three opening postures and six cost
budgets. After reviewing float serialization, **48 ledgers are complete**:
40 have zero volume and eight have nonzero volume. The remaining **24**
fail with `resting_order_during_trade_gap`; their partial volume cannot be
presented as complete ten-hour capacity. The 750 ms timing audit passed,
but does not erase the source's three disconnects per market.

| Admission budget | Willing cost / $1m target volume | BTC: complete ten-hour observations | ETH: complete ten-hour observations |
| --- | ---: | --- | --- |
| 0 bps | $0 | All six arms: no fills | All six arms: no fills |
| 0.5 bps | $50 | All six arms: no fills | All six arms: no fills |
| 1 bps | $100 | All six arms: no fills | All six arms: no fills |
| 2 bps | $200 | $70.72–$469.59 volume; 0.434–1.437 bps realized cost; 1–3 cycles per arm | `long_first`: $99.72/$199.83 volume, −0.587 bps cost, one cycle each; other four arms: no fills |
| 4 bps | $400 | All six arms incomplete at a trade-feed gap | All six arms incomplete at a trade-feed gap |
| 8 bps | $800 | All six arms incomplete at a trade-feed gap | All six arms incomplete at a trade-feed gap |

Ranges compare separate arms, not pooled trades or independent observations.
Negative cost means positive modeled PnL: the two ETH long-first cases earn
only **$0.00585 / $0.01172**, with one cycle each. They do not establish a
profitable posture. BTC nonzero observations correspond to about
$43.38–$143.68 per million of target volume, but **1–3 cycles cannot support
a population cost estimate**. No variant is promoted from this curve.

Explicit fees are zero under the frozen assumptions. The curve's monetary
units assume unverified USDG/USDC parity, and RH account fees, delays,
funding terms, conversion costs and incentive eligibility remain unqualified.
Points are valued at zero. A budget controls modeled admission; actual
realized cost and the independent loss limit remain separate quantities.

The BTC $500 neutral example at 2 bps has two complete cycles and $169.60
target volume. Its additive cash decomposition is:

| Component | Signed cost in quote units |
| --- | ---: |
| Entry Maker versus fill-time midpoint | −0.0062280 |
| Entry hedge versus fill-time midpoint | +0.0038604 |
| Maker close plus hedge close | +0.0019648 |
| Both legs' price drift | +0.0055900 |
| Total price loss | +0.0051872 |
| Explicit fees | 0 |
| Modeled overhead | +0.0084799 |
| Complete-cycle total | +0.0136671 |

Its entry 1 s/5 s mid-price costs are **+0.349/−0.429 bps** of labeled
entry turnover; entry hedge slippage against submission touch totals
**+0.0042104** quote units. These overlapping diagnostics are not added to
the cash decomposition. The ETH long-first example has adverse 1 s/5 s
entry moves of +0.932/+1.734 bps but a favorable full-cycle basis move;
an early markout alone would miss its eventual tiny gain.

All complete nonzero cycles pass the engine's exact decimal cash and price
reconciliation. The first Python aggregate comparison rejected some of them
because legacy float JSON differed by as little as `5e-19`. Original output
is retained. New diagnostic amounts serialize as decimal strings, and the
separate review accepts legacy rounding only within per-value float ULP
bounds after the exact Rust checks pass. No orders or PnL were recomputed.

The append-only review is `cost-quant-review-20261114T050002Z`, report hash
`273da7e8dc690bb2d67f3c176cfe102a04a8bd8c2af0c722dee3f5ff4ce90512`.
Its source manifest hash is
`1fb981ac8cc1dc7fa0aeca4962756935df8af62ae6b2d667dfe10e5796def8fc`.
Per-cycle records, all 72 configurations, failed runs and original/reviewed
reports remain on Oasis outside Git.

### Core-only fair value: a candidate, not a validated profit claim

`core-only-reversal-20261114T044109Z` completed six independent ETH ledgers.
The external-reference controls reproduce the parent net PnL and Maker/Taker
turnover. All six runs are terminally flat with verified feed state.

| Capital / order size | Original reference net | Core-only reference net | Core-only + reversal gate net | Core-only Maker fills |
| --- | ---: | ---: | ---: | ---: |
| $500 / $50 | −$0.33399 | +$0.12700 | $0, no fills | 15 |
| $1,000 / $100 | −$0.27347 | +$0.41813 | $0, no fills | 18 |

All net values include 0.25 bps modeled overhead on gross turnover.
Core-only gross turnover is $988.73 / $2,085.70, giving approximately
**+1.284 / +2.005 bps**. The fixed reversal gate is signal-ready about 40%
of the time but produces no fills with Core-only pricing. Its zero PnL is
inactivity, not evidence of a zero-cost strategy. The fair-value change and
the entry filter must not be credited as one successful mechanism.

This is a small, previously inspected sample. It establishes a reproducible
candidate to test; it does not establish expected profit, capacity or a
production execution edge. All 15 Core-only Maker fills at $500 and all
18 at $1,000 occur while cancellation is pending, so the result remains
especially dependent on
matching and delay assumptions. Their 1 s/5 s markouts are +0.603/+1.041 bps
and +1.222/+2.271 bps respectively, not additional realized revenue.
Core-only mode publishes no external book, and
synthetic regressions require identical results when external prices are
ordinary, displaced or omitted. Risk, pending cancellations, funding guards
and terminal-drain rules remain active.

### Overhead and size effects

The 0.25 bps amount is an **uncalibrated model allowance**, not Lighter's
published fee for Standard regular orders. The official schedule lists
zero Maker/Taker fees for those orders and separate fees for advanced order
types. This does not confirm an account's tier or Robinhood-domain costs.
[Lighter fee schedule](https://docs.lighter.xyz/trading/trading-fees)

For the original four baselines, removing this allowance still leaves
negative bps: ETH $1,000 **−0.0279**, BTC $1,000 **−0.0871**, ETH $500
**−0.2929**, BTC $500 **−0.8152**, approximately. Their reported sign is
therefore not solely an artifact of adding the allowance. Reports now retain
pre-overhead PnL and 0/0.1/0.25/0.5 bps fixed-ledger sensitivities.

The two ETH sizes do not share the same fill path: the original $500 case
has 109 Maker and 66 Taker fills, versus 100 and 63 at $1,000. Average Maker
fill notional is about $29.03 versus $53.41. Overhead is proportional and
explicit fees are zero in the model. These observations do not establish
minimum order size or a fixed liquidation charge as the cause of worse bps;
changing size also changes matching, queue consumption and subsequent exits.

### Indexed basis and collection status

The queued indexed comparison is now completed separately as
`indexed-basis-followup-20261114T045120Z`, preserving the earlier incomplete
queue attempt. Both BTC and ETH cover **01:57:48.507–11:57:48.507 Beijing**,
with a separate one-minute terminal drain and the unchanged basis policy.
Both diagnostic ledgers complete flat with **zero entries and zero volume**.
The best evaluated net edges are **−5.421 / −6.144 bps**, below the
unchanged +2 bps entry requirement. Zero PnL here is inactivity, not profitable
arbitrage. No historical fee reduction or entry-threshold change was used
to manufacture trades.

By **12:56 Beijing**, the native HL and indexed captures had sealed, as had
the expanded venues source (**247,981,500 compressed bytes**, through
12:45:44.708) and its fast-book supplement (**54,282,193 bytes**, through
12:50:26.916). The separate RH 12-hour capture is also complete. Intermediate
`incomplete` statuses while the broad collectors were finishing archive
verification were not final source failures; both subsequently reached
`sealed`. These seals verify capture artifacts, not universal economic
coverage. Arcus/BULK acknowledged trade channels, BULK funding and RWA
contract/session/reference qualification remain gaps; Arcus funding was
supplemented separately as recorded below.

The **12:57** append-only inventory contains **448 unique public archives**,
**26 metadata objects** and **4,267,910,691 compressed archive bytes**.
It excludes active partials and is not a coverage certificate. Its hash is
`2d5a73267a92dab8c8d2c801c4d55a0152944003835d128092c7e30ade9cc65e`.

Oasis had **18.84 GiB** free disk and **10.71 GiB** available memory at that
checkpoint. The existing 48-hour collection and new prospective study retain
their original evidence. No account operation, live order or existing-service
restart was performed.

### Verification and evidence preservation

Local `make fmt-check`, `make check`, `make test`, `make build` and the
offline example-profile check passed. This includes Go race checks,
**547 Rust library/binary tests** on macOS (six ignored) and **81 Python
research tests**. The final Oasis snapshot passed Clippy, **549 Rust tests**
(seven ignored), the same 81 Python tests and a release research-engine
build. All **201 source files** match the final local snapshot by SHA-256.
The platform count difference comes from existing platform-gated tests.

Regression coverage includes Core-only reference isolation, exact cycle
cash/price reconciliation, all three posture ledgers, missing markout labels,
legacy JSON rounding bounds and zero-volume/incomplete reporting. Original
engine reports retain their frozen source versions; the subsequent cost
review only corrects serialization comparison and adds aggregate diagnostics.
Documentation links, CLI help, whitespace and sensitive-pattern scans passed.
Changes remain uncommitted, with other existing workspace edits preserved.

`research-checkpoint-20261114T050301Z/inventory-entry.json` binds seven
source/report identities, including the public inventory, final tested
source, selected quant result, basis result, RH inputs/review and prospective
policy. The aggregate review driver is saved beside its report. The selected
quant report hash is
`adcf9b96a183e29939445304b680ed861da0928b67dcf419301cb2ada0da9315`;
the indexed basis report hash is
`ff636bc06e7088425265e020b751ce5135ebbe8ea8c0d3a950fcaff434144084`.

### Prospective window already frozen

`core-rh-prospective-10h-20261114T044946Z` freezes the same six ETH arms
before recording starts. Decision time is **November 14 13:00–23:00
Beijing**, with warm-up **12:59–13:00** and terminal drain through
**23:00:15**. Public collection began at **12:49:46**, requests 10 h 15 m,
and is followed automatically by CRC/EOF verification, archive partitioning,
compilation and replay. A capture, feed, funding or terminal failure remains
incomplete; no resampling or window reset is allowed.

The frozen policy SHA-256 is
`9dea62dc023728e154c5a2295ecc50439b7fe6497f926f27a8acac9ca3163f35`.
It includes exact configs, selected-parent hashes, engine/compiler identity,
current native market metadata and the fixed window. Collection also retains
BTC and RH data for independent later studies; those are not automatically
holdout evidence for an unfrozen future turnover policy. The job uses public
endpoints only, low CPU/I/O priority, a 2 GiB capture cap and 14 GiB disk
reserve. Existing collectors and services were not restarted.

## External review follow-up

The local reversal hypothesis now has an offline entry gate and paired
runner, with the original ETH baseline retained as the economic control.
Its parameters, source-age rules and 1/5/30-second fill diagnostics are
specified in the [runbook](TEN_HOUR_RESEARCH.md#core-local-participation-probe).
This is a fixed mechanism probe, not the fitted classifier suggested by
the external review. It uses local entry evidence while retaining the
baseline fair-value reference, cancellation delays and exit controls.

### Local reversal: fewer losses, worse unit economics

The fixed no-hold probe completed on Oasis as
`local-reversal-probe-20261114T030153Z`. All four ledgers cover the original
continuous ten-hour decision window, with one startup, ten verified funding
guards, zero terminal positions and no unresolved orders or reconciliation.
Both controls reproduce the previous PnL and Maker/Taker turnover totals.
All results include the same 0.25 bps overhead on gross turnover.

| ETH capital | Control net / 10 h | Local gate net / 10 h | Control net bps | Local gate net bps | Local Maker fills / cancel-pending |
| --- | ---: | ---: | ---: | ---: | ---: |
| $500 | −$0.3340 | −$0.0692 | −0.543 | −1.257 | 14 / 13 |
| $1,000 | −$0.2735 | −$0.1321 | −0.278 | −2.089 | 13 / 13 |

Local signal readiness is **39.84% / 39.82%** of decisions, substantially
more active than the earlier 250 ms external-reference gate. Readiness
requires usable history; it does not mean the reversal condition passes.
The gate admits only 1,916 / 1,906 entry targets. These are repeated quote
decisions, not independent signals or trades. Both runs reject 185 late or
otherwise invalid-time prints from predictors while retaining the original
matching tape.

Gross turnover falls from $6,152.21 to $550.29 and from $9,841.77 to $632.28.
The smaller absolute losses therefore do **not** establish a better edge:
both unit returns worsen and both full ledgers remain negative. The local
Maker fills have 1/5/30-second markouts of **−1.187 / +1.251 / +0.315 bps**
at $500 and **−0.979 / +0.217 / +3.629 bps** at $1,000. All horizon labels
are covered, but these are only 14/13 fill observations. Later positive
markouts do not imply that an executable held position would earn them.

The original controls have 64/109 and 51/100 Maker fills during pending
cancellation; the local gate has 13/14 and 13/13. This motivated one
separately frozen **1,000 ms participation-permit hold** comparison. It
preserves source freshness, risk limits and baseline quote replacement;
it is not a promise to leave an unsafe order resting.

The hold comparison, `local-reversal-hold-20261114T030835Z`, also completed
all four ledgers and reproduced both controls:

| ETH capital | 1 s permit hold net / 10 h | Net bps of gross turnover | Maker fills / cancel-pending | Held-permit entry targets |
| --- | ---: | ---: | ---: | ---: |
| $500 | −$0.0883 | −1.719 | 12 / 12 | 2,654 |
| $1,000 | −$0.2500 | −3.349 | 12 / 12 | 2,647 |

Both outcomes worsen relative to the no-hold gate. The one-second Maker
markout improves to −0.489 / −0.066 bps, while the complete net ledger
worsens; optimizing this single label would select the wrong economic
outcome. Neither fixed gate qualifies for promotion. This result does not
test a mandatory minimum order lifetime, since baseline replacements remain
active. It also does not reject every learned reversal mechanism.

Across both experiments, all eight runs have complete terminal and funding
evidence. They reuse **one** inspected market window. Both failed profit
variants, their controls and all source/config/engine hashes remain intact.
The final review SHA-256 is
`85a26d3fbaaaf39e0d4b202cb9f29847f3ebe3ef7bff75eb37e68a7acb663f45`;
the hold comparison report SHA-256 is
`f0fc05aff957121cecc3e2b5ca0883f0f5fcdb1067f9a1bf7b1072354c43b595`.

At **11:12 Beijing**, Oasis had **22.18 GiB** disk and **10.66 GiB**
available memory; six collectors, the existing feeder and Shadowsocks
processes remained present. No trading, service restart or collector restart
was performed. All **197** files in the final isolated research snapshot
matched the current local source by SHA-256.

The first Linux snapshot attempt, `local-reversal-probe-20261114T025728Z`,
omitted four existing synthetic telemetry fixtures and failed those tests.
It is retained unchanged. The second snapshot includes them and passed
Clippy, the full offline Rust suite, all 76 Python research tests and release
engine construction before any replay.

Final verification passed `make fmt-check`, `make check`, `make test` and
`make build` locally, including Go race tests and 76 Python research tests.
Oasis passed Clippy, 544 Rust library/binary tests (seven pre-existing
ignored cases), the same 76 Python tests and the research-engine release
build. Eight new Rust regressions cover causal signal history, freshness,
disconnects, exit preservation, bounded permit holding and multi-horizon
markout accounting. Documentation links and `git diff --check` passed;
the reviewed change paths had no sensitive-pattern findings. Changes remain
uncommitted; earlier local changes and all runtime evidence were preserved.

### Funding and current market terms

New public captures and audits are immutable on Oasis, outside Git. The
14-day interval is **October 31 10:00 through November 14 10:00 Beijing**.
Five HIP-3 series contain 336 hourly slots; `io:AAPL` contains only 271.
All averages use matched observed slots, with no filling of missing values.

| Pair | Matched hours | Mean `xyz − io`, bps/hour | Prior 10 h direction: next-block positive / tested | Mean next-block gross funding, bps/10 h |
| --- | ---: | ---: | ---: | ---: |
| NVDA | 336 | −0.11473 | 23/32 | +0.48061 |
| SNDK | 336 | −0.07295 | 25/32 | +0.58978 |
| AAPL | 271 | −0.14721 | 16/25 | +1.00240 |

The negative mean favors long xyz / short io for funding alone. The
lagged-direction diagnostic uses consecutive, complete, nonoverlapping
ten-hour blocks anchored to the interval start. It assumes the previous
block's published slots were available then; downloaded history does not
reconstruct their original receipt times. No fees, spread, impact, basis
P\&L or collateral costs are deducted. This is historical selection, not
a profitable or fully causal execution test, and is too weak to justify
short-cycle carry on its own.

Current metadata confirms Growth Mode for all six io/xyz NVDA/SNDK/AAPL
markets. `io:AAPL` explicitly has `onlyIsolated`; missing fields on other
markets are not evidence of account margin capacity. The reviewed io
GOOGL/MU/META/INTC entries are delisted. Growth Mode is market-specific and
applies to the current fee context; a historical 3/9 bps blanket assumption
or blanket 0.3/0.9 bps assumption is inappropriate.
[Entropy fee schedule](https://docs.entropy.io/equity-perp-mechanics/fees)

Arcus funding history is now supplemented for BTC, SPY, QQQ, NVDA, MU and
SNDK: **1,000 records per market, including 336/336 hourly slots in this
14-day window**, no duplicate timestamps and matching native market IDs.
Official documentation confirms microsecond timestamps and hourly decimal
rates. Arcus-minus-xyz NVDA averages **+0.04562 bps/hour**, before any
execution or basis cost; the earlier +1.04 bps/hour snapshot is not a
persistent-rate assumption. Published rates still need payment marks and
contract/position coverage to become simulated cash flows. Acknowledged trade-feed coverage for the new Arcus capture and BULK
funding remain separate gaps.
[Funding sources and transformations](RESEARCH_DATA_SOURCES.md#november-14-funding-supplement)

Preserved evidence includes `public-funding-review-20261114T025117Z`,
`arcus-funding-supplement-20261114T025903Z` and
`public-funding-audit-20261114T030022Z`. The final units-reviewed report has
SHA-256 `c5774bf71744fb5ef0d20fe2204258b235f908b7e078210fd21b4fa11fd2ef29`.
Prior incomplete schema assertions and their original reports remain intact.

### Interpretation of the external comments

- The two ETH size outcomes show insufficient robustness; one shared window
  does not identify the cause as statistical noise. The local gate removes
  external-reference dependence from participation only. It does not prove
  a 50–200 ms execution edge or eliminate the baseline reference dependency.
- The cited reversal paper assumes the most favorable Binance fee tier and
  treats latency as negligible in its analysis. It motivates a hypothesis;
  it cannot establish Lighter returns with our cancellation/transport costs.
  [Primary paper](https://arxiv.org/html/2502.18625v2)
- The funding paper's abstract says 95% forced exits, but section 4.7 reports
  19/20 spread reversals and 12/20 forced exits under its simulated rule.
  This internal distinction is not a universal liquidation probability.
  [Primary study, section 4.7](https://www.mdpi.com/2227-7390/14/2/346)
- Boros is registered as a documentation-stage candidate. Fixed funding
  cash flows still require matching indices, settlement and collateral;
  basis dislocations and Boros liquidation can break the intended result.
  [Boros risk specification](https://docs.pendle.finance/boros-docs/arbitrage-with-crossex/risks-and-security)
- A delayed Maker hedge changes single-leg exposure. Robinhood/Core dual
  Maker execution therefore needs its own bounded lifecycle and independent
  ledger; zero displayed fees do not establish zero cycle cost. Current
  immediate-hedge accounting remains the control. XEMM profitability also
  belongs in a separate quant ledger, with no turnover cross-subsidy.

## Morning execution and source-quality update

At **10:32 Beijing**, the shared `2026-11-13-continuous10h-153508` campaign
was sealed: 10 h 15 m of recorded time, 729,795,803 compressed bytes, SHA-256
`3c5d8b4fa76bb33df36b52caf2d389785eeaf06d906fef757ba7416dad0f4f67`.
Six original collectors remained active at the 10:28 process check;
available disk was 22.33 GiB at 10:32.
The existing feeder and Shadowsocks processes remained present. No existing
collector or service was restarted.

The decision window is **November 13 23:45:09.240 through November 14
09:45:09.240 Beijing** (`1794584709240..1794620709240` ms). Quant has a
separate 60-second warm-up and both programs retain a 15-second final drain.
All comparisons below reuse this same market period. Variant counts do not
multiply independent market hours. The frozen job labels the source as
historical reuse; subsequent timing choices are explicitly inspected-history
sensitivities, not untouched holdouts.

### Quant: complete baselines and reference availability

All four baseline ledgers completed with zero terminal position, no resting
orders or unresolved reconciliation, verified trade-feed continuity and ten
flat-at-funding boundaries. Values below include 0.25 bps modeled overhead
on both Maker and Taker gross turnover; capital labels are paper scenarios.

| Baseline | Order size | Net quote after overhead / 10 h | Net bps of gross turnover |
| --- | ---: | ---: | ---: |
| BTC, $500 capital | $50 | −$0.5526 | −1.065 |
| BTC, $1,000 capital | $100 | −$0.3129 | −0.337 |
| ETH, $500 capital | $50 | −$0.3340 | −0.543 |
| ETH, $1,000 capital | $100 | −$0.2735 | −0.278 |

The originally frozen 250 ms Hyperliquid imbalance filter also completed.
It worsened both ETH ledgers: −$0.3480 at $500 and −$0.5295 at $1,000.
Only **23 / 158,609** and **23 / 160,304** decisions, respectively, had a
usable reference within that age bound (about 0.014%). It withdrew only
14 and 13 quote targets. These results measure an almost inactive filter.

The independent source audit found 336,062 ETH input book frames including
warm-up and drain. Hyperliquid source age had p50 **1,011 ms**, p95
**1,568 ms**, p99 **1,711 ms**; only 63 frames were at most 250 ms old.
A separate 90-second fast-book probe received 167 messages per BTC/ETH
market. Receipt-time age had p50 336 ms and p95 503 ms, with only one
message per market at most 250 ms old. No future source timestamps were
observed. This short probe establishes neither ten-hour coverage nor
50–200 ms execution feasibility; fast subscriptions alone did not satisfy
the original 250 ms condition on Oasis.

An independently frozen **1,000 ms reference-age sensitivity** retained the
same 0.60 imbalance threshold, fees, order sizes and execution lifecycle:

| ETH capital | Filter reference availability | Net / 10 h | Change versus control / 10 h |
| --- | ---: | ---: | ---: |
| $500 | 43.84% | +$0.0828 | +$0.4168 |
| $1,000 | 43.91% | −$0.6703 | −$0.3969 |

Both ledgers completed. The small positive result is not consistent across
size, and the $500 result is only +0.133 bps of gross turnover. It does not
support scaling capital or promoting this filter to live execution. The
next quant mechanism needs a complete-cash-flow improvement across sizes
and new periods, together with explicit signal availability. One-second
markout and late-cancel counts remain diagnostics, not additional PnL terms.

### Turnover: delivery admission before repeated execution

The original 750 ms Extended Maker delivery bound failed in all 24 arms.
After subscription-history deduplication, BTC had 3 / 12,063 target prints
over that bound (maximum 989 ms); ETH had 3 / 9,606 (maximum 815 ms).
Their full-window net costs remain unknown. A successful process exit did
not make those engine reports complete.

A separate 2,000 ms historical sensitivity retains the original prints and
extends the engine's late-fill/cancel waiting lifecycle. It does not reset
inventory, suppress gaps, change the 15-second drain or raise capital/loss
limits. The 750 ms defaults and failed reports are retained. The BTC matrix
completed all 12 arms: zero-cost admission produced no volume; at the
4 bps admission allowance, neutral/short-first each completed one cycle,
with target volume $99.85 / $199.70 at the two sizes and realized cost
**4.134 bps**. Long-first produced no volume. This again shows that an
entry-cost allowance is not a guaranteed realized-cost ceiling.

The full **24/24** sensitivity matrix has now completed. Every arm verified
ten funding slots on each leg and ended with both positions flat. All
twelve zero-cost admission arms produced zero target turnover. The six
ETH arms permitting 4 bps projected entry cost produced:

| ETH posture | $50 target size: target volume / net / cost | $100 target size: target volume / net / cost |
| --- | ---: | ---: |
| Neutral | $96.76 / −$0.0893 / 9.233 bps | $197.39 / −$0.1823 / 9.233 bps |
| Long-first | $293.20 / −$0.1012 / 3.450 bps | $595.61 / −$0.2405 / 4.037 bps |
| Short-first | $99.78 / −$0.0461 / 4.617 bps | $199.60 / −$0.0913 / 4.573 bps |

Costs use **all target-venue turnover**, including exits; points and
unconfirmed rebates remain zero. Each neutral/short-first arm completed
one cycle with a Maker close. Each long-first arm completed three cycles
with forced IOC exits. These few, overlapping opportunities cannot select
a reliable future posture.

The new result changes the priority of the next mechanism test. BTC's
completed Maker-close cycles paid zero modeled explicit fees but lost
3.633 bps through full-cycle prices plus 0.500 bps overhead. ETH neutral
also closed passively with zero explicit fees, yet lost 8.733 bps through
prices plus 0.500 bps overhead. A Maker exit or lower fee alone does not
repair the edge. The next entry policy should be judged on both venues'
arrival execution, hedge and closing cash flows, with late-cancel exposure
retained. Increasing the loss allowance is not evidence of improvement.

Robinhood data is present in the sealed shared source. Its independent
trade-only audit found **28,200 BTC** and **19,248 ETH** prints in the same
decision/drain interval; maximum delivery was 583 / 540 ms, with no 750 ms
violations. Both market streams retained three disconnect events. Passing
trade timing does not certify gap exposure, executable exits, USDG/USDC
economics, funding, fees or incentive eligibility. No Robinhood profit or
turnover result is inferred from this audit.

### Architecture and reproducibility

- Quant and turnover can be scheduled independently with `--program`; asset
  selection uses `--symbols`. Turnover no longer requires loading the quant
  engine or strategy config.
- A shared, hash-bound Maker trade-timing audit runs before the turnover
  matrix. Every rejected arm retains its frozen configuration and an unknown
  PnL, without starting an engine. Timing success remains separate from
  stream quality and economic qualification. The runner now exits nonzero
  for rejected/incomplete results.
- Quant comparison reports expose `fresh_reference_fraction`. The default
  250 ms filter and separate 1,000 ms sensitivity have explicit labels.
- Original raw data, failed replays and frozen jobs remain intact. New jobs
  reuse the sealed tapes and preserve their own configs, source/executable
  hashes and ledgers. No execution-engine or live strategy behavior changed.

The 10:15 append-only inventory contains **427 unique public archives**,
**26 metadata objects**, and **3,257,541,027 compressed archive bytes**.
It predates the 90-second fast-book probe, excludes active partials and does
not certify stream coverage. Its hash
is `aa68923e368cf9a270f356a6cdf195c2697ce6e0219fcf49232a0ce9e39eb356`.

| Evidence under `~/aleph-tx-research` | Purpose |
| --- | --- |
| `independent-profit-validation-20261113T174809Z` | Original continuous controls and 250 ms paired filter; indexed basis remains queued |
| `trade-timing-sensitivity-20261114T021520Z` | Original timing audits, immutable `public-inventory.json`, and separate 2 s turnover matrix |
| `reference-age-sensitivity-20261114T021937Z` | Separate 1 s ETH signal comparison at both capital/size cases |
| `hyperliquid-public-fast-probe-20261114T021937Z` | Sealed 90 s native fast-book probe; diagnostic only |
| `source-readiness-audit-20261114T022359Z` | Robinhood timing and ETH reference-age audit, hash `31cde1a4883b96a6ab24c0bbcfc3b224ae84e86f5afbf8a524158ee4411c69c5` |
| `research-admission-tests-20261114T022217Z` | Isolated Linux source snapshot, identity manifest and passing test log |
| `research-morning-review-20261114T023208Z` | Final 24-arm costs, terminal-position verification, parent report hashes and tested-source comparison; report hash `88d755c1408e33bb0b15ec6a87e4cbfc02c30a75704290e6c1184ae00bcadceb` |

Validation: **74 synthetic research tests passed locally and on Oasis
Linux**; CLI help and whitespace checks passed. Changes affect Python
research orchestration/reporting and documentation, not Rust/Go wire formats
or live execution. The final review matched all 31 Linux-tested source files
against the current local snapshot. The initial turnover sensitivity snapshot
predates the subsequent wrapper config-hash checks; engine and policy
semantics are unchanged, and each snapshot retains its own identity manifest.
[Runbook and reproduction flags](TEN_HOUR_RESEARCH.md#independent-scheduling-and-input-admission)

The collection milestone is now a usable baseline corpus, not full-platform
economic coverage. Indexed Extended/Core basis is still waiting for its
scheduled seal around 12:03 Beijing; native Hyperliquid and expanded RWA
jobs continue toward their earlier schedules. Arcus/BULK still lack the new
trade/funding channels, and RWA terms/session/reference evidence remains
unqualified. The earlier checkpoint below is retained as historical evidence.

## Economic priorities

| Program | Controlled next comparison | Acceptance and exclusions |
| --- | --- | --- |
| Quant profit | ETH Core Standard baseline versus event participation filtering; preserve the same orders, fees, latency and capital assumptions in both arms | Compare complete net dollars/hour, net bps, turnover, late-cancel fills and conditional markout. Keep directional inventory gains separate. The existing frozen participation experiment remains queued. |
| Hedge arbitrage | Complete the indexed Extended/Core basis baseline; use the new Entropy/TradeXYZ RWA tape to screen persistent executable basis and funding carry | Match payoff, units, collateral, oracle and settlement rules first. Charge both entry and exit. Funding forecasts remain risky carry, not locked income. New RWA raw collection is running; no RWA profit simulation is claimed here. |
| Incentive turnover | Extended/Core baseline and a separate Robinhood/Core candidate; independent `neutral`, `long_first`, `short_first` ledgers | Opening direction changes which Maker side may start, while confirmed fills remain fully hedged. Compare target volume and full-cycle cost with points and unconfirmed rebates at zero. The Robinhood runner is added locally; it has not replaced the already frozen Oasis job. |

Each program retains its own capital and loss budgets. The $500/$1,000 cases
are research budgets, not demonstrated margin capacity. Zero-cost turnover
and the explicit 4 bps paid-turnover allowance are different experiments;
the latter cannot pass a zero-cost claim. Every accepted run needs ten
continuous decision hours, separately measured warm-up and final exits.
[Measurement contract](TEN_HOUR_RESEARCH.md)

The supplied external review identifies useful candidates, but its single
snapshot and 76-minute diagnostic do not establish a ten-hour edge. In
particular:

- Entropy's published standard effective fees are 3/9 bps, while enabled
  growth-mode markets scale fees differently. Archive the individual
  market's mode and effective fees before classifying a route as expensive
  or cheap. [Entropy fees](https://docs.entropy.io/equity-perp-mechanics/fees)
- Lighter Core Standard regular orders have zero explicit fees and a
  300 ms additional Taker/cancel delay. This does not mean zero transport
  latency. Advanced order types have separate fees.
  [Lighter fees](https://docs.lighter.xyz/trading/trading-fees)
- Robinhood is a separate Lighter domain using USDG collateral. Core
  history, fee eligibility and settlement assumptions cannot silently
  substitute for it. The new runner requires explicit target fees and
  hash-bound native market mappings for both legs. Its target cancel/Taker
  delay is an explicit 300 ms research assumption, pending domain-specific
  confirmation; transport remains 250 ms. It does not certify API rewards,
  margin or USDG/USDC conversion economics.
  [Robinhood Lighter domains](https://docs.robinhood.com/chain/lighter-domains/)
- NVDA carry candidates need actual payment history and executable exits.
  SKHY/SKHX, SPACEX/SPCX and other similarly named products need explicit
  payoff and multiplier checks. Mixed last/mark/mid prices are insufficient
  basis evidence. Arcus incentive claims in the supplied review remain
  unverified for the account and API path.
- Submitting a new quote while cancellation is pending reserves risk for
  both orders. It cannot bypass cancel latency or release old exposure.

## Capture checkpoint

At **2026-11-13 18:33:52 UTC / November 14 02:33:52 Beijing**, the original
five collectors and the new broad-venue collector were running. Oasis had
27.97 GiB free disk, 13.70 GiB available of 15.61 GiB RAM, and a one-minute
load average of 0.26. The existing feeder and Shadowsocks processes remained
present; this was not an end-to-end client connectivity test.

| Recording | Progress at checkpoint | Scheduled end, Beijing |
| --- | --- | --- |
| Shared Core/RH/Hyperliquid/Extended/Coinbase | About 2 h 59 m; 250 MiB active compressed file | November 14 09:50 |
| Extended/Core 48-hour collection | About 7 h 20 m; 30 sealed segments plus active segment | November 15 19:13 |
| Separate RH/Core collection | About 3 h 16 m; 13 sealed segments plus active segment | November 14 11:17 |
| Native Hyperliquid trades/books/funding | About 59 m; still collecting | November 14 11:49 |
| Extended/Core with index evidence | About 46 m; still collecting | November 14 12:03 |
| New Entropy/TradeXYZ/Arcus/BULK | Started 02:30; initial channels verified | November 14 12:45 |

These are requested run boundaries, not certified uninterrupted coverage.
The two segmented jobs retain their original segment accounting. Gzip buffer
flush times also differ from exchange-message receipt times.

The new `public-venues-10h-20261113T183044Z` source records:

| Domain | Markets | Initially observed data |
| --- | --- | --- |
| TradeXYZ | NVDA, SNDK, SKHX, SP500 | Books, asset contexts, acknowledged public trades, funding-history responses |
| Entropy | NVDA, SNDK, SKHX, ANTH | Books, asset contexts, acknowledged public trades, funding-history responses |
| Arcus | BTC, SPY, QQQ, NVDA, MU, SNDK | WebSocket L2 books for all six markets |
| BULK | BTC, NVDA | WebSocket L2 snapshots for both markets |

Native symbols retain `xyz:`/`io:` or the venue's `-USD` suffix. Arcus/BULK
trade streams and funding history are **not** included in this new recording.
Book snapshots alone cannot prove queue position or Maker execution. RWA
independent equity prices, session and corporate-action evidence still need
to be joined and validated before economic qualification.

The initial full-depth HIP-3 books arrived roughly every 12 seconds. A
complementary `public-venues-fast-10h-20261113T183526Z` job therefore started
at 02:35 Beijing, without restarting the existing jobs. Its fast subscriptions
recorded 80 book messages per market over an initial roughly 43-second
prefix, materially denser but not proof of 50–200 ms opportunity coverage.
It is scheduled through 12:50 Beijing. Both new jobs run at low CPU/I/O
priority, with a 2 GiB compressed-file cap per job and a 14 GiB disk reserve
stop. Reaching either guard leaves an explicitly incomplete recording.

## What is decoupled, and what remains

Collectors accept venues, domains, markets and channels; they do not accept
strategy objectives or capital allocations. New sealed captures partition
into `exchange/domain/source/UTC-date/UTC-hour.ndjson.gz`, preserving original
availability order and hashes. The three programs consume those common
sources with independent state and ledgers.

Operational campaign directories and older grouped files still exist.
Some jobs overlap in markets or channels; overlapping observations cannot
be counted twice as market time. This is not yet a single consolidated,
deduplicated venue-wide repository. Preserve the original files and lineage;
merge complementary sources by receipt time with explicit deduplication and
gap checks. [Archive contract](PUBLIC_MARKET_ARCHIVE.md)

The inventory scanner now discovers the newer `hyperliquid-public-*`,
`public-venues-*` and index-smoke campaigns and exact `metadata` directories.
It retains explicit public metadata allowlists and skips active partials,
private-looking files and symlinks. The immutable
`public-inventory-20261113T183101Z/catalog.json` found **365 unique public
archives**, **20 unique metadata objects**, and four duplicate aliases;
unique compressed raw archives totaled **2,033,373,487 bytes (1.89 GiB)**.
Its inventory hash is
`8482ede31cfee42de8f9144ca7cf386fdd3ec8984665afe5d581dcdf554bb221`.
This allowlisted inventory excludes active files, derived results and
unrecognized evidence; it is not total disk usage or a coverage certificate.

Raw data and prior backtest results were retained. No existing collector,
node service or frozen experiment was restarted or rewritten in this update.
The next economic conclusions must come from sealed, qualified ten-hour
windows with final inventory and both legs' costs reconciled.

## Binance 同机链路与下一窗口冻结（2026-11-14 16:04 BJT）

审计修复先完成提交并推送 `2d5a5c7`。随后新增公开 Binance 单币 BBO、同进程精确时间归档、独立盘口编译和研究专用方向风险过滤；协议见 [Binance A/B](BINANCE_REFERENCE_AB.md)。原有两份其他工作文档未修改或暂存。

**新连续验证尚未开始，没有新的十小时盈利结论。** 90 秒 Oasis 同机探测已封存，共 127,842 条消息，覆盖 RH/Core、Arcus、BULK、Entropy、TradeXYZ；Binance BTC/ETH/NVDA/SNDK 均有 BBO。ETH 原始 51,884 条 BBO 全部通过本轮字段、时钟差诊断和原始序号检查。全归档同进程序号验证通过，队列消费延迟最大约 4.31 ms。源时间与本机接收差值中位数约 18–21 ms，仅为未校时差值。

派生 874 个独立盘口帧，Core/RH 新鲜帧分别 871/870。短样本四组链路回放均退出正常、终端平仓，均无成交；只验证输入与过滤路径，不能用作收益证据。完整原始数据保留，25 ms 完结桶只用于派生参考输入。

Claude 新报告的 RH Standard 200/200/300 ms 延迟与前一审计表一致，下一窗口另加 250 ms transport 假设。RH 新单 450 ms、取消 450 ms；Core 新单 250 ms、取消 550 ms。RH/Core Premium 费率保持分别建模，没有升级账户。Standard-to-Standard 奖励限制的来源与定义有争议，设计文档已改为待核实；Hedge Boost 可在激励场景自己的两腿账本内研究，不借用其他场景资本。

### 冻结与调度

- 决策：**11 月 14 日 23:20 至 11 月 15 日 09:20 BJT**；warm-up 23:19，drain 到 09:20:15。采集 23:10 至 09:22，完整地位于旧冻结决策窗口之后。
- RH ETH 是主域，Core ETH 为对照。两档资本 500/1000、A 本地报价、B 同报价加 Binance 方向过滤，共八个独立账本；RH 用 USDG、Core 用 USDC，不合并现金。
- 冻结 policy SHA-256：`db9189a71069bde9194cbf9b0be8cbc5ae0e9e362b5efcc8c7748c036bb848b5`。
- Oasis 新目录 `~/aleph-tx-research/binance-reference-ab-20261114T152000Z-v2`，等待进程 350587，capture-plan SHA-256 `abfe5edc81b408111ef9e14b507e894b103ec58afb485c5cf2e5cc66471d8deb`。
- 本机隔离目录 `/private/tmp/aleph-binance-ab-20261114T152000Z-v2`，封存等待进程 97702。封存后校验 SHA、下载公开原始文件、分片归档、准入检查，再运行八臂；本机休眠会延后后处理，不影响 Oasis 采集。研究脚本和二进制已复制冻结，后续工作区修改不会改变这一版本。
- v1 在窗口开始前因补上“原始 BBO 序号先审计再采样”而被 v2 取代；v1 的 policy 和追加记录全部保留，未采集验证数据、未生成结果。只停止了本轮两个尚在等待的 v1 进程。
- 新任务设置 14 GiB 磁盘余量与 2.5 GiB 采集文件预算；触发后只停止自己的任务，记为 incomplete，不能拼短段补足。为控制体量，新十小时 Binance 仅 ETH/NVDA/SNDK；BTC 同机 Binance 本轮仍只有短探测。
- 原 `core-rh-prospective-10h-20261114T044946Z` 采集 PID 337404 仍在运行，策略哈希仍为 `9dea62dc023728e154c5a2295ecc50439b7fe6497f926f27a8acac9ca3163f35`；未改其二进制、配置或原始数据。

### 验证

Mac：`make fmt-check`、`make check`、`make test`、`make build` 和 research release 构建通过；Rust **561 通过、6 忽略**，Python research **111 通过**，Go 全部 race checks 通过。新增回归覆盖时钟/进程混用、向上取整、原始序号回退、采样不能回填、保留减仓、A 组现金不变、未知币种配对不得执行、错过 warm-up 不重排窗口等。

Oasis 独立目录、单核低优先级离线 release 构建通过；Rust **563 通过、7 忽略**。第一次 Linux 测试有四项因为上传包遗漏 `deploy/oasis/fixtures` 的既有合成夹具失败，补齐后完整重跑通过；失败日志保留，没有修改实现或测试来回避失败。构建源码、验证二进制与日志保留，只清理本轮可再生 target 缓存。Go Linux 采集器还完成上述真实公开流探测。

所有测试与短探测均不证明真实执行延迟、限流可行性、奖励资格或策略盈利。新过滤目前只进入 maker replay；双腿套利与激励 turnover 的新 Binance 执行消融仍属于后续独立工作，不能把 quant 的结果转记过去。

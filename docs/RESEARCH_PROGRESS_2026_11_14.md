# Independent strategy research — November 14, 2026

The new public recordings are shared evidence for three independent programs:
quant profit, hedge arbitrage and incentive turnover. No new ten-hour profit
result is available at this checkpoint. The frozen Oasis validation job is
waiting for sealed inputs. Capture duration, a quoted spread and a funding
forecast are not realized revenue.

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

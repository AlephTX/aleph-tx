# Shared public market archive

Record public market events once. Quant, arbitrage and turnover use the same
sealed source archive. The compiler produces reusable market tapes. Each
strategy supplies its own configuration, execution state, balances and report.

## Hyperliquid collection

```sh
feeder/perp-capture -hyperliquid BTC,ETH -hl-trades \
  -hl-funding-interval 1m -duration 10h15m -gzip \
  -output /path/to/capture.ndjson.gz.partial
```

`-hyperliquid` records books and active asset contexts, including the observed
funding estimate, mark and oracle. `-hl-trades` opens a public trade subscription
per market. The collector keeps the exact price, size, side, venue time and
trade ID. It omits participant addresses and transaction hashes. A new
connection records `connected`, then `subscribed` when the exchange acknowledges
the requested feed. Transport failures record `disconnect` before retrying.
Replay deduplicates trades using market, block time and trade ID. Cached prints
from before the subscription acknowledgment cannot fill a simulated order.

`-hl-funding-interval` polls the public `fundingHistory` endpoint over a rolling
three-hour window. Each response keeps its request bounds, send time, receipt
time and first availability. Repeated rate observations are retained as received.
The funding research reader reconciles rates by market and payment slot and
preserves conflicts. Actual account cash flows still require settlement evidence.
The book compiler accepts the history observations without booking a payment.
[Hyperliquid subscriptions](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket/subscriptions) ·
[Hyperliquid funding history](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint/perpetuals#retrieve-historical-funding-rates)

## Seal and reuse

After a successful capture, validate its actual duration and compressed stream,
then rename the partial file and ingest it into a new archive directory.

```sh
python3 scripts/archive_public_capture.py ingest \
  /path/to/sealed.ndjson.gz /path/to/public-archive
python3 scripts/archive_public_capture.py compile \
  /path/to/public-archive target/release/cross_venue \
  /path/to/compiled/BTC-capture.json /path/to/compiled/BTC-freshest.ndjson.zst
```

Shards use `exchange/domain/source/UTC-date/UTC-hour.ndjson.gz`. Native market
IDs remain in the records. Lighter Core and Robinhood are separate domains.
Hyperliquid HIP-3 markets retain the deployer prefix. Connection events covering
several markets are stored under the shared domain. An hour boundary rotates
storage while retaining original timestamps and event order. Each shard has a
SHA-256 and event count. The capture manifest also binds the complete original
stream, so an export can reproduce the exact input, including equal timestamps
and connection events. Validate time coverage and gaps separately before
admitting a continuous ten-hour research window.

The quant and turnover runner accepts `--public-archive` and `--compiled-dir`.
Keep each `BTC-capture.json` or `ETH-capture.json` with its compiled tape.
The runner verifies the profile hash before using its market IDs and terms.
For turnover on native Hyperliquid, use `--turnover-venue hyperliquid` with
explicit `--turnover-fees-bps MAKER TAKER`. Both strategies then reference the
same `BTC-freshest` or `ETH-freshest` tape while retaining independent ledgers.
`replay_ten_hour_arbitrage.py --public-archive` verifies that tape's source
manifest. Use the same `--start-ms` in both ten-hour runners to compare the
same decision window. `replay_funding_campaign.py --public-archive` includes the recorded
funding history using its original availability times. Existing campaign inputs
remain supported.

## Compact rebuilt inputs

Use the pinned historical generator and source files to reproduce the
partition, compiled tape and derived input before compacting it. The proof
records the generator, compiler, input configuration and matching hashes. The
compaction command takes an explicit JSON list of paths under
`results/.../inputs`. Stop any replay that consumes those files before running it.

```sh
python3 scripts/compact_replay_inputs.py compact \
  /path/to/campaign /path/to/rebuild/status.json \
  /path/to/campaign/results/rebuilt-input-store /path/to/inputs.json
python3 scripts/compact_replay_inputs.py restore \
  /path/to/campaign /path/to/campaign/results/rebuilt-input-store \
  results/study/inputs/BTC-123456.ndjson
```

Objects are stored once by the decoded SHA-256. The command verifies each
compressed object and publishes the restore manifest before removing verified
plain copies. Restore recreates the legacy file at the requested path after
checking its hash. Keep the source archives, rebuild proofs, report directories
and replay versions for reproducibility.

## Oasis verification on November 14, 2026

An isolated 75-second Hyperliquid capture recorded 240 BTC trades, 168 ETH
trades, six subscription acknowledgments and four funding-history responses.
Exporting the five domain shards reproduced the original event stream hash.
A local WebSocket test covers disconnect and renewed acknowledgment.

The historical rebuild matched 44 single-venue maker inputs and 48 paired
inputs, including the upstream capture partitions and compiled tapes. Their
matching copies and path aliases now reference 92 compressed objects in
`results/rebuilt-input-store-v1` and `results/rebuilt-paired-input-store-v1`.
The stores replaced 232 plain files and saved approximately 12.6 GiB.
Each batch passed a sample restore and report hash comparison. Other
historical inputs retain their original storage until verified.

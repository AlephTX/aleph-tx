# Shared-memory market data and private events

AlephTX uses shared memory for two data paths. Go publishes market and
account observations for Rust strategies, while a separate ring carries
ordered private events for the Lighter order tracker. The supervisor
sets `ALEPH_SHM_DIR` to a session-owned directory under `/dev/shm`.
Standalone processes default to the legacy paths when the variable is
absent. [Supervised launch and cleanup](LIVE_TRADING.md#shutdown-and-restart).

## Data paths

| Path | Published data | Consumer contract |
| --- | --- | --- |
| Market and account observations | BBO, depth and account statistics | Check source and observation age before quoting or sizing. Keep the shared-memory symbol and exchange IDs separate from the venue market ID. |
| Lighter private events | A single-writer ring of creates, fills, cancels, rejects and private stream loss events | Apply events in sequence and reconcile authoritative state after an integrity alarm or a stream gap. |

## Depth and book continuity

The Lighter public adapter reconstructs full sorted depth in Go and publishes
top-5 levels into `aleph-depth` (`ShmDepthSnapshot`: 256 bytes, C layout with
64-byte alignment) alongside `aleph-matrix` (`ShmBboMessage`: 64 bytes).

- **Delta sequence continuity**: `lighterBook.apply` validates `nonce`,
  `begin_nonce` (`begin_nonce == previous nonce`), and monotonic
  `last_updated_at` when present on Lighter order-book messages. Duplicate or
  older nonces/offsets are ignored; any sequence gap, timestamp regression, or
  crossed book clears the reconstructed state and forces a WebSocket reconnect
  from a fresh snapshot.
- **Disconnect and one-sided invalidation**: Startup, WebSocket disconnects,
  and one-sided books immediately invalidate both `aleph-matrix` (`InvalidateBBO`)
  and `aleph-depth` (`InvalidateDepth`, zeroing `TimestampNs`, `Bids`, and
  `Asks` under the seqlock) so stale pre-disconnect depth weights are never
  readable during reconnection.

## V2 event ABI

The supervised strategy reads `aleph-events-v2`. The V2 ring has a
64-byte header and 1,024 slots of 128 bytes. Rust lays each record out
with C representation and 64-byte alignment. The Go writer uses the same
field offsets for the sequence, exchange and symbol IDs, order IDs,
fill and fee fields, and feeder timestamp. The Rust type assertions and Go layout test
are the source for exact offsets. Legacy V1 uses 64-byte events in its
own ring. [Rust types](../src/types/events.rs) ·
[Go writer](../feeder/shm/events.go) · [ABI tests](../feeder/shm/shm_test.go).

| Header bytes | Meaning |
| --- | --- |
| 0–7 | Atomic write index. This is the next sequence number after a committed slot. |
| 8–15 | Atomic generation. Odd while the producer writes a slot. Even after publication. |
| 16–63 | Padding to the first record. |

The writer marks the generation odd, copies the event, advances the
write index and publishes the even generation. The reader checks that
the generation is even before copying and unchanged afterwards. A
concurrent write causes a retry. The reader checks the slot sequence
against its expected position and raises an integrity alarm on overflow,
a corrupt slot or a producer index reset. [Rust reader](../src/shm_event_reader.rs).

## Recovery and verification

The strategy marks its order tracker for reconciliation after integrity
loss. Releasing pending exposure requires terminal order evidence and a
position read that covers the applicable fill watermark. Run `make test`
to exercise both languages, including layout and publication tests. For
live recovery behavior, use the [execution runbook](LIVE_TRADING.md#execution-and-recovery).

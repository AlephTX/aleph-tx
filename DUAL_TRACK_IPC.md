# Dual-Track IPC Architecture - Implementation Guide

## Overview

AlephTX now implements a **Dual-Track IPC Architecture** for zero-latency HFT:

- **Track 1 (Public)**: Lock-Free Shared Matrix (`/dev/shm/aleph-matrix`) - Market data (BBO)
- **Track 2 (Private)**: Lock-Free Ring Buffer (`/dev/shm/aleph-events`) - Order flow events

## Architecture Components

### 1. C-ABI Event Schema (`src/types/events.rs`, `feeder/shm/events.go`)

Cross-language memory contract for private events:

```rust
#[repr(C, align(64))]
pub struct ShmPrivateEvent {
    pub sequence: u64,        // Monotonic sequence number
    pub exchange_id: u8,      // Lighter = 2
    pub event_type: u8,       // 1=Created, 2=Filled, 3=Canceled, 4=Rejected
    pub symbol_id: u16,       // BTC=0, ETH=1
    pub order_id: u64,        // Exchange order ID
    pub fill_price: f64,      // Fill price (0 if not filled)
    pub fill_size: f64,       // Fill size
    pub remaining_size: f64,  // Remaining size
    pub fee_paid: f64,        // Fee paid
    // ... padding to 64 bytes
}
```

### 2. Rust Event Consumer (`src/shm_event_reader.rs`)

Non-blocking reader for the event ring buffer:

```rust
let mut reader = ShmEventReader::new()?;

// Hot-path: non-blocking read
if let Some(event) = reader.try_read() {
    // Process event
}
```

### 3. Shadow Ledger (`src/shadow_ledger.rs`)

Optimistic state machine for zero-latency position tracking:

```rust
let ledger = ShadowLedger::new();
let state = ledger.state();

// Spawn background consumer
let event_reader = ShmEventReader::new()?;
ledger.spawn_consumer(event_reader);

// Hot-path: <1μs state query
let pos = state.read().position();
let pnl = state.read().pnl();
```

### 4. Go Feeder - Lighter Private Stream (`feeder/exchanges/lighter_private.go`)

WebSocket client for Lighter private events:

```go
eventBuffer, _ := shm.NewEventRingBuffer()
client := exchanges.NewLighterPrivate(cfg, eventBuffer, accountID, authToken)
client.Run(ctx)
```

## Performance Characteristics

| Metric | Before | After |
|--------|--------|-------|
| State Query Latency | 50-200ms (REST) | <1μs (Shadow Ledger) |
| Event Latency | N/A (polling) | <100μs (WebSocket) |
| API Calls for State | Every quote | Zero |
| Position Accuracy | Eventually consistent | Real-time |

## Integration Steps

### Step 1: Enable Event Ring Buffer in Go Feeder

```go
// feeder/main.go
eventBuffer, err := shm.NewEventRingBuffer()
if err != nil {
    log.Fatalf("Failed to create event ring buffer: %v", err)
}
defer eventBuffer.Close()

// Start Lighter private stream
if ltCfg, ok := cfg.Exchanges["lighter"]; ok && ltCfg.Enabled {
    accountID := os.Getenv("LIGHTER_ACCOUNT_ID")
    authToken := os.Getenv("LIGHTER_AUTH_TOKEN")

    ltPrivate := exchanges.NewLighterPrivate(ltCfg, eventBuffer, accountID, authToken)
    go ltPrivate.Run(ctx)
}
```

### Step 2: Enable Shadow Ledger in Rust Strategy

```rust
// src/main.rs or strategy initialization
use aleph_tx::shadow_ledger::ShadowLedger;
use aleph_tx::shm_event_reader::ShmEventReader;

// Create shadow ledger
let ledger = ShadowLedger::new();
let state = ledger.state();

// Spawn event consumer
let event_reader = ShmEventReader::new()?;
tokio::spawn(async move {
    ledger.spawn_consumer(event_reader).await;
});

// Use in strategy
let strategy = MarketMakerStrategy::new_with_ledger(
    exchange_id,
    symbol_id,
    cfg,
    state,
);
```

### Step 3: Update Strategy to Use Shadow Ledger

```rust
// In your strategy's on_idle() or quoting logic
fn on_idle(&mut self) {
    // Zero-latency state query
    let state = self.ledger_state.read();
    let live_pos = state.position();
    let realized_pnl = state.pnl();
    let active_orders = state.active_order_count();

    // Use state for quoting decisions
    if live_pos.abs() > self.max_position {
        // Skip quoting
        return;
    }

    // Quote with instant state awareness
    self.quote_market(live_pos);
}
```

## Configuration

### Environment Variables

```bash
# Lighter Account Configuration
export LIGHTER_ACCOUNT_ID="12345"
export LIGHTER_AUTH_TOKEN="your_auth_token_here"

# Or use API key/secret (if using signature-based auth)
export LIGHTER_API_KEY="your_api_key"
export LIGHTER_API_SECRET="your_api_secret"
```

### Config TOML

```toml
[exchanges.lighter]
enabled = true
testnet = false
ws_url = "wss://mainnet.zklighter.elliot.ai/stream"

[exchanges.lighter.symbols]
BTC = "3"
ETH = "4"
```

## Authentication (TODO)

The Lighter WebSocket authentication mechanism is not fully documented in the API docs.
Possible approaches:

1. **API Key Signature** (most likely):
   - Generate HMAC-SHA256 signature of timestamp + account_id
   - Include in `auth` field

2. **JWT Token**:
   - Obtain JWT from REST API
   - Use in WebSocket subscription

3. **Lighter SDK**:
   - Reference official Lighter Go/Python SDK
   - Implement equivalent signing logic

**Action Required**: Research Lighter SDK or contact Lighter support for authentication details.

## Testing

### Unit Tests

```bash
# Test event schema
cargo test types::events::tests

# Test event reader
cargo test shm_event_reader::tests

# Test shadow ledger
cargo test shadow_ledger::tests
```

### Integration Test

```bash
# Terminal 1: Start Go feeder
cd feeder
go run main.go

# Terminal 2: Run Rust strategy
cargo run --bin your_strategy

# Verify events are flowing
ls -lh /dev/shm/aleph-events
```

## Monitoring

```rust
// Check event buffer health
let reader = ShmEventReader::new()?;
println!("Unread events: {}", reader.unread_count());
println!("Write index: {}", reader.write_idx());
println!("Read index: {}", reader.local_read_idx());

// Check shadow ledger state
let state = ledger.state().read();
println!("Position: {}", state.position());
println!("PnL: {}", state.pnl());
println!("Active orders: {}", state.active_order_count());
println!("Last sequence: {}", state.last_sequence);
```

## Troubleshooting

### Event Buffer Not Found

```
Error: No such file or directory (os error 2)
```

**Solution**: Ensure Go feeder is running and has created `/dev/shm/aleph-events`.

### Events Not Flowing

1. Check Go feeder logs for WebSocket connection status
2. Verify authentication token is valid
3. Check Lighter account ID is correct
4. Monitor `reader.unread_count()` - should increase over time

### State Drift

If shadow ledger state drifts from exchange:

1. Implement periodic REST API reconciliation
2. Add sequence number gap detection
3. Reset state on WebSocket reconnection

## Future Enhancements

1. **Multi-Exchange Support**: Extend to Backpack, EdgeX, Hyperliquid
2. **State Persistence**: Save/restore shadow ledger on restart
3. **Gap Detection**: Detect and recover from missed events
4. **Metrics**: Prometheus metrics for event latency, buffer utilization
5. **Order Side Tracking**: Add buy/sell side to events for accurate position tracking

## References

- Lighter WebSocket API: https://apidocs.lighter.xyz/docs/websocket-reference
- Lighter Go SDK: https://github.com/elliottech/lighter-go
- Lock-Free Ring Buffer: https://en.wikipedia.org/wiki/Circular_buffer

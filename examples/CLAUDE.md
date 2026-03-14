---
description: Example programs - debug utilities, signature tests, and benchmarks
alwaysApply: true
---

# examples/

> Debugging, testing, and benchmark utilities. Production strategy binaries live in `src/bin/`.

## Key Files

| File | Description |
|------|-------------|
| test_account_stats.rs | Simple account stats SHM reader demo |

### EdgeX Debugging & Signature Verification

| File | Description |
|------|-------------|
| debug_edgex_signature.rs | Debug L2 signature generation step-by-step |
| test_edgex_auth.rs | Test EdgeX API authentication flow |
| test_edgex_order.rs | End-to-end EdgeX order placement test |
| test_edgex_pedersen.rs | Verify EdgeX-compatible Pedersen hash output |
| check_edgex_key.rs | Validate EdgeX StarkNet key format |
| test_l2_signature.rs | L2 signature correctness test |
| test_signature_format.rs | Signature encoding format validation |
| test_order_simple.rs | Minimal order placement test |
| diagnostic_l2_hash.rs | Diagnostic output for L2 hash computation |
| verify_packing.rs | Verify field packing for L2 signature |
| test_packing.rs | Test shift-and-add field packing |

### Benchmarks

| File | Description |
|------|-------------|
| bench_pedersen.rs | Pedersen hash performance benchmark |
| bench_signature.rs | Full L2 signature pipeline benchmark |
| test_pedersen.rs | Pedersen hash correctness test |

## Usage

```bash
# Run a test/debug example
cargo run --release --example test_edgex_order

# Run a benchmark
cargo run --release --example bench_pedersen
```

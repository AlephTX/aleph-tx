---
description: Pre-built FFI shared library for Lighter DEX Poseidon2 + EdDSA signing
alwaysApply: true
---

# lib/

> Pre-built shared libraries for FFI integration.

## Key Files

| File | Description |
|------|-------------|
| lighter-signer-linux-amd64.so | Go shared library for Lighter DEX signing (Poseidon2 + EdDSA/Schnorr) |

## Usage

- Loaded by Rust via `lighter_ffi.rs` at runtime.
- `LD_LIBRARY_PATH` must include this directory (Makefile sets this automatically).
- Built from `lighter-go/sharedlib` - do NOT rebuild unless the signing protocol changes.

## Exported Functions

| Function | Description |
|----------|-------------|
| `CreateClient()` | Initialize signer with private key |
| `SignCreateOrder()` | Sign order creation (returns multipart form data) |
| `SignCancelOrder()` | Sign order cancellation |
| `CreateAuthToken()` | Generate WS authentication token |
| `free()` | Free C strings allocated by Go (CRITICAL for memory safety) |

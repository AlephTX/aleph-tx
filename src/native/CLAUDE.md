---
description: Native FFI shared libraries - Lighter DEX Poseidon2 and Schnorr signer
alwaysApply: true
---

# Native Libraries

This directory contains native shared libraries loaded by Rust via FFI.

## Lighter DEX Signing Library

- `lighter-signer-linux-amd64.so` — official Linux x86-64 asset from [lighter-go v1.0.13](https://github.com/elliottech/lighter-go/releases/tag/v1.0.13)
- `liblighter-signer-linux-amd64.so` — symlink to `lighter-signer-linux-amd64.so`

Published SHA-256 digests:
- Shared library: `e4f0355e499c8608db4eae26824b81105b0d6a9fb91374661e61c1b4c70cc72c`
- Linux x86-64 C header: `fcbe90e05378e51f6cb2370db6836badc3e65f7dd87b5363ce8cf131a9fbc743`

Runtime loader setup on Linux x86-64:

```bash
export LD_LIBRARY_PATH=$(pwd)/src/native:$LD_LIBRARY_PATH
```

## Venue Cryptography Summary

- **Lighter (Core & Robinhood)**: Uses `lighter-signer-linux-amd64.so` on Linux x86-64 for Poseidon2 + Schnorr signing (`CreateClient` scoped per `api_key_index` lane). macOS offline builds stub live signing fail-closed so tests and replay run portably.
- **Hyperliquid**: Pure Rust `k256` + `sha3` EIP-712 signing (no native library needed).

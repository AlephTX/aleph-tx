---
description: Native FFI shared libraries - Lighter DEX Poseidon2 and Schnorr signer
alwaysApply: true
---

# Native Libraries

This directory contains native shared libraries loaded by Rust via FFI.

## Lighter DEX Signing Library

- `lighter-signer-linux-amd64.so` — official Linux x86-64 asset from [lighter-go v1.0.12](https://github.com/elliottech/lighter-go/releases/tag/v1.0.12)
- `liblighter-signer-linux-amd64.so` — symlink to `lighter-signer-linux-amd64.so`

Published SHA-256 digests:
- Shared library: `7f107a0e2240aa40c9f0ae8aba396380fe62892fe199dd3691e3103fc9a6fd8e`
- Linux x86-64 C header: `e060d6a15e9e5ca79b5a03c22c192f26a2182346d359b6f1d2d24a1046847302`

Runtime loader setup on Linux x86-64:

```bash
export LD_LIBRARY_PATH=$(pwd)/src/native:$LD_LIBRARY_PATH
```

## Venue Cryptography Summary

- **Lighter (Core & Robinhood)**: Uses `lighter-signer-linux-amd64.so` on Linux x86-64 for Poseidon2 + Schnorr signing (`CreateClient` scoped per `api_key_index` lane). macOS offline builds stub live signing fail-closed so tests and replay run portably.
- **Hyperliquid**: Pure Rust `k256` + `sha3` EIP-712 signing (no native library needed).

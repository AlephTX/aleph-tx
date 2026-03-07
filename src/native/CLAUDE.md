---
description: Native FFI shared libraries - Lighter DEX Ed25519 signer
alwaysApply: true
---

# Native Libraries

This directory contains native (C/C++) libraries used by Rust via FFI.

## Lighter DEX Signing Library

- `lighter-signer-linux-amd64.so` - Ed25519 signing library for Lighter DEX
- `liblighter-signer-linux-amd64.so` - Symlink to the above

This library is loaded at runtime and requires `LD_LIBRARY_PATH` to be set:

```bash
export LD_LIBRARY_PATH=$(pwd)/src/native:$LD_LIBRARY_PATH
```

The Makefile automatically handles this for Lighter strategies.

## Exchange-Specific Notes

- **Lighter**: Requires this .so library for Ed25519 signing
- **EdgeX**: Uses pure Rust `starknet-crypto` crate, no native library needed
- **Backpack**: Uses pure Rust crypto, no native library needed
- **Hyperliquid**: Uses pure Rust crypto, no native library needed

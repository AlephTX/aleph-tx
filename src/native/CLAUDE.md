---
description: Native FFI shared libraries - Lighter DEX Poseidon2 and Schnorr signer
alwaysApply: true
---

# Native Libraries

This directory contains native (C/C++) libraries used by Rust via FFI.

## Lighter DEX Signing Library

- `lighter-signer-linux-amd64.so` - official Linux x86-64 asset from [lighter-go v1.0.12](https://github.com/elliottech/lighter-go/releases/tag/v1.0.12)
- `liblighter-signer-linux-amd64.so` - Symlink to the above

The released binary has SHA-256
`7f107a0e2240aa40c9f0ae8aba396380fe62892fe199dd3691e3103fc9a6fd8e`.
The Rust declarations match the release's Linux x86-64 C header, whose SHA-256
is `e060d6a15e9e5ca79b5a03c22c192f26a2182346d359b6f1d2d24a1046847302`.
The asset reports build revision `eb70ce6a31b2ac76182b499b8f16490dff8e1978`
and a modified build tree. Verify the published asset digest when updating it.

This library is loaded at runtime and requires `LD_LIBRARY_PATH` to be set:

```bash
export LD_LIBRARY_PATH=$(pwd)/src/native:$LD_LIBRARY_PATH
```

The Makefile automatically handles this for Lighter strategies.

## Exchange-Specific Notes

- **Lighter**: Requires this library for Poseidon2 and Schnorr signing
- **EdgeX**: Uses pure Rust `starknet-crypto` crate, no native library needed
- **Backpack**: Uses pure Rust crypto, no native library needed
- **Hyperliquid**: Uses pure Rust crypto, no native library needed

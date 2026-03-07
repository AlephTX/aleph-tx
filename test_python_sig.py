#!/usr/bin/env python3
import hashlib
from starknet_py.utils.crypto.facade import sign

# Same test data
private_key = 0xREDACTED_PRIVATE_KEY
message = "1772894985474GET/api/v1/private/account/getAccountAssetaccountId=0"

# Keccak256 hash
hash_bytes = hashlib.sha3_256(message.encode()).digest()
print(f"Message: {message}")
print(f"Keccak256 Hash (hex): {hash_bytes.hex()}")

# Convert to integer
msg_hash = int.from_bytes(hash_bytes, 'big')
print(f"Hash as int: {hex(msg_hash)}")

# Sign
sig = sign(msg_hash, private_key)
print(f"\nSignature r: {hex(sig.r)}")
print(f"Signature s: {hex(sig.s)}")
print(f"\nCombined (r+s): {sig.r:064x}{sig.s:064x}")

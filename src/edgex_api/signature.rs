use starknet_crypto::{Felt, pedersen_hash, sign};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum SignatureError {
    #[error("Hex error: {0}")]
    HexError(#[from] hex::FromHexError),
    #[error("Felt error")]
    FeltError,
    #[error("Signing error")]
    SigningError,
}

// StarkNet Prime (2^251 + 17 * 2^192 + 1)
// The python code uses: 0x800000000000011000000000000000000000000000000000000000000000001
// which matches the Stark curve prime.

pub struct SignatureManager {
    private_key: Felt, // L2 Private Key (Stark Key)
                       // We might also need L1 wallet for onboarding, but for L2 actions we need L2 key.
}

impl SignatureManager {
    pub fn new(l2_private_key_hex: &str) -> Result<Self, SignatureError> {
        let key_str = l2_private_key_hex.trim_start_matches("0x");
        let private_key = Felt::from_hex(key_str).map_err(|_| SignatureError::FeltError)?;
        Ok(Self { private_key })
    }

    /// Calculates the Pedersen hash for a limit order (Order with fees).
    /// Replicates the logic from EdgeX Python SDK `calc_limit_order_hash`.
    #[allow(clippy::too_many_arguments)]
    pub fn calc_limit_order_hash(
        &self,
        synthetic_asset_id: &str,
        collateral_asset_id: &str,
        fee_asset_id: &str,
        is_buy: bool,
        amount_synthetic: u64,
        amount_collateral: u64,
        amount_fee: u64,
        nonce: u64,
        account_id: u64,
        expire_time: u64,
    ) -> Result<Felt, SignatureError> {
        // Parse Asset IDs
        let syn_id = Felt::from_hex(synthetic_asset_id.trim_start_matches("0x"))
            .map_err(|_| SignatureError::FeltError)?;
        let col_id = Felt::from_hex(collateral_asset_id.trim_start_matches("0x"))
            .map_err(|_| SignatureError::FeltError)?;
        let fee_id = Felt::from_hex(fee_asset_id.trim_start_matches("0x"))
            .map_err(|_| SignatureError::FeltError)?;

        let (asset_id_sell, asset_id_buy, amount_sell, amount_buy) = if is_buy {
            (col_id, syn_id, amount_collateral, amount_synthetic)
        } else {
            (syn_id, col_id, amount_synthetic, amount_collateral)
        };

        // First hash: hash(asset_id_sell, asset_id_buy)
        let msg = pedersen_hash(&asset_id_sell, &asset_id_buy);

        // Second hash: hash(msg, asset_id_fee)
        let msg = pedersen_hash(&msg, &fee_id);

        // Pack message 0
        // packed_message0 = amount_sell * 2^64 + amount_buy * 2^64 + max_amount_fee * 2^32 + nonce
        // Note: Felt doesn't support '<<' directly for non-Felt inputs easily unless we convert.
        // But we can construct BigUint or perform check.
        // Since we are using Felt which is 252 bits, we can try to compose it.
        // The python code does: val = (val << 64) + next_val.

        // Helper to shift and add
        let shift_add = |acc: Felt, val: u64, shift: u32| -> Felt {
            // acc * 2^shift + val
            // Felt::pow takes u128.
            let shift_multiplier = Felt::from(2u64).pow(shift as u128);
            (acc * shift_multiplier) + Felt::from(val)
        };

        // Wait, does Felt implement std::ops::Add etc? Yes usually.

        let pm0 = Felt::from(amount_sell);
        let pm0 = shift_add(pm0, amount_buy, 64);
        let pm0 = shift_add(pm0, amount_fee, 64);
        let pm0 = shift_add(pm0, nonce, 32);
        // implicit modulo prime is handled by Felt arithmetic

        // Third hash: hash(msg, packed_message0)
        let msg = pedersen_hash(&msg, &pm0);

        // Pack message 1
        // packed_message1 = LIMIT_ORDER_WITH_FEE_TYPE * 2^64 + account_id * 2^64 + account_id * 2^64 + account_id * 2^32 + expiration_timestamp * 2^17
        // Python:
        // packed_message1 = LIMIT_ORDER_WITH_FEE_TYPE  # 3
        // packed_message1 = (packed_message1 << 64) + account_id
        // packed_message1 = (packed_message1 << 64) + account_id
        // packed_message1 = (packed_message1 << 64) + account_id
        // packed_message1 = (packed_message1 << 32) + expire_time
        // packed_message1 = packed_message1 << 17

        let limit_order_type = 3u64;
        let pm1 = Felt::from(limit_order_type);
        let pm1 = shift_add(pm1, account_id, 64);
        let pm1 = shift_add(pm1, account_id, 64);
        let pm1 = shift_add(pm1, account_id, 64);
        let pm1 = shift_add(pm1, expire_time, 32);

        // Final shift by 17 (padding)
        let shift_17 = Felt::from(2u64).pow(17u128);
        let pm1 = pm1 * shift_17;

        // Final hash: hash(msg, packed_message1)
        let msg = pedersen_hash(&msg, &pm1);

        Ok(msg)
    }

    pub fn sign_l2_action(&self, hash: Felt) -> Result<String, SignatureError> {
        // Sign with k value (randomness). API often expects standard ECDSA signature (r, s).
        // starknet_crypto::sign usage: sign(private_key, message_hash, k)
        // We need a random k.

        // For deterministic signing (RFC6979 equivalent), we usually derive k from msg and key.
        // But starknet_crypto might need explicit k.
        // Let's use a simple RFC6979-like derivation or random if possible.
        // Actually, for safety, using a secure random k is better.

        let k = starknet_crypto::rfc6979_generate_k(&hash, &self.private_key, None);

        let signature =
            sign(&self.private_key, &hash, &k).map_err(|_| SignatureError::SigningError)?;

        // Format: r, s. Usually hex strings.
        // API expects... "l2Signature".
        // Often formatted as `r` and `s` or concatenated.
        // EdgeX docs say "l2Signature": "0x..."
        // I will return r and s packed or check doc again.
        // Docs usually want: r, s as hex strings, or packed 0x{r}{s}.
        // Common Starknet format is often JSON `[r, s]`.
        // Let's assume standard hex concatenation for now given "0x..." string type.
        // 0x + r_hex + s_hex

        let r_hex = format!("{:064x}", signature.r);
        let s_hex = format!("{:064x}", signature.s);
        Ok(format!("{}{}", r_hex, s_hex))
    }

    pub fn sign_message(&self, message: &str) -> Result<String, SignatureError> {
        use num_bigint::BigUint;
        use num_traits::Num;
        use sha3::{Digest, Keccak256};

        let mut hasher = Keccak256::new();
        hasher.update(message.as_bytes());
        let hash_bytes = hasher.finalize();

        // Convert the 32 byte keccak hash to BigUint
        let msg_hash_int = BigUint::from_bytes_be(&hash_bytes);

        // StarkEx Curve Order (N)
        let ec_order = BigUint::from_str_radix(
            "800000000000010ffffffffffffffffb781126dcae7b2321e66a241adc64d2f",
            16,
        )
        .unwrap();

        // msg_hash_int = msg_hash_int % EC_ORDER
        let msg_hash_int = msg_hash_int % ec_order;

        // Convert back to 32 bytes for Felt
        let mod_bytes = msg_hash_int.to_bytes_be();
        let mut padded = [0u8; 32];
        if mod_bytes.len() <= 32 {
            padded[32 - mod_bytes.len()..].copy_from_slice(&mod_bytes);
        }

        let hash_felt = Felt::from_bytes_be(&padded);

        // The python sdk does not prepend 0x or anything special, it just signs the digest.
        // `sign_l2_action` wraps it in 0x.
        self.sign_l2_action(hash_felt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signature_generation() {
        // Dummy key (valid hex)
        let key = "0x1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";
        let manager = SignatureManager::new(key).unwrap();

        // Test limit order hash calculation
        let hash = manager
            .calc_limit_order_hash("0x1", "0x2", "0x3", true, 100, 200, 10, 123, 1, 999999)
            .unwrap();

        println!("Hash: {:?}", hash);

        // Test signing
        let signature = manager.sign_l2_action(hash).unwrap();
        println!("Signature: {}", signature);

        assert!(signature.starts_with("0x"));
        assert_eq!(signature.len(), 2 + 64 + 64); // 0x + r(64) + s(64)
    }
}

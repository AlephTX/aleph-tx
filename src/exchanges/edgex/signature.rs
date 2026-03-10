use starknet_crypto::{Felt, sign};
use thiserror::Error;
use num_bigint::BigUint;

use super::pedersen::PedersenHash;

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
    public_key: Felt,  // L2 Public Key (derived from private key)
    pedersen: PedersenHash, // EdgeX-compatible Pedersen hash
}

impl SignatureManager {
    pub fn new(l2_private_key_hex: &str) -> Result<Self, SignatureError> {
        let key_str = l2_private_key_hex.trim_start_matches("0x");
        let private_key = Felt::from_hex(key_str).map_err(|_| SignatureError::FeltError)?;

        // Derive public key from private key
        let public_key = starknet_crypto::get_public_key(&private_key);

        // Initialize EdgeX-compatible Pedersen hash
        let pedersen = PedersenHash::new();

        Ok(Self { private_key, public_key, pedersen })
    }

    // Helper function to convert Felt to BigUint
    fn felt_to_biguint(felt: &Felt) -> BigUint {
        let bytes = felt.to_bytes_be();
        BigUint::from_bytes_be(&bytes)
    }

    // Helper function to convert BigUint to Felt
    fn biguint_to_felt(biguint: &BigUint) -> Result<Felt, SignatureError> {
        let bytes = biguint.to_bytes_be();
        let mut padded = [0u8; 32];
        if bytes.len() <= 32 {
            padded[32 - bytes.len()..].copy_from_slice(&bytes);
            Ok(Felt::from_bytes_be(&padded))
        } else {
            Err(SignatureError::FeltError)
        }
    }

    // EdgeX-compatible Pedersen hash
    fn pedersen_hash(&self, a: &Felt, b: &Felt) -> Result<Felt, SignatureError> {
        let a_big = Self::felt_to_biguint(a);
        let b_big = Self::felt_to_biguint(b);
        let hash_big = self.pedersen.hash(&[a_big, b_big]);
        Self::biguint_to_felt(&hash_big)
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

        tracing::debug!("asset_id_sell: 0x{:064x}", asset_id_sell);
        tracing::debug!("asset_id_buy: 0x{:064x}", asset_id_buy);
        tracing::debug!("amount_sell: {}", amount_sell);
        tracing::debug!("amount_buy: {}", amount_buy);
        tracing::debug!("amount_fee: {}", amount_fee);
        tracing::debug!("nonce: {}", nonce);

        // First hash: hash(asset_id_sell, asset_id_buy)
        let msg = self.pedersen_hash(&asset_id_sell, &asset_id_buy)?;
        tracing::debug!("hash1 (assets): 0x{:064x}", msg);

        // Second hash: hash(msg, asset_id_fee)
        let msg = self.pedersen_hash(&msg, &fee_id)?;
        tracing::debug!("hash2 (+ fee_asset): 0x{:064x}", msg);

        // Helper to shift and add
        let shift_add = |acc: Felt, val: u64, shift: u32| -> Felt {
            let shift_multiplier = Felt::from(2u64).pow(shift as u128);
            (acc * shift_multiplier) + Felt::from(val)
        };

        let pm0 = Felt::from(amount_sell);
        let pm0 = shift_add(pm0, amount_buy, 64);
        let pm0 = shift_add(pm0, amount_fee, 64);
        let pm0 = shift_add(pm0, nonce, 32);
        tracing::debug!("packed_message0: 0x{:064x}", pm0);

        // Third hash: hash(msg, packed_message0)
        let msg = self.pedersen_hash(&msg, &pm0)?;
        tracing::debug!("hash3 (+ pm0): 0x{:064x}", msg);

        let limit_order_type = 3u64;
        let pm1 = Felt::from(limit_order_type);
        let pm1 = shift_add(pm1, account_id, 64);
        let pm1 = shift_add(pm1, account_id, 64);
        let pm1 = shift_add(pm1, account_id, 64);
        let pm1 = shift_add(pm1, expire_time, 32);

        // Final shift by 17 (padding)
        let shift_17 = Felt::from(2u64).pow(17u128);
        let pm1 = pm1 * shift_17;
        tracing::debug!("packed_message1: 0x{:064x}", pm1);

        // Final hash: hash(msg, packed_message1)
        let msg = self.pedersen_hash(&msg, &pm1)?;
        tracing::debug!("final hash: 0x{:064x}", msg);

        Ok(msg)
    }

    pub fn sign_l2_action(&self, hash: Felt) -> Result<String, SignatureError> {
        tracing::debug!("L2 hash to sign: 0x{:064x}", hash);

        // Convert Felt to bytes
        let hash_bytes = hash.to_bytes_be();

        // Convert to BigUint and reduce modulo EC_ORDER (not field prime!)
        use num_bigint::BigUint;
        use num_traits::Num;

        let hash_int = BigUint::from_bytes_be(&hash_bytes);

        // EC_ORDER (curve order, not field prime)
        let ec_order = BigUint::from_str_radix(
            "800000000000010ffffffffffffffffb781126dcae7b2321e66a241adc64d2f",
            16,
        ).unwrap();

        // Reduce modulo EC_ORDER
        let hash_int = hash_int % &ec_order;

        // Convert back to Felt
        let mod_bytes = hash_int.to_bytes_be();
        let mut padded = [0u8; 32];
        if mod_bytes.len() <= 32 {
            padded[32 - mod_bytes.len()..].copy_from_slice(&mod_bytes);
        }
        let hash_reduced = Felt::from_bytes_be(&padded);

        tracing::debug!("L2 hash reduced mod EC_ORDER: 0x{:064x}", hash_reduced);

        // Use RFC6979 for deterministic k generation
        let k = starknet_crypto::rfc6979_generate_k(&hash_reduced, &self.private_key, None);

        let signature =
            sign(&self.private_key, &hash_reduced, &k).map_err(|_| SignatureError::SigningError)?;

        let r_hex = format!("{:064x}", signature.r);
        let s_hex = format!("{:064x}", signature.s);

        tracing::debug!("L2 signature r: {}", r_hex);
        tracing::debug!("L2 signature s: {}", s_hex);

        // Verify the signature locally
        let is_valid = starknet_crypto::verify(&self.public_key, &hash_reduced, &signature.r, &signature.s)
            .map_err(|_| SignatureError::SigningError)?;

        if !is_valid {
            tracing::error!("Local signature verification failed!");
            return Err(SignatureError::SigningError);
        }
        tracing::debug!("Local signature verification: OK");

        Ok(format!("{}{}", r_hex, s_hex))
    }

    pub fn sign_message(&self, message: &str) -> Result<String, SignatureError> {
        use sha3::{Digest, Keccak256};
        use num_bigint::BigUint;
        use num_traits::Num;

        // Keccak256 hash the message
        let mut hasher = Keccak256::new();
        hasher.update(message.as_bytes());
        let hash_bytes = hasher.finalize();

        // Convert the 32 byte keccak hash to BigUint
        let msg_hash_int = BigUint::from_bytes_be(&hash_bytes);

        // StarkEx Curve Order (N) - NOT the field prime!
        // This is the order of the elliptic curve group
        let ec_order = BigUint::from_str_radix(
            "800000000000010ffffffffffffffffb781126dcae7b2321e66a241adc64d2f",
            16,
        )
        .unwrap();

        // Reduce hash modulo EC_ORDER (not field prime)
        let msg_hash_int = msg_hash_int % ec_order;

        // Convert back to 32 bytes for Felt
        let mod_bytes = msg_hash_int.to_bytes_be();
        let mut padded = [0u8; 32];
        if mod_bytes.len() <= 32 {
            padded[32 - mod_bytes.len()..].copy_from_slice(&mod_bytes);
        }

        let hash_felt = Felt::from_bytes_be(&padded);

        // Sign the hash
        let signature = sign(&self.private_key, &hash_felt, &Felt::from_hex("0x1").unwrap())
            .map_err(|_| SignatureError::SigningError)?;

        // Format: r + s (each 64 hex chars)
        let r_hex = format!("{:064x}", signature.r);
        let s_hex = format!("{:064x}", signature.s);

        Ok(format!("{}{}", r_hex, s_hex))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signature_generation() {
        // Valid Stark private key (must be < STARK_PRIME)
        // Using a smaller valid key for testing
        let key = "0x1234567890abcdef";
        let manager = SignatureManager::new(key).unwrap();

        // Test limit order hash calculation with valid hex asset IDs
        let hash = manager
            .calc_limit_order_hash("0x1", "0x2", "0x3", true, 100, 200, 10, 123, 1, 999999)
            .unwrap();

        println!("Hash: {:?}", hash);

        // Test signing - output is r(64) + s(64) without 0x prefix
        let signature = manager.sign_l2_action(hash).unwrap();
        println!("Signature: {}", signature);

        assert_eq!(signature.len(), 64 + 64); // r(64) + s(64)
        assert!(signature.chars().all(|c| c.is_ascii_hexdigit()));
    }
}

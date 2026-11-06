//! L1 action signing, checked against Hyperliquid's official Python SDK vectors.
//! MessagePack map order is part of the hash. Sign typed wire structs, never a
//! JSON map whose key order can differ between serializers.

use super::model::{Network, validate_hex};
use anyhow::{Context, Result};
use k256::ecdsa::SigningKey;
use k256::elliptic_curve::zeroize::Zeroizing;
use serde::Serialize;
use sha3::{Digest, Keccak256};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct L1Signature {
    pub r: String,
    pub s: String,
    pub v: u8,
}

pub struct HyperliquidSigner {
    key: SigningKey,
}

impl HyperliquidSigner {
    pub fn from_private_key(private_key: &str) -> Result<Self> {
        // Key bytes never enter log/error output. k256 zeroizes key material.
        let bytes = Zeroizing::new(
            hex::decode(private_key.strip_prefix("0x").unwrap_or(private_key))
                .context("Invalid signing-key encoding")?,
        );
        let key = SigningKey::from_slice(&bytes).context("Invalid secp256k1 signing key")?;
        Ok(Self { key })
    }

    pub fn sign_action<T: Serialize>(
        &self,
        action: &T,
        network: Network,
        nonce: u64,
        vault_address: Option<&str>,
        expires_after: Option<u64>,
    ) -> Result<L1Signature> {
        let hash = action_hash(action, vault_address, nonce, expires_after)?;
        let digest = l1_digest(hash, network);
        let (signature, recovery) = self
            .key
            .sign_prehash_recoverable(&digest)
            .map_err(|_| anyhow::anyhow!("L1 signing failed"))?;
        let bytes = signature.to_bytes();
        Ok(L1Signature {
            r: format!("0x{}", hex::encode(&bytes[..32]).trim_start_matches('0')),
            s: format!("0x{}", hex::encode(&bytes[32..]).trim_start_matches('0')),
            v: 27 + recovery.to_byte(),
        })
    }
}

pub fn action_hash<T: Serialize>(
    action: &T,
    vault_address: Option<&str>,
    nonce: u64,
    expires_after: Option<u64>,
) -> Result<[u8; 32]> {
    let mut payload = rmp_serde::to_vec_named(action).context("Cannot encode action")?;
    payload.extend_from_slice(&nonce.to_be_bytes());
    if let Some(address) = vault_address {
        validate_hex(address, 20)?;
        payload.push(1);
        payload.extend_from_slice(&hex::decode(&address[2..])?);
    } else {
        payload.push(0);
    }
    if let Some(expiry) = expires_after {
        payload.push(0);
        payload.extend_from_slice(&expiry.to_be_bytes());
    }
    Ok(keccak(&payload))
}

fn keccak(input: &[u8]) -> [u8; 32] {
    Keccak256::digest(input).into()
}

fn l1_digest(connection_id: [u8; 32], network: Network) -> [u8; 32] {
    let mut domain = Vec::with_capacity(160);
    domain.extend_from_slice(&keccak(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    ));
    domain.extend_from_slice(&keccak(b"Exchange"));
    domain.extend_from_slice(&keccak(b"1"));
    let mut chain_id = [0_u8; 32];
    chain_id[24..].copy_from_slice(&1337_u64.to_be_bytes());
    domain.extend_from_slice(&chain_id);
    domain.extend_from_slice(&[0; 32]);
    let domain_hash = keccak(&domain);

    let mut agent = [0_u8; 96];
    agent[..32].copy_from_slice(&keccak(b"Agent(string source,bytes32 connectionId)"));
    agent[32..64].copy_from_slice(&keccak(match network {
        Network::Mainnet => b"a",
        Network::Testnet => b"b",
    }));
    agent[64..].copy_from_slice(&connection_id);
    let agent_hash = keccak(&agent);
    let mut typed = [0_u8; 66];
    typed[..2].copy_from_slice(&[0x19, 0x01]);
    typed[2..34].copy_from_slice(&domain_hash);
    typed[34..].copy_from_slice(&agent_hash);
    keccak(&typed)
}

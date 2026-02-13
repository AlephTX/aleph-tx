//! EVM Signer for DEX (EdgeX, GMX, etc.)

use k256::ecdsa::{SigningKey, VerifyingKey, signature::{Signer, Verifier}};
use k256::SecretKey;
use std::sync::Arc;
use parking_lot::RwLock;

use crate::adapter::Signer;
use crate::adapter::SignerType;

/// EVM ECDSA Signer (k256)
pub struct EvmSigner {
    key: Arc<RwLock<Option<SigningKey>>>,
    address: String,
}

impl EvmSigner {
    /// Create from hex private key
    pub fn from_hex(private_key_hex: &str) -> Result<Self, hex::FromHexError> {
        let bytes = hex::decode(private_key_hex)?;
        let secret = SecretKey::from_bytes(&bytes)?;
        let signing_key = SigningKey::from(secret);
        let verifying_key = VerifyingKey::from(&signing_key);
        let address = format!("0x{:x}", verifying_key.address());
        
        Ok(Self {
            key: Arc::new(RwLock::new(Some(signing_key))),
            address,
        })
    }

    /// Get EVM address
    pub fn address(&self) -> &str {
        &self.address
    }
}

impl Signer for EvmSigner {
    fn sign(&self, payload: &[u8]) -> Vec<u8> {
        let key = self.key.read();
        if let Some(signing_key) = key.as_ref() {
            let signature: k256::ecdsa::Signature = signing_key.sign(payload);
            signature.to_bytes().to_vec()
        } else {
            vec![]
        }
    }

    fn address(&self) -> &str {
        &self.address
    }

    fn signer_type(&self) -> SignerType {
        SignerType::Evm
    }
}

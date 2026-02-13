//! HMAC Signer for CEX (Binance, OKX, etc.)

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::adapter::Signer;
use crate::adapter::SignerType;

/// HMAC-SHA256 Signer
pub struct HmacSigner {
    api_key: String,
    api_secret: String,
}

impl HmacSigner {
    pub fn new(api_key: impl Into<String>, api_secret: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            api_secret: api_secret.into(),
        }
    }

    /// Sign query string for Binance/OKX
    pub fn sign(&self, message: &str) -> String {
        type HmacSha256 = Hmac<Sha256>;
        
        let mut mac = HmacSha256::new_from_slice(
            self.api_secret.as_bytes()
        ).expect("HMAC can take key of any size");
        
        mac.update(message.as_bytes());
        
        hex::encode(mac.finalize().into_bytes())
    }

    pub fn key_id(&self) -> &str {
        &self.api_key
    }
}

impl Signer for HmacSigner {
    fn sign(&self, payload: &[u8]) -> Vec<u8> {
        type HmacSha256 = Hmac<Sha256>;
        
        let mut mac = HmacSha256::new_from_slice(
            self.api_secret.as_bytes()
        ).expect("HMAC can take key of any size");
        
        mac.update(payload);
        
        mac.finalize().into_bytes().to_vec()
    }

    fn address(&self) -> &str {
        &self.api_key
    }

    fn signer_type(&self) -> SignerType {
        SignerType::Hmac
    }
}

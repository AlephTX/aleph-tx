pub trait Signer: Send + Sync {
    fn sign(&self, payload: &[u8]) -> Vec<u8>;
    fn address(&self) -> String;
    fn signer_type(&self) -> SignerType;
}

#[derive(Debug, Clone, Copy)]
pub enum SignerType { Hmac, Evm, StarkNet, EdDSA }

pub struct HmacSigner {
    api_key: String,
    api_secret: String,
}

impl HmacSigner {
    pub fn new(api_key: impl Into<String>, api_secret: impl Into<String>) -> Self {
        Self { api_key: api_key.into(), api_secret: api_secret.into() }
    }
}

impl Signer for HmacSigner {
    fn sign(&self, payload: &[u8]) -> Vec<u8> {
        use hmac::Mac;
        let mut m = hmac::Hmac::<sha2::Sha256>::new_from_slice(self.api_secret.as_bytes()).unwrap();
        m.update(payload);
        m.finalize().into_bytes().to_vec()
    }
    fn address(&self) -> String { self.api_key.clone() }
    fn signer_type(&self) -> SignerType { SignerType::Hmac }
}

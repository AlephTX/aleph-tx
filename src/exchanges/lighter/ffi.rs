//! Lighter Go Signer FFI Bindings
//!
//! This module provides Rust bindings to the lighter-go CGO signer library.
//! The library handles Poseidon2 + Schnorr signing for Lighter DEX transactions.
//!
//! Architecture: Rust → FFI → Go CGO → Poseidon2/Schnorr → Signed TX

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_longlong};

/// C struct for signed transaction response
#[repr(C)]
pub struct SignedTxResponse {
    pub tx_type: u8,
    pub tx_info: *mut c_char,
    pub tx_hash: *mut c_char,
    pub message_to_sign: *mut c_char,
    pub err: *mut c_char,
}

/// C struct for create order request
#[repr(C)]
pub struct CreateOrderTxReq {
    pub market_index: u8,
    pub client_order_index: i64,
    pub base_amount: i64,
    pub price: u32,
    pub is_ask: u8,
    pub order_type: u8,
    pub time_in_force: u8,
    pub reduce_only: u8,
    pub trigger_price: u32,
    pub order_expiry: i64,
}

/// C struct for API key response
#[repr(C)]
pub struct ApiKeyResponse {
    pub private_key: *mut c_char,
    pub public_key: *mut c_char,
    pub err: *mut c_char,
}

/// C struct for string or error
#[repr(C)]
pub struct StrOrErr {
    pub str: *mut c_char,
    pub err: *mut c_char,
}

// External C functions from lighter-signer.so
unsafe extern "C" {
    /// Create a new client instance
    pub fn CreateClient(
        url: *const c_char,
        private_key: *const c_char,
        chain_id: c_int,
        api_key_index: c_int,
        account_index: c_longlong,
    ) -> *mut c_char;

    /// Check if client exists
    pub fn CheckClient(api_key_index: c_int, account_index: c_longlong) -> *mut c_char;

    /// Sign a create order transaction
    pub fn SignCreateOrder(
        market_index: c_int,
        client_order_index: c_longlong,
        base_amount: c_longlong,
        price: c_int,
        is_ask: c_int,
        order_type: c_int,
        time_in_force: c_int,
        reduce_only: c_int,
        trigger_price: c_int,
        order_expiry: c_longlong,
        nonce: c_longlong,
        api_key_index: c_int,
        account_index: c_longlong,
    ) -> SignedTxResponse;

    /// Sign a cancel order transaction
    pub fn SignCancelOrder(
        market_index: c_int,
        order_index: c_longlong,
        nonce: c_longlong,
        api_key_index: c_int,
        account_index: c_longlong,
    ) -> SignedTxResponse;

    /// Create authentication token
    pub fn CreateAuthToken(
        deadline: c_longlong,
        api_key_index: c_int,
        account_index: c_longlong,
    ) -> StrOrErr;

    /// Free C string allocated by Go
    pub fn free(ptr: *mut c_char);
}

/// Safe wrapper for SignedTxResponse
pub struct SignedTransaction {
    pub tx_type: u8,
    pub tx_info: String,
    pub tx_hash: String,
}

impl SignedTxResponse {
    /// Convert C response to safe Rust struct
    ///
    /// # Safety
    /// This function is unsafe because it dereferences raw pointers from C FFI.
    /// The caller must ensure that:
    /// - The `err` pointer (if not null) points to a valid null-terminated C string
    /// - The `tx_info` and `tx_hash` strings are valid UTF-8
    /// - The response was properly initialized by the C library
    pub unsafe fn to_rust(self) -> Result<SignedTransaction, String> {
        // Check for error first
        if !self.err.is_null() {
            unsafe {
                let err_cstr = CStr::from_ptr(self.err);
                let err_msg = err_cstr.to_string_lossy().to_string();
                free(self.err);
                return Err(err_msg);
            }
        }

        // Extract tx_info
        let tx_info = if !self.tx_info.is_null() {
            unsafe {
                let cstr = CStr::from_ptr(self.tx_info);
                let s = cstr.to_string_lossy().to_string();
                free(self.tx_info);
                s
            }
        } else {
            return Err("tx_info is null".to_string());
        };

        // Extract tx_hash
        let tx_hash = if !self.tx_hash.is_null() {
            unsafe {
                let cstr = CStr::from_ptr(self.tx_hash);
                let s = cstr.to_string_lossy().to_string();
                free(self.tx_hash);
                s
            }
        } else {
            return Err("tx_hash is null".to_string());
        };

        Ok(SignedTransaction {
            tx_type: self.tx_type,
            tx_info,
            tx_hash,
        })
    }
}

/// Lighter signer client
///
/// Safety: LighterSigner only holds integer indices. The underlying Go signer
/// state is managed by the Go runtime (global, thread-safe). FFI calls are
/// stateless lookups by (api_key_index, account_index).
pub struct LighterSigner {
    api_key_index: i64,
    account_index: i64,
}

unsafe impl Send for LighterSigner {}
unsafe impl Sync for LighterSigner {}

impl LighterSigner {
    /// Create a new signer instance
    pub fn new(
        base_url: &str,
        private_key: &str,
        chain_id: i32,
        api_key_index: i64,
        account_index: i64,
    ) -> Result<Self, String> {
        let url_cstr = CString::new(base_url).map_err(|e| e.to_string())?;
        let key_cstr = CString::new(private_key).map_err(|e| e.to_string())?;

        unsafe {
            let err_ptr = CreateClient(
                url_cstr.as_ptr(),
                key_cstr.as_ptr(),
                chain_id,
                api_key_index as c_int,
                account_index as c_longlong,
            );

            if !err_ptr.is_null() {
                let err_cstr = CStr::from_ptr(err_ptr);
                let err_msg = err_cstr.to_string_lossy().to_string();
                free(err_ptr);
                return Err(err_msg);
            }
        }

        Ok(Self {
            api_key_index,
            account_index,
        })
    }

    /// Sign a create order transaction
    #[allow(clippy::too_many_arguments)]
    pub fn sign_create_order(
        &self,
        market_index: u8,
        client_order_index: i64,
        base_amount: i64,
        price: u32,
        is_ask: bool,
        order_type: u8,
        time_in_force: u8,
        reduce_only: bool,
        trigger_price: u32,
        order_expiry: i64,
        nonce: i64,
    ) -> Result<SignedTransaction, String> {
        unsafe {
            let response = SignCreateOrder(
                market_index as c_int,
                client_order_index,
                base_amount,
                price as c_int,
                is_ask as c_int,
                order_type as c_int,
                time_in_force as c_int,
                reduce_only as c_int,
                trigger_price as c_int,
                order_expiry,
                nonce,
                self.api_key_index as c_int,
                self.account_index,
            );

            response.to_rust()
        }
    }

    /// Sign a cancel order transaction
    pub fn sign_cancel_order(
        &self,
        market_index: u8,
        order_index: i64,
        nonce: i64,
    ) -> Result<SignedTransaction, String> {
        unsafe {
            let response = SignCancelOrder(
                market_index as c_int,
                order_index,
                nonce,
                self.api_key_index as c_int,
                self.account_index,
            );

            response.to_rust()
        }
    }

    /// Create authentication token for WebSocket
    pub fn create_auth_token(&self, deadline_ms: i64) -> Result<String, String> {
        unsafe {
            let response =
                CreateAuthToken(deadline_ms, self.api_key_index as c_int, self.account_index);

            if !response.err.is_null() {
                let err_cstr = CStr::from_ptr(response.err);
                let err_msg = err_cstr.to_string_lossy().to_string();
                free(response.err);
                return Err(err_msg);
            }

            if !response.str.is_null() {
                let token_cstr = CStr::from_ptr(response.str);
                let token = token_cstr.to_string_lossy().to_string();
                free(response.str);
                Ok(token)
            } else {
                Err("Token is null".to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signer_creation() {
        // This test requires valid credentials
        // Skip in CI/CD
        if std::env::var("API_KEY_PRIVATE_KEY").is_err() {
            return;
        }

        let private_key = std::env::var("API_KEY_PRIVATE_KEY").unwrap();
        let account_index = std::env::var("LIGHTER_ACCOUNT_INDEX")
            .unwrap()
            .parse()
            .unwrap();
        let api_key_index = std::env::var("LIGHTER_API_KEY_INDEX")
            .unwrap()
            .parse()
            .unwrap();

        let signer = LighterSigner::new(
            "https://mainnet.zklighter.elliot.ai",
            &private_key,
            1, // Mainnet
            api_key_index,
            account_index,
        );

        assert!(signer.is_ok());
    }
}

//! Lighter Go Signer FFI Bindings
//!
//! This module provides Rust bindings to the lighter-go CGO signer library.
//! The library handles Poseidon2 + Schnorr signing for Lighter DEX transactions.
//!
//! Architecture: Rust → FFI → Go CGO → Poseidon2/Schnorr → Signed TX

use k256::elliptic_curve::zeroize::Zeroizing;
use std::ffi::{CStr, CString, c_void};
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
    pub market_index: i64,
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

// Signatures from lighter-signer-linux-amd64.h in lighter-go v1.0.13.
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
        market_index: c_longlong,
        client_order_index: c_longlong,
        base_amount: c_longlong,
        price: c_int,
        is_ask: c_int,
        order_type: c_int,
        time_in_force: c_int,
        reduce_only: c_int,
        trigger_price: c_int,
        order_expiry: c_longlong,
        integrator_account_index: c_longlong,
        integrator_taker_fee: c_int,
        integrator_maker_fee: c_int,
        self_trade_behavior_mode: u8,
        self_trade_equality_mode: u8,
        skip_nonce: u8,
        nonce: c_longlong,
        api_key_index: c_int,
        account_index: c_longlong,
    ) -> SignedTxResponse;

    /// Sign a cancel order transaction
    pub fn SignCancelOrder(
        market_index: c_longlong,
        order_index: c_longlong,
        skip_nonce: u8,
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

    /// Release memory allocated by the native SDK.
    pub fn Free(ptr: *mut c_void);
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
        // SAFETY: all non-null response fields are independently owned C strings
        // allocated by the bundled Go library's C.CString allocator.
        unsafe fn take(ptr: *mut c_char) -> Option<String> {
            if ptr.is_null() {
                return None;
            }
            // SAFETY: guaranteed by the caller's response ownership contract.
            let value = unsafe { CStr::from_ptr(ptr) }
                .to_string_lossy()
                .into_owned();
            // SAFETY: the pointer is owned by the caller and allocated by the SDK.
            unsafe { Free(ptr.cast()) };
            Some(value)
        }
        // Consume all fields before returning, including on malformed/error responses.
        let (error, info, hash) = unsafe {
            let error = take(self.err);
            let info = take(self.tx_info);
            let hash = take(self.tx_hash);
            let _ = take(self.message_to_sign);
            (error, info, hash)
        };
        if let Some(error) = error {
            return Err(error);
        }
        let tx_info = info.ok_or("tx_info is null")?;
        let tx_hash = hash.ok_or("tx_hash is null")?;

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
    #[cfg(test)]
    mock_only: bool,
}

impl LighterSigner {
    #[cfg(test)]
    pub(crate) fn offline_for_test() -> Self {
        Self {
            api_key_index: 0,
            account_index: 0,
            mock_only: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn mock_for_test(api_key_index: i64, account_index: i64) -> Self {
        Self {
            api_key_index,
            account_index,
            mock_only: true,
        }
    }

    /// Create a new signer instance
    pub fn new(
        base_url: &str,
        private_key: &str,
        chain_id: i32,
        api_key_index: i64,
        account_index: i64,
    ) -> Result<Self, String> {
        if api_key_index < 0 || api_key_index > u8::MAX as i64 || account_index < 0 {
            return Err("Invalid signer account or API key index".into());
        }
        let url_cstr =
            CString::new(base_url).map_err(|_| "Invalid Lighter base URL".to_string())?;
        let key_bytes: Zeroizing<Vec<u8>> =
            super::super::nul_terminated_secret_bytes(private_key).map_err(str::to_string)?;

        unsafe {
            let err_ptr = CreateClient(
                url_cstr.as_ptr(),
                key_bytes.as_ptr().cast::<c_char>(),
                chain_id,
                api_key_index as c_int,
                account_index as c_longlong,
            );

            if !err_ptr.is_null() {
                let err_cstr = CStr::from_ptr(err_ptr);
                let err_msg = err_cstr.to_string_lossy().to_string();
                Free(err_ptr.cast());
                return Err(err_msg);
            }
        }

        Ok(Self {
            api_key_index,
            account_index,
            #[cfg(test)]
            mock_only: false,
        })
    }

    /// Sign a create order transaction
    #[allow(clippy::too_many_arguments)]
    pub fn sign_create_order(
        &self,
        market_index: impl Into<i64>,
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
        let market_index = market_index.into();
        if !super::super::valid_market_id(market_index) {
            return Err("Invalid Lighter market ID".into());
        }
        let price_c = c_int::try_from(price)
            .map_err(|_| "Lighter order price exceeds C int range (i32::MAX)".to_string())?;
        let trigger_price_c = c_int::try_from(trigger_price)
            .map_err(|_| "Lighter trigger price exceeds C int range (i32::MAX)".to_string())?;
        #[cfg(test)]
        if self.mock_only {
            let tx_info = serde_json::json!({
                "AccountIndex": self.account_index,
                "ApiKeyIndex": self.api_key_index,
                "MarketIndex": market_index,
                "ClientOrderIndex": client_order_index,
                "BaseAmount": base_amount,
                "Price": price,
                "IsAsk": u8::from(is_ask),
                "Type": order_type,
                "TimeInForce": time_in_force,
                "ReduceOnly": u8::from(reduce_only),
                "OrderExpiry": order_expiry,
                "Nonce": nonce,
            })
            .to_string();
            return Ok(SignedTransaction {
                tx_type: 14,
                tx_info,
                tx_hash: format!(
                    "mock-create-{}-{nonce}-{client_order_index}",
                    self.api_key_index
                ),
            });
        }
        // SAFETY: argument widths and order match the v1.0.13 release header.
        unsafe {
            let response = SignCreateOrder(
                market_index,
                client_order_index,
                base_amount,
                price_c,
                is_ask as c_int,
                order_type as c_int,
                time_in_force as c_int,
                reduce_only as c_int,
                trigger_price_c,
                order_expiry,
                0, // No integrator account or fees.
                0,
                0,
                0, // SDK default self-trade protection.
                0,
                0, // Check the explicit nonce.
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
        market_index: impl Into<i64>,
        order_index: i64,
        nonce: i64,
    ) -> Result<SignedTransaction, String> {
        let market_index = market_index.into();
        if !super::super::valid_market_id(market_index) {
            return Err("Invalid Lighter market ID".into());
        }
        #[cfg(test)]
        if self.mock_only {
            let tx_info = serde_json::json!({
                "AccountIndex": self.account_index,
                "ApiKeyIndex": self.api_key_index,
                "MarketIndex": market_index,
                "Index": order_index,
                "Nonce": nonce,
            })
            .to_string();
            return Ok(SignedTransaction {
                tx_type: 15,
                tx_info,
                tx_hash: format!("mock-cancel-{}-{nonce}-{order_index}", self.api_key_index),
            });
        }
        // SAFETY: argument widths and order match the v1.0.13 release header.
        unsafe {
            let response = SignCancelOrder(
                market_index,
                order_index,
                0, // Check the explicit nonce.
                nonce,
                self.api_key_index as c_int,
                self.account_index,
            );

            response.to_rust()
        }
    }

    /// Create authentication token for WebSocket
    pub fn create_auth_token(&self, deadline_secs: i64) -> Result<String, String> {
        #[cfg(test)]
        if self.mock_only {
            return Ok(format!(
                "{deadline_secs}:{}:{}:mock",
                self.account_index, self.api_key_index
            ));
        }
        unsafe {
            let response = CreateAuthToken(
                deadline_secs,
                self.api_key_index as c_int,
                self.account_index,
            );

            if !response.err.is_null() {
                let err_cstr = CStr::from_ptr(response.err);
                let err_msg = err_cstr.to_string_lossy().to_string();
                Free(response.err.cast());
                if !response.str.is_null() {
                    Free(response.str.cast());
                }
                return Err(err_msg);
            }

            if !response.str.is_null() {
                let token_cstr = CStr::from_ptr(response.str);
                let token = token_cstr.to_string_lossy().to_string();
                Free(response.str.cast());
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
    fn uninitialized_native_client_fails_without_signing() {
        let signer = LighterSigner::offline_for_test();
        // No CreateClient, credentials or HTTP: getClient must return an error.
        assert!(signer.sign_cancel_order(0, 1, 0).is_err());
    }

    #[test]
    #[ignore = "requires explicit credentials and network access"]
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
            304, // Mainnet
            api_key_index,
            account_index,
        );

        assert!(signer.is_ok());
    }

    #[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn native_signer_v1_0_13_concurrent_create_cancel_and_auth_stress() {
        use std::collections::HashSet;
        use std::time::{SystemTime, UNIX_EPOCH};

        let synthetic_key = "11".repeat(40);
        let api_key_index = 2_i64;
        let account_index = 999_999_i64;

        let signer = LighterSigner::new(
            "http://127.0.0.1:1",
            &synthetic_key,
            304,
            api_key_index,
            account_index,
        )
        .expect("synthetic mainnet client creation against v1.0.13 .so must succeed");

        let now_secs = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let base_deadline = now_secs + 600;

        let results: Vec<(String, String, String, i64)> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4_i64)
                .map(|thread_idx| {
                    let signer_ref = &signer;
                    scope.spawn(move || -> Vec<(String, String, String, i64)> {
                        let mut out = Vec::with_capacity(25);
                        for step in 0..25_i64 {
                            let nonce = thread_idx * 100 + step + 1;
                            let client_order_index = 50_000 + nonce;
                            let created = signer_ref
                                .sign_create_order(
                                    0i64,
                                    client_order_index,
                                    1_000,
                                    200_000,
                                    thread_idx % 2 == 0,
                                    0,
                                    0,
                                    false,
                                    0,
                                    0,
                                    nonce,
                                )
                                .expect("native sign_create_order must succeed");
                            assert!(!created.tx_info.is_empty());
                            assert!(!created.tx_hash.is_empty());
                            let create_json: serde_json::Value =
                                serde_json::from_str(&created.tx_info).unwrap();
                            assert_eq!(create_json["AccountIndex"], account_index);
                            assert_eq!(create_json["ApiKeyIndex"], api_key_index);
                            assert_eq!(create_json["MarketIndex"], 0);
                            assert_eq!(create_json["ClientOrderIndex"], client_order_index);
                            assert_eq!(create_json["Nonce"], nonce);

                            let order_index = (1i64 << 48) + nonce;
                            let canceled = signer_ref
                                .sign_cancel_order(0i64, order_index, nonce)
                                .expect("native sign_cancel_order must succeed");
                            assert!(!canceled.tx_info.is_empty());
                            assert!(!canceled.tx_hash.is_empty());
                            let cancel_json: serde_json::Value =
                                serde_json::from_str(&canceled.tx_info).unwrap();
                            assert_eq!(cancel_json["AccountIndex"], account_index);
                            assert_eq!(cancel_json["ApiKeyIndex"], api_key_index);
                            assert_eq!(cancel_json["MarketIndex"], 0);
                            assert_eq!(cancel_json["Index"], order_index);
                            assert_eq!(cancel_json["Nonce"], nonce);

                            let deadline = base_deadline + nonce;
                            let token = signer_ref
                                .create_auth_token(deadline)
                                .expect("native create_auth_token must succeed");
                            out.push((created.tx_hash, canceled.tx_hash, token, deadline));
                        }
                        out
                    })
                })
                .collect();

            handles
                .into_iter()
                .flat_map(|handle| handle.join().expect("worker thread must not panic"))
                .collect()
        });

        assert_eq!(results.len(), 100);
        let mut create_hashes = HashSet::with_capacity(100);
        let mut cancel_hashes = HashSet::with_capacity(100);
        for (create_hash, cancel_hash, token, deadline) in results {
            assert!(
                create_hashes.insert(create_hash),
                "duplicate create tx_hash"
            );
            assert!(
                cancel_hashes.insert(cancel_hash),
                "duplicate cancel tx_hash"
            );
            assert!(!token.is_empty());
            let parts: Vec<&str> = token.split(':').collect();
            assert_eq!(parts.len(), 4, "unexpected auth token format: {token}");
            assert_eq!(parts[0], deadline.to_string());
            assert_eq!(parts[1], account_index.to_string());
            assert_eq!(parts[2], api_key_index.to_string());
            assert!(!parts[3].is_empty());
        }
        assert_eq!(create_hashes.len(), 100);
        assert_eq!(cancel_hashes.len(), 100);
        assert!(create_hashes.is_disjoint(&cancel_hashes));

        let bad_non_hex = "zz-synthetic-invalid-key-do-not-leak-0123456789abcdef0123456789abcdef";
        let err_non_hex = LighterSigner::new(
            "http://127.0.0.1:1",
            bad_non_hex,
            304,
            api_key_index,
            account_index,
        )
        .err()
        .expect("non-hex key must fail");
        assert!(!err_non_hex.contains(bad_non_hex));
        assert!(!err_non_hex.contains("zz-synthetic-invalid-key"));
        assert!(!err_non_hex.contains(&synthetic_key));

        let bad_short = "22".repeat(10);
        let err_short = LighterSigner::new(
            "http://127.0.0.1:1",
            &bad_short,
            304,
            api_key_index,
            account_index,
        )
        .err()
        .expect("short key must fail");
        assert!(!err_short.contains(&bad_short));
        assert!(!err_short.contains(&synthetic_key));

        let bad_nul = format!("{}\0{}", "33".repeat(20), "44".repeat(19));
        let err_nul = LighterSigner::new(
            "http://127.0.0.1:1",
            &bad_nul,
            304,
            api_key_index,
            account_index,
        )
        .err()
        .expect("embedded NUL key must fail");
        assert!(!err_nul.contains(&"33".repeat(20)));
        assert!(!err_nul.contains(&"44".repeat(19)));

        let uninit = LighterSigner {
            api_key_index: 251,
            account_index: 999_998,
            mock_only: false,
        };
        for err in [
            uninit
                .sign_create_order(0i64, 1, 100, 200_000, false, 0, 0, false, 0, 0, 1)
                .err()
                .expect("uninitialized create must fail"),
            uninit
                .sign_cancel_order(0i64, 1, 1)
                .err()
                .expect("uninitialized cancel must fail"),
            uninit
                .create_auth_token(base_deadline)
                .err()
                .expect("uninitialized auth token must fail"),
        ] {
            assert!(!err.contains(&synthetic_key));
        }
    }
}

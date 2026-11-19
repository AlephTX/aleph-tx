//! Platform gate for the pinned Linux x86-64 Lighter signer.
//! Other platforms can build and run offline analytics, but cannot sign orders.

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[path = "ffi_native.rs"]
mod native;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub use native::*;

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
mod unavailable {
    #[cfg(test)]
    use crate::exchanges::lighter::valid_market_id;

    const ERROR: &str = "Lighter signing requires the pinned Linux x86-64 signer; this platform supports offline testing only";

    pub struct SignedTransaction {
        pub tx_type: u8,
        pub tx_info: String,
        pub tx_hash: String,
    }

    pub struct LighterSigner {
        #[cfg(test)]
        mock_identity: Option<(i64, i64)>,
    }

    impl LighterSigner {
        #[cfg(test)]
        pub(crate) fn offline_for_test() -> Self {
            Self {
                mock_identity: None,
            }
        }

        #[cfg(test)]
        pub(crate) fn mock_for_test(api_key_index: i64, account_index: i64) -> Self {
            Self {
                mock_identity: Some((api_key_index, account_index)),
            }
        }

        pub fn new(_: &str, _: &str, _: i32, _: i64, _: i64) -> Result<Self, String> {
            Err(ERROR.into())
        }

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
            _trigger_price: u32,
            order_expiry: i64,
            nonce: i64,
        ) -> Result<SignedTransaction, String> {
            std::ffi::c_int::try_from(price)
                .map_err(|_| "Lighter order price exceeds C int range (i32::MAX)".to_string())?;
            std::ffi::c_int::try_from(_trigger_price)
                .map_err(|_| "Lighter trigger price exceeds C int range (i32::MAX)".to_string())?;
            #[cfg(test)]
            if let Some((api_key_index, account_index)) = self.mock_identity {
                let market_index = market_index.into();
                if !valid_market_id(market_index) {
                    return Err("Invalid Lighter market ID".into());
                }
                let tx_info = serde_json::json!({
                    "AccountIndex": account_index,
                    "ApiKeyIndex": api_key_index,
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
                    tx_hash: format!("mock-create-{api_key_index}-{nonce}-{client_order_index}"),
                });
            }
            let _ = (
                market_index,
                client_order_index,
                base_amount,
                price,
                is_ask,
                order_type,
                time_in_force,
                reduce_only,
                order_expiry,
                nonce,
            );
            Err(ERROR.into())
        }

        pub fn sign_cancel_order(
            &self,
            market_index: impl Into<i64>,
            order_index: i64,
            nonce: i64,
        ) -> Result<SignedTransaction, String> {
            #[cfg(test)]
            if let Some((api_key_index, account_index)) = self.mock_identity {
                let market_index = market_index.into();
                if !valid_market_id(market_index) {
                    return Err("Invalid Lighter market ID".into());
                }
                let tx_info = serde_json::json!({
                    "AccountIndex": account_index,
                    "ApiKeyIndex": api_key_index,
                    "MarketIndex": market_index,
                    "Index": order_index,
                    "Nonce": nonce,
                })
                .to_string();
                return Ok(SignedTransaction {
                    tx_type: 15,
                    tx_info,
                    tx_hash: format!("mock-cancel-{api_key_index}-{nonce}-{order_index}"),
                });
            }
            let _ = (market_index, order_index, nonce);
            Err(ERROR.into())
        }

        pub fn create_auth_token(&self, deadline_secs: i64) -> Result<String, String> {
            #[cfg(test)]
            if let Some((api_key_index, account_index)) = self.mock_identity {
                return Ok(format!(
                    "{deadline_secs}:{account_index}:{api_key_index}:mock"
                ));
            }
            let _ = deadline_secs;
            Err(ERROR.into())
        }
    }

    #[test]
    fn unsupported_platform_fails_closed() {
        assert!(LighterSigner::new("", "", 304, 0, 0).is_err());
    }
}
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub use unavailable::*;

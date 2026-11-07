//! Platform gate for the pinned Linux x86-64 Lighter signer.
//! Other platforms can build and run offline analytics, but cannot sign orders.

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[path = "ffi_native.rs"]
mod native;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub use native::*;

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
mod unavailable {
    const ERROR: &str = "Lighter signing requires the pinned Linux x86-64 signer; this platform supports offline testing only";

    pub struct SignedTransaction {
        pub tx_type: u8,
        pub tx_info: String,
        pub tx_hash: String,
    }

    pub struct LighterSigner;

    impl LighterSigner {
        #[cfg(test)]
        pub(crate) fn offline_for_test() -> Self {
            Self
        }

        pub fn new(_: &str, _: &str, _: i32, _: i64, _: i64) -> Result<Self, String> {
            Err(ERROR.into())
        }

        #[allow(clippy::too_many_arguments)]
        pub fn sign_create_order(
            &self,
            _: impl Into<i64>,
            _: i64,
            _: i64,
            _: u32,
            _: bool,
            _: u8,
            _: u8,
            _: bool,
            _: u32,
            _: i64,
            _: i64,
        ) -> Result<SignedTransaction, String> {
            Err(ERROR.into())
        }

        pub fn sign_cancel_order(
            &self,
            _: impl Into<i64>,
            _: i64,
            _: i64,
        ) -> Result<SignedTransaction, String> {
            Err(ERROR.into())
        }

        pub fn create_auth_token(&self, _: i64) -> Result<String, String> {
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

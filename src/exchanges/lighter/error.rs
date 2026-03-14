//! Lighter DEX typed error codes
//!
//! Replaces string-based error matching with strongly-typed enum.

use serde::Deserialize;

/// Lighter API error codes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LighterErrorCode {
    /// 21104: Invalid nonce (nonce too low or already used)
    InvalidNonce = 21104,
    /// 21711: Invalid expiry (timestamp out of range)
    InvalidExpiry = 21711,
    /// 21120: Invalid signature (chain_id mismatch or signature verification failed)
    InvalidSignature = 21120,
    /// 21301: Insufficient margin (not enough collateral)
    InsufficientMargin = 21301,
    /// 21706: Invalid order base or quote amount
    InvalidOrderAmount = 21706,
    /// 21739: Not enough margin to create the order
    NotEnoughMargin = 21739,
    /// Unknown error code
    Unknown,
}

impl LighterErrorCode {
    /// Parse error code from integer
    pub fn from_code(code: i32) -> Self {
        match code {
            21104 => Self::InvalidNonce,
            21711 => Self::InvalidExpiry,
            21120 => Self::InvalidSignature,
            21301 => Self::InsufficientMargin,
            21706 => Self::InvalidOrderAmount,
            21739 => Self::NotEnoughMargin,
            _ => Self::Unknown,
        }
    }

    /// Check if this error requires nonce reset
    pub fn requires_nonce_reset(self) -> bool {
        matches!(self, Self::InvalidNonce | Self::InvalidExpiry)
    }

    /// Check if this error is a margin issue
    pub fn is_margin_error(self) -> bool {
        matches!(self, Self::InsufficientMargin | Self::NotEnoughMargin)
    }
}

/// Lighter API error response structure
#[derive(Debug, Clone, Deserialize)]
pub struct LighterErrorResponse {
    pub code: i32,
    pub message: Option<String>,
}

impl LighterErrorResponse {
    /// Parse error code as enum
    pub fn error_code(&self) -> LighterErrorCode {
        LighterErrorCode::from_code(self.code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_code_parsing() {
        assert_eq!(
            LighterErrorCode::from_code(21104),
            LighterErrorCode::InvalidNonce
        );
        assert_eq!(
            LighterErrorCode::from_code(21711),
            LighterErrorCode::InvalidExpiry
        );
        assert_eq!(
            LighterErrorCode::from_code(21120),
            LighterErrorCode::InvalidSignature
        );
        assert_eq!(
            LighterErrorCode::from_code(21301),
            LighterErrorCode::InsufficientMargin
        );
        assert_eq!(
            LighterErrorCode::from_code(99999),
            LighterErrorCode::Unknown
        );
    }

    #[test]
    fn test_nonce_reset_check() {
        assert!(LighterErrorCode::InvalidNonce.requires_nonce_reset());
        assert!(LighterErrorCode::InvalidExpiry.requires_nonce_reset());
        assert!(!LighterErrorCode::InvalidSignature.requires_nonce_reset());
        assert!(!LighterErrorCode::InsufficientMargin.requires_nonce_reset());
    }

    #[test]
    fn test_margin_error_check() {
        assert!(LighterErrorCode::InsufficientMargin.is_margin_error());
        assert!(!LighterErrorCode::InvalidNonce.is_margin_error());
        assert!(!LighterErrorCode::InvalidSignature.is_margin_error());
    }
}

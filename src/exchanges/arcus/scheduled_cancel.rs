//! The venue's dead-man switch is a cancellation backstop, not terminal order evidence.
use super::{Account, ArcusSigner, SignedRequest};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

const MIN_LEAD_NS: u64 = 5_000_000_000;
const MAX_LEAD_NS: u64 = 300_000_000_000;

impl ArcusSigner {
    /// Arm/refresh one independent scope, or disarm that scope with `None`.
    /// The deadline uses microseconds; the signature header uses nanoseconds.
    /// Refreshes must move the deadline forward. The caller owns that history
    /// and must stop new exposure if renewal is uncertain or expires.
    pub fn schedule_cancel(
        &self,
        market_id: Option<u16>,
        deadline_us: Option<u64>,
        timestamp_ns: u64,
    ) -> Result<SignedRequest> {
        if let Some(deadline) = deadline_us {
            let lead_ns = deadline
                .checked_mul(1_000)
                .and_then(|value| value.checked_sub(timestamp_ns))
                .context("Arcus cancel deadline overflow or in the past")?;
            ensure!(
                (MIN_LEAD_NS..=MAX_LEAD_NS).contains(&lead_ns),
                "Arcus cancel deadline must be 5 seconds to 5 minutes ahead"
            );
        }
        // Scheme 2 signs timestamp + action + compact canonical JSON. Field
        // order is lexical, including optional scope/time; send this exact body.
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Body<'a> {
            account_index: u8,
            address: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            market_id: Option<u16>,
            #[serde(skip_serializing_if = "Option::is_none")]
            time: Option<u64>,
        }
        let body = Body {
            account_index: self.account.index,
            address: &self.account.address,
            market_id,
            time: deadline_us,
        };
        let mut message = format!("{timestamp_ns}scheduleCancel").into_bytes();
        serde_json::to_writer(&mut message, &body)?;
        self.sign_bytes(
            "/v1/scheduleCancel",
            timestamp_ns,
            &message,
            serde_json::to_value(body)?,
        )
    }
}

/// A correlated HTTP 200 confirms scheduling only. It does not prove an
/// eventual cancel, release reserved exposure, or reconcile fills/positions.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledCancelReceipt {
    address: String,
    account_index: u8,
    market_id: Option<u16>,
    time: Option<u64>,
    status: String,
}

impl ScheduledCancelReceipt {
    pub fn deadline_us(&self) -> Option<u64> {
        self.time
    }

    pub fn market_id(&self) -> Option<u16> {
        self.market_id
    }

    pub(super) fn validate(
        &self,
        account: &Account,
        market_id: Option<u16>,
        deadline_us: Option<u64>,
        received_ns: u64,
    ) -> Result<()> {
        let expected_status = if deadline_us.is_some() {
            "scheduled"
        } else {
            "disarmed"
        };
        // Arcus truncates microsecond deadlines down to whole milliseconds when
        // storing and echoing `time` on `/v1/scheduleCancel`.
        let time_matches = match (deadline_us, self.time) {
            (None, None) => true,
            (Some(expected), Some(actual)) => {
                actual == expected || actual == (expected / 1_000) * 1_000
            }
            _ => false,
        };
        ensure!(
            self.address.eq_ignore_ascii_case(&account.address)
                && self.account_index == account.index
                && self.market_id == market_id
                && time_matches
                && self.status == expected_status,
            "Uncorrelated Arcus cancel schedule receipt"
        );
        if let Some(confirmed_us) = self.time {
            ensure!(
                confirmed_us
                    .checked_mul(1_000)
                    .is_some_and(|ns| ns > received_ns),
                "Arcus cancel schedule already expired on receipt"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, SigningKey, Verifier};
    use serde_json::json;

    const NOW: u64 = 1_800_000_000_000_000_000;
    const ADDRESS: &str = "0x1111111111111111111111111111111111111111";

    #[test]
    fn signing_preserves_units_scope_and_canonical_body() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let public = key.verifying_key();
        let signer = ArcusSigner::new(key, ADDRESS, 2).unwrap();
        for scope in [None, Some(88)] {
            for deadline in [None, Some(NOW / 1_000 + 60_000_000)] {
                let request = signer.schedule_cancel(scope, deadline, NOW).unwrap();
                let scope_json = scope
                    .map(|s| format!(",\"marketId\":{s}"))
                    .unwrap_or_default();
                let time_json = deadline
                    .map(|t| format!(",\"time\":{t}"))
                    .unwrap_or_default();
                let body =
                    format!(r#"{{"accountIndex":2,"address":"{ADDRESS}"{scope_json}{time_json}}}"#);
                assert_eq!(serde_json::to_string(&request.body).unwrap(), body);
                let signature =
                    Signature::from_slice(&hex::decode(request.signature).unwrap()).unwrap();
                public
                    .verify(format!("{NOW}scheduleCancel{body}").as_bytes(), &signature)
                    .unwrap();
                assert_eq!(request.path, "/v1/scheduleCancel");
            }
        }
    }

    #[test]
    fn deadline_limits_do_not_round_away_sub_microseconds() {
        let signer = ArcusSigner::new(SigningKey::from_bytes(&[7; 32]), ADDRESS, 0).unwrap();
        for lead in [5_000_000, 300_000_000] {
            assert!(
                signer
                    .schedule_cancel(None, Some(NOW / 1_000 + lead), NOW)
                    .is_ok()
            );
        }
        for deadline in [
            0,
            NOW / 1_000,
            NOW / 1_000 + 4_999_999,
            NOW / 1_000 + 300_000_001,
            u64::MAX,
        ] {
            assert!(signer.schedule_cancel(None, Some(deadline), NOW).is_err());
        }
        assert!(
            signer
                .schedule_cancel(None, Some(NOW / 1_000 + 5_000_000), NOW + 1)
                .is_err()
        );
        assert!(signer.schedule_cancel(None, None, 0).is_err());
    }

    #[test]
    fn receipt_requires_exact_identity_scope_deadline_and_status() {
        let account = Account::new(ADDRESS, 2).unwrap();
        let deadline = NOW / 1_000 + 60_000_000;
        let valid = json!({"address":ADDRESS,"accountIndex":2,"marketId":88,"time":deadline,"status":"scheduled"});
        let parse = |v| serde_json::from_value::<ScheduledCancelReceipt>(v).unwrap();
        assert!(
            parse(valid.clone())
                .validate(&account, Some(88), Some(deadline), NOW)
                .is_ok()
        );
        for (key, value) in [
            (
                "address",
                json!("0x2222222222222222222222222222222222222222"),
            ),
            ("accountIndex", json!(1)),
            ("marketId", json!(89)),
            ("marketId", json!(null)),
            ("time", json!(deadline + 1)),
            ("time", json!(null)),
            ("status", json!("disarmed")),
            ("status", json!("ACK")),
        ] {
            let mut bad = valid.clone();
            bad[key] = value;
            assert!(
                parse(bad)
                    .validate(&account, Some(88), Some(deadline), NOW)
                    .is_err()
            );
        }
        assert!(
            parse(valid)
                .validate(&account, Some(88), Some(deadline), deadline * 1_000)
                .is_err()
        );
        let disarmed = parse(json!({"address":ADDRESS,"accountIndex":2,"status":"disarmed"}));
        assert!(disarmed.validate(&account, None, None, NOW).is_ok());

        let sub_ms_deadline = NOW / 1_000 + 60_000_042;
        let ms_trunc = parse(json!({
            "address": ADDRESS,
            "accountIndex": 2,
            "marketId": 88,
            "time": (sub_ms_deadline / 1_000) * 1_000,
            "status": "scheduled"
        }));
        assert!(
            ms_trunc
                .validate(&account, Some(88), Some(sub_ms_deadline), NOW)
                .is_ok()
        );
        let bad_trunc = parse(json!({
            "address": ADDRESS,
            "accountIndex": 2,
            "marketId": 88,
            "time": sub_ms_deadline - 1,
            "status": "scheduled"
        }));
        assert!(
            bad_trunc
                .validate(&account, Some(88), Some(sub_ms_deadline), NOW)
                .is_err()
        );
    }
}

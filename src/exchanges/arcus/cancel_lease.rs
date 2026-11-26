//! Process-local DMS protection. No transport, persistence, or exposure release.
use super::{Account, ScheduledCancelReceipt};
use anyhow::{Context, Result, ensure};
use std::time::{Duration, Instant};

/// Explicit caller policy; no strategy capital or live defaults are inferred.
#[derive(Clone, Copy)]
pub struct CancelLeasePolicy {
    pub lifetime: Duration,
    pub refresh_after: Duration,
    pub stop_before_expiry: Duration,
    /// Maximum change in wall-minus-monotonic time during a request. Absolute
    /// venue clock synchronization remains a separate admission requirement.
    pub max_clock_step: Duration,
}

struct Pending {
    sent_wall_ns: u64,
    sent_mono: Instant,
    deadline_us: u64,
    expires: Instant,
    refresh_at: Instant,
    previous_expires: Option<Instant>,
}

struct Confirmed {
    observed_at: Instant,
    expires: Instant,
    refresh_at: Instant,
}

/// Forward this exact scope and deadline to `ArcusTrading::schedule_cancel`.
/// This is a request plan, never an acknowledgement or order permission.
pub struct CancelRenewal {
    market_id: Option<u16>,
    deadline_us: u64,
}

impl CancelRenewal {
    pub fn market_id(&self) -> Option<u16> {
        self.market_id
    }

    pub fn deadline_us(&self) -> u64 {
        self.deadline_us
    }
}

/// A new instance always starts unprotected. Deliberately not serializable:
/// restarting must reconcile exposure and obtain a new confirmed lease.
///
/// This gate is only one condition for new exposure. It cannot authorize an
/// order, prove a cancel, release reserves, or clear a journal/restart barrier.
pub struct CancelLease {
    account: Account,
    market_id: Option<u16>,
    policy: CancelLeasePolicy,
    pending: Option<Pending>,
    confirmed: Option<Confirmed>,
    last_deadline_us: Option<u64>,
    last_observed_mono: Option<Instant>,
    faulted: bool,
}

impl CancelLease {
    pub(super) fn matches_account(&self, account: &Account) -> bool {
        self.account.address == account.address && self.account.index == account.index
    }

    pub fn new(
        account: Account,
        market_id: Option<u16>,
        policy: CancelLeasePolicy,
    ) -> Result<Self> {
        ensure!(
            policy.lifetime > Duration::from_secs(5)
                && policy.lifetime <= Duration::from_secs(300)
                && policy.lifetime.subsec_nanos().is_multiple_of(1_000)
                && !policy.refresh_after.is_zero()
                && !policy.stop_before_expiry.is_zero()
                && policy
                    .refresh_after
                    .checked_add(policy.stop_before_expiry)
                    .is_some_and(|total| total < policy.lifetime)
                && policy.max_clock_step <= policy.stop_before_expiry,
            "Invalid Arcus cancel lease policy"
        );
        Ok(Self {
            account,
            market_id,
            policy,
            pending: None,
            confirmed: None,
            last_deadline_us: None,
            last_observed_mono: None,
            faulted: false,
        })
    }

    pub fn needs_renewal(&self, now: Instant) -> bool {
        !self.requires_reconciliation(now)
            && self.pending.is_none()
            && self
                .confirmed
                .as_ref()
                .is_none_or(|lease| now < lease.observed_at || now >= lease.refresh_at)
    }

    /// Allocation-free quote-path predicate. Pending, failed, expired or
    /// safety-margin leases deny new exposure, regardless of an older ACK.
    pub fn allows_new_exposure(&self, now: Instant) -> bool {
        !self.requires_reconciliation(now)
            && self.pending.is_none()
            && self.confirmed.as_ref().is_some_and(|lease| {
                now >= lease.observed_at
                    && lease
                        .expires
                        .checked_duration_since(now)
                        .is_some_and(|left| left > self.policy.stop_before_expiry)
            })
    }

    /// A failure or a possibly fired switch is a sticky recovery boundary.
    /// A new successful schedule alone cannot reconcile outstanding orders.
    pub fn requires_reconciliation(&self, now: Instant) -> bool {
        self.faulted
            || self
                .confirmed
                .as_ref()
                .is_some_and(|lease| now >= lease.expires)
            || self.pending.as_ref().is_some_and(|request| {
                now >= request.expires
                    || request.previous_expires.is_some_and(|expiry| now >= expiry)
            })
    }

    /// Call immediately before dispatch. A single request may be in flight;
    /// every later attempt uses a later deadline. After uncertainty or expiry,
    /// reconcile both venues and create a new guard at the recovery boundary.
    pub fn begin_renewal(&mut self, wall_ns: u64, now: Instant) -> Result<CancelRenewal> {
        if self.requires_reconciliation(now) {
            self.invalidate();
            anyhow::bail!("Arcus cancel lease requires reconciliation");
        }
        // Any failed preparation closes this instance too: do not discard the
        // previous expiry and then accidentally rearm across a protection gap.
        self.faulted = true;
        let previous_expires = self.confirmed.take().map(|lease| lease.expires);
        ensure!(
            self.pending.is_none(),
            "Arcus cancel renewal already pending"
        );
        ensure!(wall_ns > 0, "Missing Arcus cancel renewal wall time");
        ensure!(
            self.last_observed_mono.is_none_or(|last| now >= last),
            "Arcus lease clock reversed"
        );
        let lifetime_us = u64::try_from(self.policy.lifetime.as_micros())?;
        let deadline_us = (wall_ns / 1_000)
            .checked_add(lifetime_us)
            .context("Arcus cancel lease deadline overflow")?;
        ensure!(
            self.last_deadline_us.is_none_or(|last| deadline_us > last),
            "Arcus renewal deadline must advance"
        );
        let lead_ns = deadline_us
            .checked_mul(1_000)
            .and_then(|deadline| deadline.checked_sub(wall_ns))
            .context("Arcus cancel lease clock overflow")?;
        ensure!(
            (5_000_000_000..=300_000_000_000).contains(&lead_ns),
            "Arcus cancel lease outside venue window"
        );
        let expires = now
            .checked_add(Duration::from_nanos(lead_ns))
            .context("Arcus monotonic deadline overflow")?;
        let refresh_at = now
            .checked_add(self.policy.refresh_after)
            .context("Arcus refresh clock overflow")?;
        self.last_deadline_us = Some(deadline_us);
        self.last_observed_mono = Some(now);
        self.pending = Some(Pending {
            sent_wall_ns: wall_ns,
            sent_mono: now,
            deadline_us,
            expires,
            refresh_at,
            previous_expires,
        });
        self.faulted = false;
        Ok(CancelRenewal {
            market_id: self.market_id,
            deadline_us,
        })
    }

    /// Accept only the current request's correlated REST receipt. A late ACK
    /// cannot extend the lease: expiry is anchored at dispatch, not receipt.
    /// On every error the gate stays closed and this request is consumed.
    pub fn confirm_renewal(
        &mut self,
        receipt: &ScheduledCancelReceipt,
        received_wall_ns: u64,
        now: Instant,
    ) -> Result<()> {
        self.confirmed = None;
        let already_faulted = self.faulted;
        self.faulted = true;
        ensure!(
            !already_faulted,
            "Arcus cancel lease requires reconciliation"
        );
        let request = self
            .pending
            .take()
            .context("No pending Arcus cancel renewal")?;
        ensure!(
            request.previous_expires.is_none_or(|expiry| now < expiry),
            "Arcus previous cancel lease may have fired before renewal confirmation"
        );
        let elapsed = now
            .checked_duration_since(request.sent_mono)
            .context("Arcus lease clock reversed")?;
        self.last_observed_mono = Some(now);
        let wall_elapsed = received_wall_ns
            .checked_sub(request.sent_wall_ns)
            .map(Duration::from_nanos)
            .context("Arcus wall clock moved backwards")?;
        ensure!(
            wall_elapsed.abs_diff(elapsed) <= self.policy.max_clock_step,
            "Arcus wall clock stepped during renewal"
        );
        receipt.validate(
            &self.account,
            self.market_id,
            Some(request.deadline_us),
            received_wall_ns,
        )?;
        let trunc_delta_us = receipt
            .deadline_us()
            .map_or(0, |confirmed| request.deadline_us.saturating_sub(confirmed));
        let confirmed_expires = request
            .expires
            .checked_sub(Duration::from_micros(trunc_delta_us))
            .context("Arcus monotonic deadline underflow")?;
        ensure!(
            confirmed_expires
                .checked_duration_since(now)
                .is_some_and(|left| left > self.policy.stop_before_expiry),
            "Arcus cancel protection too short on receipt"
        );
        self.confirmed = Some(Confirmed {
            observed_at: now,
            expires: confirmed_expires,
            refresh_at: request.refresh_at,
        });
        self.faulted = false;
        Ok(())
    }

    /// Call on transport errors, rejection, timeout, stream/clock uncertainty,
    /// or before explicit disarming. Never implies that orders are canceled.
    /// This guard cannot be rearmed afterward; recovery must create a new one.
    pub fn invalidate(&mut self) {
        self.pending = None;
        self.confirmed = None;
        self.faulted = true;
    }
}

#[cfg(test)]
mod tests;

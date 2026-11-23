//! Optional causal opening-entry microstructure and inventory guards (`ARB-IMP-02` & `ARB-IMP-03`).
//! Strictly opt-in (`None` by default) and scoped solely to opening entries (`!closing`);
//! never blocks or tightens closing quotes, hedge IOCs, or forced drains.

use super::*;

/// Convex exposure-based opening threshold surcharge (`ARB-IMP-02`).
/// Strictly forbidden in `PaperMode::Turnover` so inventory ladders cannot slow hedging.
/// Requires `require_net_edge` to enforce the surcharge at the opening boundary.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryLadderConfig {
    pub floor_frac: f64,
    #[serde(with = "rust_decimal::serde::str")]
    pub scale_bps: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_position_notional: Decimal,
    pub convex_power: f64,
}

impl InventoryLadderConfig {
    pub(super) fn validate(self, config: &MakerFirstConfig) -> Result<()> {
        ensure!(
            config.pair.mode.is_pair_hedge(),
            "inventory_ladder is restricted to PairHedge mode"
        );
        ensure!(
            config.require_net_edge,
            "inventory_ladder requires require_net_edge = true"
        );
        ensure!(
            self.floor_frac.is_finite()
                && (0.0..1.0).contains(&self.floor_frac)
                && self.scale_bps > Decimal::ZERO
                && self.scale_bps <= Decimal::from(1_000)
                && self.max_position_notional > Decimal::ZERO
                && bounded(self.max_position_notional)
                && self.convex_power.is_finite()
                && (1.0..=6.0).contains(&self.convex_power),
            "invalid inventory ladder configuration"
        );
        Ok(())
    }

    /// Extra required opening edge (bps) at `projected_notional`. Zero when `increasing_exposure`
    /// is `false` or utilization `u <= floor_frac`.
    pub fn extra_edge_bps(self, projected_notional: Decimal, increasing_exposure: bool) -> Decimal {
        if !increasing_exposure || projected_notional <= Decimal::ZERO {
            return Decimal::ZERO;
        }
        let u = (projected_notional / self.max_position_notional)
            .to_f64()
            .unwrap_or(1.0)
            .clamp(0.0, 1.0);
        if u <= self.floor_frac {
            return Decimal::ZERO;
        }
        let norm = ((u - self.floor_frac) / (1.0 - self.floor_frac)).clamp(0.0, 1.0);
        let factor = norm.powf(self.convex_power);
        (self.scale_bps * Decimal::from_f64_retain(factor).unwrap_or(Decimal::ONE)).round_dp(8)
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct InventoryLadderReport {
    pub evaluations: usize,
    pub blocked_entries: usize,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_extra_edge_bps: Decimal,
}

#[derive(Default)]
pub(super) struct InventoryLadderState {
    evaluations: usize,
    blocked_entries: usize,
    max_extra_edge_bps: Decimal,
}

impl InventoryLadderState {
    pub(super) fn record_eval(&mut self, extra_bps: Decimal) {
        self.evaluations += 1;
        if extra_bps > self.max_extra_edge_bps {
            self.max_extra_edge_bps = extra_bps;
        }
    }

    pub(super) fn record_block(&mut self) {
        self.blocked_entries += 1;
    }

    pub(super) fn report(&self) -> InventoryLadderReport {
        InventoryLadderReport {
            evaluations: self.evaluations,
            blocked_entries: self.blocked_entries,
            max_extra_edge_bps: self.max_extra_edge_bps,
        }
    }
}

/// Causal receipt-time opening-edge persistence gate (`ARB-IMP-03`).
/// Requires `PairHedge` mode and `require_net_edge = true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EntryEdgePersistence {
    pub min_persist_ms: u64,
    pub max_gap_ms: u64,
}

impl EntryEdgePersistence {
    pub(super) fn validate(self, config: &MakerFirstConfig) -> Result<()> {
        ensure!(
            config.pair.mode.is_pair_hedge(),
            "entry_edge_persistence is restricted to PairHedge mode"
        );
        ensure!(
            config.require_net_edge,
            "entry_edge_persistence requires require_net_edge = true"
        );
        ensure!(
            (1..=60_000).contains(&self.min_persist_ms) && (1..=60_000).contains(&self.max_gap_ms),
            "invalid entry edge persistence configuration"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct EntryEdgePersistReport {
    pub min_persist_ms: u64,
    pub max_gap_ms: u64,
    pub blocked_entries: usize,
    pub resets: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BookEvidence {
    None,
    Duplicate,
    New,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LegBookFingerprint {
    source_ms: u64,
    content_hash: [u64; 2],
}

fn feed_hash_bytes(h1: &mut u64, h2: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h1 ^= u64::from(b);
        *h1 = h1.wrapping_mul(0x1000_0000_01b3);
        *h2 = h2.wrapping_add(u64::from(b).wrapping_add(0x9e37_79b9_7f4a_7c15));
        *h2 = h2.rotate_left(13).wrapping_mul(0xff51_afd7_ed55_8ccd);
    }
}

pub(super) fn fingerprint_leg_book(book: &Book) -> LegBookFingerprint {
    let mut h1 = 0xcbf2_9ce4_8422_2325_u64;
    let mut h2 = 0x9e37_79b9_7f4a_7c15_u64;
    feed_hash_bytes(&mut h1, &mut h2, &(book.market.len() as u64).to_le_bytes());
    feed_hash_bytes(&mut h1, &mut h2, book.market.as_bytes());
    feed_hash_bytes(&mut h1, &mut h2, &book.timestamp_ms.to_le_bytes());
    feed_hash_bytes(&mut h1, &mut h2, &[u8::from(book.tradable)]);
    feed_hash_bytes(
        &mut h1,
        &mut h2,
        &book.available_notional.normalize().serialize(),
    );
    match book.bounds {
        None => feed_hash_bytes(&mut h1, &mut h2, &[0]),
        Some(b) => {
            feed_hash_bytes(&mut h1, &mut h2, &[1]);
            feed_hash_bytes(&mut h1, &mut h2, &b.lower.normalize().serialize());
            feed_hash_bytes(&mut h1, &mut h2, &b.upper.normalize().serialize());
        }
    }
    for levels in [&book.bids, &book.asks] {
        feed_hash_bytes(&mut h1, &mut h2, &(levels.len() as u64).to_le_bytes());
        for level in levels {
            feed_hash_bytes(&mut h1, &mut h2, &level.price.normalize().serialize());
            feed_hash_bytes(&mut h1, &mut h2, &level.contracts.normalize().serialize());
        }
    }
    LegBookFingerprint {
        source_ms: book.timestamp_ms,
        content_hash: [h1, h2],
    }
}

#[derive(Default)]
pub(super) struct EntryEdgePersistState {
    since_ms: [Option<u64>; 2],
    last_eval_ms: [Option<u64>; 2],
    last_legs: [Option<(LegBookFingerprint, bool)>; 2],
    last_pair_source_ms: Option<[u64; 2]>,
    blocked_entries: usize,
    resets: usize,
}

impl EntryEdgePersistState {
    pub(super) fn reset_all(&mut self) {
        if self.since_ms.iter().any(Option::is_some) {
            self.resets += 1;
        }
        self.since_ms = [None; 2];
        self.last_eval_ms = [None; 2];
    }

    pub(super) fn classify_fingerprints(
        &mut self,
        legs: [Option<LegBookFingerprint>; 2],
        pair_usable: bool,
    ) -> BookEvidence {
        // Track independent per-leg source identity even when the counterpart leg prevents quote evidence.
        let mut current_legs_match_sources = true;
        for (state, observed) in self.last_legs.iter_mut().zip(legs) {
            let Some(observed) = observed else {
                continue;
            };
            match state {
                Some((previous, conflicted)) if observed.source_ms == previous.source_ms => {
                    *conflicted |= previous.content_hash != observed.content_hash;
                }
                Some((previous, _)) if observed.source_ms < previous.source_ms => {
                    current_legs_match_sources = false;
                }
                _ => *state = Some((observed, false)),
            }
        }
        let [Some(a), Some(b)] = legs else {
            self.reset_all();
            return BookEvidence::None;
        };
        if !pair_usable
            || !current_legs_match_sources
            || self
                .last_legs
                .iter()
                .flatten()
                .any(|(_, conflicted)| *conflicted)
        {
            self.reset_all();
            return BookEvidence::None;
        }
        let sources = [a.source_ms, b.source_ms];
        let advanced = self
            .last_pair_source_ms
            .replace(sources)
            .is_none_or(|previous| sources[0] > previous[0] || sources[1] > previous[1]);
        if advanced {
            BookEvidence::New
        } else {
            BookEvidence::Duplicate
        }
    }

    fn advance_clock(
        &mut self,
        policy: EntryEdgePersistence,
        side: HedgeSide,
        now: u64,
        new_book_evidence: bool,
    ) -> bool {
        let idx = side_index(side);
        let opp = 1 - idx;
        if self.since_ms[opp].is_some() {
            self.since_ms[opp] = None;
            self.last_eval_ms[opp] = None;
            self.resets += 1;
        }
        if self.last_eval_ms[idx].is_some_and(|prev| now.saturating_sub(prev) > policy.max_gap_ms) {
            self.since_ms[idx] = None;
            self.last_eval_ms[idx] = None;
            self.resets += 1;
        }
        if new_book_evidence {
            let start = *self.since_ms[idx].get_or_insert(now);
            self.last_eval_ms[idx] = Some(now);
            now.saturating_sub(start) >= policy.min_persist_ms
        } else {
            matches!(
                (self.since_ms[idx], self.last_eval_ms[idx]),
                (Some(start), Some(last_book_ms))
                    if last_book_ms.saturating_sub(start) >= policy.min_persist_ms
            )
        }
    }

    /// Advances persistence clock on an arrived new valid book even when quote submission is throttled,
    /// without charging a quote-attempt block.
    pub(super) fn observe_passing(
        &mut self,
        policy: EntryEdgePersistence,
        side: HedgeSide,
        now: u64,
    ) {
        let _ = self.advance_clock(policy, side, now, true);
    }

    pub(super) fn check_and_advance(
        &mut self,
        policy: EntryEdgePersistence,
        side: HedgeSide,
        now: u64,
        new_book_evidence: bool,
    ) -> bool {
        let passed = self.advance_clock(policy, side, now, new_book_evidence);
        if !passed {
            self.blocked_entries += 1;
        }
        passed
    }

    pub(super) fn report(&self, policy: EntryEdgePersistence) -> EntryEdgePersistReport {
        EntryEdgePersistReport {
            min_persist_ms: policy.min_persist_ms,
            max_gap_ms: policy.max_gap_ms,
            blocked_entries: self.blocked_entries,
            resets: self.resets,
        }
    }
}

/// Post-fill fresh book advancement barrier for opening entries (`ARB-IMP-03`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PostFillRefreshMode {
    AnyLeg,
    BothLegs,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct PostFillRefreshReport {
    pub mode: PostFillRefreshMode,
    pub locks_armed: usize,
    pub locks_cleared: usize,
    pub blocked_entries: usize,
    pub locked_at_cutoff: bool,
}

#[derive(Default)]
pub(super) struct PostFillRefreshState {
    locked_book_ts: Option<[u64; 2]>,
    locks_armed: usize,
    locks_cleared: usize,
    blocked_entries: usize,
}

impl PostFillRefreshState {
    pub(super) fn arm(&mut self, book_ts: [u64; 2]) {
        self.locked_book_ts = Some(book_ts);
        self.locks_armed += 1;
    }

    pub(super) fn observe_books(
        &mut self,
        mode: PostFillRefreshMode,
        usable: [bool; 2],
        source_ms: [u64; 2],
    ) {
        let Some(lock_ts) = self.locked_book_ts else {
            return;
        };
        let adv0 = usable[0] && source_ms[0] > lock_ts[0];
        let adv1 = usable[1] && source_ms[1] > lock_ts[1];
        let cleared = match mode {
            PostFillRefreshMode::AnyLeg => adv0 || adv1,
            PostFillRefreshMode::BothLegs => adv0 && adv1,
        };
        if cleared {
            self.locked_book_ts = None;
            self.locks_cleared += 1;
        }
    }

    pub(super) fn is_locked(&self) -> bool {
        self.locked_book_ts.is_some()
    }

    pub(super) fn record_block(&mut self) {
        self.blocked_entries += 1;
    }

    pub(super) fn report(&self, mode: PostFillRefreshMode) -> PostFillRefreshReport {
        PostFillRefreshReport {
            mode,
            locks_armed: self.locks_armed,
            locks_cleared: self.locks_cleared,
            blocked_entries: self.blocked_entries,
            locked_at_cutoff: self.is_locked(),
        }
    }
}

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const UPPER_MS: [u64; 19] = [
    0,
    1,
    5,
    10,
    25,
    50,
    100,
    200,
    350,
    500,
    750,
    1_000,
    1_500,
    2_200,
    3_000,
    5_000,
    10_000,
    60_000,
    u64::MAX,
];

#[derive(Debug, Default, Serialize)]
pub struct TimingSummary {
    pub observations: usize,
    pub p50_upper_ms: Option<u64>,
    pub p95_upper_ms: Option<u64>,
    pub p99_upper_ms: Option<u64>,
    pub max_ms: Option<u64>,
}

#[derive(Default)]
pub(super) struct TimingHistogram {
    counts: [usize; UPPER_MS.len()],
    max_ms: u64,
}

impl TimingHistogram {
    pub fn observe(&mut self, age_ms: u64) {
        self.counts[UPPER_MS.partition_point(|&limit| limit < age_ms)] += 1;
        self.max_ms = self.max_ms.max(age_ms);
    }

    pub fn summarize(self) -> TimingSummary {
        let observations = self.counts.iter().sum::<usize>();
        if observations == 0 {
            return TimingSummary::default();
        }
        let upper = |percent: u128| {
            let rank = (observations as u128 * percent).div_ceil(100) as usize;
            let mut total = 0;
            self.counts.iter().zip(UPPER_MS).find_map(|(count, limit)| {
                total += count;
                (total >= rank).then_some(limit.min(self.max_ms))
            })
        };
        TimingSummary {
            observations,
            p50_upper_ms: upper(50),
            p95_upper_ms: upper(95),
            p99_upper_ms: upper(99),
            max_ms: Some(self.max_ms),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureTiming {
    pub run_id: String,
    pub sequence: u64,
    pub connection_epoch: u64,
    pub receipt_wall_ns: u64,
    pub receipt_monotonic_ns: u64,
    pub available_wall_ns: u64,
    pub available_monotonic_ns: u64,
}

#[derive(Default)]
pub(super) struct PreciseClockAudit {
    run_id: Option<String>,
    sequence: u64,
    last_wall_ns: u64,
    last_mono_ns: u64,
    origin_ns: i128,
    epochs: HashMap<(String, Option<String>), u64>,
}

impl PreciseClockAudit {
    pub fn observe(
        &mut self,
        stamp: Option<CaptureTiming>,
        source: &str,
        market: Option<&str>,
        received_at_ms: u64,
        available_at_ms: u64,
    ) -> Result<u64> {
        let Some(t) = stamp else {
            bail!("invalid precise capture clock fields");
        };
        let valid_hex = t.run_id.len() == 32
            && t.run_id
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        let rw = t.receipt_wall_ns;
        let aw = t.available_wall_ns;
        let rm = t.receipt_monotonic_ns;
        let am = t.available_monotonic_ns;
        if !valid_hex
            || t.sequence == 0
            || rw == 0
            || aw < rw
            || am < rm
            || rw / 1_000_000 != received_at_ms
            || aw / 1_000_000 != available_at_ms
            || (aw - rw).abs_diff(am - rm) > 1_000_000
            || (source == "collector") != (t.connection_epoch == 0)
        {
            bail!("inconsistent precise capture clocks");
        }

        if self.run_id.is_none() {
            self.run_id = Some(t.run_id.clone());
            self.origin_ns = i128::from(aw) - i128::from(am);
        }
        let key = (source.to_owned(), market.map(str::to_owned));
        let prior_epoch = self.epochs.get(&key).copied().unwrap_or(0);
        ensure!(
            self.run_id.as_deref() == Some(t.run_id.as_str())
                && t.sequence == self.sequence + 1
                && aw >= self.last_wall_ns
                && am >= self.last_mono_ns
                && (i128::from(aw) - i128::from(am) - self.origin_ns).abs() <= 1_000_000
                && t.connection_epoch >= prior_epoch,
            "capture identity, sequence, epoch or clock continuity failed"
        );
        self.sequence = t.sequence;
        self.last_wall_ns = aw;
        self.last_mono_ns = am;
        self.epochs.insert(key, t.connection_epoch);
        // Ceiling division ensures availability never precedes receipt.
        Ok(aw.div_ceil(1_000_000))
    }

    pub fn run_id(&self) -> Option<&str> {
        self.run_id.as_deref()
    }

    pub fn records(&self) -> u64 {
        self.sequence
    }
}

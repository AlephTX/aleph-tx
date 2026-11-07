use serde::Serialize;

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

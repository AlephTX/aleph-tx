use crate::shm_reader::ShmBboMessage;

const SECOND_NS: u64 = 1_000_000_000;
const MINUTE_NS: u64 = 60 * SECOND_NS;
const MIN_SAMPLES: u8 = 45;
const HISTORY: usize = 5;

#[derive(Default)]
struct Minute {
    index: u64,
    samples: u8,
    low: f64,
    high: f64,
}

#[derive(Default)]
pub(super) struct VolatilityRegime {
    current: Minute,
    last_source_ns: u64,
    last_second: Option<u64>,
    ranges_bps: [f64; HISTORY],
    completed: usize,
}

impl VolatilityRegime {
    pub(super) fn observe(&mut self, book: Option<&ShmBboMessage>, now_ns: u64, max_age_ms: u64) {
        let Some(book) = book else { return };
        let source = book.timestamp_ns;
        let [bid, ask] = [book.bid_price, book.ask_price];
        let second = now_ns / SECOND_NS;
        if source <= self.last_source_ns
            || source > now_ns
            || now_ns - source > max_age_ms.saturating_mul(1_000_000)
            || self.last_second.is_some_and(|prior| prior >= second)
            || !(bid.is_finite() && ask.is_finite() && bid > 0.0 && ask > bid)
        {
            return;
        }
        self.last_source_ns = source;
        self.last_second = Some(second);
        self.sample(now_ns / MINUTE_NS, 0.5 * bid + 0.5 * ask);
    }

    fn sample(&mut self, minute: u64, mid: f64) {
        let current = &mut self.current;
        if current.samples == 0 || current.index != minute {
            if current.index + 1 == minute && current.samples >= MIN_SAMPLES {
                let i = self.completed % HISTORY;
                self.ranges_bps[i] = (current.high.ln() - current.low.ln()) * 10_000.0;
                self.completed += 1;
            } else {
                self.completed = 0;
            }
            *current = Minute {
                index: minute,
                samples: 1,
                low: mid,
                high: mid,
            };
        } else {
            current.samples += 1;
            current.low = current.low.min(mid);
            current.high = current.high.max(mid);
        }
    }

    pub(super) fn pressure(&self, now_ns: u64, trigger_bps: f64) -> f64 {
        if self.completed < HISTORY || self.current.index != now_ns / MINUTE_NS {
            return 0.0;
        }
        let mean_square = self.ranges_bps.iter().map(|r| r * r).sum::<f64>() / HISTORY as f64;
        (mean_square.sqrt() / trigger_bps - 1.0).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_only_covered_completed_minutes_and_expires_after_a_gap() {
        let mut regime = VolatilityRegime::default();
        for minute in 10..15 {
            for i in 0..MIN_SAMPLES {
                regime.sample(minute, 100.0 + f64::from(i % 2));
            }
            assert_eq!(regime.pressure(minute * MINUTE_NS, 20.0), 0.0);
        }
        regime.sample(15, 100.0);
        let pressure = regime.pressure(15 * MINUTE_NS, 80.0);
        assert!(pressure > 0.0 && pressure < 1.0);
        regime.sample(15, 200.0);
        assert_eq!(regime.pressure(15 * MINUTE_NS, 80.0), pressure);
        assert_eq!(regime.pressure(16 * MINUTE_NS, 80.0), 0.0);
        regime.sample(16, 100.0);
        assert_eq!(regime.pressure(16 * MINUTE_NS, 80.0), 0.0);
    }
}

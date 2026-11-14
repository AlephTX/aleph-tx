//! Offline cycle attribution. Price costs reconcile to the flat cash ledger;
//! forward markouts are overlapping diagnostics and must never be added to it.
use super::*;

const HORIZONS: [u64; 2] = [1_000, 5_000];
const LABEL_GRACE_MS: u64 = 1_000;

#[derive(Clone, Copy)]
pub(super) enum Phase {
    EntryMaker = 0,
    EntryHedge = 1,
    CloseMaker = 2,
    CloseHedge = 3,
    ForcedClose = 4,
}

#[derive(Debug, Default, Serialize)]
pub struct StageCost {
    pub phase: &'static str,
    pub fills: usize,
    #[serde(with = "rust_decimal::serde::str")]
    pub turnover: Decimal,
    /// Signed cost against the same leg's fresh mid at fill receipt.
    #[serde(with = "rust_decimal::serde::str")]
    pub execution_cost_quote: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub fees_quote: Decimal,
}

#[derive(Debug, Default, Serialize)]
pub struct EntryMarkout {
    pub horizon_ms: u64,
    pub marked_fills: usize,
    #[serde(with = "rust_decimal::serde::str")]
    pub marked_turnover: Decimal,
    /// Positive means price moved against the opening Maker inventory.
    #[serde(with = "rust_decimal::serde::str")]
    pub mid_move_cost_quote: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub markout_quote: Decimal,
    pub expired_fills: usize,
    pub missing_entry_mid: usize,
    pub pending_at_cutoff: usize,
}

#[derive(Debug, Serialize)]
pub struct CycleCost {
    pub cycle_id: usize,
    pub start_ms: u64,
    pub end_ms: Option<u64>,
    pub completed: bool,
    pub stages: [StageCost; 5],
    pub entry_markouts: [EntryMarkout; 2],
    #[serde(with = "rust_decimal::serde::str")]
    pub target_turnover: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub gross_turnover: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub fees_quote: Decimal,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub overhead_quote: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub net_before_overhead: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub net_after_overhead: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub price_loss_quote: Option<Decimal>,
    /// Signed sum(q * (fill-time mid - cycle-entry mid)), across both legs.
    #[serde(with = "rust_decimal::serde::str_option")]
    pub price_drift_cost_quote: Option<Decimal>,
    /// Separate benchmark; overlaps entry hedge execution cost, not additive.
    #[serde(with = "rust_decimal::serde::str")]
    pub opening_hedge_slippage_vs_submit_touch: Decimal,
    pub missing_mid_fills: usize,
    pub cash_reconciled: bool,
    pub price_decomposition_reconciled: bool,
    #[serde(skip)]
    reference_mids: [Option<Decimal>; 2],
    #[serde(skip)]
    positions: [Decimal; 2],
    #[serde(skip)]
    cash: Decimal,
    #[serde(skip)]
    drift: Decimal,
}

#[derive(Debug)]
struct Pending {
    cycle: usize,
    due: u64,
    signed_base: Decimal,
    notional: Decimal,
    mid: Decimal,
    price: Decimal,
}

#[derive(Debug, Serialize)]
pub struct Costs {
    pub markout_anchor: &'static str,
    pub cycles: Vec<CycleCost>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub whole_ledger_net_before_overhead: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::str_option")]
    pub whole_ledger_net_after_overhead: Option<Decimal>,
    pub whole_ledger_cash_reconciled: bool,
    #[serde(skip)]
    active: Option<usize>,
    #[serde(skip)]
    pending: [VecDeque<Pending>; 2],
}

pub(super) struct CostFill {
    pub now: u64,
    pub leg: usize,
    pub maker_leg: usize,
    pub phase: Phase,
    pub side: HedgeSide,
    pub base: Decimal,
    pub notional: Decimal,
    pub fee: Decimal,
    pub mid: Option<Decimal>,
    pub submit_touch: Option<Decimal>,
}

impl Costs {
    pub(super) fn new() -> Self {
        Self {
            markout_anchor: "simulated_fill_receipt",
            cycles: Vec::new(),
            whole_ledger_net_before_overhead: None,
            whole_ledger_net_after_overhead: None,
            whole_ledger_cash_reconciled: false,
            active: None,
            pending: std::array::from_fn(|_| VecDeque::new()),
        }
    }

    pub(super) fn begin(&mut self, now: u64, mids: [Option<Decimal>; 2]) {
        let id = self.cycles.len();
        self.cycles.push(CycleCost {
            cycle_id: id,
            start_ms: now,
            end_ms: None,
            completed: false,
            stages: [
                "entry_maker",
                "entry_hedge",
                "close_maker",
                "close_hedge",
                "forced_close",
            ]
            .map(|phase| StageCost {
                phase,
                ..Default::default()
            }),
            entry_markouts: HORIZONS.map(|horizon_ms| EntryMarkout {
                horizon_ms,
                ..Default::default()
            }),
            target_turnover: Decimal::ZERO,
            gross_turnover: Decimal::ZERO,
            fees_quote: Decimal::ZERO,
            overhead_quote: None,
            net_before_overhead: None,
            net_after_overhead: None,
            price_loss_quote: None,
            price_drift_cost_quote: None,
            opening_hedge_slippage_vs_submit_touch: Decimal::ZERO,
            missing_mid_fills: 0,
            cash_reconciled: false,
            price_decomposition_reconciled: false,
            reference_mids: mids,
            positions: [Decimal::ZERO; 2],
            cash: Decimal::ZERO,
            drift: Decimal::ZERO,
        });
        self.active = Some(id);
    }

    pub(super) fn fill(&mut self, fill: CostFill) {
        let id = self.active.expect("diagnosed fill has an active cycle");
        let row = &mut self.cycles[id];
        let sign = if fill.side == HedgeSide::Buy {
            Decimal::ONE
        } else {
            -Decimal::ONE
        };
        let signed = sign * fill.base;
        row.positions[fill.leg] += signed;
        row.cash -= sign * fill.notional + fill.fee;
        row.gross_turnover += fill.notional;
        row.fees_quote += fill.fee;
        if fill.leg == fill.maker_leg {
            row.target_turnover += fill.notional;
        }
        let stage = &mut row.stages[fill.phase as usize];
        stage.fills += 1;
        stage.turnover += fill.notional;
        stage.fees_quote += fill.fee;
        if let Some((mid, reference)) = fill.mid.zip(row.reference_mids[fill.leg]) {
            stage.execution_cost_quote += sign * fill.notional - signed * mid;
            row.drift += signed * (mid - reference);
        } else {
            row.missing_mid_fills += 1;
        }
        if matches!(fill.phase, Phase::EntryHedge) {
            row.opening_hedge_slippage_vs_submit_touch +=
                sign * fill.notional - signed * fill.submit_touch.expect("IOC submission touch");
        }
        if matches!(fill.phase, Phase::EntryMaker) {
            for (index, horizon) in HORIZONS.into_iter().enumerate() {
                if let Some(mid) = fill.mid {
                    self.pending[index].push_back(Pending {
                        cycle: id,
                        due: fill.now + horizon,
                        signed_base: signed,
                        notional: fill.notional,
                        mid,
                        price: fill.notional / fill.base,
                    });
                } else {
                    row.entry_markouts[index].missing_entry_mid += 1;
                }
            }
        }
    }

    pub(super) fn book(&mut self, now: u64, book: Option<(u64, Decimal)>) {
        for (index, queue) in self.pending.iter_mut().enumerate() {
            while let Some(mark) = queue.front() {
                let expired = now > mark.due + LABEL_GRACE_MS;
                let eligible = book.is_some_and(|(source, _)| source >= mark.due && source <= now);
                if !expired && !eligible {
                    break;
                }
                let mark = queue.pop_front().expect("pending label");
                let label = &mut self.cycles[mark.cycle].entry_markouts[index];
                if expired {
                    label.expired_fills += 1;
                } else {
                    let mid = book.expect("eligible book").1;
                    label.marked_fills += 1;
                    label.marked_turnover += mark.notional;
                    label.mid_move_cost_quote += mark.signed_base * (mark.mid - mid);
                    label.markout_quote += mark.signed_base * (mid - mark.price);
                }
            }
        }
    }

    pub(super) fn close(&mut self, now: u64, pnl: Decimal, overhead: Decimal) {
        let id = self.active.take().expect("diagnosed cycle");
        let row = &mut self.cycles[id];
        row.end_ms = Some(now);
        row.completed = true;
        row.cash_reconciled = row.positions == [Decimal::ZERO; 2] && row.cash == pnl;
        row.net_before_overhead = Some(pnl);
        row.net_after_overhead = Some(pnl - overhead);
        row.overhead_quote = Some(overhead);
        row.price_loss_quote = Some(-pnl - row.fees_quote);
        if row.missing_mid_fills == 0 {
            row.price_drift_cost_quote = Some(row.drift);
            let execution: Decimal = row.stages.iter().map(|s| s.execution_cost_quote).sum();
            row.price_decomposition_reconciled =
                row.cash_reconciled && execution + row.drift == -pnl - row.fees_quote;
        }
    }

    pub(super) fn finish(&mut self, now: u64) {
        self.book(now, None);
        for (index, pending) in self.pending.iter().enumerate() {
            for mark in pending {
                self.cycles[mark.cycle].entry_markouts[index].pending_at_cutoff += 1;
            }
        }
    }

    pub(super) fn reconcile_ledger(&mut self, net: Option<Decimal>, after: Option<Decimal>) {
        self.whole_ledger_net_before_overhead = net;
        self.whole_ledger_net_after_overhead = after;
        self.whole_ledger_cash_reconciled = self.active.is_none()
            && self.cycles.iter().all(|c| {
                c.completed
                    && c.cash_reconciled
                    && c.net_before_overhead.is_some()
                    && c.net_after_overhead.is_some()
            })
            && net.is_some_and(|net| {
                self.cycles
                    .iter()
                    .map(|c| c.net_before_overhead.unwrap_or_default())
                    .sum::<Decimal>()
                    == net
            })
            && after.is_some_and(|after| {
                self.cycles
                    .iter()
                    .map(|c| c.net_after_overhead.unwrap_or_default())
                    .sum::<Decimal>()
                    == after
            });
    }
}

#[cfg(test)]
mod tests;

//! Read-only inventory accounting for simulated public-tape fills. Actual e8
//! ledger quantities, not order intent, determine opening/closing/zero crossing.
//! No venue identifiers or raw fill records are serialized. Complete episodes
//! and FIFO cohort pairs are analytical allocations, not execution decisions.
use super::{MARKOUT_HORIZONS_MS, MARKOUT_MS, now_ms};
use serde::Serialize;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    static CONTEXT: Cell<Context> = const { Cell::new(Context::empty()) };
}

#[derive(Clone, Copy)]
pub(super) struct Context {
    pub pending: bool,
    pub reason: Option<&'static str>,
    pub toward_flat_at_decision: Option<bool>,
    pub source_ms: Option<u64>,
}
impl Context {
    const fn empty() -> Self {
        Self {
            pending: false,
            reason: None,
            toward_flat_at_decision: None,
            source_ms: None,
        }
    }
}
pub(super) struct FillScope(Context);
impl FillScope {
    pub(super) fn open(c: Context) -> Self {
        Self(CONTEXT.replace(c))
    }
}
impl Drop for FillScope {
    fn drop(&mut self) {
        CONTEXT.set(self.0);
    }
}

pub(super) fn reset(enabled: bool) {
    CONTEXT.set(Context::empty());
    STATE.replace(enabled.then(State::default));
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Debug)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Open,
    Add,
    Reduce,
    Close,
    CrossZero,
}
#[derive(PartialEq, Serialize)]
struct Key {
    taker: bool,
    reduce_only: bool,
    pending_at_receipt: bool,
    first_cancel_reason: Option<&'static str>,
    toward_flat_at_decision: Option<bool>,
    actual_kind: Kind,
}
#[derive(Default, Serialize)]
struct Mark {
    horizon_ms: u64,
    marked_fills: u64,
    marked_turnover: f64,
    markout_quote: f64,
    opening_markout_quote: f64,
    closing_markout_quote: f64,
    expired_fills: u64,
    pending_at_cutoff: u64,
}
#[derive(Serialize)]
struct Cohort {
    key: Key,
    fills: u64,
    turnover: f64,
    opening_turnover: f64,
    closing_turnover: f64,
    fees: f64,
    receipt_minus_source_ms: i64,
    source_clock_observations: u64,
    markouts: [Mark; 3],
}
#[derive(Default, Serialize)]
struct Episode {
    index: usize,
    side: i8,
    started_ms: u64,
    ended_ms: Option<u64>,
    opened_by_cross: bool,
    closed_by_cross: bool,
    opening_quantity: f64,
    closing_quantity: f64,
    peak_quantity: f64,
    quantity_ms: f64,
    maker_open_turnover: f64,
    maker_close_turnover: f64,
    taker_open_turnover: f64,
    taker_close_turnover: f64,
    gross_cash: f64,
    fees: f64,
    fill_edge_vs_receipt_book: f64,
    inventory_drift_on_receipt_books: f64,
    missing_book_publications_while_open: u64,
    maker_pending_markout_1s: f64,
    maker_clean_markout_1s: f64,
    taker_markout_1s: f64,
}
#[derive(Default, Serialize)]
struct PairBridge {
    horizon_ms: u64,
    labelled_quantity: f64,
    opening_markout: f64,
    closing_markout: f64,
    between_post_fill_references: f64,
    unlabelled_pair_quantity: f64,
    unlabelled_pair_gross: f64,
}
#[derive(Serialize)]
struct PairCohort {
    episode: usize,
    opening_cohort: usize,
    closing_cohort: usize,
    matched_chunks: u64,
    quantity: f64,
    quantity_ms: f64,
    gross_pnl: f64,
    allocated_fees: f64,
    markout_bridges: [PairBridge; 3],
}
#[derive(Serialize)]
pub(super) struct Report {
    scope: &'static str,
    position_mismatches: u64,
    final_position_e8: i64,
    fill_cash_less_fees: f64,
    fees: f64,
    fill_edge_vs_receipt_book: f64,
    inventory_drift_on_receipt_books: f64,
    missing_reference_fills: u64,
    fifo_closed_gross: f64,
    fifo_open_signed_cost: f64,
    fifo_cash_residual: f64,
    receipt_book_equity_residual: Option<f64>,
    cohorts: Vec<Cohort>,
    episodes: Vec<Episode>,
    fifo_pairs: Vec<PairCohort>,
}
#[derive(Clone, Copy, Default)]
enum Label {
    #[default]
    Pending,
    Expired,
    Mid(f64),
}
struct Fill {
    at: u64,
    signed_e8: i64,
    opened_e8: i64,
    price: f64,
    fee: f64,
    cohort: usize,
    pieces: Vec<(usize, i64)>,
    labels: [Label; 3],
}
struct Lot {
    fill: usize,
    remaining_e8: i64,
    episode: usize,
}
struct Pair {
    open: usize,
    close: usize,
    quantity_e8: i64,
    episode: usize,
}
#[derive(Default)]
struct State {
    position: i64,
    position_mismatches: u64,
    last_at: Option<u64>,
    last_mid: Option<f64>,
    cash: f64,
    fees: f64,
    edge: f64,
    drift: f64,
    missing_reference_fills: u64,
    fills: Vec<Fill>,
    pending: [VecDeque<usize>; 3],
    cohorts: Vec<Cohort>,
    episodes: Vec<Episode>,
    active_episode: Option<usize>,
    lots: VecDeque<Lot>,
    pairs: Vec<Pair>,
}
fn units(q: i64) -> f64 {
    q as f64 / 1e8
}
impl State {
    fn time(&mut self, at: u64) {
        if let (Some(last), Some(i)) = (self.last_at, self.active_episode) {
            self.episodes[i].quantity_ms +=
                units(self.position.abs()) * at.saturating_sub(last) as f64;
        }
        self.last_at = Some(at);
    }
    fn book(&mut self, at: u64, source: u64, mid: Option<f64>) {
        self.time(at);
        let Some(mid) = mid else {
            if let Some(i) = self.active_episode {
                self.episodes[i].missing_book_publications_while_open += 1;
            }
            return;
        };
        if let Some(old) = self.last_mid {
            let drift = units(self.position) * (mid - old);
            self.drift += drift;
            if let Some(i) = self.active_episode {
                self.episodes[i].inventory_drift_on_receipt_books += drift;
            }
        }
        self.last_mid = Some(mid);
        for (h, horizon) in MARKOUT_HORIZONS_MS.into_iter().enumerate() {
            while let Some(&index) = self.pending[h].front() {
                let fill = &self.fills[index];
                let due = fill.at + horizon;
                let expired = at > due + MARKOUT_MS;
                let eligible = source >= due && source <= at;
                if !expired && !eligible {
                    break;
                }
                self.pending[h].pop_front();
                let f = &mut self.fills[index];
                let m = &mut self.cohorts[f.cohort].markouts[h];
                if eligible && !expired {
                    f.labels[h] = Label::Mid(mid);
                    m.marked_fills += 1;
                    m.marked_turnover += units(f.signed_e8.abs()) * f.price;
                    let value = units(f.signed_e8) * (mid - f.price);
                    m.markout_quote += value;
                    let opening = units(f.opened_e8 * f.signed_e8.signum()) * (mid - f.price);
                    m.opening_markout_quote += opening;
                    m.closing_markout_quote += value - opening;
                    if h == 0 {
                        for &(episode, q) in &f.pieces {
                            let v = units(q) * (mid - f.price);
                            let e = &mut self.episodes[episode];
                            if self.cohorts[f.cohort].key.taker {
                                e.taker_markout_1s += v;
                            } else if self.cohorts[f.cohort].key.pending_at_receipt {
                                e.maker_pending_markout_1s += v;
                            } else {
                                e.maker_clean_markout_1s += v;
                            }
                        }
                    }
                } else {
                    f.labels[h] = Label::Expired;
                    m.expired_fills += 1;
                }
            }
        }
    }
    #[allow(clippy::too_many_arguments)]
    fn fill(
        &mut self,
        at: u64,
        before: i64,
        signed: i64,
        price: f64,
        fee: f64,
        taker: bool,
        reduce_only: bool,
        c: Context,
    ) {
        self.time(at);
        self.position_mismatches += u64::from(self.position != before);
        let quantity = signed.abs();
        let closing = if before.signum() == -signed.signum() {
            quantity.min(before.abs())
        } else {
            0
        };
        let opening = quantity - closing;
        let kind = if before == 0 {
            Kind::Open
        } else if closing == 0 {
            Kind::Add
        } else if opening > 0 {
            Kind::CrossZero
        } else if closing == before.abs() {
            Kind::Close
        } else {
            Kind::Reduce
        };
        let key = Key {
            taker,
            reduce_only,
            pending_at_receipt: c.pending,
            first_cancel_reason: c.reason,
            toward_flat_at_decision: c.toward_flat_at_decision,
            actual_kind: kind,
        };
        let cohort = self
            .cohorts
            .iter()
            .position(|x| x.key == key)
            .unwrap_or_else(|| {
                self.cohorts.push(Cohort {
                    key,
                    fills: 0,
                    turnover: 0.0,
                    opening_turnover: 0.0,
                    closing_turnover: 0.0,
                    fees: 0.0,
                    receipt_minus_source_ms: 0,
                    source_clock_observations: 0,
                    markouts: MARKOUT_HORIZONS_MS.map(|horizon_ms| Mark {
                        horizon_ms,
                        ..Default::default()
                    }),
                });
                self.cohorts.len() - 1
            });
        let group = &mut self.cohorts[cohort];
        group.fills += 1;
        group.turnover += units(quantity) * price;
        group.opening_turnover += units(opening) * price;
        group.closing_turnover += units(closing) * price;
        group.fees += fee;
        if let Some(source) = c.source_ms {
            group.receipt_minus_source_ms += at as i64 - source as i64;
            group.source_clock_observations += 1;
        }
        let index = self.fills.len();
        let mut pieces = Vec::with_capacity(2);
        if closing > 0 {
            let episode = self.active_episode.expect("nonflat diagnostic episode");
            self.leg(
                episode,
                signed.signum() * closing,
                price,
                fee * closing as f64 / quantity as f64,
                taker,
                false,
            );
            pieces.push((episode, signed.signum() * closing));
            let mut left = closing;
            while left > 0 {
                let lot = self.lots.front_mut().expect("diagnostic FIFO coverage");
                let q = left.min(lot.remaining_e8);
                self.pairs.push(Pair {
                    open: lot.fill,
                    close: index,
                    quantity_e8: q,
                    episode: lot.episode,
                });
                lot.remaining_e8 -= q;
                left -= q;
                if lot.remaining_e8 == 0 {
                    self.lots.pop_front();
                }
            }
            if closing == before.abs() {
                let e = &mut self.episodes[episode];
                e.ended_ms = Some(at);
                e.closed_by_cross = opening > 0;
                self.active_episode = None;
            }
        }
        if opening > 0 {
            let episode = if let Some(i) = self.active_episode {
                i
            } else {
                let i = self.episodes.len();
                self.episodes.push(Episode {
                    index: i,
                    side: signed.signum() as i8,
                    started_ms: at,
                    opened_by_cross: closing > 0,
                    ..Default::default()
                });
                self.active_episode = Some(i);
                i
            };
            self.leg(
                episode,
                signed.signum() * opening,
                price,
                fee * opening as f64 / quantity as f64,
                taker,
                true,
            );
            pieces.push((episode, signed.signum() * opening));
            self.lots.push_back(Lot {
                fill: index,
                remaining_e8: opening,
                episode,
            });
        }
        self.position = before + signed;
        if let Some(i) = self.active_episode {
            self.episodes[i].peak_quantity = self.episodes[i]
                .peak_quantity
                .max(units(self.position.abs()));
        }
        self.cash -= units(signed) * price + fee;
        self.fees += fee;
        if let Some(mid) = self.last_mid {
            self.edge += units(signed) * (mid - price);
        } else {
            self.missing_reference_fills += 1;
        }
        self.fills.push(Fill {
            at,
            signed_e8: signed,
            opened_e8: opening,
            price,
            fee,
            cohort,
            pieces,
            labels: [Label::Pending; 3],
        });
        for queue in &mut self.pending {
            queue.push_back(index);
        }
    }
    fn leg(&mut self, i: usize, signed: i64, price: f64, fee: f64, taker: bool, opening: bool) {
        let q = units(signed.abs());
        let e = &mut self.episodes[i];
        if opening {
            e.opening_quantity += q;
        } else {
            e.closing_quantity += q;
        }
        match (taker, opening) {
            (false, true) => e.maker_open_turnover += q * price,
            (false, false) => e.maker_close_turnover += q * price,
            (true, true) => e.taker_open_turnover += q * price,
            (true, false) => e.taker_close_turnover += q * price,
        }
        e.gross_cash -= units(signed) * price;
        e.fees += fee;
        if let Some(mid) = self.last_mid {
            e.fill_edge_vs_receipt_book += units(signed) * (mid - price);
        }
    }
    fn finish(mut self, at: u64) -> Report {
        self.time(at);
        for (h, queue) in self.pending.iter().enumerate() {
            for &i in queue {
                self.cohorts[self.fills[i].cohort].markouts[h].pending_at_cutoff += 1;
            }
        }
        let mut pairs: Vec<PairCohort> = Vec::new();
        for p in self.pairs {
            let a = &self.fills[p.open];
            let b = &self.fills[p.close];
            let q = units(p.quantity_e8);
            let sign = a.signed_e8.signum() as f64;
            let gross = sign * q * (b.price - a.price);
            let idx = pairs
                .iter()
                .position(|c| {
                    c.episode == p.episode
                        && c.opening_cohort == a.cohort
                        && c.closing_cohort == b.cohort
                })
                .unwrap_or_else(|| {
                    pairs.push(PairCohort {
                        episode: p.episode,
                        opening_cohort: a.cohort,
                        closing_cohort: b.cohort,
                        matched_chunks: 0,
                        quantity: 0.0,
                        quantity_ms: 0.0,
                        gross_pnl: 0.0,
                        allocated_fees: 0.0,
                        markout_bridges: MARKOUT_HORIZONS_MS.map(|horizon_ms| PairBridge {
                            horizon_ms,
                            ..Default::default()
                        }),
                    });
                    pairs.len() - 1
                });
            let c = &mut pairs[idx];
            c.matched_chunks += 1;
            c.quantity += q;
            c.quantity_ms += q * b.at.saturating_sub(a.at) as f64;
            c.gross_pnl += gross;
            c.allocated_fees += a.fee * p.quantity_e8 as f64 / a.signed_e8.abs() as f64
                + b.fee * p.quantity_e8 as f64 / b.signed_e8.abs() as f64;
            for (h, bridge) in c.markout_bridges.iter_mut().enumerate() {
                if let (Label::Mid(ma), Label::Mid(mb)) = (a.labels[h], b.labels[h]) {
                    bridge.labelled_quantity += q;
                    bridge.opening_markout += sign * q * (ma - a.price);
                    bridge.closing_markout -= sign * q * (mb - b.price);
                    bridge.between_post_fill_references += sign * q * (mb - ma);
                } else {
                    bridge.unlabelled_pair_quantity += q;
                    bridge.unlabelled_pair_gross += gross;
                }
            }
        }
        let gross = pairs.iter().map(|c| c.gross_pnl).sum::<f64>();
        let open_cost = self
            .lots
            .iter()
            .map(|l| {
                let f = &self.fills[l.fill];
                units(l.remaining_e8 * f.signed_e8.signum()) * f.price
            })
            .sum::<f64>();
        Report {
            scope: "public_tape_simulated_inventory; receipt-book identity is accounting, not an executable-price claim",
            position_mismatches: self.position_mismatches,
            final_position_e8: self.position,
            fill_cash_less_fees: self.cash,
            fees: self.fees,
            fill_edge_vs_receipt_book: self.edge,
            inventory_drift_on_receipt_books: self.drift,
            missing_reference_fills: self.missing_reference_fills,
            fifo_closed_gross: gross,
            fifo_open_signed_cost: open_cost,
            fifo_cash_residual: self.cash - (gross - open_cost - self.fees),
            receipt_book_equity_residual: self
                .last_mid
                .filter(|_| self.missing_reference_fills == 0)
                .map(|m| {
                    self.cash + units(self.position) * m - (self.edge + self.drift - self.fees)
                }),
            cohorts: self.cohorts,
            episodes: self.episodes,
            fifo_pairs: pairs,
        }
    }
}
pub(super) fn book(source: u64, mid: Option<f64>) {
    STATE.with_borrow_mut(|s| {
        if let Some(s) = s {
            s.book(now_ms(), source, mid);
        }
    });
}
#[allow(clippy::too_many_arguments)]
pub(in crate::strategy::inventory_neutral_mm::replay) fn fill(
    before: i64,
    signed: i64,
    price: f64,
    fee: f64,
    taker: bool,
    reduce_only: bool,
) {
    STATE.with_borrow_mut(|s| {
        if let Some(s) = s {
            s.fill(
                now_ms(),
                before,
                signed,
                price,
                fee,
                taker,
                reduce_only,
                CONTEXT.get(),
            );
        }
    });
}
pub(super) fn take() -> Option<Report> {
    STATE.take().map(|s| s.finish(now_ms()))
}

#[cfg(test)]
mod tests {
    use super::*;
    const Q: i64 = 100_000_000;
    fn ctx(pending: bool) -> Context {
        Context {
            pending,
            reason: pending.then_some("bound_breach"),
            toward_flat_at_decision: Some(true),
            source_ms: None,
        }
    }
    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }
    #[test]
    fn inventory_cycles_split_partial_cross_and_short_cover_with_fees() {
        let mut s = State::default();
        s.book(0, 0, Some(100.0));
        s.fill(10, 0, 2 * Q, 100.0, 0.2, false, false, ctx(false));
        s.book(500, 500, Some(102.0));
        s.fill(1000, 2 * Q, -Q, 103.0, 0.1, false, false, ctx(true));
        s.fill(2000, Q, -2 * Q, 99.0, 0.2, false, false, ctx(true));
        s.book(3000, 3000, Some(98.0));
        s.fill(4000, -Q, Q, 98.0, 0.1, true, true, Context::empty());
        s.book(35000, 35000, Some(97.0));
        let r = s.finish(36000);
        assert_eq!(r.final_position_e8, 0);
        assert_eq!(r.position_mismatches, 0);
        close(r.fill_cash_less_fees, 2.4);
        close(r.fifo_closed_gross, 3.0);
        close(r.fifo_cash_residual, 0.0);
        close(r.receipt_book_equity_residual.unwrap(), 0.0);
        assert_eq!(r.episodes.len(), 2);
        assert!(r.episodes[0].closed_by_cross && r.episodes[1].opened_by_cross);
        close(r.episodes[0].quantity_ms, 2980.0);
        close(r.episodes[1].quantity_ms, 2000.0);
        let cross = r
            .cohorts
            .iter()
            .find(|c| c.key.actual_kind == Kind::CrossZero)
            .unwrap();
        close(cross.opening_turnover, 99.0);
        close(cross.closing_turnover, 99.0);
        for e in &r.episodes {
            close(
                e.gross_cash - e.fees,
                e.fill_edge_vs_receipt_book + e.inventory_drift_on_receipt_books - e.fees,
            );
        }
        for pair in r.fifo_pairs {
            for b in pair.markout_bridges {
                close(
                    pair.gross_pnl,
                    b.opening_markout
                        + b.closing_markout
                        + b.between_post_fill_references
                        + b.unlabelled_pair_gross,
                );
            }
        }
    }
    #[test]
    fn inventory_cycles_better_pending_markout_can_coexist_with_worse_cash() {
        let run = |exit: f64, exit_mid: f64| {
            let mut s = State::default();
            s.book(0, 0, Some(100.0));
            s.fill(0, 0, Q, 100.0, 0.0, false, false, ctx(false));
            s.book(1000, 1000, Some(101.0));
            s.book(2000, 2000, Some(exit));
            s.fill(2000, Q, -Q, exit, 0.0, false, false, ctx(true));
            s.book(3000, 3000, Some(exit_mid));
            s.finish(4000)
        };
        let a = run(101.0, 102.0);
        let b = run(99.0, 98.0);
        close(a.fill_cash_less_fees, 1.0);
        close(b.fill_cash_less_fees, -1.0);
        close(a.episodes[0].maker_pending_markout_1s, -1.0);
        close(b.episodes[0].maker_pending_markout_1s, 1.0);
        for r in [a, b] {
            let p = &r.fifo_pairs[0];
            let x = &p.markout_bridges[0];
            close(
                p.gross_pnl,
                x.opening_markout + x.closing_markout + x.between_post_fill_references,
            );
        }
    }
    #[test]
    fn inventory_cycles_keep_open_lots_and_unknown_labels_unknown() {
        let mut s = State::default();
        s.book(0, 0, Some(100.0));
        s.fill(0, 0, 2 * Q, 100.0, -0.2, false, false, ctx(false));
        s.fill(100, 2 * Q, -Q, 101.0, 0.1, true, true, Context::empty());
        s.book(3000, 3000, Some(102.0)); // 1s labels expired; no backward fill.
        let r = s.finish(3100);
        assert_eq!(r.final_position_e8, Q);
        close(r.fifo_closed_gross, 1.0);
        close(r.fifo_open_signed_cost, 100.0);
        close(r.fifo_cash_residual, 0.0);
        close(r.receipt_book_equity_residual.unwrap(), 0.0);
        assert!(r.episodes[0].ended_ms.is_none());
        assert_eq!(r.cohorts[0].markouts[0].expired_fills, 1);
        assert_eq!(r.cohorts[0].markouts[2].pending_at_cutoff, 1);
        close(
            r.fifo_pairs[0].markout_bridges[0].unlabelled_pair_gross,
            1.0,
        );
    }
}

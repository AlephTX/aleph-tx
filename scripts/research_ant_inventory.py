"""Offline continuous-inventory primitives. No venue client or live authority.

Public queue allocation is a conditional replay assumption, never private fill
or terminal evidence. Unknown results preserve remaining order exposure. One
wallet persists across quote cycles; only final flat cash is a terminal PnL.
"""

from collections import Counter
from dataclasses import dataclass
from decimal import Decimal, ROUND_DOWN, ROUND_UP

D = Decimal
ZERO = D(0)
HOUR_NS = 3_600_000_000_000


def number(value):
    if isinstance(value, (float, bool)):
        raise ValueError("use exact decimal input")
    result = D(value)
    if not result.is_finite():
        raise ValueError("finite decimal required")
    return result


@dataclass(frozen=True)
class Policy:
    initial_each: Decimal = D(1000)
    venue_cap: Decimal = D(500)
    net_cap: Decimal = D(500)
    hedge_band: Decimal = D(100)
    loss_limit: Decimal = D(10)
    lot: Decimal = D(".01")
    minimum: Decimal = D(10)
    arcus_taker_fee: Decimal = D(".000112")
    xyz_taker_fee: Decimal = D(".00009")


@dataclass
class Pending:
    venue: str
    buy: bool
    quantity: Decimal
    remaining: Decimal
    limit: Decimal
    fee: Decimal
    role: str
    unknown: bool = False


def sweep(levels, quantity, buy, limit=None):
    """Return known partial quantity/notional within a fixed protection price."""
    left = number(quantity)
    if left < 0:
        raise ValueError("negative quantity")
    cap = None if limit is None else number(limit)
    filled = cash = ZERO
    for price, size in levels:
        price, size = number(price), number(size)
        if price <= 0 or size < 0:
            raise ValueError("invalid book level")
        if cap is not None and (price > cap if buy else price < cap):
            continue
        take = min(left, size)
        filled += take
        cash += take * price
        left -= take
        if left == 0:
            break
    return filled, cash


def protection(levels, quantity):
    """Worst decision-time price needed; fail if displayed size is insufficient."""
    left = number(quantity)
    if left <= 0:
        return None
    for price, size in levels:
        left -= number(size)
        if left <= 0:
            return number(price)
    return None


def displayed_ahead(book, buy, price):
    """Validated complete L2 prefix: absent prices *within* its span have zero
    displayed queue. Outside that prefix, absence cannot prove zero queue.
    Hidden liquidity and endogenous book changes remain outside this model.
    """
    if book is None:
        return None, "arrival_book_missing"
    price = number(price)
    bid, ask = book[1][0][0], book[2][0][0]
    if (buy and price >= ask) or (not buy and price <= bid):
        return ZERO, "post_only_reject"
    ahead = dict(book[1 if buy else 2]).get(price)
    if ahead is not None:
        return ahead, "visible_level"
    if bid < price < ask:
        return ZERO, "inside_spread"
    levels = book[1 if buy else 2]
    if min(p for p, _ in levels) < price < max(p for p, _ in levels):
        return ZERO, "visible_price_gap"
    return None, "arrival_level_unobserved"


def queue_allocation(events, start, stop, buy, price, quantity, ahead,
                     uninterrupted=True):
    """events=(source_ns,receipt_ns,side,price,qty,acknowledged,conflict).

    Receipt-delayed relevant prints cannot become timely fills. A bad suffix
    preserves the modeled known prefix quantity and withholds terminal evidence.
    Source time selects the active interval; receipt time governs knowledge.
    """
    flow = ZERO
    reasons = set() if uninterrupted else {"subscription_gap"}
    for source, receipt, side, match_price, size, ack, conflict in events:
        if not start < source < stop or side != ("SELL" if buy else "BUY"):
            continue
        if not (match_price <= price if buy else match_price >= price):
            continue
        if conflict:
            reasons.add("conflicting_public_print")
        elif not ack or not 0 <= receipt-source <= 2_000_000_000 or receipt > stop:
            reasons.add("late_or_unconfirmed_print")
        else:
            flow += number(size)
    known = min(number(quantity), max(ZERO, flow-number(ahead)))
    # Order granularity is not fill granularity. Do not erase partial-fill dust.
    return known, sorted(reasons)


class InventoryWallet:
    def __init__(self, policy=Policy()):
        self.policy = policy
        self.position = {v: ZERO for v in ("arcus", "xyz")}
        self.cash = {v: policy.initial_each for v in self.position}
        self.volume = dict.fromkeys(self.position, ZERO)
        self.fees = dict.fromkeys(self.position, ZERO)
        self.pending = {}
        self.used_ids = set()
        self.executions = {}
        self.withheld = set()
        self.stop_new = False
        self.counts = Counter()
        self.role_volume = Counter()
        self.clock = None
        self.marks = []

    def bounds(self, venue=None):
        position = self.position[venue] if venue else sum(self.position.values(), ZERO)
        low = high = position
        for order in self.pending.values():
            if venue is not None and venue != order.venue:
                continue
            if order.buy:
                high += order.remaining
            else:
                low -= order.remaining
        return low, high

    def capacity(self, venue, buy, reference):
        reference = number(reference)
        if reference <= 0:
            raise ValueError("positive reference required")
        low, high = self.bounds(venue)
        net_low, net_high = self.bounds()
        cap = min(self.policy.venue_cap/reference-(high if buy else -low),
                  self.policy.net_cap/reference-(net_high if buy else -net_low))
        return max(ZERO, (cap/self.policy.lot).to_integral_value(rounding=ROUND_DOWN)*self.policy.lot)

    def reserve(self, identifier, venue, buy, quantity, limit, fee, role, reference,
                reduce_only=False):
        quantity, limit, fee = map(number, (quantity, limit, fee))
        if identifier in self.used_ids:
            raise ValueError("order identity reused")
        if venue not in self.position or quantity <= 0 or limit <= 0:
            raise ValueError("invalid order")
        if quantity % self.policy.lot or quantity*limit < self.policy.minimum:
            return False
        if self.withheld or (self.stop_new and not reduce_only):
            return False
        if reduce_only:
            position = self.position[venue]
            if (not position or buy == (position > 0) or quantity > abs(position)
                    or any(o.venue == venue for o in self.pending.values())):
                return False
        if quantity > self.capacity(venue, buy, max(number(reference), limit)):
            return False
        self.used_ids.add(identifier)
        self.pending[identifier] = Pending(venue, buy, quantity, quantity, limit, fee, role)
        self.counts["reserved_"+role] += 1
        return True

    def fill(self, identifier, execution_id, quantity, notional):
        """Apply one known simulated execution; a duplicate cannot exceed reserve."""
        quantity, notional = map(number, (quantity, notional))
        evidence = (identifier, quantity, notional)
        if execution_id in self.executions:
            if self.executions[execution_id] == evidence:
                return False
            self.withheld.add("conflicting_execution_identity")
            self.stop_new = True
            raise ValueError("execution identity changed")
        order = self.pending[identifier]
        if order.unknown or quantity <= 0 or quantity > order.remaining:
            raise ValueError("unknown, duplicate or oversized fill")
        if notional <= 0 or (notional > quantity*order.limit if order.buy else notional < quantity*order.limit):
            raise ValueError("fill violates fixed protection price")
        fee = notional*order.fee
        self.cash[order.venue] += (-notional if order.buy else notional)-fee
        self.position[order.venue] += quantity if order.buy else -quantity
        self.volume[order.venue] += notional
        self.fees[order.venue] += fee
        self.role_volume[order.role] += notional
        order.remaining -= quantity
        self.executions[execution_id] = evidence
        self.counts["filled_"+order.role] += 1
        return True

    def modeled_terminal(self, identifier):
        """A replay oracle callback; NOT permission to clear any live reserve."""
        if self.pending[identifier].unknown:
            raise ValueError("unknown order cannot be cleared by replay timeout")
        del self.pending[identifier]

    def unknown(self, identifier, reason):
        self.pending[identifier].unknown = True
        self.withheld.add(reason)
        self.stop_new = True

    def advance(self, now):
        if self.clock is not None:
            if now < self.clock:
                raise ValueError("clock regression")
            if now//HOUR_NS > self.clock//HOUR_NS and (self.pending or any(self.position.values())):
                self.withheld.add("hourly_funding_payment_unavailable")
                self.stop_new = True
        self.clock = now

    def mark(self, books):
        """Estimated current full-depth liquidation, never unknown-as-zero."""
        if self.pending or self.withheld:
            self.counts["mark_unknown"] += 1
            return None
        value = sum(self.cash.values(), ZERO)-2*self.policy.initial_each
        for venue, position in self.position.items():
            if not position:
                continue
            book = books.get(venue)
            if book is None:
                self.counts["mark_missing_book"] += 1
                return None
            buy = position < 0
            qty, notional = sweep(book[2 if buy else 1], abs(position), buy)
            if qty != abs(position):
                self.counts["mark_missing_depth"] += 1
                return None
            fee = self.policy.arcus_taker_fee if venue == "arcus" else self.policy.xyz_taker_fee
            value += (-notional if buy else notional)-notional*fee
        self.marks.append(value)
        if value <= -self.policy.loss_limit:
            self.stop_new = True
            self.counts["marked_loss_stop"] += 1
        return value

    def hedge_quantity(self, reference):
        reference = number(reference)
        if reference <= 0:
            raise ValueError("positive reference required")
        if self.pending or self.withheld or self.stop_new:
            return None
        net = sum(self.position.values(), ZERO)
        excess = abs(net)-self.policy.hedge_band/reference
        if excess <= 0:
            return None
        quantity = max(excess, self.policy.minimum/reference)
        quantity = (quantity/self.policy.lot).to_integral_value(rounding=ROUND_UP)*self.policy.lot
        return net < 0, min(quantity, abs(net))

    def result(self):
        flat = not self.pending and not any(self.position.values()) and not self.withheld
        return {"terminal_pnl_usd": sum(self.cash.values(), ZERO)-2*self.policy.initial_each if flat else None,
                "positions": dict(self.position), "position_bounds": {v:self.bounds(v) for v in self.position},
                "net_position_bounds": self.bounds(), "pending_orders": len(self.pending),
                "withheld_reasons": sorted(self.withheld), "stopped_new": self.stop_new,
                "known_turnover": dict(self.volume), "known_fees": dict(self.fees),
                "known_role_turnover": dict(self.role_volume), "counts": dict(self.counts),
                "observed_marks": len(self.marks), "lowest_observed_mark": min(self.marks) if self.marks else None,
                "continuous_inventory": True, "live_ready": False}


def replay_wallet(first, end, view, events, uninterrupted):
    """One frozen candidate on a receipt-causal public-book callback.

    view(t) -> (Arcus book, xyz book, status). Public prints were deduplicated by
    the sealed reader. No order/position reset between quote cycles. A market
    replay terminal is only a model assumption, never live terminal evidence.
    """
    second, delay, grid = 1_000_000_000, 310_000_000, 15_000_000_000
    if first % grid or end % grid or end <= first:
        raise ValueError("aligned nonempty replay required")
    w = InventoryWallet()
    w.advance(first)
    counters = Counter()
    stopped_at = None

    def books(t):
        arc, xyz, state = view(t)
        return {"arcus": arc, "xyz": xyz}, state

    def reference(mapping):
        return max(b[2][0][0] for b in mapping.values() if b is not None)

    def ioc(key, venue, buy, quantity, now, role, reduce_only=False):
        w.advance(now)
        decision, _ = books(now)
        book = decision[venue]
        if book is None:
            counters[role+"_decision_missing"] += 1
            w.stop_new = True
            return
        limit = protection(book[2 if buy else 1], quantity)
        if limit is None:
            counters[role+"_decision_depth_missing"] += 1
            w.stop_new = True
            return
        fee = w.policy.arcus_taker_fee if venue == "arcus" else w.policy.xyz_taker_fee
        if not w.reserve(key, venue, buy, quantity, limit, fee, role, reference(decision), reduce_only):
            counters[role+"_risk_lot_or_minimum_blocked"] += 1
            w.stop_new = True
            return
        w.advance(now+delay)
        arrival, _ = books(now+delay)
        if arrival[venue] is None:
            w.unknown(key, role+"_arrival_unknown")
            return
        qty, value = sweep(arrival[venue][2 if buy else 1], quantity, buy, limit)
        if qty:
            w.fill(key, key+"-fill", qty, value)
        if qty < quantity:
            counters[role+"_partial_or_no_fill"] += 1
        w.modeled_terminal(key)

    for t in range(first, end, grid):
        w.advance(t)
        decision, state = books(t)
        w.mark(decision)
        if w.stop_new:
            stopped_at = t
            break
        counters["quote_decisions"] += 1
        if any(b is None for b in decision.values()):
            counters["decision_"+state] += 1
            continue
        arc = decision["arcus"]
        ref = reference(decision)
        orders = []
        for buy in (True, False):
            price, visible = arc[1 if buy else 2][0]
            target = min(D(100)/ref, visible*D(".2"), w.capacity("arcus", buy, ref))
            target = (target/w.policy.lot).to_integral_value(rounding=ROUND_DOWN)*w.policy.lot
            if target <= 0:
                counters["quote_capacity_empty"] += 1
                continue
            key = str(t)+("-bid" if buy else "-ask")
            if w.reserve(key, "arcus", buy, target, price, ZERO, "maker", ref):
                orders.append((key, buy, price, target))
            else:
                counters["quote_below_minimum_or_risk"] += 1
        if not orders:
            continue
        start, stop = t+delay, t+3*second+delay
        w.advance(start)
        arrival, _ = books(start)
        candidates = []
        for key, buy, price, target in orders:
            ahead, reason = displayed_ahead(arrival["arcus"], buy, price)
            counters[reason] += 1
            if reason == "post_only_reject":
                w.modeled_terminal(key)
            elif ahead is None:
                w.unknown(key, reason)
            else:
                candidates.append((key, buy, price, target, ahead))
        # Both orders were dispatched. A bad side must not erase the known
        # executions on the other side, even though it prevents future quotes.
        w.advance(stop)
        for key, buy, price, target, ahead in candidates:
            filled, reasons = queue_allocation(events, start, stop, buy, price, target, ahead,
                                                uninterrupted(start, stop))
            if filled:
                w.fill(key, key+"-maker", filled, filled*price)
            if reasons:
                for reason in reasons:
                    w.unknown(key, reason)
            else:
                w.modeled_terminal(key)
        after, _ = books(stop)
        w.mark(after)
        if w.stop_new:
            stopped_at = stop
            break
        if any(b is None for b in after.values()):
            if any(w.position.values()):
                counters["hedge_decision_missing"] += 1
                w.stop_new = True
                stopped_at = stop
                break
            continue
        hedge = w.hedge_quantity(reference(after))
        if hedge:
            buy, quantity = hedge
            ioc(str(t)+"-hedge", "xyz", buy, quantity, stop, "hedge")
            after_hedge, _ = books(stop+delay)
            w.mark(after_hedge)
            if not w.pending and not w.withheld and any(b is not None for b in after_hedge.values()):
                if abs(sum(w.position.values(), ZERO))*reference(after_hedge) > w.policy.hedge_band:
                    counters["hedge_band_not_restored"] += 1
                    w.stop_new = True
            if w.stop_new:
                stopped_at = stop+delay
                break
    # A modeled stop never authorizes sending around an unresolved order.
    drain = max(w.clock, stopped_at if stopped_at is not None else end)
    for index, venue in enumerate(("arcus", "xyz")):
        if w.pending or w.withheld:
            break
        quantity = abs(w.position[venue])
        if quantity:
            ioc("drain-"+venue, venue, w.position[venue] < 0, quantity,
                drain+index*delay, "drain", True)
    out = w.result()
    out.update(dispositions=dict(counters), first_ns=first, end_ns=end,
               stopped_at_ns=stopped_at, last_action_ns=w.clock,
               assumptions="public visible-queue model, no market reaction; no private execution evidence")
    return out

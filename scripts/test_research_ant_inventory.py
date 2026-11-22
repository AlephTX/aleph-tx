import unittest
from decimal import Decimal as D

from research_ant_inventory import (
    HOUR_NS, InventoryWallet, Policy, displayed_ahead, protection, queue_allocation, replay_wallet, sweep,
)


def book(bid="99", ask="101", size="10"):
    return 0, [(D(bid), D(size))], [(D(ask), D(size))]


class ContinuousInventoryTests(unittest.TestCase):
    def reserve(self, w, key, buy, qty="1", price="100", venue="arcus", reduce=False):
        return w.reserve(key, venue, buy, qty, price, "0", "maker", "100", reduce)

    def fill(self, w, key, buy, qty="1", price="100", venue="arcus"):
        self.assertTrue(self.reserve(w, key, buy, qty, price, venue))
        w.fill(key, "fill-"+key, qty, D(qty)*D(price))
        w.modeled_terminal(key)

    def test_opposite_pending_sides_do_not_cancel_reserves(self):
        w = InventoryWallet()
        self.assertTrue(self.reserve(w, "buy", True, "4"))
        self.assertTrue(self.reserve(w, "sell", False, "4"))
        self.assertEqual(w.bounds(), (D(-4), D(4)))
        self.assertFalse(self.reserve(w, "extra", True, "2", venue="xyz"))
        self.assertEqual(len(w.pending), 2)

    def test_unknown_cancel_retains_remaining_exposure_and_known_partial(self):
        w = InventoryWallet()
        self.assertTrue(self.reserve(w, "q", True, "2"))
        w.fill("q", "known-part", "1", "100")
        w.unknown("q", "late_print")
        self.assertEqual(w.bounds("arcus"), (D(1), D(2)))
        self.assertEqual(w.volume["arcus"], 100)
        with self.assertRaises(ValueError):
            w.modeled_terminal("q")
        self.assertIsNone(w.result()["terminal_pnl_usd"])

    def test_continuous_inventory_naturally_offsets_without_forced_hedge(self):
        w = InventoryWallet()
        self.fill(w, "buy", True, price="99")
        self.assertIsNone(w.result()["terminal_pnl_usd"])
        self.assertIsNone(w.hedge_quantity("100"))
        self.fill(w, "sell", False, price="101")
        self.assertEqual(w.result()["terminal_pnl_usd"], D(2))
        self.assertEqual(w.volume["xyz"], 0)

    def test_bounded_netting_hedges_excess_and_handles_existing_hedge(self):
        w = InventoryWallet()
        self.fill(w, "maker", True, "2")
        self.assertEqual(w.hedge_quantity("100"), (False, D(1)))
        self.fill(w, "hedge", False, "1", venue="xyz")
        self.assertIsNone(w.hedge_quantity("100"))
        self.fill(w, "maker-sell", False, "2")
        self.assertEqual(w.position["arcus"], 0)
        self.assertEqual(w.position["xyz"], -1)
        self.assertIsNone(w.result()["terminal_pnl_usd"])

    def test_subminimum_excess_hedges_minimum_without_crossing_zero(self):
        w = InventoryWallet()
        self.fill(w, "maker", True, "1.01")
        self.assertEqual(w.hedge_quantity("100"), (False, D(".10")))

    def test_protected_ioc_keeps_partial_residual(self):
        w = InventoryWallet()
        self.fill(w, "entry", True, "2")
        self.assertTrue(w.reserve("exit", "arcus", False, "2", "99", ".000112", "drain", "100", True))
        quantity, value = sweep([(D(99), D(1)), (D(98), D(9))], D(2), False, D(99))
        self.assertEqual((quantity, value), (D(1), D(99)))
        w.fill("exit", "partial-exit", quantity, value)
        w.modeled_terminal("exit")
        self.assertEqual(w.position["arcus"], 1)
        self.assertIsNone(w.result()["terminal_pnl_usd"])
        self.assertEqual(w.fees["arcus"], D(".011088"))

    def test_reduce_only_cannot_cross_zero_or_double_reserve(self):
        w = InventoryWallet()
        self.fill(w, "entry", True)
        self.assertFalse(self.reserve(w, "bad", True, reduce=True))
        self.assertFalse(self.reserve(w, "too-large", False, "2", reduce=True))
        self.assertTrue(self.reserve(w, "first", False, reduce=True))
        self.assertFalse(self.reserve(w, "second", False, reduce=True))

    def test_fill_identity_dedup_and_conflict(self):
        w = InventoryWallet()
        self.assertTrue(self.reserve(w, "q", True, "2"))
        self.assertTrue(w.fill("q", "f", "1", "100"))
        self.assertFalse(w.fill("q", "f", "1", "100"))
        self.assertEqual(w.position["arcus"], 1)
        with self.assertRaises(ValueError):
            w.fill("q", "f", "1", "99")
        self.assertTrue(w.stop_new)
        self.assertIsNone(w.result()["terminal_pnl_usd"])

    def test_unknown_mark_and_loss_stop_allow_only_reduction(self):
        w = InventoryWallet()
        self.fill(w, "q", True)
        self.assertIsNone(w.mark({"arcus": None, "xyz": None}))
        value = w.mark({"arcus": book("89", "90"), "xyz": None})
        self.assertLess(value, D(-10))
        self.assertTrue(w.stop_new)
        self.assertFalse(self.reserve(w, "new", True))
        self.assertTrue(self.reserve(w, "reduce", False, reduce=True))

    def test_hourly_funding_not_filled_with_zero_even_when_net_flat(self):
        w = InventoryWallet()
        w.advance(HOUR_NS-1)
        self.fill(w, "arc", True)
        self.fill(w, "xyz", False, venue="xyz")
        self.assertEqual(sum(w.position.values()), 0)
        w.advance(HOUR_NS)
        self.assertIn("hourly_funding_payment_unavailable", w.withheld)
        self.assertIsNone(w.result()["terminal_pnl_usd"])
        with self.assertRaises(ValueError):
            w.advance(HOUR_NS-1)

    def test_flat_hourly_boundary_does_not_invent_funding(self):
        w = InventoryWallet()
        w.advance(HOUR_NS-1)
        w.advance(HOUR_NS)
        self.assertEqual(w.result()["terminal_pnl_usd"], 0)

    def test_queue_consumption_known_prefix_and_late_suffix(self):
        events = [(1, 2, "SELL", D(100), D(2), True, False),
                  (3, 11, "SELL", D(100), D(1), True, False)]
        known, reasons = queue_allocation(events, 0, 10, True, D(100), D(2), D(1))
        self.assertEqual(known, D(1))
        self.assertEqual(reasons, ["late_or_unconfirmed_print"])
        known, reasons = queue_allocation(events[:1], 0, 10, True, D(100), D(2), D(3))
        self.assertEqual(known, 0)
        self.assertFalse(reasons)

    def test_before_activation_wrong_side_and_after_cancel_are_not_fills(self):
        events = [(0, 2, "SELL", D(100), D(9), True, False),
                  (3, 4, "BUY", D(100), D(9), True, False),
                  (10, 11, "SELL", D(100), D(9), True, False)]
        self.assertEqual(queue_allocation(events, 0, 10, True, D(100), D(2), D(0)), (D(0), []))

    def test_inside_spread_visible_queue_and_missing_level(self):
        self.assertEqual(displayed_ahead(book(), True, D(100)), (D(0), "inside_spread"))
        self.assertEqual(displayed_ahead(book(), True, D(101))[1], "post_only_reject")
        self.assertEqual(displayed_ahead(book(), True, D(99))[0], D(10))
        self.assertIsNone(displayed_ahead(book(), True, D(98))[0])
        self.assertEqual(protection([(D(100), D(1)), (D(101), D(2))], D(2)), D(101))

    def test_final_dust_and_unknown_order_are_not_flat_profit(self):
        w = InventoryWallet()
        self.assertTrue(self.reserve(w, "q", True))
        w.fill("q", "dust", ".01", "1")
        w.modeled_terminal("q")
        self.assertFalse(self.reserve(w, "dust-close", False, ".01", reduce=True))
        self.assertIsNone(w.result()["terminal_pnl_usd"])

    def test_partial_fills_preserve_finer_than_order_lot_quantity(self):
        w = InventoryWallet()
        self.assertTrue(self.reserve(w, "q", True))
        w.fill("q", "small", ".005", ".5")
        self.assertEqual(w.position["arcus"], D(".005"))
        events = [(1, 2, "SELL", D(100), D("1.005"), True, False)]
        self.assertEqual(queue_allocation(events, 0, 10, True, D(100), D(1), D(1))[0], D(".005"))

    def test_sparse_complete_prefix_has_zero_displayed_queue_only_inside_span(self):
        sparse = (0, [(D(99), D(10)), (D(97), D(10))],
                  [(D(101), D(10)), (D(103), D(10))])
        self.assertEqual(displayed_ahead(sparse, True, D(98)), (D(0), "visible_price_gap"))
        self.assertEqual(displayed_ahead(sparse, False, D(102)), (D(0), "visible_price_gap"))
        self.assertIsNone(displayed_ahead(sparse, True, D(96))[0])
        self.assertIsNone(displayed_ahead(sparse, False, D(104))[0])

    def test_two_quote_cycles_keep_inventory_then_naturally_flatten(self):
        # Queue ahead=10; first active interval buys .99 at99; second sells
        # .99 at101. No forced taker roundtrip or cash reset between cycles.
        view = lambda t: (book(), book(), "paired")
        events = [(1_000_000_000, 1_100_000_000, "SELL", D(99), D("10.99"), True, False),
                  (16_000_000_000, 16_100_000_000, "BUY", D(101), D("10.99"), True, False)]
        result = replay_wallet(0, 30_000_000_000, view, events, lambda a,b: True)
        self.assertEqual(result["terminal_pnl_usd"], D("1.98"))
        self.assertEqual(result["known_turnover"]["xyz"], 0)
        self.assertNotIn("drain", result["known_role_turnover"])

    def test_delayed_arrival_unknown_stops_continuous_wallet(self):
        view = lambda t: (None, book(), "missing_arcus") if t == 310_000_000 else (book(), book(), "paired")
        result = replay_wallet(0, 30_000_000_000, view, [], lambda a,b: True)
        self.assertEqual(result["pending_orders"], 2)
        self.assertIsNone(result["terminal_pnl_usd"])
        self.assertEqual(result["dispositions"]["quote_decisions"], 1)


if __name__ == "__main__":
    unittest.main()

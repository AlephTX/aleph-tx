"""Synthetic checks for native box recognition and conservative valuation."""

from decimal import Decimal
import unittest

from probe_deribit_box import scan


NOW = 1_794_567_000_000
EXPIRY = 1_800_086_400_000


def sample(orientation=1, period="day"):
    terms = {
        "base_currency": "BTC", "price_index": "btc_usdc", "index_id": 33,
        "expiration_timestamp": EXPIRY, "contract_size": 1, "is_active": True,
        "state": "open", "instrument_type": "linear", "quote_currency": "USDC",
        "settlement_currency": "USDC", "min_trade_amount": Decimal("0.01"),
        "settlement_period": period,
    }
    legs = []
    instruments = []
    for strike, kind, sign in ((1000, "call", 1), (1000, "put", -1), (1100, "call", -1), (1100, "put", 1)):
        name = f"test-{strike}-{kind}"
        legs.append({"instrument_name": name, "amount": sign * orientation})
        instruments.append(terms | {"strike": strike, "option_type": kind, "instrument_name": name, "kind": "option"})
    return {
        "get_combos": [{"id": "test-opaque-name", "instrument_id": 100, "state": "active", "legs": legs}],
        "get_instruments": instruments,
        "get_instrument": terms | {"instrument_name": "test-opaque-name", "instrument_id": 100, "kind": "option_combo"},
        "get_order_book": {
            "instrument_name": "test-opaque-name", "state": "open", "timestamp": NOW,
            "index_price": 1050, "asks": [[85, "0.4"], [90, "0.8"]], "bids": [[-85, "0.4"], [-90, "0.8"]],
        },
    }


def fixture_report(data, **args):
    def get(method, **_):
        return data[method], NOW

    return scan(get, quantity=Decimal(1), entry_index_cap=Decimal(1100), financing=Decimal(2), **args)


class DeribitBoxTest(unittest.TestCase):
    def test_uses_native_depth_for_each_orientation(self):
        for orientation, side in ((1, "buy"), (-1, "sell")):
            with self.subTest(side=side):
                report = fixture_report(sample(orientation=orientation))
                quote, = report["quotes"]
                self.assertEqual(quote["side"], side)
                self.assertEqual(quote["entry_debit_usdc"], Decimal(88))
                self.assertEqual(quote["limit_debit_usdc"], Decimal(90))
                self.assertEqual(quote["native_fok_limit_price"], Decimal(90 * orientation))
                self.assertEqual(quote["gross_terminal_usdc"], Decimal(100))
                self.assertEqual(quote["conditional_net_usdc"], Decimal("6.68"))
                self.assertEqual(report["positive_conditional_quotes"], 1)
                self.assertEqual(report["verified_positive_executable_candidates"], 0)
                self.assertIsNone(report["account_margin_verified"])

    def test_nondaily_requires_explicit_delivery_bound(self):
        data = sample(period="month")
        self.assertIsNone(fixture_report(data)["quotes"][0]["conditional_net_usdc"])
        quote, = fixture_report(data, delivery_index_cap=Decimal(2000))["quotes"]
        self.assertEqual(quote["delivery_fee_bound_usdc"], Decimal("0.60"))
        self.assertEqual(quote["conditional_net_usdc"], Decimal("6.08"))

    def test_rejects_mismatched_settlement_and_stale_or_insufficient_depth(self):
        data = sample()
        data["get_instruments"][0]["index_id"] = 44
        self.assertEqual(fixture_report(data)["native_boxes"], 0)

        data = sample()
        data["get_order_book"]["timestamp"] -= 3001
        self.assertEqual(fixture_report(data)["quotes"][0]["status"], "book_or_instrument_unavailable")

        data = sample()
        data["get_order_book"]["asks"] = [[85, "0.4"]]
        self.assertEqual(fixture_report(data)["quotes"][0]["status"], "insufficient_native_depth")


if __name__ == "__main__":
    unittest.main()

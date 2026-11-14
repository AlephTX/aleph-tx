import unittest
from audit_binance_attribution import ADDED_FIELDS, verify


class BinanceAttributionTests(unittest.TestCase):
    def rows(self):
        original = {"net_pnl": 7, "binance_participation": {
            "decisions": 2, "fresh_reference_decisions": 1,
            "withdrawn_bid_targets": 1, "withdrawn_ask_targets": 0}}
        diagnostic = {"net_pnl": 7, "binance_participation": original["binance_participation"] | {
            k: 0 for k in ADDED_FIELDS}}
        diagnostic["binance_participation"].update(unavailable_reference_decisions=1,
            fresh_reference_fraction=0.5, shock_reference_decisions=1, withdrawn_bid_targets_by_shock=1)
        return original, diagnostic

    def test_only_added_diagnostics_may_differ(self):
        original, diagnostic = self.rows()
        self.assertEqual(verify(original, diagnostic), (True, None))
        diagnostic["net_pnl"] = 8
        self.assertEqual(verify(original, diagnostic), (False, "original_decisions_or_ledger_changed"))

    def test_inconsistent_partitions_and_old_binary_fail_closed(self):
        original, diagnostic = self.rows()
        self.assertFalse(verify(original, original)[0])
        diagnostic["binance_participation"]["withdrawn_bid_targets_by_unavailable"] = 1
        self.assertEqual(verify(original, diagnostic), (False, "withdrawal_partition_mismatch"))

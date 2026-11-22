import copy
from datetime import datetime, timezone, timedelta
from decimal import Decimal
import hashlib
import json
from pathlib import Path
import tempfile
import unittest

from audit_ant_campaign import SOURCES, audit_rules, fee_scenarios, load_evidence


def fixture():
    fees = [{"level": 0, "makerFeePpm": 0, "takerFeePpm": 112},
            {"level": 5, "makerFeePpm": -20, "takerFeePpm": 50}]
    return {
        "markets.json": {"markets": [{"marketDisplayName": "ANT-USD", "marketId": 88,
            "status": "ONLINE", "type": "PERPETUAL", "baseAsset": "ANT", "quoteAsset": "USD",
            "category": "EQUITIES", "feeTiers": copy.deepcopy(fees), "effectiveMakerFeePpm": 0,
            "effectiveTakerFeePpm": 112, "hasCustomFeeSchedule": True, "tickSize": "0.001",
            "stepSize": "0.000001", "minOrderSize": "0.000001", "maxOrderSize": "500",
            "minOrderNotional": "5", "initialMarginFraction": "0.1", "offHoursInitialMarginFraction": "0.15",
            "isOutsideRth": True, "lowerTradingBound": "95", "upperTradingBound": "105", "markTier": "FAST"}]},
        "fees.json": {"feeSchedules": [{"marketId": 88, "marketDisplayName": "ANT-USD",
            "hasCustomSchedule": True, "tiers": fees}]},
        "xyz_meta.json": [{"universe": [{"name": "xyz:ANT", "szDecimals": 2,
            "growthMode": "enabled", "deployerFeeScale": "1.0"}], "collateralToken": 0}, [{}]],
    }


class AntRulesTest(unittest.TestCase):
    def test_current_fee_units_and_four_leg_denominators(self):
        result = audit_rules(fixture())
        self.assertFalse(result["live_ready"])
        self.assertEqual(result["arcus_base_tier_taker_bps"], "1.12")
        self.assertEqual(Decimal(result["hl_tier0_unstaked_unreferred_unaligned_taker_bps_assumption"]), Decimal("0.9"))
        fees = result["fee_scenarios"]
        self.assertEqual(Decimal(fees["arcus_maker_entry_and_exit"]["four_leg_fees_bps_of_one_leg_principal"]), Decimal("1.8"))
        self.assertEqual(Decimal(fees["arcus_maker_entry_taker_exit"]["four_leg_fees_bps_of_one_leg_principal"]), Decimal("2.92"))
        self.assertEqual(Decimal(fees["both_venues_taker_entry_and_exit"]["fees_usd_per_million_all_four_leg_turnover"]), Decimal("101"))
        self.assertEqual(result["arcus_expansion_pressure_seconds"], 600)
        self.assertEqual(result["arcus_initial_band_fraction_of_settlement"], "0.05")
        self.assertEqual(result["points_cash_credit_usd"], "0")

    def test_metadata_failures_withhold_consistency(self):
        edits = [("status", "HALTED"), ("isOutsideRth", None), ("markTier", "UNKNOWN"),
                 ("lowerTradingBound", None), ("upperTradingBound", "90"),
                 ("tickSize", "NaN"), ("minOrderSize", "501"),
                 ("offHoursInitialMarginFraction", "0.05"), ("effectiveTakerFeePpm", 56)]
        for key, value in edits:
            with self.subTest(key=key):
                data = fixture()
                data["markets.json"]["markets"][0][key] = value
                with self.assertRaises(ValueError):
                    audit_rules(data)

    def test_duplicate_or_wrong_market_fee_identity_is_rejected(self):
        for kind in ["market", "tier", "fee", "symbol"]:
            data = fixture()
            if kind == "market":
                data["markets.json"]["markets"] *= 2
            elif kind == "tier":
                data["fees.json"]["feeSchedules"][0]["tiers"] *= 2
            elif kind == "fee":
                data["fees.json"]["feeSchedules"] *= 2
            else:
                data["fees.json"]["feeSchedules"][0]["marketDisplayName"] = "OTHER-USD"
            with self.assertRaises(ValueError):
                audit_rules(data)

    def test_session_change_cannot_reuse_previous_bands(self):
        data = fixture()
        market = data["markets.json"]["markets"][0]
        market["isOutsideRth"] = False
        with self.assertRaises(ValueError):
            audit_rules(data)
        market["lowerTradingBound"] = market["upperTradingBound"] = None
        self.assertFalse(audit_rules(data)["arcus_is_outside_rth"])

    def test_discount_removal_and_scale_changes_reprice_instead_of_double_discount(self):
        data = fixture()
        asset = data["xyz_meta.json"][0]["universe"][0]
        asset["growthMode"] = "disabled"
        self.assertEqual(Decimal(audit_rules(data)["hl_tier0_unstaked_unreferred_unaligned_taker_bps_assumption"]), 9)
        asset["deployerFeeScale"] = "0.5"
        self.assertEqual(Decimal(audit_rules(data)["hl_tier0_unstaked_unreferred_unaligned_taker_bps_assumption"]), Decimal("6.75"))
        asset["growthMode"] = "unknown"
        with self.assertRaises(ValueError):
            audit_rules(data)

    def test_twelve_percent_eight_hours_does_not_cover_baseline_fees(self):
        fees = fee_scenarios(Decimal(0), Decimal("1.12"), Decimal("0.9"))
        self.assertGreater(Decimal(fees["arcus_maker_entry_and_exit"]["hours_at_hypothetical_12pct_net_simple_apr_365d"]), 8)

    def test_receipts_hash_clocks_requests_and_failures(self):
        now = datetime(2026, 11, 20, 8, tzinfo=timezone.utc)
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            receipts = []
            for name, (url, body) in SOURCES.items():
                raw = json.dumps(fixture()[name]).encode()
                (directory / name).write_bytes(raw)
                receipts.append({"file": name, "url": url, "request_body": body, "status": 200,
                    "tls_verified": True, "requested_at": (now - timedelta(seconds=2)).isoformat(),
                    "received_at": (now - timedelta(seconds=1)).isoformat(),
                    "bytes": len(raw), "sha256": hashlib.sha256(raw).hexdigest()})
            def write(rows):
                (directory / "receipts.json").write_text(json.dumps(rows))
            write(receipts)
            data, provenance = load_evidence(directory, now)
            self.assertTrue(audit_rules(data)["public_rules_consistent"])
            self.assertEqual(len(provenance), 3)
            for key, value in [("sha256", "0" * 64), ("status", "failed"), ("tls_verified", False),
                               ("requested_at", (now - timedelta(seconds=61)).isoformat()),
                               ("requested_at", (now - timedelta(seconds=12)).isoformat()),
                               ("received_at", (now + timedelta(seconds=1)).isoformat()),
                               ("request_body", {"type": "userFees"}), ("url", "https://example.com")]:
                with self.subTest(key=key, value=value):
                    changed = copy.deepcopy(receipts)
                    changed[0][key] = value
                    write(changed)
                    with self.assertRaises(ValueError):
                        load_evidence(directory, now)
            write(receipts + [receipts[0]])
            with self.assertRaises(ValueError):
                load_evidence(directory, now)
            write(receipts)
            (directory / "markets.json").write_text("{}")
            with self.assertRaises(ValueError):
                load_evidence(directory, now)


if __name__ == "__main__":
    unittest.main()

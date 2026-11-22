from decimal import Decimal
from pathlib import Path
import re
import unittest
from audit_lighter_program_scope import (
    VENUE_CAPABILITY_SNAPSHOT_VERSION,
    cost_per_million_eligible_maker,
    decompose_execution_friction,
    multi_venue_program_scope,
    program_scope,
    score_program_cost_effectiveness,
)


class ProgramScopeTest(unittest.TestCase):
    def test_core_standard_only_has_no_direct_rh_denominator(self):
        result = program_scope(core_turnover="42012.412", rh_turnover="0", account_tier="standard")
        self.assertEqual(result["rh_direct_rule_eligible_turnover_quote"], "0")
        self.assertEqual(result["lpp_status"], "ineligible_account_tier")
        self.assertIsNone(result["venue_confirmed_eligible_turnover_quote"])

    def test_positive_rh_amount_is_only_an_upper_bound(self):
        result = program_scope(core_turnover="90000", rh_turnover="50", account_tier="premium")
        self.assertEqual(result["rh_direct_eligible_volume_ceiling_quote"], "50")
        self.assertIsNone(result["rh_direct_rule_eligible_turnover_quote"])
        self.assertEqual(result["lpp_status"], "requires_eligible_asset_and_resting_order_snapshots")

    def test_missing_domain_and_uncertain_tier_stay_unknown(self):
        result = program_scope(core_turnover="0", rh_turnover=None, account_tier="unknown")
        self.assertIsNone(result["rh_direct_eligible_volume_ceiling_quote"])
        self.assertEqual(result["lpp_status"], "account_tier_unknown")

    def test_invalid_notional_rejected(self):
        for invalid in ("-1", "NaN", "Infinity"):
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                program_scope(core_turnover=invalid, rh_turnover=0, account_tier="standard")

    def test_multi_venue_eligibility_and_zero_denominator_cost_semantics(self):
        # Standard-to-standard on RH is explicitly unrewarded -> $0 eligible denominator -> None $/M.
        rh_std = multi_venue_program_scope(
            venue="lighter_robinhood",
            liquidity_domain="lighter:robinhood",
            asset_class="crypto",
            account_tier="standard",
            maker_turnover="50000",
            counterparty_tier="standard",
        )
        self.assertEqual(rh_std["confirmed_eligible_maker_quote"], "0")
        self.assertEqual(rh_std["program_status"], "ineligible_standard_to_standard_excluded")
        self.assertIsNone(cost_per_million_eligible_maker("-1.0", rh_std["confirmed_eligible_maker_quote"]))

        # Core BTC Mode B has $0 RH points eligibility regardless of gross Core turnover.
        core_btc = multi_venue_program_scope(
            venue="lighter",
            liquidity_domain="lighter",
            asset_class="crypto",
            account_tier="standard",
            maker_turnover="300000",
        )
        self.assertEqual(core_btc["eligible_target_maker_ceiling_quote"], "0")
        self.assertIsNone(cost_per_million_eligible_maker("-3.0", core_btc["eligible_target_maker_ceiling_quote"]))

        # Arcus RWA L0 has 0 bp maker but flags Annex C on delta-neutral/looped turnover.
        arcus_rwa = multi_venue_program_scope(
            venue="arcus",
            liquidity_domain="arcus",
            asset_class="equity",
            account_tier="level_0",
            maker_turnover="25000",
            strategy_posture="delta_neutral_hedged",
        )
        self.assertEqual(arcus_rwa["maker_fee_bps"], "0.00")
        self.assertIn("arcus_annex_c_neutral_hedged_or_looped_volume_disqualifiable", arcus_rwa["disqualification_risks"])
        self.assertIsNone(arcus_rwa["confirmed_eligible_maker_quote"])

        # Arcus L4 requires $200M/30d; Crypto L0 is 1.50 bp ($150/M).
        arcus_l4 = multi_venue_program_scope(
            venue="arcus", liquidity_domain="arcus", asset_class="crypto",
            account_tier="level_4", maker_turnover="10000",
        )
        self.assertEqual(arcus_l4["tier_30d_volume_requirement_usd"], "200000000")

        # At a deployer scale of 1, growth mode reduces the maker fee to 0.30 bp.
        hip3_crypto = multi_venue_program_scope(
            venue="trade_xyz", liquidity_domain="trade_xyz", asset_class="crypto",
            account_tier="tier_0", maker_turnover="10000", growth_mode=True,
        )
        self.assertEqual(hip3_crypto["program_status"], "ineligible_hip3_growth_mode_excludes_core_crypto_assets")
        hip3_rwa = multi_venue_program_scope(
            venue="trade_xyz", liquidity_domain="trade_xyz", asset_class="equity",
            account_tier="tier_0", maker_turnover="1000000", growth_mode=True,
        )
        self.assertEqual(hip3_rwa["maker_fee_bps"], "0.30")
        self.assertEqual(hip3_rwa["volume_tier_credit_multiplier"], "0.10")

        # Cost per million eligible maker with positive confirmed denominator computes accurately.
        self.assertEqual(Decimal(cost_per_million_eligible_maker("-5.0", "250000")), Decimal("20"))

    def test_three_part_friction_and_program_score_per_dollar(self):
        report = {
            "ioc_slippage": "0.25",
            "ioc_depth_impact": "0.10",
            "post_only_rejected": 12,
            "ioc_expired": 3,
            "forced_closes": 2,
            "forced_close_pnl": "-0.40",
            "fees": "0.15",
            "cycle_attribution": {
                "forced_ioc_fees": ["0.05", "0.02"],
                "forced_ioc_slippage": ["0.03", "0.01"],
                "reduce_ioc_dust_fills": 4,
                "sub_min_top_up_unwinds": 1,
                "sub_min_recovery_turnover": ["12.5", "0"],
            },
        }
        friction = decompose_execution_friction(report, fixed_overhead_usd_per_day="1.6666666667")
        self.assertEqual(Decimal(friction["simulated_slippage"]["slippage_quote"]), Decimal("0.25"))
        self.assertEqual(friction["rejection_and_retry"]["post_only_rejections"], 12)
        self.assertEqual(friction["rejection_and_retry"]["ioc_misses_or_expired"], 3)
        self.assertEqual(
            Decimal(friction["partial_fill_residual"]["partial_residual_cost_quote"]),
            Decimal("0.44"),
        )
        self.assertEqual(friction["modeled_extra_overhead_bps"], "0")

        # Core Standard has $0 RH eligible denominator -> score per dollar must remain None.
        rh_core = score_program_cost_effectiveness(
            program="rh_points",
            net_variable_pnl_quote="-5.0",
            eligible_maker_turnover_quote="0",
        )
        self.assertIsNone(rh_core["eligible_score_per_usd_cost"])
        self.assertEqual(rh_core["disqualified_or_undefined_reason"], "zero_eligible_rh_maker_turnover")

        # Arcus Annex C neutral/hedged flag disqualifies incentive score.
        arcus_hedged = score_program_cost_effectiveness(
            program="arcus_rwa",
            net_variable_pnl_quote="-10.0",
            eligible_maker_turnover_quote="500000",
            annex_c_hedged_or_looped=True,
        )
        self.assertIsNone(arcus_hedged["eligible_score_per_usd_cost"])

        # HIP-3 growth mode cuts 14d tier volume credit by 90% (0.10x).
        hip3 = score_program_cost_effectiveness(
            program="hip3_tier_volume",
            net_variable_pnl_quote="-30.0",
            eligible_maker_turnover_quote="1000000",
            hip3_growth_mode=True,
        )
        self.assertEqual(Decimal(hip3["eligible_score_units"]), Decimal("0.1"))
        self.assertEqual(Decimal(hip3["usd_cost_per_eligible_score_unit"]), Decimal("300"))
        self.assertAlmostEqual(float(hip3["eligible_score_per_usd_cost"]), 0.1 / 30.0)

    def test_lighter_core_and_rh_plus_premium_fees_do_not_conflate_and_fail_closed(self):
        core_plus = multi_venue_program_scope(
            venue="lighter", liquidity_domain="lighter", asset_class="crypto",
            account_tier="plus", maker_turnover="10000",
        )
        rh_plus = multi_venue_program_scope(
            venue="lighter_robinhood", liquidity_domain="lighter:robinhood", asset_class="equity",
            account_tier="plus", maker_turnover="10000",
        )
        self.assertEqual((core_plus["maker_fee_bps"], core_plus["taker_fee_bps"]), ("0.50", "0.50"))
        self.assertEqual((rh_plus["maker_fee_bps"], rh_plus["taker_fee_bps"]), ("1.50", "1.50"))

        core_prem = multi_venue_program_scope(
            venue="lighter", liquidity_domain="lighter", asset_class="crypto",
            account_tier="premium", maker_turnover="10000",
        )
        rh_prem = multi_venue_program_scope(
            venue="lighter_robinhood", liquidity_domain="lighter:robinhood", asset_class="equity",
            account_tier="premium", maker_turnover="10000",
        )
        self.assertEqual((core_prem["maker_fee_bps"], core_prem["taker_fee_bps"]), ("0.4", "2.8"))
        self.assertEqual((rh_prem["maker_fee_bps"], rh_prem["taker_fee_bps"]), ("1.20", "3.50"))
        self.assertNotEqual(core_prem["taker_fee_bps"], rh_prem["taker_fee_bps"])

        for venue, dom, expected_status in (
            ("lighter", "lighter", "unverified_lighter_account_tier"),
            ("lighter_robinhood", "lighter:robinhood", "unverified_rh_account_tier"),
        ):
            unknown = multi_venue_program_scope(
                venue=venue, liquidity_domain=dom, asset_class="equity",
                account_tier="vip_unknown", maker_turnover="10000",
            )
            self.assertIsNone(unknown["maker_fee_bps"])
            self.assertIsNone(unknown["taker_fee_bps"])
            self.assertIsNone(unknown["eligible_target_maker_ceiling_quote"])
            self.assertEqual(unknown["program_status"], expected_status)

        plus_scope = program_scope(core_turnover="0", rh_turnover="50", account_tier="plus")
        self.assertEqual(plus_scope["lpp_status"], "plus_tier_lpp_eligibility_unverified")

    def test_arcus_unknown_tier_and_hip3_vs_native_hl_separation(self):
        arcus_unknown = multi_venue_program_scope(
            venue="arcus",
            liquidity_domain="arcus",
            asset_class="equity",
            account_tier="vip_unknown",
            maker_turnover="10000",
        )
        self.assertIsNone(arcus_unknown["eligible_target_maker_ceiling_quote"])
        self.assertIsNone(arcus_unknown["confirmed_eligible_maker_quote"])
        self.assertIsNone(arcus_unknown["maker_fee_bps"])
        self.assertIsNone(arcus_unknown["taker_fee_bps"])
        self.assertEqual(arcus_unknown["program_status"], "unverified_arcus_tier")

        with self.assertRaises(ValueError):
            multi_venue_program_scope(
                venue="hyperliquid",
                liquidity_domain="hyperliquid:core",
                asset_class="equity",
                account_tier="base",
                maker_turnover="10000",
                growth_mode=True,
            )

        hl_rebate = multi_venue_program_scope(
            venue="hyperliquid",
            liquidity_domain="hyperliquid:core",
            asset_class="crypto",
            account_tier="base",
            maker_turnover="10000",
            maker_14d_share="0.016",
        )
        self.assertEqual((hl_rebate["maker_fee_bps"], hl_rebate["taker_fee_bps"]), ("1.50", "4.50"))
        self.assertEqual(hl_rebate["maker_rebate_bps"], "0.2")

        for venue, dom in (
            ("trade_xyz", "hyperliquid:hip3-xyz"),
            ("entropy", "hyperliquid:hip3-io"),
        ):
            non_growth = multi_venue_program_scope(
                venue=venue,
                liquidity_domain=dom,
                asset_class="equity",
                account_tier="tier_0",
                maker_turnover="10000",
                growth_mode=False,
                maker_14d_share="0.05",
            )
            self.assertEqual((non_growth["maker_fee_bps"], non_growth["taker_fee_bps"]), ("3.00", "9.00"))
            self.assertEqual(non_growth["maker_rebate_bps"], "0")
            self.assertEqual(non_growth["volume_tier_credit_multiplier"], "1")

            scaled_non_growth = multi_venue_program_scope(
                venue=venue,
                liquidity_domain=dom,
                asset_class="equity",
                account_tier="tier_0",
                maker_turnover="10000",
                growth_mode=False,
                deployer_fee_scale="0.5",
            )
            self.assertEqual(
                (scaled_non_growth["maker_fee_bps"], scaled_non_growth["taker_fee_bps"]),
                ("2.25", "6.75"),
            )
            high_scale_non_growth = multi_venue_program_scope(
                venue=venue,
                liquidity_domain=dom,
                asset_class="equity",
                account_tier="tier_0",
                maker_turnover="10000",
                deployer_fee_scale="2",
            )
            self.assertEqual(
                (high_scale_non_growth["maker_fee_bps"], high_scale_non_growth["taker_fee_bps"]),
                ("6.00", "18.00"),
            )

            scaled_growth = multi_venue_program_scope(
                venue=venue,
                liquidity_domain=dom,
                asset_class="equity",
                account_tier="tier_0",
                maker_turnover="10000",
                growth_mode=True,
                deployer_fee_scale="0.5",
            )
            self.assertEqual(
                (scaled_growth["maker_fee_bps"], scaled_growth["taker_fee_bps"]),
                ("0.225", "0.675"),
            )
            self.assertEqual(scaled_growth["volume_tier_credit_multiplier"], "0.10")

            zero_scale_growth = multi_venue_program_scope(
                venue=venue,
                liquidity_domain=dom,
                asset_class="equity",
                account_tier="tier_0",
                maker_turnover="10000",
                growth_mode=True,
                deployer_fee_scale="0",
            )
            self.assertEqual(zero_scale_growth["maker_fee_bps"], "0.15")
            self.assertEqual(zero_scale_growth["program_status"], "hip3_growth_mode_90pct_fee_and_volume_cut")

            with self.assertRaises(ValueError):
                multi_venue_program_scope(
                    venue=venue,
                    liquidity_domain=dom,
                    asset_class="equity",
                    account_tier="tier_0",
                    maker_turnover="10000",
                    deployer_fee_scale="6",
                )

    def test_rust_venues_rs_capability_parity(self):
        venues_rs = (Path(__file__).resolve().parent.parent / "src/venues.rs").read_text(encoding="utf-8")
        fn_match = re.search(
            r"pub fn audited_execution_capabilities\(\)\s*->\s*Vec<VenueExecutionCapability>\s*\{(.*?)\n\}",
            venues_rs,
            flags=re.S,
        )
        self.assertIsNotNone(fn_match)
        blocks = re.findall(r"VenueExecutionCapability\s*\{(.*?)\n\s*\}", fn_match.group(1), flags=re.S)
        self.assertGreaterEqual(len(blocks), 11)
        scope_map = {"All": ("crypto", "equity"), "Crypto": ("crypto",), "Equity": ("equity",)}
        for block in blocks:
            venue_id = re.search(r'venue_id:\s*"([^"]+)"', block).group(1)
            domain = re.search(r'liquidity_domain:\s*"([^"]+)"', block).group(1)
            tier = re.search(r'tier:\s*"([^"]+)"', block).group(1)
            asset_scope = re.search(r'asset_scope:\s*(All|Crypto|Equity)', block).group(1)
            rust_maker = Decimal(re.search(r'maker_fee_bps:\s*dec\("([^"]+)"\)', block).group(1))
            rust_taker = Decimal(re.search(r'taker_fee_bps:\s*dec\("([^"]+)"\)', block).group(1))
            for asset_class in scope_map[asset_scope]:
                py_scope = multi_venue_program_scope(
                    venue=venue_id,
                    liquidity_domain=domain,
                    asset_class=asset_class,
                    account_tier=tier,
                    maker_turnover="1000",
                )
                self.assertEqual(py_scope["capability_snapshot_version"], VENUE_CAPABILITY_SNAPSHOT_VERSION)
                self.assertEqual(Decimal(py_scope["maker_fee_bps"]), rust_maker)
                self.assertEqual(Decimal(py_scope["taker_fee_bps"]), rust_taker)

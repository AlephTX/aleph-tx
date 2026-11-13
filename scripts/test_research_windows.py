import copy
from hashlib import sha256
import unittest

from research_windows import (TEN_HOURS_MS, funding_run_binding_sha256, source_coverage,
                              validate_independent_runs, validate_run)


START = 1_800_000_000_000
END = START + TEN_HOURS_MS
NOW = END + 10_000
HASH = "a" * 64


def fixture():
    catalog = {}
    for kind in ("order_book", "trades", "funding_settlements"):
        catalog[kind] = {"venue": "lighter", "domain": "lighter:core", "kind": kind,
                         "instrument": "BTC", "visibility": "public",
                         "origin": "https://example.invalid/public",
                         "artifacts": [{"sha256": HASH, "start_ms": START, "end_ms": END,
                                        "sealed_at_ms": END + 1, "quality_verified": True,
                                        "quality_sha256": "b" * 64, "gaps": []}]}
    run = {"scenario": "quant", "ledger_id": "q1", "experiment_id": "frozen-grid",
           "comparison_group": "same-tape", "independent_sample_id": "btc-window-1",
           "instrument": "BTC", "venue_domains": {"lighter": "lighter:core"},
           "maker_venue": "lighter", "source_ids": list(catalog),
           "window": {"start_ms": START, "end_ms": END}, "engine_sha256": HASH,
           "config_sha256": HASH, "fee_model_sha256": HASH, "fee_model_verified": True,
           "frozen_at_ms": START - 100, "purpose": "out_of_sample", "train_end_ms": START - 100,
           "embargo_ms": 100, "execution": {"supports_funding_boundaries": True,
           "funding_verified": True, "warmup_complete_at_ms": START,
           "final_exit_included": True, "flat_at_end": True,
           "segments": [{"start_ms": START, "end_ms": END, "state_reset": True,
                         "initial_state_sha256": HASH, "final_state_sha256": "c" * 64}]}}
    return catalog, run


def avoidance_fixture():
    catalog, run = fixture()
    del catalog['funding_settlements']
    run['venue_domains']['hyperliquid'] = 'hyperliquid:native'
    catalog['hyperliquid_order_book'] = copy.deepcopy(catalog['order_book'])
    catalog['hyperliquid_order_book'].update(venue='hyperliquid', domain='hyperliquid:native')
    for source in catalog.values():
        source['artifacts'][0].update(start_ms=START - 2000, end_ms=END + 2000,
                                      sealed_at_ms=END + 2001)
    streams = list(catalog)
    schedules, proofs = {}, {}
    for venue, hours in (('lighter', 1), ('hyperliquid', 8)):
        source_id = f'{venue}_funding_schedule'
        schedules[venue] = source_id
        interval = hours * 3_600_000
        catalog[source_id] = {
            'venue': venue, 'domain': run['venue_domains'][venue], 'kind': 'funding_schedule',
            'instrument': 'BTC', 'visibility': 'public', 'origin': 'https://example.invalid/public/rules',
            'funding_schedule': {'interval_ms': interval, 'offset_ms': 0,
                'valid_from_ms': START - interval, 'valid_until_ms': END + interval,
                'reviewed': True, 'reviewed_at_ms': START - 50, 'review_sha256': 'd' * 64,
                'metadata_sha256': 'e' * 64},
            'artifacts': [{'sha256': 'e' * 64, 'captured_at_ms': START - 200,
                           'sealed_at_ms': START - 100}],
        }
        first = (START // interval + 1) * interval
        proofs[venue] = [{
            'at_ms': at, 'guard_start_ms': at - 1000, 'guard_end_ms': at + 1001,
            'observed_at_ms': at + 1001,
            'proof_sha256': sha256(str(at).encode()).hexdigest(), 'state_sha256': 'f' * 64,
            'max_abs_positions_base': {v: '0' for v in run['venue_domains']},
            'max_open_order_counts': {v: 0 for v in run['venue_domains']},
            'max_pending_ioc_counts': {v: 0 for v in run['venue_domains']},
            'max_pending_fill_notice_counts': {v: 0 for v in run['venue_domains']},
            'max_active_cycle_count': 0,
            'source_watermarks_ms': {s: at + 1001 for s in streams},
        } for at in range(first, END + 1, interval)]
    run['source_ids'] = list(catalog)
    run['execution']['funding_mode'] = 'funding_avoidance'
    run['execution'].pop('funding_verified')
    run['execution']['funding_avoidance'] = {
        'audit_sha256': '9' * 64, 'audited_at_ms': END + 2002,
        'run_binding_sha256': funding_run_binding_sha256(run),
        'settlement_guard_ms': 1000, 'schedule_source_ids': schedules,
        'verified_slots_by_venue': proofs,
    }
    return catalog, run


class ResearchWindowTests(unittest.TestCase):
    def test_exact_window_and_known_complete_execution(self):
        catalog, run = fixture()
        result = validate_run(run, catalog, NOW)
        self.assertTrue(result["data_coverage_qualified"])
        self.assertTrue(result["execution_qualified"])

    def test_short_or_future_window_is_not_ten_hours(self):
        catalog, run = fixture()
        run["window"]["end_ms"] -= 1
        self.assertIn("window_not_exactly_10h", validate_run(run, catalog, NOW)["data_issues"])
        run["window"]["end_ms"] += 1
        self.assertIn("window_not_finished", validate_run(run, catalog, END - 1)["data_issues"])

    def test_millisecond_gap_is_preserved_and_never_rounded_away(self):
        catalog, run = fixture()
        artifact = catalog["trades"]["artifacts"][0]
        artifact["gaps"] = [[START + 5_000, START + 5_001]]
        result = validate_run(run, catalog, NOW)
        self.assertFalse(result["data_coverage_qualified"])
        self.assertEqual(result["coverage"]["trades"]["verified_coverage_ms"], TEN_HOURS_MS - 1)
        self.assertEqual(result["coverage"]["trades"]["missing_or_unverified_intervals"], artifact["gaps"])

    def test_shard_envelope_without_quality_does_not_prove_stream(self):
        catalog, run = fixture()
        del catalog["trades"]["artifacts"][0]["quality_sha256"]
        r = validate_run(run, catalog, NOW)
        self.assertEqual(r["coverage"]["trades"]["recorded_envelope_ms"], TEN_HOURS_MS)
        self.assertEqual(r["coverage"]["trades"]["verified_coverage_ms"], 0)
        self.assertFalse(r["execution_qualified"])
        catalog["trades"]["kind"] = "raw_archive"
        self.assertIn("required_maker_trade_stream_missing", validate_run(run, catalog, NOW)["data_issues"])

    def test_hourly_flat_restarts_cannot_be_pooled(self):
        catalog, run = fixture()
        first = run["execution"]["segments"][0]
        run["execution"]["segments"] = [first | {"start_ms": START + i * TEN_HOURS_MS // 10,
           "end_ms": START + (i + 1) * TEN_HOURS_MS // 10,
           "final_state_sha256": HASH} for i in range(10)]
        result = validate_run(run, catalog, NOW)
        self.assertTrue(result["data_coverage_qualified"])
        self.assertIn("pooled_restarted_executions", result["execution_issues"])

    def test_checkpoint_continuation_requires_state_hash_chain(self):
        catalog, run = fixture()
        first = run["execution"]["segments"][0]
        middle = START + TEN_HOURS_MS // 2
        run["execution"]["segments"] = [first | {"end_ms": middle},
            first | {"start_ms": middle, "state_reset": False, "initial_state_sha256": "c" * 64}]
        self.assertTrue(validate_run(run, catalog, NOW)["execution_qualified"])
        run["execution"]["segments"][1]["initial_state_sha256"] = HASH
        self.assertIn("state_continuity_mismatch", validate_run(run, catalog, NOW)["execution_issues"])

    def test_funding_and_fee_unknowns_remain_unknown(self):
        catalog, run = fixture()
        run["fee_model_verified"] = False
        run["execution"]["supports_funding_boundaries"] = False
        run["execution"]["funding_verified"] = False
        issues = validate_run(run, catalog, NOW)["execution_issues"]
        self.assertTrue({"fee_model_unverified", "engine_not_continuous_across_funding",
                         "missing_funding_settlement_evidence"}.issubset(issues))

    def test_future_fit_and_training_overlap_are_rejected(self):
        catalog, run = fixture()
        run["frozen_at_ms"] = START + 1
        run["train_end_ms"] = START
        issues = validate_run(run, catalog, NOW)["execution_issues"]
        self.assertIn("configuration_frozen_after_evaluation_started", issues)
        self.assertIn("training_evaluation_overlap", issues)

    def test_domains_instrument_and_missing_provenance_fail_closed(self):
        catalog, run = fixture()
        catalog["trades"]["domain"] = "lighter:robinhood"
        catalog["order_book"]["instrument"] = "ETH"
        del catalog["funding_settlements"]["artifacts"][0]["sha256"]
        r = validate_run(run, catalog, NOW)
        self.assertTrue({"source_domain_mismatch", "source_instrument_mismatch", "source_coverage_incomplete"}.issubset(r["data_issues"]))

    def test_duplicate_artifact_cannot_manufacture_later_coverage(self):
        catalog, _ = fixture()
        source = catalog["trades"]
        first = source["artifacts"][0]
        middle = START + TEN_HOURS_MS // 2
        source["artifacts"] = [first | {"end_ms": middle}, first | {"start_ms": middle}]
        r = source_coverage(source, START, END, NOW)
        self.assertIn("conflicting_artifact_interval", r["issues"])
        self.assertEqual(r["verified_coverage_ms"], TEN_HOURS_MS // 2)

    def test_shared_tape_across_objectives_is_one_observation_not_pooled_pnl(self):
        catalog, quant = fixture()
        turnover = copy.deepcopy(quant)
        turnover.update(scenario="turnover", ledger_id="t1", experiment_id="turnover-only")
        r = validate_independent_runs([quant, turnover], catalog, NOW)
        self.assertTrue(r["all_execution_qualified"])
        self.assertEqual(r["run_count"], 2)
        self.assertEqual(r["distinct_market_time_windows"], 1)
        self.assertNotIn("net_pnl", r)

    def test_paired_variants_do_not_become_independent_samples(self):
        catalog, first = fixture()
        second = copy.deepcopy(first)
        second.update(ledger_id="q2", config_sha256="d" * 64)
        self.assertFalse(validate_independent_runs([first, second], catalog, NOW)["independence_issues"])
        second["independent_sample_id"] = "pretend-new-sample"
        self.assertEqual(validate_independent_runs([first, second], catalog, NOW)["independence_issues"][0]["reason"], "overlap_claimed_as_independent")

    def test_paired_fee_models_and_reused_ledgers_are_rejected(self):
        catalog, first = fixture()
        second = copy.deepcopy(first)
        second["fee_model_sha256"] = "d" * 64
        r = validate_independent_runs([first, second], catalog, NOW)
        self.assertEqual({x["reason"] for x in r["independence_issues"]}, {"ledger_reused", "paired_evidence_or_fee_model_mismatch"})

    def test_renamed_source_cannot_relabel_identical_bytes_as_later_data(self):
        catalog, run = fixture()
        catalog["renamed"] = copy.deepcopy(catalog["trades"])
        catalog["renamed"]["artifacts"][0].update(start_ms=END, end_ms=END + TEN_HOURS_MS)
        result = validate_independent_runs([run], catalog, NOW)
        self.assertIn("artifact_reused_with_conflicting_interval", {i["reason"] for i in result["independence_issues"]})

    def test_source_order_does_not_change_paired_evidence(self):
        catalog, first = fixture()
        second = copy.deepcopy(first)
        second.update(ledger_id="q2", source_ids=list(reversed(first["source_ids"])))
        self.assertFalse(validate_independent_runs([first, second], catalog, NOW)["independence_issues"])

    def test_unknown_final_inventory_cannot_qualify(self):
        catalog, run = fixture()
        run["execution"]["flat_at_end"] = None
        self.assertIn("residual_or_unknown_position", validate_run(run, catalog, NOW)["execution_issues"])

    def test_reviewed_funding_avoidance_qualifies_without_imputed_settlements(self):
        catalog, run = avoidance_fixture()
        result = validate_run(run, catalog, NOW)
        self.assertTrue(result['data_coverage_qualified'])
        self.assertTrue(result['execution_qualified'], result['execution_issues'])
        expected = result['funding_avoidance']['expected_slots_by_venue']
        self.assertEqual(len(expected['lighter']), 10)
        self.assertEqual(len(expected['hyperliquid']), 1)
        self.assertEqual(expected['lighter'][-1], END)
        self.assertNotIn('funding_settlements', catalog)

    def test_missing_extra_wrong_or_duplicate_funding_slots_fail(self):
        for change in ('missing', 'extra', 'wrong', 'duplicate'):
            with self.subTest(change=change):
                catalog, run = avoidance_fixture()
                rows = run['execution']['funding_avoidance']['verified_slots_by_venue']['lighter']
                if change == 'missing':
                    rows.pop()
                elif change == 'wrong':
                    rows[0]['at_ms'] += 1
                else:
                    rows.append(copy.deepcopy(rows[-1]))
                    if change == 'extra':
                        rows[-1]['at_ms'] += 3_600_000
                result = validate_run(run, catalog, NOW)
                self.assertIn('funding_avoidance_slot_mismatch', result['execution_issues'])
                self.assertFalse(result['execution_qualified'])

    def test_unknown_unreviewed_or_outdated_schedule_cannot_waive_funding(self):
        for field, value in (('interval_ms', None), ('reviewed', False),
                             ('valid_until_ms', END - 1), ('metadata_sha256', '0' * 64)):
            with self.subTest(field=field):
                catalog, run = avoidance_fixture()
                catalog['lighter_funding_schedule']['funding_schedule'][field] = value
                result = validate_run(run, catalog, NOW)
                self.assertIn('funding_avoidance_schedule_unverified', result['execution_issues'])
                self.assertFalse(result['execution_qualified'])

    def test_zero_net_and_boolean_flat_do_not_prove_funding_avoidance(self):
        for field, value in (
                ('max_abs_positions_base', {'lighter': '0.1', 'hyperliquid': '-0.1'}),
                ('max_open_order_counts', {'lighter': 0, 'hyperliquid': 1}),
                ('max_pending_ioc_counts', {'lighter': 0, 'hyperliquid': 1}),
                ('max_pending_fill_notice_counts', {'lighter': 1, 'hyperliquid': 0}),
                ('max_abs_positions_base', True)):
            with self.subTest(field=field, value=value):
                catalog, run = avoidance_fixture()
                slot = run['execution']['funding_avoidance']['verified_slots_by_venue']['lighter'][0]
                slot[field] = value
                slot['flat'] = True
                result = validate_run(run, catalog, NOW)
                self.assertIn('funding_avoidance_residual_or_unknown_risk', result['execution_issues'])
                self.assertFalse(result['execution_qualified'])

    def test_slot_proof_watermark_and_post_window_guard_data_are_required(self):
        for change in ('proof', 'watermark', 'tail'):
            with self.subTest(change=change):
                catalog, run = avoidance_fixture()
                slot = run['execution']['funding_avoidance']['verified_slots_by_venue']['lighter'][-1]
                if change == 'proof':
                    del slot['proof_sha256']
                    expected = 'funding_avoidance_slot_proof_missing'
                elif change == 'watermark':
                    slot['source_watermarks_ms']['trades'] = END
                    expected = 'funding_avoidance_watermark_unverified'
                else:
                    catalog['trades']['artifacts'][0]['end_ms'] = END
                    expected = 'funding_avoidance_guard_data_incomplete'
                result = validate_run(run, catalog, NOW)
                self.assertIn(expected, result['execution_issues'])
                self.assertFalse(result['execution_qualified'])

    def test_audit_cannot_be_reused_for_another_ledger_or_relabel_slots(self):
        catalog, run = avoidance_fixture()
        run['ledger_id'] = 'another-ledger'
        self.assertIn('funding_avoidance_audit_unverified',
                      validate_run(run, catalog, NOW)['execution_issues'])
        catalog, run = avoidance_fixture()
        rows = run['execution']['funding_avoidance']['verified_slots_by_venue']['lighter']
        rows[1]['proof_sha256'] = rows[0]['proof_sha256']
        self.assertIn('funding_avoidance_proof_reused_for_another_slot',
                      validate_run(run, catalog, NOW)['execution_issues'])

    def test_avoidance_cannot_bypass_engine_support_or_unknown_fees(self):
        catalog, run = avoidance_fixture()
        run['fee_model_verified'] = False
        run['execution']['supports_funding_boundaries'] = False
        result = validate_run(run, catalog, NOW)
        self.assertTrue(result['funding_avoidance']['qualified'])
        self.assertFalse(result['execution_qualified'])
        self.assertTrue({'fee_model_unverified', 'engine_not_continuous_across_funding'}
                        .issubset(result['execution_issues']))

    def test_avoidance_boolean_and_count_alone_do_not_qualify(self):
        catalog, run = avoidance_fixture()
        run['execution']['funding_avoidance'] = {'flat': True, 'verified_slots': 11}
        result = validate_run(run, catalog, NOW)
        self.assertFalse(result['execution_qualified'])
        self.assertTrue({'funding_avoidance_audit_unverified', 'funding_avoidance_schedules_missing',
                         'funding_avoidance_slots_missing'}.issubset(result['execution_issues']))

    def test_no_implicit_waiver_and_unknown_stream_quality_stays_blocked(self):
        catalog, run = avoidance_fixture()
        run['execution'].pop('funding_mode')
        result = validate_run(run, catalog, NOW)
        self.assertEqual(result['funding_mode'], 'settlements')
        self.assertIn('missing_funding_settlement_evidence', result['execution_issues'])
        catalog, run = avoidance_fixture()
        catalog['trades']['artifacts'][0]['quality_verified'] = False
        result = validate_run(run, catalog, NOW)
        self.assertFalse(result['data_coverage_qualified'])
        self.assertFalse(result['execution_qualified'])
        self.assertIn('funding_avoidance_guard_data_incomplete', result['execution_issues'])


if __name__ == "__main__":
    unittest.main()

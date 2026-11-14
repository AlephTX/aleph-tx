import copy
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from capture_timing import ClockAudit, precise_available_ms
from replay_binance_ab import reference_events, sampled_reference, save_new, market, write_input


def record(sequence=1, offset=0, event='message'):
    ns = 1_800_000_000_000_000_001 + offset
    return {'source': 'binance', 'market': 'ETHUSDT', 'event': event,
            'received_at_ms': ns // 1_000_000, 'available_at_ms': ns // 1_000_000,
            'capture_timing': {'run_id': 'a' * 32, 'sequence': sequence, 'connection_epoch': 1,
                'receipt_wall_ns': ns, 'available_wall_ns': ns + 2,
                'receipt_monotonic_ns': offset + 1, 'available_monotonic_ns': offset + 3},
            'payload': {'e': 'bookTicker', 's': 'ETHUSDT', 'u': 2 ** 53 + 1,
                        'E': ns // 1_000_000, 'T': ns // 1_000_000,
                        'b': '99.99', 'a': '100.01', 'B': '2', 'A': '3'}}


class BinanceInputTests(unittest.TestCase):
    def test_sampling_waits_for_bucket_close_and_disconnect_discards_pending(self):
        def event(at, mid):
            return {'kind': 'binance', 'at_ms': at, 'observed_ms': at, 'mid': mid, 'epoch': 1,
                    'update_id': at if mid is not None else None}
        events = list(sampled_reference([event(1001, 100), event(1024, 101), event(1026, 102),
                                         event(1027, None), event(1051, 103)]))
        self.assertEqual([(r['at_ms'], r['observed_ms'], r['mid']) for r in events],
                         [(1025, 1024, 101), (1027, 1027, None), (1075, 1051, 103)])

    def test_input_converts_exact_trade_size_and_checks_terminal_book_coverage(self):
        books = [{'kind': 'book', 'at_ms': at, 'lighter': {'timestamp_ms': at, 'bids': [[99, 2]], 'asks': [[101, 2]]}}
                 for at in (900, 1000, 1100, 1200)]
        trades = [{'kind': 'trade_feed', 'at_ms': 950, 'leg': 1, 'online': True},
                  {'kind': 'trade', 'at_ms': 1050, 'leg': 1, 'source_ms': 1040,
                   'price': '100.1', 'contracts': '0.005', 'aggressor': 'buy'}]
        def reference(_archive, _symbol, stats):
            stats['valid_bbo'] += 1
            yield {'kind': 'binance', 'at_ms': 1001, 'observed_ms': 1001, 'mid': 100, 'epoch': 1, 'update_id': 1}
        with tempfile.TemporaryDirectory() as temp, \
                patch('replay_binance_ab.verify_compiled', return_value={'availability_rule': 'ceil_available_wall_ns_to_ms', 'capture_run_id': 'a'*32}), \
                patch('replay_binance_ab.local_books', return_value=iter(books)), \
                patch('replay_binance_ab.trade_events', return_value=iter(trades)), \
                patch('replay_binance_ab.reference_events', side_effect=reference):
            out = Path(temp) / 'tape.ndjson'
            evidence = write_input(Path(temp), Path(temp), 'lighter_robinhood', 0,
                                   {'warmup_start_ms': 1000, 'drain_until_ms': 1200}, out)
            rows = [json.loads(line) for line in out.read_text().splitlines()]
            trade = next(r for r in rows if r['kind'] == 'trade')
            self.assertEqual(trade['size'], 0.005)
            self.assertNotIn('contracts', trade)
            self.assertTrue(rows[0]['online'])
            self.assertEqual(evidence['events']['trade'], 1)

    def test_nanoseconds_never_round_observation_back_into_the_past(self):
        row = record()
        self.assertEqual(precise_available_ms(row), row['available_at_ms'] + 1)
        audit = ClockAudit()
        audit.observe(row)
        audit.observe(record(2, 100))
        with self.assertRaises(ValueError):
            audit.observe(record(4, 200))

    def test_mixed_host_missing_clock_and_clock_step_rejected(self):
        for mutation in ('identity', 'wall', 'missing', 'backwards'):
            audit = ClockAudit()
            audit.observe(record())
            row = record(2, 1_000_000)
            if mutation == 'identity':
                row['capture_timing']['run_id'] = 'b' * 32
            elif mutation == 'wall':
                row['capture_timing']['available_monotonic_ns'] += 3_000_000
            elif mutation == 'missing':
                row.pop('capture_timing')
            else:
                row['capture_timing']['available_wall_ns'] -= 10_000_000
            with self.assertRaises((ValueError, KeyError)):
                audit.observe(row)

    def test_missing_source_time_and_stale_arrival_invalidate_signal(self):
        rows = [record(), record(2, 100), record(3, 200), record(4, 300, 'disconnect')]
        del rows[1]['payload']['T']
        rows[2]['payload']['T'] -= 2001
        stats = __import__('collections').Counter()
        with patch('replay_binance_ab.records', return_value=[json.dumps(r) for r in rows]):
            events = list(reference_events(Path('synthetic'), 'ETHUSDT', stats))
        self.assertEqual(events[0]['update_id'], 2 ** 53 + 1)
        self.assertEqual(events[0]['mid'], 100)
        self.assertTrue(all(e['mid'] is None for e in events[1:]))
        self.assertEqual(stats['invalid_bbo'], 2)
        self.assertEqual(events[0]['at_ms'], rows[0]['available_at_ms'] + 1)

    def test_raw_sequence_regression_cannot_hide_inside_sample_bucket(self):
        rows = [record(1, 0), record(2, 1_000_000), record(3, 2_000_000)]
        rows[0]['payload']['u'] = 10
        rows[1]['payload']['u'] = 9
        rows[2]['payload']['u'] = 11
        stats = __import__('collections').Counter()
        with patch('replay_binance_ab.records', return_value=[json.dumps(r) for r in rows]):
            events = list(sampled_reference(reference_events(Path('synthetic'), 'ETHUSDT', stats)))
        self.assertEqual(stats['sequence_rejections'], 1)
        self.assertIsNone(events[0]['mid'])
        self.assertEqual(events[-1]['update_id'], 11)

    def test_market_identity_and_exclusive_report(self):
        metadata = {'order_books': [{'symbol': 'ETH', 'market_type': 'perp', 'market_id': 0,
                    'supported_price_decimals': 2, 'supported_size_decimals': 4,
                    'min_base_amount': '0.005', 'min_quote_amount': '10'}]}
        self.assertEqual(market(metadata, 'ETH')['min_base_amount'], '0.005')
        metadata['order_books'].append(copy.deepcopy(metadata['order_books'][0]))
        with self.assertRaises(ValueError):
            market(metadata, 'ETH')
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / 'report.json'
            save_new(path, {'synthetic': True})
            with self.assertRaises(FileExistsError):
                save_new(path, {})


if __name__ == '__main__':
    unittest.main()

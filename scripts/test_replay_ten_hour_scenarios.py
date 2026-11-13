import json
import gzip
import io
from contextlib import contextmanager
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from replay_ten_hour_scenarios import quant_input, strict_trade_rows, turnover_input, turnover_policies, robinhood_market_id


class ContinuousSourceTests(unittest.TestCase):
    def test_robinhood_profile_binds_both_domain_ids_and_hash(self):
        profile = {'pair': {'legs': [{'venue': 'lighter', 'market': 'BTC'},
                                    {'venue': 'lighter_robinhood', 'market': 'BTC'}]},
                   'legs': [{'lighter_market_id': 1}, {'lighter_market_id': 42}]}
        with patch('replay_ten_hour_scenarios.load', return_value=profile), \
                patch('replay_ten_hour_scenarios.digest', return_value='a' * 64):
            lineage = {'config_sha256': 'a' * 64}
            self.assertEqual(robinhood_market_id(Path('synthetic'), lineage, 'BTC', 1), 42)
            for symbol, core in (('ETH', 1), ('BTC', 2)):
                with self.assertRaisesRegex(ValueError, 'domain mapping'):
                    robinhood_market_id(Path('synthetic'), lineage, symbol, core)
            with self.assertRaisesRegex(ValueError, 'compilation profile'):
                robinhood_market_id(Path('synthetic'), {'config_sha256': 'b' * 64}, 'BTC', 1)
            for identifier in (True, '42', -1, 65536):
                profile['legs'][1]['lighter_market_id'] = identifier
                with self.assertRaisesRegex(ValueError, 'native market ID'):
                    robinhood_market_id(Path('synthetic'), lineage, 'BTC', 1)

    def test_robinhood_uses_its_native_id_and_separate_action_delays(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            pair = {'legs': [{'venue': 'lighter', 'market': 'BTC'},
                             {'venue': 'lighter_robinhood', 'market': 'BTC'}]}
            frames = [{'pair': pair}] + [{'now_ms': at, 'legs': [
                {'book': {'available_notional': '0'}}, {'book': {'available_notional': '0'}}]}
                for at in (90, 150, 300)]
            def events(archive, venue, market, leg):
                self.assertEqual((venue, market), (('lighter', '1'), ('lighter_robinhood', '42'))[leg])
                return iter({'kind': 'trade_feed', 'at_ms': at, 'leg': leg, 'online': True}
                            for at in (80, 300))
            window = {'start_ms': 100, 'end_ms': 285, 'drain_until_ms': 300}
            with patch('replay_ten_hour_scenarios.unseal', return_value=io.StringIO(
                    '\n'.join(map(json.dumps, frames)))), \
                    patch('replay_ten_hour_scenarios.trade_events', side_effect=events), \
                    patch('replay_ten_hour_scenarios.digest', return_value='a' * 64):
                _, report = turnover_input(root, 'BTC', window, root / 'input.ndjson',
                    public_archive=root, compiled=root/'tape', lighter_id=1,
                    target='lighter_robinhood', target_market_id=42)
            self.assertEqual(report['explicit_feed_states_by_leg'], [2, 2])
            for _, policy in turnover_policies(pair, window, ('0', '0')):
                self.assertEqual(policy['maker_cancel_delay_ms'], 300)
                self.assertEqual(policy['taker_delay_ms'], [300, 300])
            with self.assertRaisesRegex(ValueError, 'native market ID'):
                turnover_input(root, 'BTC', window, root/'invalid.ndjson', target='lighter_robinhood')

    @contextmanager
    def turnover_fixture(self, arcus_end=300):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'raw').mkdir()
            pair = {'legs': [{'venue': 'lighter', 'market': 'BTC'},
                             {'venue': 'arcus', 'market': 'BTC-USD'}]}
            for leg in pair['legs']:
                last = arcus_end if leg['venue'] == 'arcus' else 300
                rows = [{'kind': 'feed_state', 'received_at_ms': 80, 'state': 'connected'},
                        {'kind': 'feed_state', 'received_at_ms': 90, 'state': 'disconnected'}]
                for at in (95, 150, last):
                    rows.append({'kind': 'trade', 'market': leg['market'],
                                 'received_at_ms': at, 'source_time_ms': at - 1,
                                 'side': 'sell', 'price': '100', 'size': '0.1',
                                 'trade_id': f'{leg["venue"]}-{at}'})
                path = root / 'raw' / f'{leg["venue"]}-trades-synthetic.jsonl.gz'
                with gzip.open(path, 'wt') as stream:
                    for row in rows:
                        stream.write(json.dumps(row) + '\n')
            frames = []
            for at in (90, 160, 300):
                book = {'available_notional': '0', 'timestamp_ms': at, 'tradable': True,
                        'bids': [{'price': '100', 'contracts': '1'}],
                        'asks': [{'price': '101', 'contracts': '1'}]}
                frames.append({'now_ms': at, 'legs': [{'book': book}, {'book': book}]})
            source = '\n'.join(json.dumps(row) for row in [{'pair': pair}] + frames) + '\n'
            with patch('replay_ten_hour_scenarios.unseal', return_value=io.BytesIO(source.encode())), \
                    patch('replay_ten_hour_scenarios.digest', return_value='a' * 64):
                yield root, {'start_ms': 100, 'drain_until_ms': 300}, root / 'turnover.ndjson'

    def replay(self, rows):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'raw').mkdir()
            (root / 'raw/lighter-trades-synthetic.jsonl.gz').touch()
            output = root / 'input.ndjson'
            books = [{'kind': 'book', 'at_ms': 100}, {'kind': 'book', 'at_ms': 200}]
            with patch('replay_ten_hour_scenarios.books', return_value=iter(books)), \
                    patch('replay_ten_hour_scenarios.strict_trade_rows', return_value=iter(rows)), \
                    patch('replay_ten_hour_scenarios.digest', return_value='a' * 64):
                report = quant_input(root, 'BTC', {'warmup_start_ms': 100, 'drain_until_ms': 200}, output)
            return report, [json.loads(line) for line in output.read_text().splitlines()]

    def test_a_previous_trade_cannot_invent_a_connected_feed(self):
        _, rows = self.replay([{'kind': 'trade', 'at_ms': 99}])
        self.assertEqual(rows[0], {'kind': 'trade_feed', 'at_ms': 100, 'online': False})
        self.assertFalse(any(r['at_ms'] >= 200 for r in rows))

    def test_explicit_disconnect_is_carried_across_window_boundary(self):
        _, rows = self.replay([{'kind': 'trade_feed', 'at_ms': 80, 'online': True},
                               {'kind': 'trade_feed', 'at_ms': 90, 'online': False}])
        self.assertFalse(rows[0]['online'])

    def test_reconnect_and_live_print_keep_source_and_receipt_times(self):
        report, rows = self.replay([{'kind': 'trade_feed', 'at_ms': 110, 'online': True},
            {'kind': 'trade', 'at_ms': 120, 'source_ms': 115, 'price': '100',
             'contracts': '0.1', 'aggressor': 'sell'},
            {'kind': 'trade_feed', 'at_ms': 150, 'online': False}])
        trade = next(r for r in rows if r['kind'] == 'trade')
        self.assertEqual((trade['at_ms'], trade['source_ms']), (120, 115))
        self.assertEqual([r['online'] for r in rows if r['kind'] == 'trade_feed'], [False, True, False])
        self.assertEqual(report['events']['trade'], 1)


    def test_future_source_time_survives_for_engine_rejection(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'synthetic.jsonl.gz'
            row = {'kind': 'trade', 'market': 'BTC', 'received_at_ms': 100,
                   'source_time_ms': 101, 'side': 'buy', 'price': '100', 'size': '0.1', 'trade_id': 1}
            with gzip.open(path, 'wt') as stream:
                stream.write(json.dumps(row) + '\n')
            result = list(strict_trade_rows([path], 'BTC', 0, 200))
            self.assertEqual(result[0]['source_ms'], 101)
            self.assertEqual(result[0]['at_ms'], 100)

    def test_conflicting_trade_identity_is_not_silently_deduplicated(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'synthetic.jsonl.gz'
            row = {'kind': 'trade', 'market': 'BTC', 'received_at_ms': 100,
                   'source_time_ms': 99, 'side': 'buy', 'price': '100', 'size': '0.1', 'trade_id': 1}
            with gzip.open(path, 'wt') as stream:
                for value in (row, row | {'received_at_ms': 101, 'size': '0.2'}):
                    stream.write(json.dumps(value) + '\n')
            with self.assertRaisesRegex(ValueError, 'conflicting duplicate'):
                list(strict_trade_rows([path], 'BTC', 0, 200))

    def test_turnover_cached_prints_do_not_restore_disconnected_feeds(self):
        with self.turnover_fixture() as (root, window, output):
            _, report = turnover_input(root, 'BTC', window, output)
            rows = [json.loads(line) for line in output.read_text().splitlines()]
            feeds = [row for row in rows if row['kind'] == 'trade_feed']
            self.assertEqual(feeds, [
                {'kind': 'trade_feed', 'at_ms': 100, 'leg': 0, 'online': False},
                {'kind': 'trade_feed', 'at_ms': 100, 'leg': 1, 'online': False},
            ])
            self.assertEqual(len([row for row in rows if row['kind'] == 'trade']), 2)
            self.assertEqual(rows[-1], {'kind': 'end', 'at_ms': 300})
            self.assertEqual(report['trade_envelopes_ms'], [[80, 300], [80, 300]])
            self.assertFalse(report['stream_quality_verified'])

    def test_turnover_arcus_source_shortfall_cannot_emit_complete_end(self):
        with self.turnover_fixture(arcus_end=200) as (root, window, output):
            with self.assertRaisesRegex(ValueError, 'lacks requested book/trade envelope'):
                turnover_input(root, 'BTC', window, output)
            rows = [json.loads(line) for line in output.read_text().splitlines()]
            self.assertFalse(any(row['kind'] == 'end' for row in rows))
            self.assertTrue(any(row['kind'] == 'book' for row in rows))


if __name__ == '__main__':
    unittest.main()

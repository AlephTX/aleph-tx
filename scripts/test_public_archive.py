import json
from pathlib import Path
import tempfile
import unittest

from archive_public_capture import ingest, records
from compact_replay_inputs import compact, digest, restore
from public_replay_sources import funding_samples, trade_events


class PublicArchiveTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.t = 1_800_000_000_000

    def row(self, source, event, offset, market=None, payload=None):
        row = {'source': source, 'event': event, 'received_at_ms': self.t + offset,
               'available_at_ms': self.t + offset + 1}
        if market is not None:
            row['market'] = market
        if payload is not None:
            row['payload'] = payload
        return row

    def capture(self, rows):
        source = self.root / 'capture.ndjson'
        source.write_text(''.join(json.dumps(row) + '\n' for row in rows))
        archive = self.root / 'archive'
        ingest(source, archive)
        return source, archive

    def test_venue_partition_preserves_equal_time_order_and_session_boundaries(self):
        rows = [self.row('hyperliquid', 'connected', 0),
                self.row('lighter', 'message', 0, payload={'channel': 'order_book:0'}),
                self.row('lighter_robinhood', 'message', 0, payload={'channel': 'order_book:0'}),
                self.row('hyperliquid', 'message', 0, 'xyz:NVDA', {'channel': 'l2Book'}),
                self.row('hyperliquid', 'disconnect', 3_600_000),
                self.row('collector', 'end', 3_600_001)]
        source, archive = self.capture(rows)
        self.assertEqual(b''.join(records(archive)), source.read_bytes())
        self.assertTrue((archive / 'hyperliquid/hip3-xyz').is_dir())
        self.assertTrue((archive / 'lighter/core').is_dir())
        self.assertTrue((archive / 'lighter/robinhood').is_dir())
        shard = next(archive.rglob('*.gz'))
        shard.write_bytes(shard.read_bytes() + b'corruption')
        with self.assertRaisesRegex(ValueError, 'shard hash'):
            list(records(archive))

    def test_trade_recovery_discards_cached_prints_but_preserves_disconnect(self):
        def trade(at, stamp, ident):
            return self.row('hyperliquid_trades', 'message', at, 'BTC', {'data': [
                {'coin': 'BTC', 'side': 'B', 'px': '100.01', 'sz': '0.1', 'tid': ident, 'time': self.t + stamp}]})

        def ack(at):
            return self.row('hyperliquid_trades', 'subscribed', at, 'BTC', {'type': 'trades', 'coin': 'BTC'})

        _, archive = self.capture([ack(1), trade(2, 1, 5),
            self.row('hyperliquid_trades', 'disconnect', 3, 'BTC'),
            self.row('hyperliquid_trades', 'connected', 4, 'BTC'), ack(5),
            trade(6, 1, 5), trade(7, 4, 6), trade(8, 6, 7), self.row('collector', 'end', 9)])
        events = list(trade_events(archive, 'hyperliquid', 'BTC', 1))
        self.assertEqual([e.get('online') for e in events if e['kind'] == 'trade_feed'], [True, False, True])
        self.assertEqual([e['source_ms'] for e in events if e['kind'] == 'trade'], [self.t + 1, self.t + 6])

    def test_funding_availability_comes_from_receive_time(self):
        _, archive = self.capture([self.row('hyperliquid_funding', 'message', 8000, 'ETH', {'data': [
            {'coin': 'ETH', 'fundingRate': '-0.0001', 'time': self.t + 76}]}), self.row('collector', 'end', 9000)])
        sample, = funding_samples(archive)
        self.assertEqual(sample['available_at_ms'], self.t + 8001)
        self.assertEqual(sample['rates'][0]['slot_ms'], self.t)
        self.assertEqual(sample['rates'][0]['rate_bps'], '-1.0000')

    def test_compaction_requires_rebuilt_hash_and_restores_legacy_path(self):
        paths = ['results/a/inputs/BTC-1.ndjson', 'results/b/inputs/BTC-1.ndjson', 'results/c/inputs/BTC-1.ndjson']
        for i, name in enumerate(paths):
            path = self.root / name
            path.parent.mkdir(parents=True)
            path.write_bytes(b'{"frame":1}\n' if i < 2 else b'{"frame":2}\n')
        expected = digest(self.root / paths[0])
        proof = self.root / 'proof.json'
        proof.write_text(json.dumps({'stage': 'complete', 'assets': {'BTC': {'partition_payload_matches': True,
            'compiled_matches': True, 'matched_inputs': [{'name': 'BTC-1.ndjson', 'sha256': expected}]}}}))
        store = self.root / 'results/store'
        alias = self.root / 'results/alias'
        alias.symlink_to(self.root / 'results/a', target_is_directory=True)
        paths.append('results/alias/inputs/BTC-1.ndjson')
        result = compact(self.root, proof, store, paths)
        self.assertEqual(result['aliases_verified'], 3)
        self.assertEqual(result['uncompressed_files_removed'], 2)
        self.assertEqual(result['unique_objects'], 1)
        self.assertTrue((self.root / paths[2]).exists())
        restore(self.root, store, paths[0])
        self.assertEqual(digest(self.root / paths[0]), expected)

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from archive_public_capture import digest, ingest
from audit_replay_trade_delivery import linked_audit
from public_replay_sources import trade_events
from replay_trade_preflight import audit as preflight


class LinkedTradeClockTests(unittest.TestCase):
    def setUp(self):
        d=tempfile.TemporaryDirectory();self.addCleanup(d.cleanup)
        self.root=Path(d.name);self.t=1_800_000_000_000;self.seq=0
        self.venue='lighter';self.leg=0
        self.config=dict(start_ms=self.t,end_ms=self.t+10_000,maker_leg=0,trade_delivery_grace_ms=2500,
            flat_at_funding={'settlement_guard_ms':1000},pair={'legs':[{'venue':'lighter'},{'venue':'lighter_robinhood'}]})

    def trade(self,identity=1,**extra):
        return dict(trade_id=identity,market_id=1,timestamp=self.t,price='100',size='0.1',
                    is_maker_ask=True,**extra)

    def row(self,offset,trades,*,snapshot=False):
        self.seq+=1;rw=(self.t+offset)*1_000_000+321_000;aw=rw+8000
        source='lighter_trades' if self.venue=='lighter' else 'lighter_robinhood_trades'
        return dict(source=source,event='message',received_at_ms=rw//1_000_000,available_at_ms=aw//1_000_000,
            capture_timing=dict(sequence=self.seq,connection_epoch=1,run_id='0'*32,
                receipt_wall_ns=rw,available_wall_ns=aw,receipt_monotonic_ns=offset*1_000_000+321_000,
                available_monotonic_ns=offset*1_000_000+329_000),
            payload=dict(channel='trade:1',type='subscribed/trade' if snapshot else 'update/trade',trades=trades))

    def build(self,data):
        capture=self.root/'capture.ndjson';archive=self.root/'archive';tape=self.root/'tape.ndjson'
        terminal=self.row(10_000,[]);terminal.update(source='collector',event='end',payload={})
        terminal['capture_timing']['connection_epoch']=0
        capture.write_text(''.join(json.dumps(r)+'\n' for r in [*data,terminal]));ingest(capture,archive)
        events=[e for e in trade_events(archive,self.venue,'1',self.leg,precise_times=True,excluded_zero_price_transfers=[])
                if self.config['start_ms']<=e['at_ms']<self.config['end_ms']]
        events.append(dict(kind='end',at_ms=self.config['end_ms']))
        tape.write_text(''.join(json.dumps(r)+'\n' for r in events));return archive,tape

    def check(self,archive,tape):
        return linked_audit(archive,tape,self.config,self.venue,'1',self.leg,expected_tape_sha256=digest(tape))

    def test_clock_bridge_preserves_precise_receipt_and_upward_projection(self):
        a,t=self.build([self.row(1,[],snapshot=True),self.row(100,[self.trade(transaction_time=self.t*1000+100123)])])
        r=self.check(a,t);c=r['components_us']['all_nonprestart']
        self.assertEqual(c['availability_minus_block_us']['sum_us'],101000)
        self.assertEqual([c[k]['sum_us'] for k in ('execution_minus_block_us','receipt_minus_execution_us',
            'availability_minus_receipt_us','projection_rounding_us')],[100123,198,8,671])
        self.assertFalse(r['guard_calibrated']);self.assertFalse(r['execution_qualified'])
        self.assertFalse(r['source_timestamp_substituted'])
        for forbidden in ('trade_id','"price"','"contracts"','"run_id"'):
            self.assertNotIn(forbidden,json.dumps(r))

    def test_legacy_late_classification_is_not_repaired_by_execution_metadata(self):
        a,t=self.build([self.row(1,[],snapshot=True),self.row(2600,[self.trade(transaction_time=(self.t+2599)*1000)])])
        r=self.check(a,t)
        self.assertEqual(r['counts']['legacy_bound_violations'],1)
        self.assertEqual(r['counts']['legacy_violation_execution_within_bound'],1)
        self.assertFalse(preflight(t,self.config)['trade_timing_admitted'])
        self.assertFalse(r['source_timestamp_substituted'])

    def test_boundary_is_inclusive_and_one_ms_beyond_is_rejected(self):
        a,t=self.build([self.row(1,[],snapshot=True),self.row(2499,[self.trade(1)]),self.row(2500,[self.trade(2)])])
        r=self.check(a,t)
        self.assertEqual(r['counts']['legacy_bound_violations'],1)
        self.assertEqual(r['first_violation_at_ms'],self.t+2501)

    def test_prestart_block_and_missing_execution_time_remain_distinct(self):
        old=self.trade(1,transaction_time=(self.t+50)*1000);old['timestamp']=self.t-1
        a,t=self.build([self.row(1,[],snapshot=True),self.row(100,[old,self.trade(2),self.trade(3,transaction_time=True)])])
        r=self.check(a,t);c=r['counts']
        self.assertEqual(c['ignored_prestart_prints'],1)
        self.assertEqual(c['prestart_block_execution_after_start'],1)
        self.assertEqual(c['missing_execution_time'],1);self.assertEqual(c['invalid_execution_time'],1)
        self.assertEqual(r['components_us']['all_nonprestart']['execution_minus_block_us']['count'],0)

    def test_future_execution_and_negative_components_are_not_clamped(self):
        a,t=self.build([self.row(1,[],snapshot=True),self.row(100,[self.trade(transaction_time=(self.t+102)*1000)])])
        r=self.check(a,t)
        self.assertEqual(r['counts']['execution_after_receipt'],1)
        self.assertEqual(r['counts']['execution_bound_violations'],1)
        self.assertLess(r['components_us']['all_nonprestart']['receipt_minus_execution_us']['min_us'],0)

    def test_snapshot_duplicate_transfer_and_metadata_conflict_are_explicit(self):
        zero=self.trade(3);zero['price']='0'
        x=self.trade(2,transaction_time=self.t*1000+50000)
        a,t=self.build([self.row(1,[self.trade(1)],snapshot=True),self.row(100,[x,zero]),
            self.row(101,[x|{'transaction_time':self.t*1000+50001}])])
        c=self.check(a,t)['counts']
        self.assertEqual((c['matched_prints'],c['snapshot_records'],c['duplicate_records'],
                          c['excluded_zero_price_transfers'],c['conflicting_clock_metadata']),(1,1,1,1,1))

    def test_mismatched_price_clock_or_omitted_record_fails_lineage(self):
        a,t=self.build([self.row(1,[],snapshot=True),self.row(100,[self.trade()])]);original=t.read_text()
        for field,value in [('price','101'),('source_ms',self.t+1),('leg',False)]:
            events=[json.loads(x) for x in original.splitlines()]
            next(x for x in events if x['kind']=='trade')[field]=value
            t.write_text(''.join(json.dumps(x)+'\n' for x in events))
            with self.subTest(field=field),self.assertRaises(ValueError):self.check(a,t)
        t.write_text('\n'.join(x for x in original.splitlines() if json.loads(x)['kind']!='trade')+'\n')
        with self.assertRaises(ValueError):self.check(a,t)

    def test_wrong_domain_and_expected_hash_fail_closed(self):
        a,t=self.build([self.row(1,[],snapshot=True),self.row(100,[self.trade()])])
        with self.assertRaises(ValueError):
            linked_audit(a,t,self.config,'lighter_robinhood','1',0,expected_tape_sha256=digest(t))
        with self.assertRaises(ValueError):
            linked_audit(a,t,self.config,self.venue,'1',0,expected_tape_sha256='0'*64)

    def test_robinhood_has_its_own_domain_and_leg(self):
        self.venue='lighter_robinhood';self.leg=1
        a,t=self.build([self.row(1,[],snapshot=True),self.row(100,[self.trade()])])
        r=self.check(a,t);self.assertEqual(r['venue'],self.venue);self.assertEqual(r['leg'],1)
        self.assertEqual(r['counts']['matched_prints'],1)
        self.assertEqual(r['projection_status'], 'matched_prints')
        self.assertTrue(r['projection_identity_verified'])

    def test_empty_projections_distinguish_missing_domain_market_and_observed_channel(self):
        archive, tape = self.build([self.row(1, [self.trade()], snapshot=True)])
        for venue, leg, market, expected, messages in [
            ('lighter_robinhood', 1, '1', 'no_venue_trade_shards', 0),
            ('lighter', 0, '8', 'no_market_messages_in_window', 0),
            ('lighter', 0, '1', 'market_messages_without_projected_prints', 1),
        ]:
            with self.subTest(expected=expected):
                result = linked_audit(archive, tape, self.config, venue, market, leg,
                                      expected_tape_sha256=digest(tape))
                self.assertFalse(result['projection_identity_verified'])
                self.assertEqual(result['projection_status'], expected)
                self.assertEqual(result['counts']['matched_prints'], 0)
                self.assertEqual(result['counts']['market_channel_messages_in_window'], messages)

    def test_pre_window_subscription_does_not_count_as_in_window_evidence(self):
        self.config['start_ms'] += 100
        archive, tape = self.build([self.row(1, [], snapshot=True)])
        result = self.check(archive, tape)
        self.assertEqual(result['projection_status'], 'no_market_messages_in_window')

    def test_cli_reports_empty_projection_consistently(self):
        archive, tape = self.build([self.row(1, [], snapshot=True)])
        cfg, output = self.root/'config.json', self.root/'result.json'
        cfg.write_text(json.dumps(self.config))
        cli = Path(__file__).with_name('audit_replay_trade_delivery.py')
        stdout = subprocess.check_output([sys.executable, str(cli), str(cfg), str(tape),
            str(archive), self.venue, '1', '0', str(output)], text=True)
        result = json.loads(output.read_text())['linked']
        status = json.loads(stdout)
        self.assertEqual(status['projection_status'], result['projection_status'])
        self.assertFalse(status['projection_identity_verified'])


if __name__=='__main__':unittest.main()

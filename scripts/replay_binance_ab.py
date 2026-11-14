#!/usr/bin/env python3
"""Freeze a future ten-hour local-price / Binance-risk-filter comparison.

Public data and offline engines only. No source-time backdating, fitted
thresholds, assumed rewards, account requests or live execution.
"""
import argparse
from collections import Counter
from decimal import Decimal
from heapq import merge
import json
import math
import os
from pathlib import Path
import time
import tomllib

from archive_public_capture import compile_archive, digest, records, verify_compiled
from capture_timing import precise_available_ms
from public_replay_sources import trade_events
from replay_local_participation import summarize_local
from replay_maker_campaign import compile_book, load, unseal
from replay_ten_hour_scenarios import quant_policy, TEN_HOURS
import subprocess

FILTER = {'lookback_ms': 1000, 'max_age_ms': 500, 'shock_bps': 1.0}
REFERENCE_SAMPLE_MS = 25
DOMAINS = ('lighter_robinhood', 'lighter')


def save_new(path, value):
    with path.open('x') as stream:
        json.dump(value, stream, indent=2, allow_nan=False)
        stream.write('\n')


def reference_events(archive, symbol, stats):
    """Malformed/missing source evidence invalidates the signal, not its clock."""
    previous_epoch, previous_id = 0, 0
    for raw in records(archive, source='binance'):
        row = json.loads(raw)
        if row.get('market') != symbol:
            continue
        at = precise_available_ms(row)
        event = {'kind': 'binance', 'at_ms': at, 'observed_ms': at,
                 'epoch': row['capture_timing']['connection_epoch'], 'mid': None, 'update_id': None}
        stats['records'] += 1
        epoch = event['epoch']
        epoch_regressed = epoch < previous_epoch
        if epoch > previous_epoch:
            previous_epoch, previous_id = epoch, 0
        if row['event'] == 'message':
            p = row['payload']
            valid = (p.get('e') == 'bookTicker' and p.get('s') == symbol and p.get('st', 1) == 1
                     and all(type(p.get(k)) is int and p[k] > 0 for k in ('E', 'T', 'u')))
            if valid:
                # A diagnostic bound, not measured network latency. Exchange
                # clocks are uncalibrated; use local availability for decisions.
                valid = (p['T'] <= p['E'] and -50 <= row['received_at_ms'] - p['T'] <= 2000
                         and -50 <= row['received_at_ms'] - p['E'] <= 2000)
            try:
                bid, ask, bsize, asize = (float(p[k]) for k in ('b', 'a', 'B', 'A'))
                valid = valid and all(math.isfinite(x) and x > 0 for x in (bid, ask, bsize, asize)) and bid < ask
            except (KeyError, ValueError, TypeError):
                valid = False
            if valid and (epoch_regressed or p['u'] <= previous_id):
                valid = False
                stats['sequence_rejections'] += 1
            if valid:
                previous_id = p['u']
                event.update(mid=bid / 2 + ask / 2, update_id=p['u'])
                stats['valid_bbo'] += 1
            else:
                stats['invalid_bbo'] += 1
        else:
            stats[row['event']] += 1
        yield event


def sampled_reference(events, interval=REFERENCE_SAMPLE_MS):
    """Publish the last observation only after its 25 ms bucket has closed.

    Receipt/availability before coalescing survives as observed_ms for aging.
    Disconnects and invalid evidence are immediate and erase pending quotes.
    """
    pending, boundary = None, None
    for event in events:
        at = event['at_ms']
        if pending is not None and boundary <= at:
            yield pending | {'at_ms': boundary}
            pending = None
        if event['mid'] is None:
            pending = None
            yield event
        else:
            pending = event
            boundary = (at // interval + 1) * interval
    if pending is not None:
        yield pending | {'at_ms': boundary}


def local_books(compiled, domain):
    with unseal(compiled) as stream:
        header = json.loads(next(stream))
        if header.get('independent_books') is not True:
            raise ValueError('local A/B requires independent book semantics')
        legs = header['pair']['legs']
        matches = [i for i, leg in enumerate(legs) if leg['venue'] == domain and leg['market'] == 'ETH']
        if len(matches) != 1:
            raise ValueError('compiled tape has an ambiguous target domain')
        for line in stream:
            frame = json.loads(line)
            yield {'kind': 'book', 'at_ms': frame['now_ms'],
                   'lighter': compile_book(frame['legs'][matches[0]]['book'])}


def write_input(archive, compiled, domain, market_id, window, output):
    lineage = verify_compiled(archive, compiled)
    if lineage.get('availability_rule') != 'ceil_available_wall_ns_to_ms':
        raise ValueError('A/B requires precise same-process compilation')
    stats, counts = Counter(), Counter()
    first, stop = window['warmup_start_ms'], window['drain_until_ms']
    timeline = merge(local_books(compiled, domain),
                     trade_events(archive, domain, str(market_id), 0, precise_times=True),
                     sampled_reference(reference_events(archive, 'ETHUSDT', stats)),
                     key=lambda row: (row['at_ms'], {'trade_feed': 0, 'trade': 1, 'book': 2, 'binance': 3}[row['kind']]))
    online, initialized, last, previous = False, False, None, None
    first_book, last_book = None, None
    with output.open('x') as stream:
        def emit(row):
            counts[row['kind']] += 1
            stream.write(json.dumps(row, separators=(',', ':'), allow_nan=False) + '\n')
        for row in timeline:
            at = row['at_ms']
            if previous is not None and at < previous:
                raise ValueError('merged input clock regressed')
            previous = at
            if row['kind'] == 'book':
                first_book = at if first_book is None else first_book
                last_book = at
            if at < first:
                if row['kind'] == 'trade_feed':
                    online = row['online']
                continue
            if at >= stop:
                continue  # Still exhaust and verify every archive.
            if not initialized:
                emit({'kind': 'trade_feed', 'at_ms': first, 'online': online})
                initialized = True
            if row['kind'] == 'trade':
                size = float(row['contracts'])
                row = {k: v for k, v in row.items() if k not in ('leg', 'contracts')}
                row['size'] = size
                # Public extractor returns exact decimal text; matching uses f64.
                row['price'] = float(row['price'])
            elif row['kind'] == 'trade_feed':
                row = {k: v for k, v in row.items() if k != 'leg'}
            if row['kind'] == 'binance' and row['mid'] is not None:
                counts['valid_reference_in_window'] += 1
            emit(row)
            last = at
    if (first_book is None or first_book > first or last_book < stop
            or last is None or stop - last > 5000 or not counts['trade'] or not counts['valid_reference_in_window']):
        raise ValueError('incomplete ten-hour local/reference/trade coverage')
    return {'events': dict(counts), 'binance': dict(stats), 'input_sha256': digest(output),
            'capture_run_id': lineage['capture_run_id'], 'availability_rule': lineage['availability_rule'],
            'equal_ms_order': 'trade_feed, trade, local_book, binance; no source-time backdating',
            'trade_source_time': 'legacy block timestamp; execution-time metadata retained in raw archive',
            'source_clock_execution_verified': False, 'execution_qualified': False}


def market(metadata, symbol):
    rows = [m for m in metadata['order_books'] if m['symbol'] == symbol and m.get('market_type', 'perp') == 'perp']
    if len(rows) != 1:
        raise ValueError('ambiguous perpetual identity')
    m = rows[0]
    return {'id': m['market_id'], 'price_decimals': m['supported_price_decimals'],
            'size_decimals': m['supported_size_decimals'], 'min_base_amount': m['min_base_amount'],
            'min_quote_amount': m['min_quote_amount']}


def freeze(args):
    now = time.time_ns() // 1_000_000
    if args.start_ms < now + 120_000:
        raise ValueError('freeze at least two minutes before a future decision window')
    out = args.output.resolve()
    out.mkdir(mode=0o700)
    window = {'warmup_start_ms': args.start_ms - 60_000, 'start_ms': args.start_ms,
              'end_ms': args.start_ms + TEN_HOURS, 'drain_until_ms': args.start_ms + TEN_HOURS + 15_000}
    base = tomllib.loads(args.base.read_text())['inventory_neutral_mm']
    profile = load(args.capture_profile)
    if [(p['venue'], p['market']) for p in profile['pair']['legs']] != [('lighter', 'ETH'), ('lighter_robinhood', 'ETH')]:
        raise ValueError('expected ETH Core/RH compile profile')
    profile['reference'] = {'kind': 'missing'}
    profile['sample_ms'] = 100
    profile['paper_entries'] = False
    profile['independent_books'] = True
    profile['pair']['allow_shared_risk_domain'] = True
    profile.pop('trade_from_ms', None)
    save_new(out / 'capture-profile.json', profile)
    markets = {'lighter': market(load(args.core_metadata), 'ETH'),
               'lighter_robinhood': market(load(args.rh_metadata), 'ETH')}
    for i, domain in enumerate(('lighter', 'lighter_robinhood')):
        terms = markets[domain]
        leg = profile['pair']['legs'][i]
        expected = {'tick': Decimal(10) ** -terms['price_decimals'],
                    'lot': Decimal(10) ** -terms['size_decimals'],
                    'min_contracts': Decimal(terms['min_base_amount']),
                    'min_notional': Decimal(terms['min_quote_amount'])}
        unit = 'USDC' if domain == 'lighter' else 'USDG'
        if (profile['legs'][i]['lighter_market_id'] != terms['id']
                or leg['quote'] != unit or leg['settlement'] != unit
                or any(Decimal(leg[k]) != v for k, v in expected.items())):
            raise ValueError('metadata or settlement does not match compiled native market terms')
    arms = {}
    for domain in DOMAINS:
        for capital in (500, 1000):
            config = quant_policy(base, markets[domain], 'ETH', window, capital, capital // 10)
            # Transport is modeled separately from current documented venue
            # delays. Historical frozen experiments retain their old values.
            config.update(core_only=True, diagnose_cross_market=True,
                          order_entry_ms=450 if domain == 'lighter_robinhood' else 250,
                          cancel_transport_ms=250, taker_transport_ms=250,
                          venue_cancel_delay_ms=200 if domain == 'lighter_robinhood' else 300)
            for enabled, arm in ((False, 'A'), (True, 'B')):
                label = f'{domain}-{capital}-{arm}'
                path = out / (label + '.config.json')
                save_new(path, config | {'binance_risk_filter': FILTER | {'enabled': enabled}})
                arms[label] = {'domain': domain, 'capital_unit': 'USDG' if domain == 'lighter_robinhood' else 'USDC', 'arm': arm, 'capital': capital, 'config_sha256': digest(path)}
    files = {str(p.resolve()): digest(p) for p in
             [args.engine, args.compiler, args.collector, args.base, args.capture_profile,
              args.core_metadata, args.rh_metadata, *Path(__file__).parent.glob('*.py')]}
    policy = {'schema': 1, 'experiment': 'prospective_binance_directional_risk_ab',
              'program': 'quant_profit', 'primary_domain': 'lighter_robinhood',
              'control_domain': 'lighter', 'symbol': 'ETH', 'binance_symbol': 'ETHUSDT',
              'frozen_at_ms': now, 'window': window, 'filter': FILTER,
              'engine': str(args.engine.resolve()), 'compiler': str(args.compiler.resolve()),
              'markets': markets, 'arms': arms, 'files_sha256': files,
              'capture_profile_sha256': digest(out / 'capture-profile.json'),
              'holdout_intent': True, 'execution_qualified': False, 'rewards_assumed': False,
              'signal': 'Binance own-symbol short return; no cross-quote price parity assumption',
              'reference_failure': 'withdraw new risk; preserve possible inventory reductions',
              'source_time_skew_admission_ms': [-50, 2000],
              'reference_sample_ms': REFERENCE_SAMPLE_MS,
              'latency_source': '2026-11-14 account-types audit; network 250ms is uncalibrated',
              'rate_limit_execution_modeled': False,
              'limitations': ['public FIFO model; no private execution calibration',
                             'no live rate-limit or volume-quota simulation',
                             'reference timing does not establish exchange clock synchronization']}
    save_new(out / 'policy.json', policy)
    return {'frozen_policy_sha256': digest(out / 'policy.json'), 'window': window, 'arms': len(arms)}


def validate_frozen(root, policy):
    if any(digest(Path(p)) != sha for p, sha in policy['files_sha256'].items()):
        raise ValueError('frozen dependency changed')
    if digest(root / 'capture-profile.json') != policy['capture_profile_sha256']:
        raise ValueError('frozen compiler configuration changed')
    for label, arm in policy['arms'].items():
        if digest(root / (label + '.config.json')) != arm['config_sha256']:
            raise ValueError('frozen strategy configuration changed')


def run(args):
    root = args.frozen.resolve()
    policy = load(root / 'policy.json')
    policy_hash = digest(root / 'policy.json')
    validate_frozen(root, policy)
    out = root / 'results'
    out.mkdir(mode=0o700)
    compiled = out / 'ETH.ndjson.zst'
    compile_archive(args.archive, Path(policy['compiler']), root / 'capture-profile.json', compiled, precise_times=True)
    evidence, inputs, runs = {}, {}, {}
    for domain in DOMAINS:
        inputs[domain] = out / (domain + '.ndjson')
        evidence[domain] = write_input(args.archive, compiled, domain, policy['markets'][domain]['id'], policy['window'], inputs[domain])
    save_new(out / 'input-evidence.json', evidence)
    for label, arm in policy['arms'].items():
        result = out / (label + '.run.json')
        with result.open('x') as stdout, result.with_suffix('.stderr').open('x') as stderr:
            try:
                process = subprocess.run([policy['engine'], str(root / (label + '.config.json')), str(inputs[arm['domain']])],
                                         stdout=stdout, stderr=stderr, timeout=3600)
                code = process.returncode
                raw = load(result) if code == 0 else {'incomplete': 'engine_exit_failure'}
            except subprocess.TimeoutExpired:
                code, raw = None, {'incomplete': 'execution_timeout'}
        raw['engine_exit_code'] = code
        summary = summarize_local(raw)
        gate = raw.get('binance_participation', {})
        summary.update(binance_participation=gate,
                       fresh_reference_fraction=(gate.get('fresh_reference_decisions', 0) / gate['decisions'] if gate.get('decisions') else None))
        runs[label] = summary
        save_new(out / (label + '.summary.json'), summary)
    validate_frozen(root, policy)
    if policy_hash != digest(root / 'policy.json') or any(digest(inputs[d]) != evidence[d]['input_sha256'] for d in inputs):
        raise ValueError('frozen policy or replay tape changed')
    complete = all(r.get('net_after_overhead') is not None for r in runs.values())
    save_new(out / 'report.json', {'policy_sha256': policy_hash, 'source_evidence': evidence, 'runs': runs,
             'comparison_complete': complete, 'execution_qualified': False,
             'profitability_established': False, 'rewards_assumed': False})
    return {'comparison_complete': complete, 'arms': len(runs)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    f = commands.add_parser('freeze')
    for name in ('engine', 'compiler', 'collector', 'capture-profile', 'base', 'core-metadata', 'rh-metadata', 'output'):
        f.add_argument('--' + name, type=Path, required=True)
    f.add_argument('--start-ms', type=int, required=True)
    r = commands.add_parser('run')
    r.add_argument('frozen', type=Path)
    r.add_argument('archive', type=Path)
    args = parser.parse_args()
    os.umask(0o077)
    print(json.dumps(freeze(args) if args.command == 'freeze' else run(args)))


if __name__ == '__main__':
    main()

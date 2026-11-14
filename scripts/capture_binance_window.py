#!/usr/bin/env python3
"""Run a separately frozen public capture after an existing validation window.

Only starts/stops its own collector. Leaves all prior experiments and partial
evidence intact. Sealed data is ready for replay_binance_ab.py after admission;
the large archive/replay workspace can be on another machine.
"""
import argparse
from collections import Counter
import gzip
from hashlib import file_digest
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import time


def digest(path):
    with path.open('rb') as stream:
        return file_digest(stream, 'sha256').hexdigest()


def save_event(root, value):
    with (root / 'campaign-events.ndjson').open('a') as stream:
        stream.write(json.dumps({'at_ms': time.time_ns() // 1_000_000, **value}) + '\n')
        stream.flush()
        os.fsync(stream.fileno())


def run(root):
    plan_path = root / 'capture-plan.json'
    plan_hash = digest(plan_path)
    plan = json.loads(plan_path.read_text())
    binary = root / 'perp-capture'
    if (digest(binary) != plan['collector_sha256']
            or plan['capture_until_ms'] <= plan['capture_from_ms']
            or plan['capture_until_ms'] - plan['capture_from_ms'] > 86_400_000
            or plan['capture_from_ms'] <= plan['protected_until_ms']
            or plan['validation_start_ms'] - plan['capture_from_ms'] < 60_000
            or plan['capture_until_ms'] < plan['validation_end_ms'] + 15_000
            or plan['validation_end_ms'] - plan['validation_start_ms'] != 36_000_000
            or plan['disk_reserve_bytes'] < 14 * 2**30):
        raise ValueError('invalid capture plan or collector identity')
    protected = Path(plan['protected_policy'])
    if digest(protected) != plan['protected_policy_sha256']:
        raise ValueError('protected experiment identity changed')
    # Exclusive lock survives a crash as evidence; never restart into the same
    # file or silently reset an incomplete validation unit.
    with (root / 'capture-driver.lock').open('x') as lock:
        lock.write(str(os.getpid()))
    save_event(root, {'state': 'scheduled', 'plan_sha256': plan_hash})
    while time.time_ns() // 1_000_000 < plan['capture_from_ms']:
        time.sleep(max(0.001, min(30, (plan['capture_from_ms'] - time.time_ns() // 1_000_000) / 1000)))
    now = time.time_ns() // 1_000_000
    if now > plan['validation_start_ms'] - 60_000:
        raise ValueError('missed full pre-window warmup; do not reset window')
    if digest(plan_path) != plan_hash or digest(binary) != plan['collector_sha256']:
        raise ValueError('capture plan changed while scheduled')
    if shutil.disk_usage(root).free < plan['disk_reserve_bytes'] + 256 * 2**20:
        raise ValueError('insufficient disk reserve to start capture')
    partial = root / 'public.ndjson.gz.partial'
    # ETH drives the frozen A/B. RWA streams remain discovery observations
    # pending multiplier/session checks; no stock price is fed into ETH.
    command = [str(binary), '-lighter', '0', '-lighter-trades', '-lighter-robinhood', '0',
               '-lighter-robinhood-trades', '-lighter-clock-refresh', '4m',
               '-binance', 'ETHUSDT,NVDAUSDT,SNDKUSDT',
               '-arcus-ws', 'ETH-USD,BTC-USD', '-bulk-ws', 'ETH-USD,BTC-USD',
               '-hyperliquid', 'io:NVDA,io:SNDK,xyz:NVDA,xyz:SNDK', '-hl-fast-book', '-hl-trades',
               '-duration', str(plan['capture_until_ms'] - now) + 'ms', '-gzip', '-output', str(partial)]
    env = dict(os.environ, GOMAXPROCS='1')
    reason = None
    with (root / 'capture.log').open('xb') as log:
        process = subprocess.Popen(['nice', '-n', '15', 'ionice', '-c', '3', *command],
                                   stdout=log, stderr=log, env=env, start_new_session=True)
        save_event(root, {'state': 'capturing', 'pid': process.pid})
        while process.poll() is None:
            if shutil.disk_usage(root).free < plan['disk_reserve_bytes']:
                reason = 'disk_reserve'
            elif partial.exists() and partial.stat().st_size > plan['max_capture_bytes']:
                reason = 'capture_size_budget'
            if reason:
                # This process group was created by this driver. Never inspect
                # or stop another campaign's collector or replay engine.
                os.killpg(process.pid, signal.SIGINT)
                try:
                    process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGTERM)
                break
            time.sleep(5)
        code = process.wait()
    if reason or code != 0:
        save_event(root, {'state': 'incomplete', 'reason': reason, 'exit_code': code})
        return 1
    counts = Counter()
    first = last = None
    with gzip.open(partial, 'rb') as stream:
        for line in stream:
            row = json.loads(line)
            if first is None:
                first = row['available_at_ms']
            last = row
            counts[row['source'] + '/' + row['event']] += 1
    if (not last or last['source'] != 'collector' or last['event'] != 'end'
            or first > plan['validation_start_ms'] - 60_000
            or last['available_at_ms'] < plan['validation_end_ms'] + 15_000):
        raise ValueError('capture has no complete validation window/footer')
    sealed = root / 'public.ndjson.gz'
    os.link(partial, sealed)  # Preserve evidence without duplicating disk blocks.
    summary = {'state': 'sealed_awaiting_replay_admission', 'capture_sha256': digest(sealed),
               'capture_bytes': sealed.stat().st_size, 'first_ms': first,
               'last_ms': last['available_at_ms'], 'counts': dict(counts),
               'plan_sha256': plan_hash,
               'protected_policy_unchanged': digest(protected) == plan['protected_policy_sha256']}
    with (root / 'capture-summary.json').open('x') as file:
        json.dump(summary, file, indent=2)
    save_event(root, summary)
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('campaign', type=Path)
    args = parser.parse_args()
    os.umask(0o077)
    try:
        return run(args.campaign.resolve())
    except Exception as error:
        save_event(args.campaign, {'state': 'failed', 'error_type': type(error).__name__, 'reason': str(error)})
        raise


if __name__ == '__main__':
    raise SystemExit(main())

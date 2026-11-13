#!/usr/bin/env python3
"""Partition a sealed public capture and reproduce its original event stream."""
import argparse
from contextlib import ExitStack, contextmanager
from datetime import datetime, timezone
import gzip
from hashlib import file_digest, sha256
from heapq import merge
import json
import os
from pathlib import Path
import re
import subprocess
from tempfile import TemporaryDirectory

HOUR_MS = 3_600_000
MAX_ROW = 2 * 1024 * 1024
SOURCES = {
    'lighter': ('lighter', 'core'), 'lighter_trades': ('lighter', 'core'),
    'lighter_robinhood': ('lighter', 'robinhood'), 'lighter_robinhood_trades': ('lighter', 'robinhood'),
    'extended': ('extended', 'mainnet'), 'extended_trades': ('extended', 'mainnet'),
    'arcus': ('arcus', 'mainnet'), 'arcus_snapshot': ('arcus', 'mainnet'),
    'bulk': ('bulk', 'mainnet'), 'bulk_snapshot': ('bulk', 'mainnet'),
    'coinbase': ('coinbase', 'spot'), 'collector': ('collector', 'session'),
    **{source: ('hyperliquid', 'core') for source in
       ('hyperliquid', 'hyperliquid_snapshot', 'hyperliquid_trades', 'hyperliquid_funding')},
}


@contextmanager
def open_capture(path):
    if path.suffix == '.gz':
        with gzip.open(path, 'rb') as stream:
            yield stream
    elif path.suffix == '.zst':
        with subprocess.Popen(['zstd', '-qdc', '--', str(path)], stdout=subprocess.PIPE) as process:
            try:
                yield process.stdout
            except BaseException:
                process.terminate()
                raise
            finally:
                process.stdout.close()
            if process.wait() != 0:
                raise ValueError('invalid compressed capture')
    else:
        with path.open('rb') as stream:
            yield stream


def digest(path):
    with path.open('rb') as stream:
        return file_digest(stream, 'sha256').hexdigest()


def verify_compiled(archive, tape):
    evidence = json.loads(tape.with_suffix('.provenance.json').read_text())
    if (evidence['archive_manifest_sha256'] != digest(archive / 'manifest.json')
            or evidence['compressed_sha256'] != digest(tape)):
        raise ValueError('compiled tape does not match the shared public archive')
    return evidence


def route(row):
    source = row['source']
    venue, domain = SOURCES[source]
    if venue == 'hyperliquid':
        market = row.get('market')
        if not market and row['event'] == 'message' and source in ('hyperliquid', 'hyperliquid_snapshot'):
            market = row['payload']['data']['coin']
        if market and ':' in market:
            domain = 'hip3-' + market.split(':', 1)[0]
            if re.fullmatch(r'hip3-[A-Za-z0-9_-]{1,30}', domain) is None:
                raise ValueError('invalid Hyperliquid trading domain')
        elif not market:
            domain = 'shared'
    return venue, domain, source


def rows(stream):
    while raw := stream.readline(MAX_ROW + 1):
        if len(raw) > MAX_ROW or not raw.endswith(b'\n'):
            raise ValueError('invalid capture line size or delimiter')
        yield raw


def ingest(source, root):
    """Seal a new archive by publishing its manifest after all shards close."""
    if '.partial' in source.name or source.name.endswith(('.next', '.tmp')):
        raise ValueError('cannot ingest an active capture')
    root.mkdir(mode=0o700)
    sequence, previous, ended = 0, 0, False
    payload_hash = sha256()
    artifacts = []
    hour = None
    writers = {}
    before = source.stat()
    with open_capture(source) as stream, ExitStack() as stack:
        for raw in rows(stream):
            row = json.loads(raw)
            at, received = row['available_at_ms'], row['received_at_ms']
            if type(at) is not int or type(received) is not int or not 0 < received <= at or at < previous or ended:
                raise ValueError('invalid capture order or footer')
            venue, domain, channel = route(row)
            event = row['event']
            if event not in ('message', 'connected', 'subscribed', 'disconnect', 'clock_refresh', 'end'):
                raise ValueError('unsupported public event')
            if (venue == 'collector') != (event == 'end'):
                raise ValueError('invalid collector footer')
            ended = event == 'end'
            previous = at
            bucket = at // HOUR_MS
            if bucket != hour:
                stack.close()
                writers.clear()
                hour = bucket
            key = venue, domain, channel
            if key not in writers:
                stamp = datetime.fromtimestamp(hour * 3600, timezone.utc).strftime('%Y-%m-%d/%H')
                name = f'{venue}/{domain}/{channel}/{stamp}.ndjson.gz'
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                file = stack.enter_context(path.open('xb'))
                writer = stack.enter_context(gzip.GzipFile(filename='', fileobj=file, mode='wb', mtime=0, compresslevel=3))
                artifact = {'path': name, 'hour': hour, 'records': 0, 'first_ms': at, 'last_ms': at}
                artifacts.append(artifact)
                writers[key] = writer, artifact
            writer, artifact = writers[key]
            writer.write(str(sequence).encode() + b'\t' + raw)
            artifact['records'] += 1
            artifact['last_ms'] = at
            sequence += 1
            payload_hash.update(raw)
    capture_sha = digest(source)
    after = source.stat()
    if not ended or sequence <= 1 or (before.st_ino, before.st_mtime_ns, before.st_size) != (after.st_ino, after.st_mtime_ns, after.st_size):
        raise ValueError('capture incomplete or changed during ingest')
    for artifact in artifacts:
        path = root / artifact['path']
        artifact.update(sha256=digest(path), bytes=path.stat().st_size)
    manifest = {'schema': 1, 'records': sequence, 'stream_sha256': payload_hash.hexdigest(),
                'capture_sha256': capture_sha, 'capture_bytes': before.st_size, 'shards': artifacts}
    temporary = root / 'manifest.next'
    with temporary.open('x') as file:
        json.dump(manifest, file, indent=2)
    temporary.rename(root / 'manifest.json')
    return manifest


def shard_rows(stream):
    previous = -1
    for line in rows(stream):
        index, raw = line.split(b'\t', 1)
        sequence = int(index)
        if sequence <= previous:
            raise ValueError('shard sequence regressed')
        previous = sequence
        yield sequence, raw


def records(root, *, source=None):
    """Merge each hour by its recorded order, including equal timestamps."""
    root = root.resolve(strict=True)
    manifest = json.loads((root / 'manifest.json').read_text())
    if manifest['schema'] != 1:
        raise ValueError('unknown capture archive version')
    hours = {}
    paths = set()
    for shard in manifest['shards']:
        path = root / shard['path']
        if source is not None and Path(shard['path']).parts[2] != source:
            continue
        if path.resolve(strict=True) != path or not path.is_relative_to(root) or path in paths:
            raise ValueError('invalid archive shard path')
        paths.add(path)
        if digest(path) != shard['sha256']:
            raise ValueError('archive shard hash mismatch')
        hours.setdefault(shard['hour'], []).append(path)
    sequence, payload_hash = -1, sha256()
    for hour in sorted(hours):
        with ExitStack() as stack:
            streams = [shard_rows(stack.enter_context(gzip.open(path, 'rb'))) for path in hours[hour]]
            for index, raw in merge(*streams):
                if index <= sequence or source is None and index != sequence + 1:
                    raise ValueError('missing or duplicate archive sequence')
                sequence = index
                if source is None:
                    payload_hash.update(raw)
                yield raw
    if source is None and (sequence + 1 != manifest['records'] or payload_hash.hexdigest() != manifest['stream_sha256']):
        raise ValueError('capture stream hash mismatch')


def compile_archive(archive, engine, config, output):
    """Build a reusable market tape. Publish only after the source verifies."""
    output = output.absolute()
    if not output.name.endswith('.ndjson.zst'):
        raise ValueError('compiled output must end in .ndjson.zst')
    evidence = output.with_suffix('.provenance.json')
    if output.exists() or evidence.exists():
        raise FileExistsError('compiled output already exists')
    archive_id = digest(archive / 'manifest.json')
    engine_id, config_id = digest(engine), digest(config)
    with TemporaryDirectory(dir=output.parent) as directory:
        path = Path(directory)
        tape, compressed, provenance = (path / name for name in ('tape.ndjson', 'tape.ndjson.zst', 'provenance.json'))
        coverage_path = path / 'coverage.json'
        with coverage_path.open('xb') as coverage_output, subprocess.Popen(
                [str(engine.resolve()), 'perp-compile', str(config.resolve()), '/dev/stdin', str(tape)],
                stdin=subprocess.PIPE, stdout=coverage_output) as process:
            try:
                with process.stdin:
                    for raw in records(archive):
                        process.stdin.write(raw)
                if process.wait() != 0:
                    raise RuntimeError('public capture compiler rejected archive')
            except BaseException:
                process.terminate()
                raise
        if (archive_id, engine_id, config_id) != (digest(archive / 'manifest.json'), digest(engine), digest(config)):
            raise ValueError('a compilation input changed')
        subprocess.run(['zstd', '-q', '-T1', '-3', str(tape), '-o', str(compressed)], check=True)
        report = {'archive_manifest_sha256': archive_id, 'engine_sha256': engine_id, 'config_sha256': config_id,
                  'decoded_sha256': digest(tape), 'compressed_sha256': digest(compressed),
                  'coverage': json.loads(coverage_path.read_text())}
        provenance.write_text(json.dumps(report, indent=2) + '\n')
        os.link(compressed, output)
        try:
            os.link(provenance, evidence)
        except BaseException:
            output.unlink()
            raise
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    store = commands.add_parser('ingest')
    store.add_argument('sealed_capture', type=Path)
    store.add_argument('archive', type=Path)
    restore = commands.add_parser('export')
    restore.add_argument('archive', type=Path)
    restore.add_argument('output', type=Path)
    compiler = commands.add_parser('compile')
    for name in ('archive', 'engine', 'config', 'output'):
        compiler.add_argument(name, type=Path)
    args = parser.parse_args()
    os.umask(0o077)
    if args.command == 'ingest':
        result = ingest(args.sealed_capture, args.archive)
        print(json.dumps({'records': result['records'], 'shards': len(result['shards']), 'stream_sha256': result['stream_sha256']}))
    elif args.command == 'compile':
        result = compile_archive(args.archive, args.engine, args.config, args.output)
        print(json.dumps({'decoded_sha256': result['decoded_sha256'], 'coverage': result['coverage']}))
    else:
        with args.output.open('xb') as file:
            try:
                for raw in records(args.archive):
                    file.write(raw)
            except BaseException:
                args.output.unlink()
                raise


if __name__ == '__main__':
    main()

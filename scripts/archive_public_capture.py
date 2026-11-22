#!/usr/bin/env python3
"""Partition a sealed public capture and reproduce its original event stream."""
import argparse
from contextlib import ExitStack, contextmanager
from datetime import datetime, timezone
import fcntl
import gzip
from hashlib import file_digest, sha256
from heapq import merge
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
from tempfile import TemporaryDirectory

HOUR_MS = 3_600_000
MAX_ROW = 2 * 1024 * 1024
PUBLIC_EVENTS = ('message', 'connected', 'subscribed', 'disconnect', 'clock_refresh',
                 'clock_probe', 'segment_start', 'segment_end', 'end')
SOURCES = {
    'binance': ('binance', 'usdm'),
    'lighter': ('lighter', 'core'), 'lighter_trades': ('lighter', 'core'),
    'lighter_robinhood': ('lighter', 'robinhood'), 'lighter_robinhood_trades': ('lighter', 'robinhood'),
    'extended': ('extended', 'mainnet'), 'extended_trades': ('extended', 'mainnet'),
    'extended_index': ('extended', 'mainnet'),
    'arcus': ('arcus', 'mainnet'), 'arcus_snapshot': ('arcus', 'mainnet'),
    'arcus_trades': ('arcus', 'mainnet'),
    'bulk': ('bulk', 'mainnet'), 'bulk_snapshot': ('bulk', 'mainnet'),
    'coinbase': ('coinbase', 'spot'), 'collector': ('collector', 'session'),
    **{source: ('hyperliquid', 'core') for source in
       ('hyperliquid', 'hyperliquid_snapshot', 'hyperliquid_trades', 'hyperliquid_funding')},
}


def validate_clock_probe(row):
    sent = row.get('request_sent_at_ms')
    if row.get('source') not in ('lighter', 'lighter_robinhood'):
        raise ValueError('unexpected clock probe source')
    if not isinstance(row.get('payload'), dict):
        raise ValueError('missing clock probe payload')
    if sent is not None and (type(sent) is not int or not 0 < sent <= row['received_at_ms']):
        raise ValueError('invalid clock probe request bracket')


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
            if event not in PUBLIC_EVENTS:
                raise ValueError('unsupported public event')
            if (venue == 'collector') != (event in ('segment_start', 'segment_end', 'end')):
                raise ValueError('invalid collector footer')
            if event == 'clock_probe':
                validate_clock_probe(row)
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


def verify_stream_cache(archive):
    archive = archive.resolve(strict=True)
    stream_zst = archive / 'verified_stream.ndjson.zst'
    meta_path = archive / 'verified_stream.json'
    if not stream_zst.is_file() or not meta_path.is_file():
        return None
    manifest = json.loads((archive / 'manifest.json').read_text())
    meta = json.loads(meta_path.read_text())
    if (meta.get('schema') != 1
            or meta.get('archive_manifest_sha256') != digest(archive / 'manifest.json')
            or meta.get('stream_sha256') != manifest['stream_sha256']
            or meta.get('records') != manifest['records']
            or meta.get('compressed_sha256') != digest(stream_zst)):
        raise ValueError('verified stream cache hash mismatch')
    return meta


def prepare_verified_stream(archive, *, raw_capture=None):
    """Materialize a single cryptographically verified Zstd stream for fast multi-pair compilation."""
    archive = archive.resolve(strict=True)
    lock_fd = os.open(archive / '.verified_stream.lock', os.O_CREAT | os.O_RDWR, 0o600)
    try:
        fcntl.flock(lock_fd, fcntl.LOCK_EX)
        cached = verify_stream_cache(archive)
        if cached is not None:
            return cached
        manifest_path = archive / 'manifest.json'
        manifest = json.loads(manifest_path.read_text())
        if manifest.get('schema') != 1:
            raise ValueError('unknown capture archive version')
        manifest_id = digest(manifest_path)
        stream_zst = archive / 'verified_stream.ndjson.zst'
        meta_path = archive / 'verified_stream.json'
        with TemporaryDirectory(dir=archive) as directory:
            tmp_dir = Path(directory)
            tmp_zst = tmp_dir / 'verified_stream.ndjson.zst'
            tmp_meta = tmp_dir / 'verified_stream.json'
            with subprocess.Popen(
                    ['zstd', '-q', '-T0', '-3', '-o', str(tmp_zst)],
                    stdin=subprocess.PIPE) as zstd_proc:
                try:
                    assert zstd_proc.stdin is not None
                    with zstd_proc.stdin:
                        if raw_capture is not None:
                            raw_capture = Path(raw_capture).resolve(strict=True)
                            if digest(raw_capture) != manifest['capture_sha256']:
                                raise ValueError('raw capture sha256 does not match archive manifest')
                            count, payload_hash = 0, sha256()
                            if raw_capture.suffix == '.gz' and shutil.which('gzip'):
                                with subprocess.Popen(['gzip', '-qdc', '--', str(raw_capture)],
                                                      stdout=subprocess.PIPE) as gz_proc:
                                    try:
                                        assert gz_proc.stdout is not None
                                        for raw in rows(gz_proc.stdout):
                                            count += 1
                                            payload_hash.update(raw)
                                            zstd_proc.stdin.write(raw)
                                    except BaseException:
                                        gz_proc.terminate()
                                        raise
                                    finally:
                                        if gz_proc.stdout is not None:
                                            gz_proc.stdout.close()
                                    if gz_proc.wait() != 0:
                                        raise ValueError('gzip decompression failed on raw capture')
                            else:
                                with open_capture(raw_capture) as src:
                                    for raw in rows(src):
                                        count += 1
                                        payload_hash.update(raw)
                                        zstd_proc.stdin.write(raw)
                            if count != manifest['records'] or payload_hash.hexdigest() != manifest['stream_sha256']:
                                raise ValueError('raw capture stream hash mismatch')
                        else:
                            for raw in records(archive):
                                zstd_proc.stdin.write(raw)
                    if zstd_proc.wait() != 0:
                        raise RuntimeError('zstd compression failed while building verified stream')
                except BaseException:
                    zstd_proc.terminate()
                    raise
            if digest(manifest_path) != manifest_id:
                raise ValueError('archive manifest changed during stream preparation')
            meta = {
                'schema': 1,
                'archive_manifest_sha256': manifest_id,
                'stream_sha256': manifest['stream_sha256'],
                'compressed_sha256': digest(tmp_zst),
                'records': manifest['records'],
            }
            tmp_meta.write_text(json.dumps(meta, indent=2) + '\n')
            os.link(tmp_zst, stream_zst)
            try:
                os.link(tmp_meta, meta_path)
            except BaseException:
                stream_zst.unlink()
                raise
            return meta
    finally:
        os.close(lock_fd)


def compile_archive(archive, engine, config, output, *, precise_times=False, raw_capture=None):
    """Build a reusable market tape. Publish only after the source verifies."""
    output = output.absolute()
    if not output.name.endswith('.ndjson.zst'):
        raise ValueError('compiled output must end in .ndjson.zst')
    evidence = output.with_suffix('.provenance.json')
    if output.exists() or evidence.exists():
        raise FileExistsError('compiled output already exists')
    if raw_capture is not None:
        prepare_verified_stream(archive, raw_capture=raw_capture)
    cached_stream = verify_stream_cache(archive)
    archive_id = digest(archive / 'manifest.json')
    engine_id, config_id = digest(engine), digest(config)
    output.parent.mkdir(parents=True, exist_ok=True)
    with TemporaryDirectory(dir=output.parent) as directory:
        path = Path(directory)
        tape, compressed, provenance = (path / name for name in ('tape.ndjson', 'tape.ndjson.zst', 'provenance.json'))
        coverage_path = path / 'coverage.json'
        cmd = [str(engine.resolve()), 'perp-compile']
        if precise_times:
            cmd.append('--precise-times')
        cmd.extend([str(config.resolve()), '-', str(tape)])
        if cached_stream is not None:
            stream_zst = (archive / 'verified_stream.ndjson.zst').resolve(strict=True)
            with coverage_path.open('xb') as coverage_output, subprocess.Popen(
                    ['zstd', '-qdc', '--', str(stream_zst)],
                    stdout=subprocess.PIPE) as zstd_proc:
                assert zstd_proc.stdout is not None
                with subprocess.Popen(
                        cmd, stdin=zstd_proc.stdout, stdout=coverage_output) as process:
                    zstd_proc.stdout.close()
                    try:
                        rc_compile = process.wait()
                        rc_zstd = zstd_proc.wait()
                        if rc_compile != 0 or rc_zstd != 0:
                            raise RuntimeError('public capture compiler rejected archive')
                    except BaseException:
                        process.terminate()
                        zstd_proc.terminate()
                        raise
        else:
            with coverage_path.open('xb') as coverage_output, subprocess.Popen(
                    cmd, stdin=subprocess.PIPE, stdout=coverage_output) as process:
                try:
                    assert process.stdin is not None
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
        coverage_obj = json.loads(coverage_path.read_text())
        availability_rule = coverage_obj.pop('availability_rule', None)
        capture_run_id = coverage_obj.pop('capture_run_id', None)
        precise_records = coverage_obj.pop('precise_records', None)
        report = {'archive_manifest_sha256': archive_id, 'engine_sha256': engine_id, 'config_sha256': config_id,
                  'decoded_sha256': digest(tape), 'compressed_sha256': digest(compressed),
                  'coverage': coverage_obj}
        if precise_times:
            report.update(availability_rule=availability_rule or 'ceil_available_wall_ns_to_ms',
                          capture_run_id=capture_run_id,
                          precise_records=precise_records)
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
    prep = commands.add_parser('prepare-stream')
    prep.add_argument('archive', type=Path)
    prep.add_argument('--raw-capture', type=Path, default=None)
    restore = commands.add_parser('export')
    restore.add_argument('archive', type=Path)
    restore.add_argument('output', type=Path)
    compiler = commands.add_parser('compile')
    for name in ('archive', 'engine', 'config', 'output'):
        compiler.add_argument(name, type=Path)
    compiler.add_argument('--precise-times', action='store_true')
    compiler.add_argument('--raw-capture', type=Path, default=None)
    args = parser.parse_args()
    os.umask(0o077)
    if args.command == 'ingest':
        result = ingest(args.sealed_capture, args.archive)
        print(json.dumps({'records': result['records'], 'shards': len(result['shards']), 'stream_sha256': result['stream_sha256']}))
    elif args.command == 'prepare-stream':
        result = prepare_verified_stream(args.archive, raw_capture=args.raw_capture)
        print(json.dumps(result))
    elif args.command == 'compile':
        result = compile_archive(args.archive, args.engine, args.config, args.output,
                                 precise_times=args.precise_times, raw_capture=args.raw_capture)
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

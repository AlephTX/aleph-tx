#!/usr/bin/env python3
"""Compact explicitly selected, byte-for-byte rebuilt replay inputs."""
import argparse
from hashlib import file_digest
import json
import os
from pathlib import Path
import re
import subprocess
from tempfile import NamedTemporaryFile


def digest(path):
    with path.open('rb') as file:
        return file_digest(file, 'sha256').hexdigest()


def decoded_digest(path):
    with subprocess.Popen(['zstd', '-qdc', '--', str(path)], stdout=subprocess.PIPE) as process:
        result = file_digest(process.stdout, 'sha256').hexdigest()
        if process.wait() != 0:
            raise ValueError('invalid compressed replay input')
        return result


def sync(path):
    fd = os.open(path, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def input_path(root, name, *, exists=True):
    path = root / name
    resolved = path.resolve()
    if (not path.is_relative_to(root) or '..' in path.relative_to(root).parts
            or not resolved.is_relative_to(root) or path.is_symlink()
            or path.parent.name != 'inputs' or path.suffix != '.ndjson'
            or 'results' not in path.relative_to(root).parts
            or resolved.parent.name != 'inputs' or 'results' not in resolved.relative_to(root).parts
            or (exists and not path.is_file())):
        raise ValueError('expected a regular derived input under results/.../inputs')
    return resolved


def store_object(path, object_path, expected):
    if not object_path.exists():
        with NamedTemporaryFile(dir=object_path.parent) as tmp:
            subprocess.run(['zstd', '-q', '-T1', '-3', '-f', str(path), '-o', tmp.name], check=True)
            if decoded_digest(Path(tmp.name)) != expected:
                raise ValueError('compressed input differs from rebuilt hash')
            sync(Path(tmp.name))
            os.link(tmp.name, object_path)
            sync(object_path.parent)
    if object_path.is_symlink() or decoded_digest(object_path) != expected:
        raise ValueError('content store hash mismatch')


def compact(root, proof, store, inputs):
    root, store = root.resolve(strict=True), store.absolute()
    if not inputs or len(set(inputs)) != len(inputs):
        raise ValueError('provide distinct input paths')
    if store.resolve() != store or not store.is_relative_to(root) or store.parent.name != 'results':
        raise ValueError('store must be an independent directory under results')
    rebuilt = json.loads(proof.read_text())
    if rebuilt['stage'] != 'complete':
        raise ValueError('rebuild is incomplete')
    hashes = {}
    for asset in rebuilt['assets'].values():
        if not asset['partition_payload_matches'] or not asset['compiled_matches']:
            raise ValueError('upstream capture rebuild differs')
        for item in asset['matched_inputs']:
            sha = item['sha256']
            if not re.fullmatch(r'[0-9a-f]{64}', sha):
                raise ValueError('invalid rebuild digest')
            if item['name'] in hashes and hashes[item['name']] != sha:
                raise ValueError('conflicting rebuilt input digest')
            hashes[item['name']] = sha
    if store.exists() and (any(p.name != 'objects' for p in store.iterdir()) or (store / 'objects').is_symlink()):
        raise FileExistsError('store already published or has unexpected contents')
    store.mkdir(mode=0o700, exist_ok=True)
    objects = store / 'objects'
    objects.mkdir(exist_ok=True)
    aliases = {}
    verified_objects = set()
    for name in inputs:
        path = input_path(root, name)
        expected = hashes.get(path.name)
        if expected is None or digest(path) != expected:
            continue
        obj = objects / f'{expected}.ndjson.zst'
        if expected not in verified_objects:
            store_object(path, obj, expected)
            verified_objects.add(expected)
        aliases[name] = {'sha256': expected, 'bytes': path.stat().st_size}
    manifest = {'schema': 1, 'rebuild_sha256': digest(proof), 'aliases': aliases}
    pending = store / 'manifest.next'
    pending.write_text(json.dumps(manifest, indent=2) + '\n')
    sync(pending)
    pending.rename(store / 'manifest.json')
    sync(store)
    sync(store.parent)
    removed = set()
    removed_bytes = 0
    for name, item in aliases.items():
        path = input_path(root, name, exists=False)
        if path in removed:
            continue
        if digest(path) != item['sha256']:
            continue
        path.unlink()
        removed.add(path)
        removed_bytes += item['bytes']
    return {'aliases_verified': len(aliases), 'uncompressed_files_removed': len(removed),
            'unique_objects': len(verified_objects), 'uncompressed_bytes': removed_bytes,
            'compressed_bytes': sum(path.stat().st_size for path in objects.iterdir())}


def restore(root, store, name):
    path = input_path(root.resolve(strict=True), name, exists=False)
    manifest = json.loads((store / 'manifest.json').read_text())
    if manifest['schema'] != 1:
        raise ValueError('unknown compacted input manifest')
    expected = manifest['aliases'][name]['sha256']
    if not re.fullmatch(r'[0-9a-f]{64}', expected):
        raise ValueError('invalid restore digest')
    obj = store / 'objects' / f'{expected}.ndjson.zst'
    with NamedTemporaryFile(dir=path.parent) as tmp:
        subprocess.run(['zstd', '-qdc', '--', str(obj)], stdout=tmp, check=True)
        if digest(Path(tmp.name)) != expected:
            raise ValueError('restored input hash mismatch')
        sync(Path(tmp.name))
        os.link(tmp.name, path)
    sync(path.parent)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    pack = commands.add_parser('compact')
    for name in ('root', 'proof', 'store', 'paths_json'):
        pack.add_argument(name, type=Path)
    unpack = commands.add_parser('restore')
    unpack.add_argument('root', type=Path)
    unpack.add_argument('store', type=Path)
    unpack.add_argument('path')
    args = parser.parse_args()
    os.umask(0o077)
    if args.command == 'compact':
        print(json.dumps(compact(args.root, args.proof, args.store, json.loads(args.paths_json.read_text()))))
    else:
        restore(args.root, args.store, args.path)


if __name__ == '__main__':
    main()

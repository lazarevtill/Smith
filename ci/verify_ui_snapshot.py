#!/usr/bin/env python3
"""Verify the repository's selected UI snapshot. No Git/network/compiler needed."""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat

REVIEWED_UI_COMMIT = 'adfa145a7bd706f744d17c67bf017c24c1bef57f'
ROOT = Path(__file__).resolve().parents[1]
MAX_FILES, MAX_FILE_BYTES, MAX_TOTAL_BYTES = 4096, 32 * 1024 * 1024, 128 * 1024 * 1024


def independent_file(path):
    metadata = path.lstat()
    if (not stat.S_ISREG(metadata.st_mode) or metadata.st_nlink != 1
            or getattr(metadata, 'st_file_attributes', 0) & 0x400):
        raise ValueError('UI snapshot requires independent regular files, not links/reparse points')
    return metadata


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError('Duplicate JSON field in UI source manifest')
        result[key] = value
    return result


def verify_snapshot(path):
    path = path.absolute()
    for parent in (path, *path.parents):
        metadata = parent.lstat()
        if stat.S_ISLNK(metadata.st_mode) or getattr(metadata, 'st_file_attributes', 0) & 0x400:
            raise ValueError('UI snapshot directory must not traverse links/reparse points')
    path = path.resolve(strict=True)
    source = path / 'SOURCE.json'
    metadata = independent_file(source)
    if metadata.st_size > 2 * 1024 * 1024:
        raise ValueError('UI source manifest exceeds its size limit')
    with source.open('rb') as stream:
        raw = stream.read(2 * 1024 * 1024 + 1)
    if len(raw) != metadata.st_size:
        raise ValueError('UI source manifest changed during validation')
    manifest = json.loads(raw, object_pairs_hook=unique_object)
    if not isinstance(manifest, dict) or set(manifest) - {
            'schema_version', 'original_commit', 'files', 'provenance', 'license_note'}:
        raise ValueError('Unknown UI source manifest fields')
    if type(manifest.get('schema_version')) is not int or manifest['schema_version'] != 1:
        raise ValueError('Unsupported UI source manifest schema')
    if manifest.get('original_commit') != REVIEWED_UI_COMMIT:
        raise ValueError('UI source commit differs from the exact reviewed original revision')
    for name in ('provenance', 'license_note'):
        if name in manifest and (not isinstance(manifest[name], str) or len(manifest[name]) > 4096):
            raise ValueError('Invalid UI source provenance metadata')
    records = manifest.get('files')
    if not isinstance(records, list) or not 1 <= len(records) <= MAX_FILES:
        raise ValueError('Invalid UI source file count')
    expected, portable_names, total = {}, set(), 0
    for item in records:
        if not isinstance(item, dict) or set(item) != {'path', 'bytes', 'sha256'}:
            raise ValueError('Invalid UI source file record')
        name, size, digest = item['path'], item['bytes'], item['sha256']
        if (not isinstance(name, str) or not name or len(name.encode('utf-8')) > 1024
                or '\\' in name or ':' in name or any(not c.isprintable() for c in name)):
            raise ValueError('Invalid UI source file path')
        relative = PurePosixPath(name)
        if (relative.is_absolute() or str(relative) != name or '..' in relative.parts
                or len(relative.parts) > 16 or relative.parts[0] not in ('ui', 'assets')):
            raise ValueError('UI source path must remain inside ui/ or assets/')
        if name.casefold() in portable_names:
            raise ValueError('Duplicate or case-colliding UI source path')
        portable_names.add(name.casefold())
        if type(size) is not int or not 0 <= size <= MAX_FILE_BYTES:
            raise ValueError('Invalid UI source file size')
        if not isinstance(digest, str) or re.fullmatch('[0-9a-f]{64}', digest) is None:
            raise ValueError('Invalid UI source file SHA256')
        total += size
        if total > MAX_TOTAL_BYTES:
            raise ValueError('UI snapshot exceeds its total byte limit')
        expected[name] = item
    if 'ui/app-window.slint' not in expected or not any(p.startswith('assets/') for p in expected):
        raise ValueError('Reviewed UI markup/assets are missing from the manifest')
    observed, directories = set(), 0
    for current, children, files in os.walk(path, followlinks=False):
        for name in children:
            directory = Path(current) / name
            info = directory.lstat()
            directories += 1
            if (directories > MAX_FILES or len(directory.relative_to(path).parts) > 16
                    or not stat.S_ISDIR(info.st_mode)
                    or getattr(info, 'st_file_attributes', 0) & 0x400):
                raise ValueError('Invalid or excessive UI snapshot directories')
        for name in files:
            file = Path(current) / name
            relative = file.relative_to(path).as_posix()
            if relative == 'SOURCE.json':
                continue
            if relative not in expected:
                raise ValueError('UI snapshot contains an undeclared file')
            info = independent_file(file)
            item = expected[relative]
            if info.st_size != item['bytes']:
                raise ValueError('UI snapshot file size differs from manifest')
            h = hashlib.sha256()
            count = 0
            with file.open('rb') as stream:
                while block := stream.read(min(1024 * 1024, item['bytes'] - count + 1)):
                    count += len(block)
                    if count > item['bytes']:
                        raise ValueError('UI snapshot file grew during validation')
                    h.update(block)
            if count != item['bytes'] or h.hexdigest() != item['sha256']:
                raise ValueError('UI snapshot file SHA256 differs from manifest')
            observed.add(relative)
    if observed != set(expected):
        raise ValueError('UI snapshot is missing declared files')
    return path, {'original_commit': REVIEWED_UI_COMMIT,
                  'source_manifest_sha256': hashlib.sha256(raw).hexdigest(),
                  'files': len(observed), 'bytes': total}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('path', nargs='?', type=Path, default=ROOT / 'vendor/smith-ui')
    selected, evidence = verify_snapshot(parser.parse_args().path)
    print(json.dumps({'verified': True, **evidence}))

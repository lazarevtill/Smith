"""Synthetic snapshot validation; no network, compiler, model or application."""
import hashlib
import json
import os
from pathlib import Path
import tempfile
import unittest

from verify_ui_snapshot import REVIEWED_UI_COMMIT, verify_snapshot


class Snapshot(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='smith-ui-snapshot-test-')
        self.root = Path(self.temp.name).resolve()
        (self.root / 'ui').mkdir()
        (self.root / 'assets').mkdir()
        self.contents = {'ui/app-window.slint': b'SYNTHETIC MARKUP', 'assets/example.txt': b'SYNTHETIC ASSET'}
        self.manifest = {'schema_version': 1, 'original_commit': REVIEWED_UI_COMMIT, 'files': []}
        for name, data in self.contents.items():
            (self.root / name).write_bytes(data)
            self.manifest['files'].append({'path': name, 'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest()})
        self.save()

    def tearDown(self):
        self.temp.cleanup()

    def save(self):
        (self.root / 'SOURCE.json').write_text(json.dumps(self.manifest))

    def test_exact_snapshot_is_valid_without_git_checkout(self):
        path, evidence = verify_snapshot(self.root)
        self.assertEqual(path, self.root)
        self.assertEqual(evidence['files'], 2)
        self.assertEqual(evidence['bytes'], sum(map(len, self.contents.values())))
        self.assertEqual(evidence['original_commit'], REVIEWED_UI_COMMIT)

    def test_changed_bytes_missing_and_extra_files_refused(self):
        file = self.root / 'assets/example.txt'
        original = file.read_bytes()
        file.write_bytes(b'X' * len(original))
        with self.assertRaisesRegex(ValueError, 'SHA256'):
            verify_snapshot(self.root)
        file.unlink()
        with self.assertRaisesRegex(ValueError, 'missing declared'):
            verify_snapshot(self.root)
        file.write_bytes(original)
        (self.root / 'assets/unlisted.txt').write_bytes(b'extra')
        with self.assertRaisesRegex(ValueError, 'undeclared'):
            verify_snapshot(self.root)

    def test_original_revision_and_unknown_fields_refused(self):
        self.manifest['original_commit'] = '0' * 40
        self.save()
        with self.assertRaisesRegex(ValueError, 'exact reviewed'):
            verify_snapshot(self.root)
        self.manifest['original_commit'] = REVIEWED_UI_COMMIT
        self.manifest['download_url'] = 'never-fetched'
        self.save()
        with self.assertRaisesRegex(ValueError, 'Unknown'):
            verify_snapshot(self.root)

    def test_traversal_and_noncanonical_paths_refused(self):
        for name in ('../outside', '/absolute', 'ui/../outside', 'ui//alias', 'ui/./alias', 'C:/absolute', 'ui\\alias'):
            with self.subTest(name=name):
                self.manifest['files'][0]['path'] = name
                self.save()
                with self.assertRaises(ValueError):
                    verify_snapshot(self.root)

    def test_duplicate_and_case_colliding_paths_refused(self):
        self.manifest['files'].append(dict(self.manifest['files'][0]))
        for name in ('ui/app-window.slint', 'ui/APP-WINDOW.slint'):
            with self.subTest(name=name):
                self.manifest['files'][-1]['path'] = name
                self.save()
                with self.assertRaises(ValueError):
                    verify_snapshot(self.root)

    def test_duplicate_json_fields_and_invalid_size_refused(self):
        (self.root / 'SOURCE.json').write_text('{"schema_version":1,"schema_version":1}')
        with self.assertRaisesRegex(ValueError, 'Duplicate JSON'):
            verify_snapshot(self.root)
        for size in (True, -1, 33 * 1024 * 1024):
            self.manifest['files'][0]['bytes'] = size
            self.save()
            with self.assertRaisesRegex(ValueError, 'file size'):
                verify_snapshot(self.root)

    @unittest.skipIf(os.name == 'nt', 'Windows link creation can require additional privilege')
    def test_symlink_and_hardlink_files_and_directory_alias_refused(self):
        file = self.root / 'assets/example.txt'
        file.unlink()
        file.symlink_to(self.root / 'ui/app-window.slint')
        with self.assertRaisesRegex(ValueError, 'regular files'):
            verify_snapshot(self.root)
        file.unlink()
        os.link(self.root / 'ui/app-window.slint', file)
        with self.assertRaisesRegex(ValueError, 'regular files'):
            verify_snapshot(self.root)
        file.unlink()
        file.write_bytes(self.contents['assets/example.txt'])
        (self.root / 'assets/alias').symlink_to(self.root / 'ui', target_is_directory=True)
        with self.assertRaisesRegex(ValueError, 'directories'):
            verify_snapshot(self.root)


if __name__ == '__main__':
    unittest.main()

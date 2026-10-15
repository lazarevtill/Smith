#!/usr/bin/env python3
"""Exercise actual packager guard functions without building or publishing a bundle."""
import ast
import hashlib
import json
import os
from pathlib import Path
import re
import tempfile
import unittest


class PackageGuards(unittest.TestCase):
    def setUp(self):
        script = Path(__file__).resolve().parents[1] / "desktop/slint/package-macos.sh"
        python = script.read_text().split("<<'PY'\n", 1)[1].rsplit("\nPY", 1)[0]
        tree = ast.parse(python)
        names = {"fail", "digest", "bounded_bytes", "confined", "copy_notice", "mlx_manifest", "copy_mlx_resource", "copy_mlx_notices", "inspect_binary"}
        selected = [node for node in tree.body
                    if isinstance(node, ast.FunctionDef) and node.name in names]
        self.assertEqual({node.name for node in selected}, names)
        selected += [node for node in tree.body if isinstance(node, ast.Assign)
                     and any(isinstance(target, ast.Name) and target.id in ("MLX_RESOURCES", "MLX_NOTICE_PATHS") for target in node.targets)]
        self.temp = tempfile.TemporaryDirectory(prefix="smith-package-review-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.source = self.root / "source"
        self.source.mkdir()
        self.stage = self.root / "stage"
        self.stage.mkdir()
        self.api = {"hashlib": hashlib, "json": json, "os": os, "re": re, "stage": self.stage,
                    "notice_count": 0, "notice_bytes": 0}
        exec(compile(ast.Module(body=selected, type_ignores=[]), str(script), "exec"), self.api)
        self.notice = self.source / "LICENSE"
        self.notice.write_text("synthetic license notice\n")

    def test_valid_notice_bytes_are_preserved(self):
        target = self.stage / "Rust/example/LICENSE"
        result = self.api["copy_notice"](self.notice, target, self.source)
        self.assertEqual(target.read_bytes(), self.notice.read_bytes())
        self.assertEqual(result["sha256"], hashlib.sha256(target.read_bytes()).hexdigest())

    def test_linked_ancestor_is_rejected_before_copy(self):
        external = self.root / "external"
        external.mkdir()
        (external / "NOTICE").write_text("synthetic external notice")
        (self.source / "LICENSES").symlink_to(external, target_is_directory=True)
        target = self.stage / "escaped"
        with self.assertRaises(ValueError):
            self.api["copy_notice"](self.source / "LICENSES/NOTICE", target, self.source)
        self.assertFalse(target.exists())

    def test_parent_traversal_returning_inside_source_is_rejected(self):
        candidate = self.source / "../source/LICENSE"
        self.assertEqual(candidate.resolve(), self.notice)
        target = self.stage / "escaped"
        with self.assertRaises(ValueError):
            self.api["copy_notice"](candidate, target, self.source)
        self.assertFalse(target.exists())

    def test_aggregate_count_fails_before_destination_write(self):
        self.api["notice_count"] = 6000
        target = self.stage / "overflow"
        with self.assertRaises(ValueError):
            self.api["copy_notice"](self.notice, target, self.source)
        self.assertFalse(target.exists())

    def mlx_fixture(self):
        records = []
        for name in sorted(self.api["MLX_RESOURCES"]):
            path = self.source / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"owned synthetic resource")
            records.append({"name": name, "bytes": path.stat().st_size, "sha256": hashlib.sha256(path.read_bytes()).hexdigest()})
        return {"schema": 1, "worker_sha256": "a" * 64, "resources": records, "os_dylibs_included": False}

    def test_mlx_manifest_and_resources_preserve_exact_reviewed_bytes(self):
        value = self.mlx_fixture()
        manifest = self.root / "runtime-resources.json"
        manifest.write_text(json.dumps(value))
        result = self.api["mlx_manifest"](manifest, self.api["digest"](manifest), "a" * 64)
        self.assertEqual(result, value)
        for record in result["resources"]:
            destination = self.stage / "MLX" / record["name"]
            self.api["copy_mlx_resource"](self.source, record, destination)
            self.assertEqual(destination.read_bytes(), (self.source / record["name"]).read_bytes())

    def test_mlx_missing_duplicate_or_extra_resource_and_wrong_worker_are_rejected(self):
        valid = self.mlx_fixture()
        manifest = self.root / "resources.json"
        for records in (valid["resources"][:-1], valid["resources"][:-1] + [valid["resources"][0]],
                        valid["resources"][:-1] + [{"name": "../outside", "bytes": 1, "sha256": "b" * 64}]):
            value = dict(valid, resources=records)
            manifest.write_text(json.dumps(value))
            with self.assertRaises(ValueError):
                self.api["mlx_manifest"](manifest, self.api["digest"](manifest), "a" * 64)
        manifest.write_text(json.dumps(valid))
        with self.assertRaises(ValueError):
            self.api["mlx_manifest"](manifest, self.api["digest"](manifest), "b" * 64)

    def test_mlx_changed_bytes_or_linked_resource_never_reach_destination(self):
        record = self.mlx_fixture()["resources"][0]
        source = self.source / record["name"]
        destination = self.stage / "changed"
        source.write_bytes(b"x" * record["bytes"])
        with self.assertRaises(ValueError):
            self.api["copy_mlx_resource"](self.source, record, destination)
        self.assertFalse(destination.exists())
        source.unlink()
        source.symlink_to(self.notice)
        with self.assertRaises(ValueError):
            self.api["copy_mlx_resource"](self.source, record, destination)
        self.assertFalse(destination.exists())

    def test_mlx_manifest_hash_and_parse_use_the_same_read(self):
        value = self.mlx_fixture()
        manifest = self.root / "resources.json"
        manifest.write_text(json.dumps(value))
        qualified = self.api["digest"](manifest)
        value["resources"][0]["sha256"] = "b" * 64
        manifest.write_text(json.dumps(value))
        # An earlier path-based hash can belong to different bytes. The actual
        # verifier must hash its parse buffer rather than trust a prior reopen.
        self.api["digest"] = lambda _: qualified
        with self.assertRaises(ValueError):
            self.api["mlx_manifest"](manifest, qualified, "a" * 64)

    def test_mlx_notices_select_only_the_reviewed_finite_list(self):
        for name in self.api["MLX_NOTICE_PATHS"]:
            source = self.source / name
            source.parent.mkdir(parents=True, exist_ok=True)
            source.write_text("synthetic license text\n")
        (self.source / "config.json").write_text('{"synthetic_private":"excluded"}')
        (self.source / "source.swift").write_text("// excluded source\n")
        destination = self.stage / "MLX"
        records = self.api["copy_mlx_notices"](self.source, destination)
        self.assertEqual(len(records), len(self.api["MLX_NOTICE_PATHS"]))
        self.assertFalse((destination / "config.json").exists())
        self.assertFalse((destination / "source.swift").exists())

    def test_macho_system_library_policy_rejects_parent_traversal_and_rpath(self):
        executable = self.root / "synthetic-binary"
        executable.write_bytes(b"synthetic Mach-O inspection fixture")
        executable.chmod(0o700)
        linked = "/usr/lib/libSystem.B.dylib"
        rpath = False
        def command(argv):
            if argv[0].endswith("/file"):
                return "Mach-O 64-bit executable arm64"
            if argv[0].endswith("/lipo"):
                return "arm64"
            if argv[1] == "-L":
                return "synthetic:\n\t" + linked + " (compatibility version 1.0.0)\n"
            return "  platform 1\n  minos 15.0\n" + ("cmd LC_RPATH\n" if rpath else "")
        self.api["command"] = command
        expected = self.api["digest"](executable)
        self.api["inspect_binary"](executable, expected)
        for linked in ("/usr/lib/../../tmp/private.dylib", "/System/Library/../private.dylib", "@rpath/private.dylib"):
            with self.assertRaises(ValueError):
                self.api["inspect_binary"](executable, expected)
        linked = "/usr/lib/libSystem.B.dylib"
        rpath = True
        with self.assertRaises(ValueError):
            self.api["inspect_binary"](executable, expected)


if __name__ == "__main__":
    unittest.main()

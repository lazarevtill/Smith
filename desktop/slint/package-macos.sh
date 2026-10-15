#!/bin/sh
# Local packaging only: never builds, downloads, launches, installs, or publishes.
set -eu
package_root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
exec python3 - "$package_root" "$@" <<'PY'
import argparse
import ctypes
import hashlib
import json
import mmap
import os
from pathlib import Path
import plistlib
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import tomllib

script_root = Path(sys.argv.pop(1)).resolve()
repo = script_root.parents[1]
parser = argparse.ArgumentParser(prog="package-macos.sh", description="Package explicitly qualified binaries into a local Smith.app; does not launch it.")
parser.add_argument("--native", required=True, help="Absolute path to smith-native-desktop")
parser.add_argument("--native-sha256", required=True)
parser.add_argument("--backend", required=True, help="Absolute path to the qualified smith-next runtime")
parser.add_argument("--backend-sha256", required=True)
parser.add_argument("--mlx-worker", help="Optional absolute path to a qualified native MLX worker")
parser.add_argument("--mlx-worker-sha256")
parser.add_argument("--mlx-resources", help="Exact qualified MLX runtime-resources.json")
parser.add_argument("--mlx-resources-sha256")
parser.add_argument("--original-ui", required=True, help="Reviewed original smith/crates/smith-ui; read only")
parser.add_argument("--cargo-cache", action="append", required=True, help="Existing registry/src directory; may be repeated")
parser.add_argument("--output", help="New .app path inside desktop/slint/target/macos; never overwritten")
parser.add_argument("--version", default="0.1.0")
args = parser.parse_args()

def fail(message):
    raise ValueError(message)

def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()

def bounded_bytes(path, maximum):
    with path.open("rb") as stream:
        data = stream.read(maximum + 1)
    if len(data) > maximum:
        fail("Input exceeded its bounded size: " + path.name)
    return data

def package_key(package):
    name, version = package.get("name"), package.get("version")
    if not isinstance(name, str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,128}", name):
        fail("Invalid Cargo package name in notice inventory.")
    if not isinstance(version, str) or not re.fullmatch(r"[A-Za-z0-9.+_-]{1,128}", version):
        fail("Invalid Cargo package version in notice inventory.")
    return name, version

def absolute(value, directory=False):
    path = Path(value)
    if not path.is_absolute():
        fail("All supplied paths must be absolute.")
    path = path.resolve(strict=True)
    if (directory and not path.is_dir()) or (not directory and not path.is_file()):
        fail("A supplied input has the wrong file type.")
    return path

def command(argv):
    result = subprocess.run(argv, check=True, capture_output=True, text=True, timeout=30)
    return result.stdout

def inspect_binary(path, expected):
    if not re.fullmatch("[0-9a-f]{64}", expected) or digest(path) != expected:
        fail("Binary does not match its explicitly supplied qualified SHA-256: " + path.name)
    if not os.access(path, os.X_OK):
        fail("Selected binary is not executable: " + path.name)
    description = command(["/usr/bin/file", "-b", str(path)]).strip()
    if "Mach-O" not in description or "executable" not in description:
        fail("Selected binary is not a macOS Mach-O executable: " + path.name)
    architectures = sorted(command(["/usr/bin/lipo", "-archs", str(path)]).split())
    if not architectures or any(item not in ("arm64", "x86_64") for item in architectures):
        fail("Unsupported or unknown Mach-O architecture.")
    links = []
    for line in command(["/usr/bin/otool", "-L", str(path)]).splitlines():
        if not line.startswith("\t"):
            continue
        linked = line.strip().split(" (", 1)[0]
        if os.path.normpath(linked) != linked or not linked.startswith(("/System/Library/", "/usr/lib/")):
            fail("Bundle would depend on an unpackaged library: " + linked)
        links.append(linked)
    load = command(["/usr/bin/otool", "-l", str(path)])
    if "LC_RPATH" in load:
        fail("A reviewed closed system-library dependency set must not contain LC_RPATH.")
    versions = re.findall(r"(?m)^\s*minos\s+(\d+(?:\.\d+){1,2})\s*$", load)
    versions += re.findall(r"cmd LC_VERSION_MIN_MACOSX\s+cmdsize \d+\s+version (\d+(?:\.\d+){1,2})", load)
    if not versions:
        fail("Mach-O deployment target could not be inspected.")
    platforms = re.findall(r"(?m)^\s*platform\s+(\S+)\s*$", load)
    if any(value not in ("1", "MACOS", "macos") for value in platforms):
        fail("Binary contains a non-macOS platform build command.")
    return {"sha256": expected, "bytes": path.stat().st_size,
            "architectures": architectures, "system_libraries": sorted(set(links)),
            "minimum_macos": max(versions, key=lambda value: tuple(map(int, value.split("."))))}

def font_metadata(data):
    if len(data) < 12:
        fail("Invalid font input.")
    tables = struct.unpack_from(">H", data, 4)[0]
    name_table = None
    for index in range(tables):
        tag, _, offset, length = struct.unpack_from(">4sIII", data, 12 + index * 16)
        if tag == b"name":
            if offset + length > len(data):
                fail("Invalid font name table.")
            name_table = offset
    if name_table is None:
        fail("Font has no name metadata.")
    _, count, strings = struct.unpack_from(">HHH", data, name_table)
    names = {}
    for index in range(count):
        platform, _, _, name, length, offset = struct.unpack_from(">HHHHHH", data, name_table + 6 + index * 12)
        if name not in (0, 1, 5, 13, 14):
            continue
        start = name_table + strings + offset
        value = data[start:start + length].decode("utf-16-be" if platform in (0, 3) else "mac_roman")
        if value not in names.setdefault(str(name), []):
            names[str(name)].append(value)
    return names

def confined(root, source):
    try:
        relative = source.relative_to(root)
    except ValueError:
        fail("Source file escapes its explicitly selected root.")
    if ".." in relative.parts:
        fail("Parent-directory components are not permitted in notice paths.")
    current = root
    for part in relative.parts:
        current = current / part
        if current.is_symlink():
            fail("Linked source file or directory is not permitted: " + source.name)
    resolved = source.resolve(strict=True)
    if resolved != root and root not in resolved.parents:
        fail("Resolved source escapes its explicitly selected root.")

MLX_RESOURCES = frozenset((
    "mlx.metallib",
    "mlx-swift_Cmlx.bundle/Contents/Info.plist",
    "mlx-swift_Cmlx.bundle/Contents/Resources/default.metallib",
    "swift-transformers_Hub.bundle/gpt2_tokenizer_config.json",
    "swift-transformers_Hub.bundle/t5_tokenizer_config.json",
    "swift-crypto_Crypto.bundle/PrivacyInfo.xcprivacy",
))
MLX_NOTICE_PATHS = (
    "README.md", "eventsource/LICENSE.md", "fmt/LICENSE", "metal-cpp/LICENSE.txt",
    "mlx-c/LICENSE", "mlx-core/LICENSE", "mlx-swift/LICENSE", "mlx-swift-lm/LICENSE",
    "nlohmann-json/LICENSE.MIT", "swift-asn1/LICENSE.txt", "swift-asn1/NOTICE.txt",
    "swift-collections/LICENSE.txt", "swift-crypto/LICENSE.txt", "swift-crypto/NOTICE.txt",
    "swift-huggingface/LICENSE", "swift-jinja/LICENSE", "swift-numerics/LICENSE.txt",
    "swift-syntax/LICENSE.txt", "swift-transformers/LICENSE", "yyjson/LICENSE",
)

def mlx_manifest(path, expected, worker_sha):
    data = bounded_bytes(path, 64 * 1024)
    if not re.fullmatch("[0-9a-f]{64}", expected) or hashlib.sha256(data).hexdigest() != expected:
        fail("MLX resource manifest does not match its qualified SHA-256.")
    def unique(pairs):
        values = {}
        for key, value in pairs:
            if key in values:
                fail("Duplicate key in MLX resource manifest.")
            values[key] = value
        return values
    value = json.loads(data, object_pairs_hook=unique)
    if set(value) != {"schema", "worker_sha256", "resources", "os_dylibs_included"} or value["schema"] != 1 or value["worker_sha256"] != worker_sha or value["os_dylibs_included"] is not False:
        fail("Unsupported MLX resource manifest.")
    records = value["resources"]
    if not isinstance(records, list) or len(records) != len(MLX_RESOURCES):
        fail("MLX resource list must contain exactly the reviewed runtime files.")
    names = set()
    for item in records:
        if not isinstance(item, dict) or set(item) != {"name", "bytes", "sha256"}:
            fail("Invalid MLX resource record.")
        name = item["name"]
        if name not in MLX_RESOURCES or name in names:
            fail("Unreviewed or duplicate MLX resource path.")
        if type(item["bytes"]) is not int or not 0 < item["bytes"] <= 32 * 1024 * 1024 or not isinstance(item["sha256"], str) or not re.fullmatch("[0-9a-f]{64}", item["sha256"]):
            fail("Invalid MLX resource size or hash.")
        names.add(name)
    return value

def copy_mlx_resource(root, record, destination):
    source = root / record["name"]
    confined(root, source)
    if not source.is_file() or source.stat().st_size != record["bytes"]:
        fail("Selected MLX resource size changed: " + record["name"])
    data = bounded_bytes(source, 32 * 1024 * 1024)
    if hashlib.sha256(data).hexdigest() != record["sha256"]:
        fail("Selected MLX resource hash changed: " + record["name"])
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(data)
    os.chmod(destination, 0o644)

notice_count = 0
notice_bytes = 0
def copy_notice(source, destination, source_root):
    global notice_count, notice_bytes
    confined(source_root, source)
    if source.is_symlink() or not source.is_file() or source.stat().st_size > 2 * 1024 * 1024:
        fail("Notice must be a bounded regular file: " + source.name)
    data = bounded_bytes(source, 2 * 1024 * 1024)
    data.decode("utf-8")
    notice_count += 1
    notice_bytes += len(data)
    if notice_count > 6000 or notice_bytes > 64 * 1024 * 1024:
        fail("Aggregate dependency notice bounds exceeded.")
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(data)
    return {"file": str(destination.relative_to(stage)), "sha256": digest(destination)}

def copy_mlx_notices(source_root, destination_root):
    return [copy_notice(source_root / relative, destination_root / relative, source_root)
            for relative in MLX_NOTICE_PATHS]

stage = None
try:
    if sys.platform != "darwin":
        fail("This package script supports macOS only.")
    if not re.fullmatch(r"\d+\.\d+\.\d+", args.version):
        fail("Use a numeric major.minor.patch bundle version.")
    native = absolute(args.native)
    backend = absolute(args.backend)
    original = absolute(args.original_ui, True)
    caches = [absolute(value, True) for value in args.cargo_cache]
    native_info = inspect_binary(native, args.native_sha256)
    backend_info = inspect_binary(backend, args.backend_sha256)
    if native_info["architectures"] != backend_info["architectures"]:
        fail("Native and runtime architecture sets must match.")
    mlx_fields = (args.mlx_worker, args.mlx_worker_sha256, args.mlx_resources, args.mlx_resources_sha256)
    if any(mlx_fields) and not all(mlx_fields):
        fail("MLX packaging requires both exact executable and resource manifest paths/hashes.")
    mlx_info = None
    if all(mlx_fields):
        mlx_worker = absolute(args.mlx_worker)
        mlx_resources = absolute(args.mlx_resources)
        mlx_info = inspect_binary(mlx_worker, args.mlx_worker_sha256)
        if mlx_info["architectures"] != native_info["architectures"]:
            fail("The selected native MLX worker architecture must match this bundle.")
        mlx_record = mlx_manifest(mlx_resources, args.mlx_resources_sha256, args.mlx_worker_sha256)
        mlx_info["resource_manifest_sha256"] = args.mlx_resources_sha256
        mlx_info["resources"] = mlx_record["resources"]
        mlx_info["auto_load"] = False
        mlx_info["models_included"] = False
    fonts = {
        "Inter.ttf": "29160a80ff49ddcab2c97711247e08b1fab27a484a329ce8b813d820dc559031",
        "Poppins-Regular.ttf": "7e65201e9b79159e2300267cc885e16c8dcef2424cdfa09a29bfb0980a94a7ba",
        "Poppins-Medium.ttf": "90373e7d838d32468438fc3e152dca0bdb12edcab99ea639f158790b1ba1fd05",
        "Poppins-SemiBold.ttf": "d3bf1bdaf0550e83da9ac0b1d1d9fe6db086835a83aa28578e609a394b9a0286",
        "SplineSansMono.ttf": "e20c1df32aa2f886e828cfcc81ca5c405d3f3b160990f6b892923e2e449bf525",
    }
    font_evidence = []
    with native.open("rb") as stream, mmap.mmap(stream.fileno(), 0, access=mmap.ACCESS_READ) as executable:
        for name, expected in fonts.items():
            source = original / "assets/fonts" / name
            confined(original, source)
            data = bounded_bytes(source, 16 * 1024 * 1024)
            if hashlib.sha256(data).hexdigest() != expected:
                fail("Original font changed since inspection: " + name)
            names = font_metadata(data)
            if "SIL Open Font License" not in " ".join(names.get("13", [])):
                fail("Font has no inspected OFL statement: " + name)
            if executable.find(data) < 0:
                fail("Native binary does not embed the reviewed font bytes: " + name)
            font_evidence.append({"file": name, "sha256": expected, "metadata": names, "embedded_bytes_verified": True})
    icon = original / "assets/icon-256.png"
    confined(original, icon)
    png = bounded_bytes(icon, 2 * 1024 * 1024)
    if hashlib.sha256(png).hexdigest() != "e85853fa2c9bdc310ff255e46b146a76de5549c0511b68a72b9aa97c366ea74b":
        fail("Original app icon changed since inspection.")
    if png[:8] != b"\x89PNG\r\n\x1a\n" or struct.unpack(">II", png[16:24]) != (256, 256):
        fail("Reviewed icon is not the expected 256-pixel PNG.")
    registry = {}
    for cache in caches:
        for source in cache.glob("*/*/Cargo.toml"):
            confined(cache, source)
            if source.stat().st_size > 2 * 1024 * 1024 or len(registry) > 10000:
                fail("Cargo manifest inventory bounds exceeded.")
            package = tomllib.loads(bounded_bytes(source, 2 * 1024 * 1024).decode("utf-8"))["package"]
            key = package_key(package)
            registry.setdefault(key, source.parent)
    slint_source = registry.get(("slint", "1.17.1"))
    if slint_source is None:
        fail("Inspected Slint 1.17.1 source-package notices are missing from supplied caches.")
    for name in ("GPL-3.0-only.txt", "LicenseRef-Slint-Royalty-free-2.0.md", "LicenseRef-Slint-Software-3.0.md"):
        if not (slint_source / "LICENSES" / name).is_file():
            fail("A published Slint license text is missing.")
    target = script_root / "target"
    if target.is_symlink():
        fail("Packaging target must not be a symlink.")
    allowed = target / "macos"
    if allowed.is_symlink():
        fail("Packaging output directory must not be a symlink.")
    allowed.mkdir(parents=True, exist_ok=True)
    output = Path(args.output) if args.output else allowed / "Smith.app"
    if not output.is_absolute() or output.suffix != ".app" or any(ord(c) < 32 for c in str(output)):
        fail("Output must be an absolute .app path.")
    if output.parent.resolve() != allowed.resolve():
        fail("Output must be a direct child of desktop/slint/target/macos.")
    if output.exists() or output.is_symlink():
        fail("Refusing to overwrite an existing bundle.")
    command(["git", "-C", str(repo), "check-ignore", "--", str(output)])
    stage = Path(tempfile.mkdtemp(prefix=".smith-package-", dir=allowed))
    contents = stage / "Contents"
    macos = contents / "MacOS"
    resources = contents / "Resources"
    notices = resources / "Notices"
    macos.mkdir(parents=True)
    notices.mkdir(parents=True)
    shutil.copyfile(native, macos / "Smith")
    shutil.copyfile(backend, resources / "smith-next")
    os.chmod(macos / "Smith", 0o755)
    os.chmod(resources / "smith-next", 0o755)
    if digest(macos / "Smith") != args.native_sha256 or digest(resources / "smith-next") != args.backend_sha256:
        fail("A qualified binary changed during packaging.")
    # PNG payloads are preserved byte-for-byte; only the standard ICNS container
    # is constructed, with 256px and 128px@2x entries. No image is resampled.
    chunks = b"".join(tag + struct.pack(">I", len(png) + 8) + png for tag in (b"ic08", b"ic13"))
    (resources / "Smith.icns").write_bytes(b"icns" + struct.pack(">I", len(chunks) + 8) + chunks)
    minimum_versions = [native_info["minimum_macos"], backend_info["minimum_macos"]]
    if mlx_info is not None:
        minimum_versions.append(mlx_info["minimum_macos"])
    minimum = max(minimum_versions,
                  key=lambda value: tuple(map(int, value.split("."))))
    info = {"CFBundleDevelopmentRegion": "en", "CFBundleDisplayName": "Smith", "CFBundleName": "Smith",
            "CFBundleExecutable": "Smith", "CFBundleIdentifier": "cloud.lazarev.SmithNext",
            "CFBundleIconFile": "Smith.icns", "CFBundleInfoDictionaryVersion": "6.0",
            "CFBundlePackageType": "APPL", "CFBundleShortVersionString": args.version,
            "CFBundleVersion": args.version, "LSMinimumSystemVersion": minimum,
            "NSHighResolutionCapable": True, "LSUIElement": False}
    with (contents / "Info.plist").open("wb") as stream:
        plistlib.dump(info, stream, sort_keys=True)
    (contents / "PkgInfo").write_bytes(b"APPL????")
    notice_files = []
    if mlx_info is not None:
        mlx_target = resources / "MLX"
        mlx_target.mkdir()
        shutil.copyfile(mlx_worker, mlx_target / "smith-mlx-worker")
        os.chmod(mlx_target / "smith-mlx-worker", 0o755)
        if digest(mlx_target / "smith-mlx-worker") != args.mlx_worker_sha256:
            fail("Qualified MLX worker changed during packaging.")
        for record in mlx_record["resources"]:
            copy_mlx_resource(mlx_worker.parent, record, mlx_target / record["name"])
        # Only the finite runtime files, pinned dependency metadata and bounded
        # license texts are selected. Test utilities/weights are never copied.
        mlx_notices = mlx_worker.parent / "ThirdPartyNotices"
        confined(mlx_worker.parent, mlx_notices)
        if not mlx_notices.is_dir():
            fail("The selected MLX worker has no dependency notices.")
        notice_files.extend(copy_mlx_notices(mlx_notices, notices / "MLX"))
        notice_files.append(copy_notice(mlx_worker.parent / "Package.resolved", notices / "MLX/Package.resolved", mlx_worker.parent))
        mlx_info["dependency_resolution_sha256"] = digest(notices / "MLX/Package.resolved")
        (mlx_target / "runtime-resources.json").write_text(json.dumps(mlx_record, indent=2) + "\n")
    for name in ("Inter-OFL.txt", "Poppins-OFL.txt", "SplineSansMono-OFL.txt"):
        notice_files.append(copy_notice(repo / "web/assets/fonts/licenses" / name, notices / "Fonts" / name, repo))
    notice_files.append(copy_notice(repo / "crates/smith-media/assets/fonts/OFL.txt", notices / "Fonts/NotoSans-OFL.txt", repo))
    notice_files.append(copy_notice(repo / "crates/smith-media/assets/fonts/PROVENANCE.md", notices / "Fonts/NotoSans-PROVENANCE.md", repo))
    (notices / "Fonts/EMBEDDED-NATIVE-FONTS.json").write_text(json.dumps(font_evidence, indent=2) + "\n")
    for name in ("GPL-3.0-only.txt", "LicenseRef-Slint-Royalty-free-2.0.md", "LicenseRef-Slint-Software-3.0.md"):
        notice_files.append(copy_notice(slint_source / "LICENSES" / name, notices / "Slint-1.17.1" / name, slint_source))
    dependencies = {}
    locks = {}
    for label, lock_path in (("native", script_root / "Cargo.lock"), ("backend", repo / "Cargo.lock")):
        locks[label] = digest(lock_path)
        for package in tomllib.loads(bounded_bytes(lock_path, 2 * 1024 * 1024).decode("utf-8")).get("package", []):
            key = package_key(package)
            item = dependencies.setdefault(key, {"name": key[0], "version": key[1], "lockfiles": [],
                                                "source": package.get("source", "local workspace"),
                                                "checksum": package.get("checksum"), "notice_files": []})
            item["lockfiles"].append(label)
    for key, item in sorted(dependencies.items()):
        source = registry.get(key)
        if source is None:
            item["notice_status"] = "local workspace" if item["source"] == "local workspace" else "not cached; inventory only"
            continue
        manifest = tomllib.loads(bounded_bytes(source / "Cargo.toml", 2 * 1024 * 1024).decode("utf-8"))["package"]
        item["license_expression"] = manifest.get("license")
        item["repository"] = manifest.get("repository")
        candidates = []
        for entry in source.iterdir():
            if entry.is_file() and entry.name.upper().startswith(("LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE")):
                confined(source, entry)
                candidates.append(entry)
            elif entry.is_dir() and entry.name.lower() in ("license", "licenses"):
                confined(source, entry)
                for path in entry.rglob("*"):
                    confined(source, path)
                    if path.is_file():
                        candidates.append(path)
                    if len(candidates) > 256:
                        fail("A dependency has too many notice files.")
        if manifest.get("license-file"):
            declared = Path(manifest["license-file"])
            if declared.is_absolute() or ".." in declared.parts:
                fail("Dependency license-file must be a direct package-relative path.")
            selected = source / declared
            confined(source, selected)
            candidates.append(selected)
        if len(set(candidates)) > 256:
            fail("A dependency has too many notice files.")
        for entry in sorted(set(candidates)):
            destination = notices / "Rust" / (key[0] + "-" + key[1]) / entry.relative_to(source)
            copied = copy_notice(entry, destination, source)
            item["notice_files"].append(copied)
        item["notice_status"] = "cached package notices preserved" if candidates else "metadata only; no notice file found"
    (notices / "DEPENDENCY-INVENTORY.json").write_text(json.dumps(list(dependencies.values()), indent=2) + "\n")
    (notices / "README.txt").write_text(
        "Local Smith native qualification bundle. Not published, signed as a distribution bundle, or notarized.\n"
        "Executable copies are byte-identical to the explicitly supplied qualified input hashes.\n"
        "Original Smith UI/brand use is authorized for this private project; no general public license is asserted.\n"
        "The icon PNG bytes are unchanged inside a macOS ICNS container; fonts and other UI images are embedded.\n"
        "Font notices and original embedded TTF copyright/license metadata are preserved in Fonts/.\n"
        "Slint's published alternative license texts are preserved in Slint-1.17.1/. This does not choose a paid license.\n"
        "Slint Royalty-free 2.0 distribution requires its specified AboutSlint widget or public webpage attribution.\n"
        "DEPENDENCY-INVENTORY.json is a superset of both lockfiles, including other-platform/build dependencies;\n"
        "it is not a resolved runtime-only graph or a complete public-distribution license certification.\n"
        "Missing notices and original/private asset rights require review before any external distribution.\n"
        "No original source tree, user configuration, profile, credential or model is packaged.\n"
        "The optional MLX component is included only with explicit qualified executable/resource hashes.\n"
        "The application minimum OS includes the selected MLX worker's minimum; no model loads on startup.\n"
    )
    manifest = {"schema": 1, "kind": "local_native_bundle", "application": "Smith",
                "bundle_id": info["CFBundleIdentifier"], "version": args.version,
                "native": native_info, "backend": backend_info, "mlx": mlx_info, "source_lock_sha256": locks,
                "font_embedding_verified": font_evidence,
                "icon": {"original_sha256": hashlib.sha256(png).hexdigest(), "pixel_bytes_changed": False,
                         "provenance": "reviewed original smith-ui/assets/icon-256.png; private-project use"},
                "notice_files": notice_files, "runtime_source_checkout_required": False,
                "downloads": False, "published": False, "distribution_signature_applied": False,
                "launch_quit_acceptance": "not run by packager; must be recorded separately for these exact hashes"}
    (resources / "PACKAGE-MANIFEST.json").write_text(json.dumps(manifest, indent=2) + "\n")
    command(["/usr/bin/plutil", "-lint", str(contents / "Info.plist")])
    for path in stage.rglob("*"):
        if path.is_symlink():
            fail("Unexpected symlink in staged package.")
    os.chmod(stage, 0o755)
    # Darwin's exclusive rename closes the race between the earlier existence
    # check and publication; even a concurrently created empty directory wins.
    darwin = ctypes.CDLL(None, use_errno=True)
    rename_exclusive = darwin.renamex_np
    rename_exclusive.argtypes = (ctypes.c_char_p, ctypes.c_char_p, ctypes.c_uint)
    rename_exclusive.restype = ctypes.c_int
    if rename_exclusive(os.fsencode(stage), os.fsencode(output), 0x00000004) != 0:
        raise OSError(ctypes.get_errno(), "Cannot publish bundle without replacing an existing path.")
    stage = None
    print(json.dumps({"bundle": str(output), "native_sha256": args.native_sha256,
                      "backend_sha256": args.backend_sha256,
                      "package_manifest_sha256": digest(output / "Contents/Resources/PACKAGE-MANIFEST.json"),
                      "status": "packaged; launch/quit acceptance remains separate"}, indent=2))
except Exception as error:
    if stage is not None:
        shutil.rmtree(stage)
    print("Packaging failed: " + str(error), file=sys.stderr)
    raise SystemExit(1)
PY

#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
configuration=${1:-release}
output_directory=${2:-"$PWD/dist"}
case "$output_directory" in /*) ;; *) printf 'Use an absolute output directory.\n' >&2; exit 64;; esac
case "$configuration" in debug|release) ;; *) exit 64;; esac
bin_dir="$PWD/.build/arm64-apple-macosx/$configuration"
[ -f "$bin_dir/smith-mlx-worker" ]
[ -f "$PWD/.build/metal/default.metallib" ]
mkdir -p "$output_directory/mlx-swift_Cmlx.bundle/Contents/Resources"
cp "$bin_dir/smith-mlx-worker" "$output_directory/"
python3 scripts/close-runtime-paths.py "$output_directory/smith-mlx-worker"
cp .build/metal/default.metallib "$output_directory/mlx-swift_Cmlx.bundle/Contents/Resources/default.metallib"
# Upstream Cmlx first supports this colocated path; keep the official resource bundle as well.
cp .build/metal/default.metallib "$output_directory/mlx.metallib"
cat > "$output_directory/mlx-swift_Cmlx.bundle/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>CFBundleIdentifier</key><string>local.smith.mlx.Cmlx</string><key>CFBundlePackageType</key><string>BNDL</string><key>CFBundleVersion</key><string>0.31.4</string></dict></plist>
PLIST
# Swift tokenizer privacy/resource bundles remain beside the child.
for bundle_name in swift-transformers_Hub.bundle swift-crypto_Crypto.bundle; do
    [ -d "$bin_dir/$bundle_name" ]
    cp -Rf "$bin_dir/$bundle_name" "$output_directory/"
done
cp Package.resolved "$output_directory/Package.resolved"
cp -Rf ThirdPartyNotices "$output_directory/"
printf 'Worker and official Metal resources packaged in %s\n' "$output_directory"

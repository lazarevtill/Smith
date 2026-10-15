#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
# Pass the exported, read-only workspace compiler path. Never invoke xcrun's installer shim.
metal_compiler=${1:?absolute path to exported Metal.xctoolchain/usr/bin/metal required}
metal_linker="$(dirname "$metal_compiler")/metallib"
source_root="$PWD/.build/checkouts/mlx-swift/Source/Cmlx/mlx-generated/metal"
output_root="$PWD/.build/metal"
mkdir -p "$output_root" "$PWD/.module-cache/metal"
export CLANG_MODULE_CACHE_PATH="$PWD/.module-cache/metal"
while IFS= read -r source; do
    name=$(basename "$source" .metal)
    "$metal_compiler" -fmodules-cache-path="$PWD/.module-cache/metal" -x metal -std=metal3.2 -mmacosx-version-min=15.0 -fno-fast-math -Wno-c++17-extensions -Wno-c++20-extensions -I "$source_root" -c "$source" -o "$output_root/$name.air"
done < <(find "$source_root" -name '*.metal' -type f | sort)
"$metal_linker" "$output_root"/*.air -o "$output_root/default.metallib"
printf 'Compiled official MLX Metal resources: %s\n' "$output_root/default.metallib"

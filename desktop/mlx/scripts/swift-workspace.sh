#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
export CLANG_MODULE_CACHE_PATH="$PWD/.module-cache"
export SWIFTPM_MODULECACHE_OVERRIDE="$PWD/.module-cache"
command_name=$1
shift
exec swift "$command_name" --cache-path "$PWD/.cache" --config-path "$PWD/.config" --security-path "$PWD/.security" --scratch-path "$PWD/.build" --disable-netrc --disable-keychain --disable-automatic-resolution "$@"

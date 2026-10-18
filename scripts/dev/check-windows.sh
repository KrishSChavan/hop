#!/bin/bash
# Type-checks and lints the whole workspace for Windows, from a Mac.
#
# The Mac has no Windows SDK, so the build script of ring (the crypto
# library under hop-net's TLS) gets stub C headers and a stand-in archiver
# from scripts/dev/winstub. Nothing is linked or run: on a real PC the MSVC
# build tools build ring properly, and CI does the same on Windows.
#
# Usage: scripts/dev/check-windows.sh

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
STUB="$REPO/scripts/dev/winstub"
export CFLAGS_x86_64_pc_windows_msvc="-I$STUB/include"
export AR_x86_64_pc_windows_msvc="$STUB/lib-shim.sh"
if [ -d /opt/homebrew/opt/rustup/bin ]; then
    export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
fi

cd "$REPO"
cargo check --workspace --all-targets --target x86_64-pc-windows-msvc
cargo clippy --workspace --all-targets --target x86_64-pc-windows-msvc -- -D warnings
echo "Windows check and lint passed."

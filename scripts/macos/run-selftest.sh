#!/bin/bash
# Runs `hop selftest` from Hop.app and prints what it wrote.
#
# Hop is launched through LaunchServices (`open`), not run from this shell,
# so macOS treats Hop itself as the app asking for permissions; run from
# Terminal, it would borrow Terminal's. Build the app first with
# scripts/macos/make-app.sh.
#
# WARNING: the self-test moves the cursor, clicks and types (inside its own
# window) and may show macOS permission prompts. Only run it with the user
# at the Mac and expecting it.
#
# Usage: scripts/macos/run-selftest.sh [extra arguments for `hop selftest`]
#
# The output goes to target/hop-selftest.log (stdout) and
# target/hop-selftest.err.log (stderr), or the same names in
# $CARGO_TARGET_DIR. They are separate files because nothing documents
# whether LaunchServices appends when both point at one file, and two
# writers that do not append overwrite each other.

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO/target}"
# Absolute, as in make-app.sh, so the paths handed to `open` mean the same
# thing wherever LaunchServices resolves them (Hop itself starts in "/").
case "$TARGET_DIR" in
    /*) ;;
    *) TARGET_DIR="$PWD/$TARGET_DIR" ;;
esac
APP="$TARGET_DIR/Hop.app"
LOG="$TARGET_DIR/hop-selftest.log"
ERR_LOG="$TARGET_DIR/hop-selftest.err.log"

if [ ! -d "$APP" ]; then
    echo "error: $APP does not exist; build it with scripts/macos/make-app.sh" >&2
    exit 1
fi
# Permissions are granted to the signed app; an unsigned or modified bundle
# would make macOS ask again, or refuse.
if ! codesign --verify --strict "$APP" 2>/dev/null; then
    echo "error: $APP is not validly signed; rebuild it with scripts/macos/make-app.sh" >&2
    exit 1
fi

: > "$LOG"
: > "$ERR_LOG"

echo "==> Running the Hop self-test from $APP"
# -n starts a new copy even if Hop is already running, so this one gets the
# arguments; -W waits until it quits. `open` does not pass on Hop's exit
# status, so the self-test's verdict is in the log.
open -n -W --stdout "$LOG" --stderr "$ERR_LOG" "$APP" --args selftest "$@"

echo "==> Output ($LOG)"
cat "$LOG"
if [ -s "$ERR_LOG" ]; then
    echo "==> Errors and diagnostics ($ERR_LOG)"
    cat "$ERR_LOG"
fi

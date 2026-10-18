#!/bin/bash
# Runs the Hop agent (`hop run`) from Hop.app, shows its status lines here,
# and says where its log is.
#
# Hop is launched through LaunchServices (`open`), not run from this shell,
# so macOS treats Hop itself as the app that uses Accessibility and Local
# Network; run from Terminal, it would borrow Terminal's permissions. Build
# the app first with scripts/macos/make-app.sh, and pair the two computers
# first (in Terminal: "<target>/Hop.app/Contents/MacOS/hop" pair).
#
# WARNING: while Hop runs, pushing the cursor past the screen edge facing
# the other computer moves this Mac's keyboard and mouse over to it. Only
# run it with the user at the Mac and expecting it.
#
# Usage: scripts/macos/run-agent.sh
#
# Press Ctrl+C here to stop Hop: the script asks Hop to quit (SIGTERM), and
# Hop gives the keyboard and mouse back before it exits. Closing this
# Terminal window stops Hop the same way. The script also turns macOS App
# Nap off for Hop (see below).
#
# Hop's own log is ~/Library/Logs/Hop/hop.log. Its stdout (one status line
# per connection or session change, shown here as it happens) goes to
# agent-stdout.log, and its stderr to agent-stderr.log, in the same folder.
# Never point them into a folder macOS protects (Desktop, Documents,
# Downloads): LaunchServices then refuses to start Hop, with error -10810.

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
LOG_DIR="$HOME/Library/Logs/Hop"
HOP_LOG="$LOG_DIR/hop.log"
LOG="$LOG_DIR/agent-stdout.log"
ERR_LOG="$LOG_DIR/agent-stderr.log"
mkdir -p "$LOG_DIR"

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

# This copy of Hop, found by its command line: the app's executable with
# `run` as its last argument (LaunchServices may add its own arguments
# before ours). pgrep and pkill read an extended regular expression, so the
# path's special characters are escaped.
HOP_BINARY="$APP/Contents/MacOS/hop"
HOP_PATTERN="^$(printf '%s' "$HOP_BINARY" | sed 's/[][\.*^$+?(){}|]/\\&/g') (.* )?run\$"

if pgrep -f "$HOP_PATTERN" >/dev/null; then
    echo "error: Hop is already running from $APP; quit it first (Ctrl+C where it was started)" >&2
    exit 1
fi

: > "$LOG"
: > "$ERR_LOG"

# Ctrl+C stops `open` (which only waits for Hop); Hop itself keeps running
# until it is asked to quit, which it does failing open. Closing this
# Terminal window (SIGHUP) asks too: Hop.app is not attached to the
# terminal, so without this it would go on running unseen.
#
# Hop gives the keyboard and mouse back as soon as it is asked and then
# needs a few seconds at most to say goodbye to the other computer. If it
# is still running after this waits 5 seconds, the next call asks again,
# and a second request makes Hop exit at once.
stop_hop() {
    if pgrep -f "$HOP_PATTERN" >/dev/null; then
        # After SIGHUP the terminal is gone and writing to it fails; that
        # must not stop the request to quit (set -e).
        { echo; echo "==> Asking Hop to quit"; } 2>/dev/null || true
        pkill -TERM -f "$HOP_PATTERN" || true
        for _ in $(seq 1 50); do
            pgrep -f "$HOP_PATTERN" >/dev/null || break
            sleep 0.1
        done
    fi
}
trap stop_hop INT TERM HUP

echo "==> Starting Hop from $APP"
echo "==> Hop's log: $HOP_LOG"
echo "==> Status lines follow. Press Ctrl+C to stop Hop."

# The status lines, as Hop writes them. A background job ignores the
# Ctrl+C meant for Hop; it is stopped below.
tail -n +1 -F "$LOG" 2>/dev/null &
TAIL_PID=$!

# -n starts a new copy even if another Hop (say a self-test) is open, so
# this one gets the arguments; -W waits until it quits. `open` does not
# pass on Hop's exit status, so the outcome is in the logs.
status=0
if ! open -n -W --stdout "$LOG" --stderr "$ERR_LOG" "$APP" --args run; then
    # Also the way out after Ctrl+C, which stops `open` too.
    status=1
fi
stop_hop
sleep 0.2
kill "$TAIL_PID" 2>/dev/null || true
wait "$TAIL_PID" 2>/dev/null || true

if [ -s "$ERR_LOG" ]; then
    echo "==> Errors and diagnostics ($ERR_LOG)"
    cat "$ERR_LOG"
    if grep -q "hop pair" "$ERR_LOG"; then
        echo
        echo "To pair, run this in Terminal on this Mac (not through open):"
        echo "  \"$HOP_BINARY\" pair"
    fi
fi
echo "==> Hop's log: $HOP_LOG"
exit "$status"

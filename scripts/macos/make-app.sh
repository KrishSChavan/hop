#!/bin/bash
# Builds Hop.app: the release `hop-app` binary (the Hop app, with its
# window) inside an app bundle with Hop's icon, signed with the user's
# personal Apple Development identity.
#
# macOS grants permissions (Accessibility, Local Network) to a signed app,
# and the grants survive rebuilds as long as the identity and the bundle id
# stay the same. A bare binary run from Terminal would borrow Terminal's
# permissions instead. So the executable keeps its old name,
# Contents/MacOS/hop, and the bundle id and identity never change: the
# Accessibility grant of earlier builds carries over.
#
# Hop.app opens a window, with a Dock icon while it is open. On the Mac,
# hop-app also runs the `hop` commands: a first argument that names one
# (`run`, `selftest`, `pair`, ...) goes to the command line instead of
# opening the window. That keeps scripts/macos/run-agent.sh and
# run-selftest.sh working; they start this bundle with `--args run` and
# `--args selftest`. scripts/macos/release.sh zips it for a GitHub Release.
#
# Usage: scripts/macos/make-app.sh
#
# The bundle lands in target/Hop.app, or in $CARGO_TARGET_DIR/Hop.app when
# that is set. Building and signing touch nothing on screen; nothing here
# runs Hop.

set -euo pipefail

# The user's personal "Apple Development: krishschavan@gmail.com" identity,
# named by its SHA-1 so that no other identity in the keychain is ever used.
IDENTITY_SHA1="DE1BEFBEC7F3E9169507D3A34CC7F2660E95B614"
IDENTITY_NAME="Apple Development: krishschavan@gmail.com"
BUNDLE_ID="com.krishschavan.hop"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO/target}"
# A relative CARGO_TARGET_DIR means relative to where this was started. Cargo
# runs from the repository below, so hand it the same directory, absolute.
case "$TARGET_DIR" in
    /*) ;;
    *) TARGET_DIR="$PWD/$TARGET_DIR" ;;
esac
export CARGO_TARGET_DIR="$TARGET_DIR"
APP="$TARGET_DIR/Hop.app"
# The app icon, drawn by scripts/dev/make-icon.py.
ICON="$REPO/assets/icon/Hop.icns"

# On this Mac rustup is Homebrew's keg-only build, which is not on PATH.
if [ -d /opt/homebrew/opt/rustup/bin ]; then
    export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
fi

# Check the identity and the icon first, so a missing one fails before a
# long build.
if ! security find-identity -v -p codesigning | grep "$IDENTITY_SHA1" | grep -q "$IDENTITY_NAME"; then
    echo "error: the signing identity \"$IDENTITY_NAME\" ($IDENTITY_SHA1) is not in the keychain, or is not valid" >&2
    exit 1
fi
if [ ! -f "$ICON" ]; then
    echo "error: the app icon $ICON is missing" >&2
    exit 1
fi

echo "==> Building the release hop-app binary"
(cd "$REPO" && cargo build --release -p hop-app)
BINARY="$TARGET_DIR/release/hop-app"
if [ ! -x "$BINARY" ]; then
    echo "error: $BINARY was not built" >&2
    exit 1
fi

# The workspace version, from the first `version = "..."` line of the root
# Cargo.toml (the [workspace.package] one).
VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$REPO/Cargo.toml" | head -n 1)"
VERSION="${VERSION:-0.0.0}"

echo "==> Assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BINARY" "$APP/Contents/MacOS/hop"
cp "$ICON" "$APP/Contents/Resources/Hop.icns"
# Notes on the keys below:
# - LSUIElement: Hop.app starts with no Dock icon, so the `hop` commands
#   the scripts start through LaunchServices (`run`, `selftest`) behave as
#   they always have. The window asks for a regular app when it opens (a
#   Dock icon, the app menu, a place in the app switcher).
# - CFBundleIconFile names Contents/Resources/Hop.icns (macOS adds .icns).
# - NSHighResolutionCapable: draw the window at full resolution on Retina
#   screens instead of scaled up.
# - LSApplicationCategoryType: the Utilities category, which Finder can
#   group apps by.
# - NSLocalNetworkUsageDescription and NSBonjourServices: the text of
#   macOS's Local Network prompt, and the mDNS services Hop looks for.
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleIdentifier</key>
	<string>$BUNDLE_ID</string>
	<key>CFBundleName</key>
	<string>Hop</string>
	<key>CFBundleDisplayName</key>
	<string>Hop</string>
	<key>CFBundleExecutable</key>
	<string>hop</string>
	<key>CFBundleIconFile</key>
	<string>Hop</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleShortVersionString</key>
	<string>$VERSION</string>
	<key>CFBundleVersion</key>
	<string>$VERSION</string>
	<key>LSUIElement</key>
	<true/>
	<key>LSMinimumSystemVersion</key>
	<string>13.0</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.utilities</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>NSLocalNetworkUsageDescription</key>
	<string>Hop finds and talks to your other computer on your home network, so one keyboard and mouse can control both.</string>
	<key>NSBonjourServices</key>
	<array>
		<string>_hop._udp</string>
		<string>_hop._tcp</string>
	</array>
</dict>
</plist>
PLIST
plutil -lint "$APP/Contents/Info.plist"

echo "==> Signing with \"$IDENTITY_NAME\""
# --options runtime: the hardened runtime. No --entitlements: Hop needs none.
# --timestamp=none: a development build needs no secure timestamp, and
# asking Apple's timestamp server would need the network. The signature
# also seals Contents/Resources, so a changed icon fails verification.
codesign --force --sign "$IDENTITY_SHA1" --identifier "$BUNDLE_ID" \
    --options runtime --timestamp=none "$APP"

echo "==> Verifying the signature"
codesign --verify --strict --verbose=2 "$APP"
codesign --display --verbose=2 "$APP" 2>&1 \
    | grep -E '^(Identifier|Format|Authority|TeamIdentifier|CodeDirectory)' || true
SIGNATURE="$(codesign --display --verbose=2 "$APP" 2>&1)"
if ! grep -q "^Authority=$IDENTITY_NAME" <<<"$SIGNATURE"; then
    echo "error: $APP is not signed by \"$IDENTITY_NAME\"" >&2
    exit 1
fi
if ! grep -q '^CodeDirectory.*(runtime)' <<<"$SIGNATURE"; then
    echo "error: $APP is signed without the hardened runtime" >&2
    exit 1
fi

echo "==> Built $APP"

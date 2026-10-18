#!/bin/bash
# Makes a GitHub Release of Hop: builds and signs Hop.app on this Mac
# (scripts/macos/make-app.sh), zips it as Hop-macOS.zip and publishes the
# release with it on github.com. Publishing starts
# .github/workflows/release.yml, which builds Hop.exe on a Windows runner and
# adds Hop-Windows.zip to the same release.
#
# Usage: scripts/macos/release.sh vX.Y.Z      (for example v0.1.0)
#
# Before running it:
# - set the version in the root Cargo.toml ([workspace.package]) to X.Y.Z,
#   then run `cargo check --workspace`, which updates Cargo.lock to match
#   (it lists the workspace's own version too);
# - write the release notes in docs/releases/vX.Y.Z.md;
# - commit everything, Cargo.lock included, and push it to origin.
#
# It refuses unless the tag is vX.Y.Z for the workspace version, the working
# tree is clean, Cargo.lock matches Cargo.toml, and HEAD is on some origin
# branch, so the release is built from exactly the commit that GitHub tags.
# If a tag vX.Y.Z already exists on GitHub, it must point at HEAD too. When
# the release already exists, this only replaces Hop-macOS.zip on it; the
# Windows build does not start again. To rebuild it, rerun the release's
# Windows run (gh run rerun RUN-ID --failed, for a run that failed), or run
# the Release workflow by hand, which GitHub allows only once
# .github/workflows/release.yml is on the default branch (main).
#
# The Windows zip is added after the release is published, so release
# immutability must stay off in the repository's settings.
#
# Every gh command goes to github.com (GH_HOST, and --repo KrishSChavan/hop).
# gh on this Mac is also signed in to the employer's GitHub Enterprise, which
# must never be used for Hop.
#
# Building, signing and zipping touch nothing on screen; nothing here runs
# Hop. Only make a release when the user asks for one.

set -euo pipefail

GITHUB_REPO="KrishSChavan/hop"
export GH_HOST="github.com"
# No questions and no update notices from gh: this script decides.
export GH_PROMPT_DISABLED=1
export GH_NO_UPDATE_NOTIFIER=1

if [ "$#" -ne 1 ]; then
    echo "usage: scripts/macos/release.sh vX.Y.Z (the version in Cargo.toml, for example v0.1.0)" >&2
    exit 2
fi
TAG="$1"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# On this Mac rustup is Homebrew's keg-only build, which is not on PATH.
if [ -d /opt/homebrew/opt/rustup/bin ]; then
    export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
fi
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO/target}"
# Absolute, as in make-app.sh, so this finds the bundle it builds.
case "$TARGET_DIR" in
    /*) ;;
    *) TARGET_DIR="$PWD/$TARGET_DIR" ;;
esac
export CARGO_TARGET_DIR="$TARGET_DIR"
APP="$TARGET_DIR/Hop.app"
DIST="$TARGET_DIR/dist"
ZIP="$DIST/Hop-macOS.zip"
NOTES="$REPO/docs/releases/$TAG.md"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# The tag names the workspace version: the first `version = "..."` line of
# the root Cargo.toml (the [workspace.package] one), as in make-app.sh.
TAG_PATTERN='^v[0-9]+\.[0-9]+\.[0-9]+$'
if ! [[ "$TAG" =~ $TAG_PATTERN ]]; then
    echo "error: \"$TAG\" is not a release tag; use vX.Y.Z, for example v0.1.0" >&2
    exit 1
fi
VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$REPO/Cargo.toml" | head -n 1)"
if [ "${TAG#v}" != "$VERSION" ]; then
    echo "error: $TAG does not match the workspace version in Cargo.toml (\"${VERSION:-none}\");" \
        "set version = \"${TAG#v}\" under [workspace.package], commit and push, then run this again" >&2
    exit 1
fi
if [ ! -f "$NOTES" ]; then
    echo "error: there are no release notes for $TAG; write docs/releases/$TAG.md, commit and push it" >&2
    exit 1
fi

# A clean working tree, untracked files included: the release must be
# exactly the commit that gets tagged.
CHANGES="$(git -C "$REPO" status --porcelain)"
if [ -n "$CHANGES" ]; then
    echo "error: the working tree is not clean; commit (and push) or remove these first:" >&2
    echo "$CHANGES" >&2
    exit 1
fi

# Cargo.lock must match Cargo.toml (after a version change, say), checked
# before the long build: the build would otherwise update it, and the check
# after the build would stop the release; the Windows build (cargo build
# --locked) refuses a stale one too.
if ! (cd "$REPO" && cargo metadata --locked --format-version 1 >/dev/null 2>"$WORK/metadata.err"); then
    echo "error: Cargo.lock does not match Cargo.toml; run cargo check --workspace, then commit and push Cargo.lock:" >&2
    cat "$WORK/metadata.err" >&2
    exit 1
fi

# origin must be the user's own repository on github.com, so the checks
# below are about the place the release goes.
ORIGIN_URL="$(git -C "$REPO" remote get-url origin)"
ORIGIN_LOWER="$(printf '%s' "$ORIGIN_URL" | tr '[:upper:]' '[:lower:]')"
case "$ORIGIN_LOWER" in
    https://github.com/krishschavan/hop | https://github.com/krishschavan/hop.git | \
        git@github.com:krishschavan/hop | git@github.com:krishschavan/hop.git | \
        ssh://git@github.com/krishschavan/hop | ssh://git@github.com/krishschavan/hop.git) ;;
    *)
        echo "error: origin is $ORIGIN_URL, not github.com/$GITHUB_REPO; Hop is released only there" >&2
        exit 1
        ;;
esac

# HEAD must already be on GitHub: gh creates the tag there, at this commit.
HEAD_SHA="$(git -C "$REPO" rev-parse HEAD)"
ON_ORIGIN="$(git -C "$REPO" branch -r --contains HEAD --list 'origin/*')"
if [ -z "$ON_ORIGIN" ]; then
    echo "error: HEAD ($HEAD_SHA) is on no origin branch; push it first (git push)" >&2
    exit 1
fi

if ! command -v gh >/dev/null; then
    echo "error: the GitHub CLI (gh) is not installed" >&2
    exit 1
fi
if ! gh auth status --hostname github.com >/dev/null 2>&1; then
    echo "error: gh is not signed in to github.com; run: GH_HOST=github.com gh auth login --hostname github.com" >&2
    exit 1
fi

# A tag that already exists on GitHub decides which commit the release (and
# so the Windows build) uses; gh ignores --target then. It must be HEAD, or
# the two zips would come from different code. An annotated tag is listed
# twice, the second time ("^{}") with the commit it points at.
REMOTE_TAGS="$(git -C "$REPO" ls-remote --tags origin)"
TAG_COMMIT="$(awk -v ref="refs/tags/$TAG" '
    $2 == ref { direct = $1 }
    $2 == ref "^{}" { peeled = $1 }
    END { print (peeled != "" ? peeled : direct) }' <<<"$REMOTE_TAGS")"
if [ -n "$TAG_COMMIT" ] && [ "$TAG_COMMIT" != "$HEAD_SHA" ]; then
    echo "error: the tag $TAG on GitHub points at $TAG_COMMIT, not at HEAD ($HEAD_SHA)." \
        "Release a new version instead, or check out that commit" >&2
    exit 1
fi

# Whether the release exists already. gh says "release not found" when it
# does not; any other failure (the network, sign-in) stops here.
if RELEASE_INFO="$(gh release view "$TAG" --repo "$GITHUB_REPO" --json isDraft,targetCommitish \
    --jq '"\(.isDraft) \(.targetCommitish)"' 2>"$WORK/view.err")"; then
    EXISTS=yes
    IS_DRAFT="${RELEASE_INFO%% *}"
    DRAFT_TARGET="${RELEASE_INFO#* }"
    # A draft has no tag yet: publishing it tags its target, and the Windows
    # build builds that. It must be HEAD, or the two zips would come from
    # different code (a draft left behind by an interrupted run, say).
    if [ "$IS_DRAFT" = true ] && [ "$DRAFT_TARGET" != "$HEAD_SHA" ]; then
        echo "error: the draft release $TAG targets $DRAFT_TARGET, not HEAD ($HEAD_SHA)." \
            "Point it at HEAD first: GH_HOST=github.com gh release edit $TAG --repo $GITHUB_REPO --target $HEAD_SHA" >&2
        exit 1
    fi
    echo "==> Release $TAG exists: Hop-macOS.zip on it will be replaced"
elif grep -q "release not found" "$WORK/view.err"; then
    EXISTS=no
    IS_DRAFT=false
    echo "==> Release $TAG does not exist yet: it will be published with Hop-macOS.zip"
else
    echo "error: could not look up release $TAG on github.com:" >&2
    cat "$WORK/view.err" >&2
    exit 1
fi

echo "==> Building Hop.app for $TAG ($HEAD_SHA)"
"$REPO/scripts/macos/make-app.sh"

# The build must not have changed anything that is committed (a Cargo.lock
# that needed updating, say), or the zip would not match the tagged commit.
CHANGES="$(git -C "$REPO" status --porcelain)"
if [ -n "$CHANGES" ]; then
    echo "error: the build changed files in the working tree; commit and push them, then run this again:" >&2
    echo "$CHANGES" >&2
    exit 1
fi

echo "==> Zipping Hop.app into $ZIP"
mkdir -p "$DIST"
rm -f "$ZIP"
# ditto, as Finder's Compress does, keeps the bundle's permissions, symbolic
# links and extended attributes, so the code signature survives the zip.
ditto -c -k --sequesterRsrc --keepParent "$APP" "$ZIP"

# Check the copy people download: unzip it and verify its signature.
ditto -x -k "$ZIP" "$WORK/unzipped"
if ! codesign --verify --strict --verbose=2 "$WORK/unzipped/Hop.app"; then
    echo "error: the Hop.app inside $ZIP is not validly signed" >&2
    exit 1
fi
echo "==> The zipped Hop.app is validly signed, for $(lipo -archs "$WORK/unzipped/Hop.app/Contents/MacOS/hop")"

SHA256="$(shasum -a 256 "$ZIP" | awk '{ print $1 }')"
echo "==> SHA-256 of Hop-macOS.zip: $SHA256"

if [ "$EXISTS" = no ]; then
    echo "==> Publishing release $TAG on github.com/$GITHUB_REPO"
    gh release create "$TAG" --repo "$GITHUB_REPO" --target "$HEAD_SHA" \
        --title "Hop $VERSION" --notes-file "$NOTES" "$ZIP"
    echo "==> Publishing started the Windows build (.github/workflows/release.yml)."
    echo "    It adds Hop-Windows.zip to the release when it finishes. To follow it:"
    echo "    GH_HOST=github.com gh run list --repo $GITHUB_REPO --workflow release.yml"
else
    echo "==> Replacing Hop-macOS.zip on release $TAG"
    gh release upload "$TAG" "$ZIP" --clobber --repo "$GITHUB_REPO"
    if [ "$IS_DRAFT" = true ]; then
        echo "==> Release $TAG is a draft. Publishing it starts the Windows build:"
        echo "    GH_HOST=github.com gh release edit $TAG --repo $GITHUB_REPO --draft=false"
    else
        echo "==> The Windows build does not start again by itself. To rebuild Hop-Windows.zip,"
        echo "    rerun the release's Windows run (find it with the first command):"
        echo "    GH_HOST=github.com gh run list --repo $GITHUB_REPO --workflow release.yml"
        echo "    GH_HOST=github.com gh run rerun RUN-ID --repo $GITHUB_REPO"
        echo "    or, once .github/workflows/release.yml is on main, run it by hand:"
        echo "    GH_HOST=github.com gh workflow run release.yml --repo $GITHUB_REPO --ref $TAG -f tag=$TAG"
    fi
fi

echo "==> Release page:"
gh release view "$TAG" --repo "$GITHUB_REPO" --json url --jq .url

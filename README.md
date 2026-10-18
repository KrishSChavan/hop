# Hop

One keyboard and mouse across a Mac and a Windows PC. Push the cursor past the screen edge and it continues on the other computer, and the keyboard follows. Copy text on one machine and paste it on the other with that machine's own shortcut. Drag files across to a Received folder.

**Status:** Hop has an app on each computer. It pairs the two computers, knows which side each one sits on, and shares one keyboard and mouse both ways, with Cmd and Ctrl swapped. The text clipboard (M4) and files (M5) come later. See [docs/ROADMAP.md](docs/ROADMAP.md).

## Install

Download `Hop-macOS.zip` (for the Mac) or `Hop-Windows.zip` (for the PC) from the [Releases page](https://github.com/KrishSChavan/hop/releases), then follow [docs/INSTALL.md](docs/INSTALL.md).

## Try it

```
cargo run -p hop -- simulate      # a narrated Mac <-> PC session, fully simulated
cargo run -p hop -- config show   # where the config lives and what's in it
```

## Build and test

```
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo build --release -p hop-app  # the app
```

Rust 1.99.1 is pinned in `rust-toolchain.toml`. Rustup installs it automatically. On the Mac, `scripts/macos/make-app.sh` builds the signed `Hop.app`.

## Layout

```
crates/
  hop-types     shared vocabulary: ids, geometry, input events, session messages
  hop-proto     wire format: framing, encoding, Hello and capability negotiation
  hop-keymap    key tables, the Cmd/Ctrl swap, pressed-key tracking
  hop-core      hand-off state machine, screen layout and edge mapping (no OS access)
  hop-config    TOML configuration
  hop-net       the encrypted link: identity, pairing, discovery
  hop-platform  OS integration traits, with hop-platform-macos and hop-platform-windows
  hop-selftest  the guided self-test of the platform layer
  hop-agent     the agent, pairing, settings and the hop command line
apps/
  hop           the hop command
  hop-app       the app: Hop.app on the Mac, Hop.exe on Windows
assets/icon/    the app icon
packaging/      what ships next to Hop.exe in the Windows zip
scripts/        Hop.app, releases, the Windows type-check
docs/           architecture, decisions, roadmap, install guide, Windows setup
```

## Docs

- [Install](docs/INSTALL.md): installing and using Hop
- [Architecture](docs/ARCHITECTURE.md): how it works
- [Decisions](docs/DECISIONS.md): requirements, constraints and design decisions
- [Roadmap](docs/ROADMAP.md): milestones
- [The app](docs/M6-APP.md): the spec for the app
- [Windows setup](docs/WINDOWS-SETUP.md): preparing the PC for building and testing Hop

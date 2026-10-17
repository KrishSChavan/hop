# Hop

One keyboard and mouse across a Mac and a Windows PC. Push the cursor past the screen edge and it continues on the other computer, and the keyboard follows. Copy text on one machine and paste it on the other with that machine's own shortcut. Drag files across to a Received folder.

**Status:** M0, the foundation, is done. The protocol, keymap, hand-off logic and configuration work and are tested, but Hop doesn't touch the OS yet (that's M1). See [docs/ROADMAP.md](docs/ROADMAP.md).

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
```

Rust 1.99.1 is pinned in `rust-toolchain.toml`. Rustup installs it automatically.

## Layout

```
crates/
  hop-types     shared vocabulary: ids, geometry, input events, session messages
  hop-proto     wire format: framing, encoding, Hello and capability negotiation
  hop-keymap    key tables, the Cmd/Ctrl swap, pressed-key tracking
  hop-core      hand-off state machine, screen layout and edge mapping (no OS access)
  hop-config    TOML configuration
  hop-platform  OS integration traits (macOS and Windows implementations from M1)
apps/
  hop           the agent
docs/           architecture, decisions, roadmap, Windows setup
```

## Docs

- [Architecture](docs/ARCHITECTURE.md): how it works
- [Decisions](docs/DECISIONS.md): requirements, constraints and design decisions
- [Roadmap](docs/ROADMAP.md): milestones
- [Windows setup](docs/WINDOWS-SETUP.md): preparing the PC

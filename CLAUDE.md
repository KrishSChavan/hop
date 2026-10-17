# Hop

Hop is the user's personal software KVM: one keyboard and mouse shared between their Mac and their Windows 11 laptop over home Wi-Fi, with two-way control, a Cmd↔Ctrl swap, text clipboard sync, and files dropped into a "Received" folder. The user does not read code; Claude writes and maintains all of it, so explain changes in plain language.

## Where things are

- `docs/DECISIONS.md`: the user's requirements and the design decisions. Read it before asking the user anything.
- `docs/ARCHITECTURE.md`: the spec, including the hand-off state machine.
- `docs/ROADMAP.md`: milestones, plus open questions tagged by milestone.
- `docs/WINDOWS-SETUP.md`: preparing the PC.

## Build and test

```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- The toolchain is pinned in `rust-toolchain.toml`.
- On the Mac, rustup is Homebrew's keg-only build: put `/opt/homebrew/opt/rustup/bin` on PATH first.
- To type-check Windows code from the Mac: `cargo check --workspace --target x86_64-pc-windows-msvc`.
- CI runs on Windows for every push. macOS CI runs only when triggered by hand, because private repos bill macOS minutes at 10x.

## Rules

- **The Mac is the user's employer-managed work laptop.** Their IT department approved running Hop on it (2026-10-17). Keep Hop transparent there: no hidden capture, nothing logged.
- **The PC is the user's personal machine** (confirmed 2026-10-17).
- **Tell the user before input moves.** Before running anything that moves the cursor, types, or blocks the mouse and keyboard, tell the user what will happen and for how long, so they're not surprised mid-task.
- **No secrets in logs.** Never log keystrokes, clipboard contents or file contents, even in debug builds.
- **Fail open.** Any error, timeout or disconnect must return both machines to local control and release every key and button Hop pressed.
- **OS code stays in `hop-platform`.** It is the only crate that calls OS APIs. Every other crate is pure, portable Rust with `#![forbid(unsafe_code)]`.
- **Keep the seams.** Keep new features behind the existing seams (platform traits, capability negotiation, the local control API), because the user plans many more features, including an AI assistant that will drive Hop through that API.
- **GitHub.** The repo is `KrishSChavan/hop` on github.com, private. On the Mac, `gh` is also signed into the employer's GitHub Enterprise; never create or push Hop repositories there. Use `GH_HOST=github.com` with `gh` commands.
- **Commits.** Commit messages end with a `Co-Authored-By:` line for the Claude model that wrote the change.

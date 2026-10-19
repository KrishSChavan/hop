# Hop

Hop is the user's personal software KVM: one keyboard and mouse shared between their Mac and their Windows 11 laptop over home Wi-Fi, with two-way control, keys as labelled (an optional Cmd↔Ctrl swap), text clipboard sync, and files dropped into a "Received" folder. The user does not read code; Claude writes and maintains all of it, so explain changes in plain language.

## Where things are

- `docs/DECISIONS.md`: the user's requirements and the design decisions. Read it before asking the user anything.
- `docs/ARCHITECTURE.md`: the spec, including the hand-off state machine.
- `docs/ROADMAP.md`: milestones, plus open questions tagged by milestone.
- `docs/M6-APP.md`: the spec for the app.
- `docs/M4-M5-DESIGN.md`: the design for the first-test fixes, the clipboard (M4) and files (M5).
- `docs/INSTALL.md`: the user's guide to installing and using the app. It quotes the app's labels from `docs/M6-APP.md`, so change the two together.
- `docs/WINDOWS-SETUP.md`: preparing the PC for building and testing.
- `docs/releases/`: release notes, one file per tag (`vX.Y.Z.md`).
- `crates/hop-agent`: the agent, pairing, settings and the `hop` command line, shared by both apps.
- `apps/hop`: the `hop` command, a one-line `main` that calls `hop_agent::cli::main()`.
- `apps/hop-app`: the app (eframe/egui). It ships as `Hop.exe` on Windows and as the executable inside `Hop.app` on the Mac. On the Mac, a first argument that names a `hop` subcommand runs that command instead of opening the window.

## Build and test

```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- The toolchain is pinned in `rust-toolchain.toml`.
- On the Mac, rustup is Homebrew's keg-only build: put `/opt/homebrew/opt/rustup/bin` on PATH first.
- To type-check and lint Windows code from the Mac, run `scripts/dev/check-windows.sh`. A plain `cargo check --target x86_64-pc-windows-msvc` fails there, because ring, under hop-net's TLS, needs the script's stub C headers on a Mac.
- Build the app with `cargo build --release -p hop-app`. On the Mac, `scripts/macos/make-app.sh` builds it into `target/Hop.app`, signed, with the icon from `assets/icon`. Opening Hop.app, or running `hop-app` with no arguments, opens the app's window.
- The app's tests draw the window headless and never touch the screen. To see the real window, `EFRAME_SCREENSHOT_TO=shot.png cargo run -p hop-app --features screenshot` saves it as first drawn and exits; it opens the window for a moment, so tell the user first.
- On the Mac, run the self-test with `scripts/macos/run-selftest.sh` and the agent with `scripts/macos/run-agent.sh`; both start Hop.app with a `hop` subcommand. Run `hop pair` from Terminal (`target/Hop.app/Contents/MacOS/hop pair`, or `target/release/hop pair` after `cargo build --release -p hop`), so the code shows there.
- CI runs on Windows for every push. macOS CI runs only when triggered by hand, because private repos bill macOS minutes at 10x.

## Releases

- Only make a release when the user asks for one.
- First set the version in the root `Cargo.toml` (`[workspace.package]`), run `cargo check --workspace` so `Cargo.lock` picks up the new version, and write `docs/releases/vX.Y.Z.md`. Commit (with `Cargo.lock`) and push.
- Then run `scripts/macos/release.sh vX.Y.Z` on the Mac. It refuses unless the tag matches the version, the working tree is clean, `Cargo.lock` matches and HEAD is on origin. It builds and signs Hop.app and publishes the release on github.com with `Hop-macOS.zip`. Publishing starts `.github/workflows/release.yml`, which builds on Windows (with `--locked`, and the C runtime linked into Hop.exe) and adds `Hop-Windows.zip` (`Hop.exe` and `packaging/windows/README.txt`).
- Running it again for an existing release only replaces the Mac zip. To rebuild the Windows zip, rerun the release's Windows run: `GH_HOST=github.com gh run list --repo KrishSChavan/hop --workflow release.yml`, then `GH_HOST=github.com gh run rerun RUN-ID --repo KrishSChavan/hop`. Running the workflow by hand (`gh workflow run release.yml --repo KrishSChavan/hop --ref vX.Y.Z -f tag=vX.Y.Z`) works only once `release.yml` is on the default branch (`main`).
- The Windows zip is added after the release is published, so release immutability must stay off in the repository's settings.

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

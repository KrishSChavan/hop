# Setting up the Windows laptop

Do this once on the PC. Claude Code on the PC builds and tests the Windows side of Hop.

1. **Git.** Install Git for Windows (`winget install Git.Git`), then open a new terminal.
2. **Rust.** Download and run `rustup-init.exe` from <https://rustup.rs>. When it offers to install the Visual Studio C++ build tools, accept. Those tools let Rust build Windows programs.
3. **Claude Code.** Install Claude Code for Windows and sign in.
4. **The code.** Once the repository is on your GitHub account:
   ```
   git clone https://github.com/<your-account>/hop.git
   cd hop
   cargo test --workspace
   ```
   Every test should pass.
5. **Smart App Control.** Open Windows Security → App & browser control → Smart App Control and note whether it says On, Evaluation or Off. Tell Claude. When it is On, Windows can block programs that aren't signed, which affects how we run test builds.
6. **Firewall (from M2 on).** The first time Hop listens on the network, Windows asks whether to allow it. Pick **Private networks** and click **Allow**, as an administrator. If you click Cancel, Windows silently blocks Hop until the rule is fixed.

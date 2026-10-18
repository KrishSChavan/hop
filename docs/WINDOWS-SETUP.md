# Setting up the Windows laptop

To use Hop every day, you don't need this page: download the app and follow [INSTALL.md](INSTALL.md). This page sets the PC up for building and testing Hop.

Do this once on the PC. Claude Code on the PC builds and tests the Windows side of Hop.

1. **Install the tools.** In PowerShell:
   ```
   winget install --id Git.Git
   winget install --id GitHub.cli
   ```
   Then download and run `rustup-init.exe` from <https://rustup.rs>. When it offers to install the Visual Studio C++ build tools, accept. Rust needs them to build Windows programs.
2. **Install Claude Code** for Windows and sign in.
3. **Open a new terminal.** Installers add their programs to the PATH, but terminal windows that were already open don't see the change. If PowerShell says `gh` or `cargo` "is not recognized", this is why. Close the window and open a new one.
4. **Sign in to GitHub and get the code.** The repository is private:
   ```
   gh auth login
   gh repo clone KrishSChavan/hop
   cd hop
   cargo test --workspace
   ```
   In `gh auth login`, choose GitHub.com, HTTPS, Yes to authenticate Git, and "Login with a web browser". Every test should pass.
5. **Smart App Control.** Open Windows Security → App & browser control → Smart App Control and note whether it says On, Evaluation or Off. Tell Claude. When it is On, Windows can block programs that aren't signed, which affects how we run test builds and the downloaded app ([INSTALL.md](INSTALL.md), step 3).
6. **Home network and firewall (from M2 on).**
   - First make sure Windows treats your home Wi-Fi as private: Settings → Network & internet → Wi-Fi → your network → Network profile type → **Private**. On a Public network, Windows blocks the other laptop from reaching Hop.
   - The first time Hop listens on the network (`hop run`, `hop pair`, or **Show a pairing code** or **Start sharing** in the app), Windows asks whether to allow it. Pick **Private networks** and click **Allow**, as an administrator. If you click Cancel, Windows silently blocks Hop until the rule is fixed.
   - The rule belongs to that one program file. Each new location (the downloaded `Hop.exe`, `target\release\hop.exe`, `target\release\hop-app.exe`) asks once, and moving a program asks again.
   - Hop uses UDP port 47800 for the link, 47801 only while pairing, and 47802 for `hop latency`.

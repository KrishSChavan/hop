//! The `hop` command. Every subcommand lives in the hop-agent library
//! (`hop_agent::cli`), which the Hop app shares, so the two can never drift
//! apart.

#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> anyhow::Result<ExitCode> {
    hop_agent::cli::main()
}

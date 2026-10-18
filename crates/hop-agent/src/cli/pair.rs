//! `hop pair`: pairs this computer with the other one, once
//! (docs/M2-DESIGN.md, "Pairing"), in a terminal. The flow itself is
//! [`crate::pairing`]'s; this is how the terminal words it.
//!
//! Without a code, this computer shows one and waits; with a code, it joins
//! the computer showing it. Pairing runs in a terminal on both computers,
//! never through `open` on the Mac: `open` would send the output, and so
//! the code, to a log file. The code is printed on screen only and never
//! logged.

use std::io::{self, BufRead as _, Write};
use std::net::IpAddr;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use hop_config::Side;
use zeroize::Zeroizing;

use crate::logging::{Destination, Filter, Log};
use crate::pairing::{self, Front, Outcome, PairEvent, PairRole, Question, Replaced, Saved, Setup};
use crate::{paths, startup};

/// Runs `hop pair`: shows a code when `code` is empty, joins otherwise.
pub(crate) fn run(code: &[String], address: Option<&str>) -> Result<ExitCode> {
    let log = start_log();
    let startup = startup::load()?;
    for note in &startup.notes {
        println!("{note}");
    }
    let setup = Setup::new(startup)?;
    let runtime = pairing::runtime()?;
    let role = if code.is_empty() {
        PairRole::Show
    } else {
        PairRole::Join {
            // "482 913" may arrive as two arguments.
            code: Zeroizing::new(code.join(" ")),
            address: address.map(str::to_owned),
        }
    };
    let result = runtime
        .block_on(pairing::pair(&setup, role, &mut Terminal))
        .map(|outcome| say_outcome(&mut io::stdout().lock(), &outcome));
    // Waiting for the y/N answer may leave a thread reading the terminal;
    // the process ends anyway.
    runtime.shutdown_timeout(Duration::from_secs(1));
    if let Some(log) = log {
        log.flush(Duration::from_secs(1));
    }
    result
}

/// Pairing writes its own log next to the agent's. Only warnings from the
/// network layer end up there (never the code).
fn start_log() -> Option<Log> {
    let path = paths::log_dir()?.join("pair.log");
    let log = Log::start(Destination::File(path)).ok()?;
    log.install(Filter::AGENT);
    Some(log)
}

/// The terminal: events and the outcome go to stdout, the answer comes
/// from stdin.
struct Terminal;

impl Front for Terminal {
    fn event(&mut self, event: PairEvent) {
        say(&mut io::stdout().lock(), &event);
    }

    async fn confirm(&mut self, question: Question) -> bool {
        ask_yes_no(&question_text(&question), question.within).await
    }
}

/// What the terminal says for `event`. Write errors are ignored: there is
/// no one to tell.
fn say(out: &mut impl Write, event: &PairEvent) {
    let _ = match event {
        PairEvent::Code(code) => say_code(out, code),
        PairEvent::NotAnnounced { reason } => writeln!(
            out,
            "Could not announce this computer on the local network ({reason}). On the other \
             computer, add --address with this computer's IP address."
        ),
        PairEvent::WrongCode { failures } => writeln!(
            out,
            "Someone entered a wrong code ({failures} of 3 tries used)."
        ),
        PairEvent::Searching => writeln!(out, "Looking for the computer showing the code..."),
        PairEvent::NoHostFound => writeln!(
            out,
            "Could not find a computer showing a pairing code. Check that hop pair is waiting \
             on the other computer, or add --address with its IP address."
        ),
        PairEvent::SeveralHosts(hosts) => say_hosts(out, hosts),
        PairEvent::Connecting => writeln!(out, "Connecting..."),
        PairEvent::WaitingForConfirmation { host } => {
            writeln!(out, "Waiting for you to confirm on {host}.")
        }
        // The question goes to `confirm`, and the end is the outcome.
        PairEvent::Confirm { .. } | PairEvent::Unconfirmed { .. } | PairEvent::Finished(_) => {
            Ok(())
        }
    };
}

/// The code, on screen only.
fn say_code(out: &mut impl Write, code: &Zeroizing<String>) -> io::Result<()> {
    writeln!(out)?;
    writeln!(out, "Pairing code: {}", code.as_str())?;
    writeln!(out)?;
    writeln!(
        out,
        "On the other computer, open a terminal and run hop pair with this code, for example: \
         hop pair {}",
        code.as_str()
    )?;
    writeln!(
        out,
        "The code works once, for 2 minutes. Never show it in a screen share. Waiting for the \
         other computer..."
    )
}

fn say_hosts(out: &mut impl Write, hosts: &[(String, Vec<IpAddr>)]) -> io::Result<()> {
    writeln!(out, "More than one computer is showing a pairing code:")?;
    for (name, ips) in hosts {
        let addresses: Vec<String> = ips.iter().map(ToString::to_string).collect();
        writeln!(out, "  {name} at {}", addresses.join(", "))?;
    }
    writeln!(
        out,
        "Run hop pair again with --address and the address of the one you mean."
    )
}

/// The y/N question on the computer showing the code.
fn question_text(question: &Question) -> String {
    let replaces = match &question.replaces {
        Some(sentence) => format!(" {sentence}"),
        None => String::new(),
    };
    format!(
        "{name} ({os}) entered the code. Pair with it?{replaces} Answer y only if {name} says \
         it is waiting for you. [y/N] ",
        name = question.name,
        os = question.os,
    )
}

/// Says how pairing ended, and gives `hop pair`'s exit code. Write errors
/// are ignored: there is no one to tell.
fn say_outcome(out: &mut impl Write, outcome: &Outcome) -> ExitCode {
    let _ = match outcome {
        Outcome::Paired(saved) => {
            let _ = say_saved(out, saved);
            return ExitCode::SUCCESS;
        }
        Outcome::SavedHereOnly { saved, error } => {
            // Saved here, but the other computer may not have heard.
            say_saved(out, saved).and_then(|()| writeln!(out, "However: {error}."))
        }
        Outcome::Declined => writeln!(out, "Not paired. Nothing was saved on either computer."),
        Outcome::Failed(error) => writeln!(out, "Not paired: {error}."),
        Outcome::NothingRecorded { name } => {
            writeln!(out, "Paired with {name}, but nothing was recorded here.")
        }
        Outcome::NotSaved { name, error } => writeln!(
            out,
            "{name} said yes, but this computer could not save the pairing: {error}. Run hop \
             pair again on both computers."
        ),
        // Already said.
        Outcome::NoHostFound | Outcome::SeveralHosts => Ok(()),
    };
    ExitCode::FAILURE
}

fn say_saved(out: &mut impl Write, saved: &Saved) -> io::Result<()> {
    writeln!(out)?;
    writeln!(
        out,
        "Paired with {} ({}). {} {}.",
        saved.name,
        saved.os,
        if saved.replaced == Replaced::Nothing {
            "Saved it in"
        } else {
            "Updated its entry in"
        },
        saved.path.display()
    )?;
    if let Replaced::OldKey(old) = &saved.replaced {
        writeln!(
            out,
            "It replaces the pairing with {old}, which used a different key: that key can no \
             longer connect to this computer."
        )?;
    }
    let (placement, edge) = match saved.side {
        Side::Left => ("to the left of", "left"),
        Side::Right => ("to the right of", "right"),
        Side::Above => ("above", "top"),
        Side::Below => ("below", "bottom"),
    };
    writeln!(
        out,
        "It sits {placement} this computer's screen: push the cursor past the {edge} edge to \
         move to it."
    )?;
    writeln!(
        out,
        "To put it on another side, change side = \"{}\" in its [[peers]] entry in that file \
         to left, right, above or below.",
        saved.side
    )?;
    writeln!(
        out,
        "If Hop is running, quit it and start it again to use the new pairing."
    )
}

/// Asks a y/N question, taking "no" if no answer comes within `within`.
async fn ask_yes_no(question: &str, within: Duration) -> bool {
    print!("{question}");
    let _ = io::stdout().flush();
    let (tx, rx) = tokio::sync::oneshot::channel();
    // A plain thread, not the runtime's blocking pool: a terminal read
    // cannot be cancelled, and the runtime must not wait for it on exit.
    let reader = std::thread::Builder::new()
        .name("hop-pair-answer".into())
        .spawn(move || {
            let mut line = String::new();
            let answer = io::stdin().lock().read_line(&mut line).ok().map(|_| line);
            let _ = tx.send(answer);
        });
    if reader.is_err() {
        return false;
    }
    match tokio::time::timeout(within, rx).await {
        Ok(Ok(Some(line))) => matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        Ok(_) => false,
        Err(_) => {
            println!();
            println!("No answer in time.");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::path::PathBuf;

    use hop_net::PairError;
    use hop_types::Os;

    use super::*;

    fn said(event: &PairEvent) -> String {
        let mut out = Vec::new();
        say(&mut out, event);
        String::from_utf8(out).expect("text")
    }

    fn outcome(outcome: &Outcome) -> (String, ExitCode) {
        let mut out = Vec::new();
        let code = say_outcome(&mut out, outcome);
        (String::from_utf8(out).expect("text"), code)
    }

    fn saved(side: Side, replaced: Replaced) -> Saved {
        Saved {
            name: "Krish's PC".to_owned(),
            os: Os::Windows,
            side,
            replaced,
            path: PathBuf::from("config.toml"),
        }
    }

    #[test]
    fn the_code_is_shown_with_how_to_use_it() {
        assert_eq!(
            said(&PairEvent::Code(Zeroizing::new("482 913".to_owned()))),
            "\nPairing code: 482 913\n\nOn the other computer, open a terminal and run hop pair \
             with this code, for example: hop pair 482 913\nThe code works once, for 2 minutes. \
             Never show it in a screen share. Waiting for the other computer...\n"
        );
    }

    #[test]
    fn progress_reads_as_it_always_has() {
        assert_eq!(
            said(&PairEvent::NotAnnounced {
                reason: "mDNS: no interface".to_owned()
            }),
            "Could not announce this computer on the local network (mDNS: no interface). On the \
             other computer, add --address with this computer's IP address.\n"
        );
        assert_eq!(
            said(&PairEvent::WrongCode { failures: 2 }),
            "Someone entered a wrong code (2 of 3 tries used).\n"
        );
        assert_eq!(
            said(&PairEvent::Searching),
            "Looking for the computer showing the code...\n"
        );
        assert_eq!(
            said(&PairEvent::NoHostFound),
            "Could not find a computer showing a pairing code. Check that hop pair is waiting on \
             the other computer, or add --address with its IP address.\n"
        );
        let two = vec![
            (
                "Mac".to_owned(),
                vec![
                    IpAddr::from(Ipv4Addr::new(192, 168, 1, 10)),
                    IpAddr::from(Ipv4Addr::new(169, 254, 3, 4)),
                ],
            ),
            (
                "Other".to_owned(),
                vec![IpAddr::from(Ipv4Addr::new(192, 168, 1, 11))],
            ),
        ];
        assert_eq!(
            said(&PairEvent::SeveralHosts(two)),
            "More than one computer is showing a pairing code:\n  Mac at 192.168.1.10, \
             169.254.3.4\n  Other at 192.168.1.11\nRun hop pair again with --address and the \
             address of the one you mean.\n"
        );
        assert_eq!(said(&PairEvent::Connecting), "Connecting...\n");
        assert_eq!(
            said(&PairEvent::WaitingForConfirmation {
                host: "Krish's MacBook".to_owned()
            }),
            "Waiting for you to confirm on Krish's MacBook.\n"
        );
        assert_eq!(said(&PairEvent::Finished(Err("no".to_owned()))), "");
    }

    #[test]
    fn the_question_says_when_a_yes_replaces_an_old_key() {
        let mut question = Question {
            name: "Krish's PC".to_owned(),
            os: Os::Windows,
            replaces: None,
            within: Duration::from_secs(100),
        };
        assert_eq!(
            question_text(&question),
            "Krish's PC (Windows) entered the code. Pair with it? Answer y only if Krish's PC \
             says it is waiting for you. [y/N] "
        );
        question.replaces = Some(
            "This replaces the pairing with Old PC, whose old key will stop working.".to_owned(),
        );
        assert_eq!(
            question_text(&question),
            "Krish's PC (Windows) entered the code. Pair with it? This replaces the pairing with \
             Old PC, whose old key will stop working. Answer y only if Krish's PC says it is \
             waiting for you. [y/N] "
        );
    }

    #[test]
    fn a_pairing_says_where_it_was_saved_and_where_the_computer_sits() {
        let (text, code) = outcome(&Outcome::Paired(saved(Side::Right, Replaced::Nothing)));
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(
            text,
            "\nPaired with Krish's PC (Windows). Saved it in config.toml.\nIt sits to the right \
             of this computer's screen: push the cursor past the right edge to move to it.\nTo \
             put it on another side, change side = \"right\" in its [[peers]] entry in that \
             file to left, right, above or below.\nIf Hop is running, quit it and start it again \
             to use the new pairing.\n"
        );
        let (text, _) = outcome(&Outcome::Paired(saved(
            Side::Below,
            Replaced::OldKey("Old PC".to_owned()),
        )));
        assert!(text.contains("Updated its entry in config.toml."), "{text}");
        assert!(
            text.contains(
                "It replaces the pairing with Old PC, which used a different key: that key can \
                 no longer connect to this computer.\n"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "It sits below this computer's screen: push the cursor past the bottom edge"
            ),
            "{text}"
        );
    }

    #[test]
    fn every_other_outcome_fails_with_its_sentence() {
        let cases = [
            (
                Outcome::SavedHereOnly {
                    saved: saved(Side::Left, Replaced::Entry),
                    error: PairError::Connection("gone".to_owned()),
                },
                "However: the connection to the other laptop was lost: gone.\n",
            ),
            (
                Outcome::Declined,
                "Not paired. Nothing was saved on either computer.\n",
            ),
            (
                Outcome::Failed(PairError::WrongCode),
                "Not paired: the code did not match; check it and run hop pair again on both \
                 laptops.\n",
            ),
            (
                Outcome::NothingRecorded {
                    name: "PC".to_owned(),
                },
                "Paired with PC, but nothing was recorded here.\n",
            ),
            (
                Outcome::NotSaved {
                    name: "PC".to_owned(),
                    error: "disk full".to_owned(),
                },
                "PC said yes, but this computer could not save the pairing: disk full. Run hop \
                 pair again on both computers.\n",
            ),
            (Outcome::NoHostFound, ""),
            (Outcome::SeveralHosts, ""),
        ];
        for (case, ending) in cases {
            let (text, code) = outcome(&case);
            assert_eq!(code, ExitCode::FAILURE, "{case:?}");
            assert!(text.ends_with(ending), "{case:?}: {text:?}");
        }
    }
}

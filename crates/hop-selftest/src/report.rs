//! The self-test's results and its text report.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use hop_types::Os;

/// The steps of the self-test, in the order they run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum StepId {
    /// 1: the OS, displays, Accessibility and Secure Input.
    Environment,
    /// 2: passive capture of the user's mouse and keyboard.
    Capture,
    /// 3: injected typing into the test window.
    Typing,
    /// 4: an injected move and click on the test window's button.
    Clicking,
    /// 5: suppression holds the cursor still, then lapses by itself.
    Suppression,
    /// 6: the cursor hides, then comes back where it was.
    Park,
    /// 7: a push against the right edge reaches Hop's core.
    EdgePush,
}

impl StepId {
    /// Every step, in order.
    pub const ALL: [StepId; 7] = [
        StepId::Environment,
        StepId::Capture,
        StepId::Typing,
        StepId::Clicking,
        StepId::Suppression,
        StepId::Park,
        StepId::EdgePush,
    ];

    /// The step's number as the user sees it, from 1.
    pub fn number(self) -> usize {
        match self {
            StepId::Environment => 1,
            StepId::Capture => 2,
            StepId::Typing => 3,
            StepId::Clicking => 4,
            StepId::Suppression => 5,
            StepId::Park => 6,
            StepId::EdgePush => 7,
        }
    }

    /// The step's name in instructions and the report.
    pub fn title(self) -> &'static str {
        match self {
            StepId::Environment => "Environment",
            StepId::Capture => "Capture",
            StepId::Typing => "Typing",
            StepId::Clicking => "Clicking",
            StepId::Suppression => "Holding the mouse still",
            StepId::Park => "Hiding the cursor",
            StepId::EdgePush => "Edge push",
        }
    }

    /// The first line of the step's instructions, such as
    /// "Step 2 of 7: Capture".
    pub fn heading(self) -> String {
        format!(
            "Step {} of {}: {}",
            self.number(),
            StepId::ALL.len(),
            self.title()
        )
    }
}

/// How a step went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Everything the step checks worked.
    Pass,
    /// Something did not work; the text says what, in plain words.
    Fail(String),
    /// The step did not run, or could not check what it is for; the text
    /// says why and what to do.
    Skipped(String),
}

impl Outcome {
    /// Whether this is [`Outcome::Pass`].
    pub fn is_pass(&self) -> bool {
        matches!(self, Outcome::Pass)
    }
}

/// The result of one step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepReport {
    /// Which step this is.
    pub id: StepId,
    /// How it went.
    pub outcome: Outcome,
    /// A short remark for the summary line, such as something the user must
    /// confirm because Hop cannot check it.
    pub note: Option<String>,
    /// What the step measured: counts, positions and states, never which
    /// keys were pressed and never clipboard contents.
    pub details: Vec<String>,
}

impl StepReport {
    /// A report for `id` with `outcome` and nothing else yet.
    pub fn new(id: StepId, outcome: Outcome) -> Self {
        Self {
            id,
            outcome,
            note: None,
            details: Vec::new(),
        }
    }

    /// The step's line in the summary, such as
    /// "2. Capture ................ PASS".
    pub fn summary_line(&self) -> String {
        let label = format!("{}. {} ", self.id.number(), self.id.title());
        let status = match &self.outcome {
            Outcome::Pass => "PASS".to_string(),
            Outcome::Fail(reason) => format!("FAIL: {reason}"),
            Outcome::Skipped(reason) => format!("SKIPPED: {reason}"),
        };
        match &self.note {
            Some(note) => format!("{label:.<28} {status} ({note})"),
            None => format!("{label:.<28} {status}"),
        }
    }
}

/// Why a run ended before every step had its turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// The user pressed the emergency chord during this step.
    EmergencyChord(StepId),
    /// The overall time limit ran out during this step.
    TimeLimit(StepId),
    /// This step panicked. Hop gave back control and skipped the rest.
    Crashed(StepId),
}

/// The overall result of a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Every step passed.
    Pass,
    /// At least one step failed.
    Fail,
    /// Nothing failed, but at least one step was skipped.
    Incomplete,
    /// The user stopped the run with the emergency chord.
    Stopped,
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Verdict::Pass => "PASS",
            Verdict::Fail => "FAIL",
            Verdict::Incomplete => "INCOMPLETE",
            Verdict::Stopped => "STOPPED",
        })
    }
}

/// The result of a self-test run. Its `Display` is the text report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// The OS the self-test ran on.
    pub os: Os,
    /// Hop's version.
    pub version: String,
    /// When the run started.
    pub started: SystemTime,
    /// One entry per step of the run, in order.
    pub steps: Vec<StepReport>,
    /// Why the run ended early, if it did.
    pub stop: Option<Stop>,
    /// Where the report was written, if it was.
    pub saved_to: Option<PathBuf>,
    /// Why writing the report failed, if it did.
    pub save_error: Option<String>,
}

impl Report {
    /// The overall result.
    pub fn verdict(&self) -> Verdict {
        if matches!(self.stop, Some(Stop::EmergencyChord(_))) {
            Verdict::Stopped
        } else if matches!(self.stop, Some(Stop::Crashed(_) | Stop::TimeLimit(_)))
            || self.count(|o| matches!(o, Outcome::Fail(_))) > 0
        {
            Verdict::Fail
        } else if self.count(|o| matches!(o, Outcome::Skipped(_))) > 0 {
            Verdict::Incomplete
        } else {
            Verdict::Pass
        }
    }

    /// The report of step `id`, if it was part of the run.
    pub fn step(&self, id: StepId) -> Option<&StepReport> {
        self.steps.iter().find(|step| step.id == id)
    }

    /// Writes the text report to `path`, creating its folder if needed.
    pub fn write_to(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, self.to_string())
    }

    fn count(&self, matches: impl Fn(&Outcome) -> bool) -> usize {
        self.steps
            .iter()
            .filter(|step| matches(&step.outcome))
            .count()
    }

    /// The last line of the report.
    pub fn verdict_line(&self) -> String {
        let total = self.steps.len();
        let failed: Vec<&str> = self
            .steps
            .iter()
            .filter(|step| matches!(step.outcome, Outcome::Fail(_)))
            .map(|step| step.id.title())
            .collect();
        let skipped = self.count(|o| matches!(o, Outcome::Skipped(_)));
        let passed = self.count(Outcome::is_pass);
        let verdict = self.verdict();
        match verdict {
            Verdict::Pass => format!("{verdict}: all {total} steps passed."),
            Verdict::Fail => format!(
                "{verdict}: {} of {total} steps failed ({}); {passed} passed, {skipped} skipped.",
                failed.len(),
                failed.join(", ")
            ),
            Verdict::Incomplete => format!(
                "{verdict}: nothing failed, but {skipped} of {total} steps were skipped; \
                 {passed} passed."
            ),
            Verdict::Stopped => {
                let during = match self.stop {
                    Some(Stop::EmergencyChord(id)) => {
                        format!(" during step {} ({})", id.number(), id.title())
                    }
                    _ => String::new(),
                };
                format!(
                    "{verdict}: you pressed the emergency chord{during}, and Hop gave the mouse \
                     and keyboard back. Before that: {passed} passed, {} failed.",
                    failed.len()
                )
            }
        }
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Hop self-test report")?;
        writeln!(
            f,
            "Hop {} on {}, started {}",
            self.version,
            self.os,
            utc(self.started)
        )?;
        writeln!(f)?;
        for step in &self.steps {
            writeln!(f, "{}", step.summary_line())?;
        }
        writeln!(f)?;
        writeln!(f, "Details")?;
        for step in &self.steps {
            writeln!(f)?;
            writeln!(f, "{}. {}", step.id.number(), step.id.title())?;
            match &step.outcome {
                Outcome::Pass => {}
                Outcome::Fail(reason) => writeln!(f, "   Failed: {reason}")?,
                Outcome::Skipped(reason) => writeln!(f, "   Skipped: {reason}")?,
            }
            for detail in &step.details {
                writeln!(f, "   - {detail}")?;
            }
        }
        writeln!(f)?;
        writeln!(f, "Overall: {}", self.verdict_line())
    }
}

/// `time` as "YYYY-MM-DD HH:MM:SS UTC".
pub(crate) fn utc(time: SystemTime) -> String {
    let Ok(since_epoch) = time.duration_since(UNIX_EPOCH) else {
        return "at an unknown time".to_string();
    };
    let secs = since_epoch.as_secs();
    let (year, month, day) = civil_from_days(secs / 86_400);
    let rest = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// The proleptic Gregorian date `days` after 1970-01-01, using Howard
/// Hinnant's `civil_from_days` algorithm (public domain).
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    // Shift the epoch to 0000-03-01 so leap days fall at the end of a year.
    let z = days + 719_468;
    let era = z / 146_097;
    let day_of_era = z % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn utc_dates() {
        assert_eq!(utc(at(0)), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc(at(951_782_400)), "2000-02-29 00:00:00 UTC");
        assert_eq!(utc(at(1_700_000_000)), "2023-11-14 22:13:20 UTC");
        assert_eq!(utc(at(1_792_281_599)), "2026-10-17 23:59:59 UTC");
        assert_eq!(
            utc(UNIX_EPOCH - Duration::from_secs(1)),
            "at an unknown time"
        );
    }

    fn sample() -> Report {
        let mut env = StepReport::new(StepId::Environment, Outcome::Pass);
        env.details.push("Operating system: macOS".to_string());
        let mut park = StepReport::new(StepId::Park, Outcome::Pass);
        park.note = Some("confirm you saw it disappear".to_string());
        Report {
            os: Os::MacOs,
            version: "0.1.0".to_string(),
            started: at(1_700_000_000),
            steps: vec![
                env,
                StepReport::new(
                    StepId::Typing,
                    Outcome::Fail("only the first 3 of 12 characters arrived".to_string()),
                ),
                StepReport::new(
                    StepId::Suppression,
                    Outcome::Skipped("Accessibility is off".to_string()),
                ),
                park,
            ],
            stop: None,
            saved_to: None,
            save_error: None,
        }
    }

    #[test]
    fn the_report_lists_steps_then_details_then_the_verdict() {
        let text = sample().to_string();
        let expected = "\
Hop self-test report
Hop 0.1.0 on macOS, started 2023-11-14 22:13:20 UTC

1. Environment ............. PASS
3. Typing .................. FAIL: only the first 3 of 12 characters arrived
5. Holding the mouse still . SKIPPED: Accessibility is off
6. Hiding the cursor ....... PASS (confirm you saw it disappear)

Details

1. Environment
   - Operating system: macOS

3. Typing
   Failed: only the first 3 of 12 characters arrived

5. Holding the mouse still
   Skipped: Accessibility is off

6. Hiding the cursor

Overall: FAIL: 1 of 4 steps failed (Typing); 2 passed, 1 skipped.
";
        assert_eq!(text, expected);
    }

    #[test]
    fn verdicts() {
        let mut report = sample();
        assert_eq!(report.verdict(), Verdict::Fail);
        report.steps.remove(1);
        assert_eq!(report.verdict(), Verdict::Incomplete);
        assert_eq!(
            report.verdict_line(),
            "INCOMPLETE: nothing failed, but 1 of 3 steps were skipped; 2 passed."
        );
        report.steps.remove(1);
        assert_eq!(report.verdict(), Verdict::Pass);
        assert_eq!(report.verdict_line(), "PASS: all 2 steps passed.");
        report.stop = Some(Stop::EmergencyChord(StepId::Park));
        assert_eq!(report.verdict(), Verdict::Stopped);
        assert!(
            report
                .verdict_line()
                .starts_with("STOPPED: you pressed the emergency chord during step 6")
        );
        // A crash or the time limit is a failure, whatever the steps say.
        report.stop = Some(Stop::Crashed(StepId::Park));
        assert_eq!(report.verdict(), Verdict::Fail);
        report.stop = Some(Stop::TimeLimit(StepId::Park));
        assert_eq!(report.verdict(), Verdict::Fail);
    }

    #[test]
    fn headings_and_numbers() {
        assert_eq!(StepId::Capture.heading(), "Step 2 of 7: Capture");
        assert_eq!(
            StepId::ALL.map(StepId::number),
            [1, 2, 3, 4, 5, 6, 7],
            "numbers follow the run order"
        );
        let mut sorted = StepId::ALL;
        sorted.sort();
        assert_eq!(sorted, StepId::ALL);
    }

    #[test]
    fn writing_creates_the_folder() {
        let dir = std::env::temp_dir().join(format!(
            "hop-selftest-report-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = dir.join("Logs").join("selftest.txt");
        let report = sample();
        report.write_to(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), report.to_string());
        fs::remove_dir_all(&dir).unwrap();
    }
}

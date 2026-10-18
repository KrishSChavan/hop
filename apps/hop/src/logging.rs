//! The agent's log (docs/M2-DESIGN.md, "Logs").
//!
//! A small `tracing` subscriber turns each event into one line of text and
//! hands it to a writer thread through a bounded queue. No thread that logs
//! ever waits for a disk: the core thread in particular must never stall on
//! a slow file, because a stalled core lets the suppression lease lapse. When
//! the queue is full a line is dropped and counted, and the next line that
//! gets through says how many were lost.
//!
//! - Lines go to `hop.log` in the log folder, capped at 10 MiB plus one old
//!   file (`hop.old.log`), so a stranger on the network who triggers
//!   warnings cannot fill the disk.
//! - Status lines (see [`status!`](crate::status)) go to stdout as well: one
//!   short line per connection or session change. In Hop.app stdout goes to a
//!   file too, so status lines follow the same rules as the log.
//! - Hop's own crates log at `info` and above; every other crate (quinn,
//!   mdns-sd) at `warn` and above.
//! - Control characters in a line are escaped, so text that came from the
//!   network can never forge a second line.
//!
//! What never goes in: keys or key codes, clipboard text, file contents,
//! pairing codes, private keys and TLS secrets. Nothing in Hop logs them;
//! the end-to-end test checks the agent's log for key names and codes.

use std::fmt::{self, Write as _};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
#[cfg(test)]
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tracing::field::{Field, Visit};
use tracing::level_filters::LevelFilter;
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::Interest;
use tracing::{Event, Level, Metadata, Subscriber};

/// The `tracing` target of status lines.
pub const STATUS_TARGET: &str = "hop::status";

/// Writes a status line: one short sentence about a connection or session
/// change, printed on stdout and written to the log. Never put keys,
/// clipboard text or codes in one.
#[macro_export]
macro_rules! status {
    ($($arg:tt)+) => {
        ::tracing::info!(target: $crate::logging::STATUS_TARGET, $($arg)+)
    };
}

/// Lines waiting for the writer thread. Generous for bursts, small enough
/// that a stuck disk cannot pile up much memory.
const QUEUE_LINES: usize = 4096;

/// The log file is rotated before it grows past this.
const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

/// A single line is cut at this many bytes, so one huge value (a long name
/// from the network) cannot bloat the queue.
const MAX_LINE_BYTES: usize = 4096;

/// Which events are written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Filter {
    /// The most verbose level for Hop's own crates (targets `hop` and
    /// `hop_*`).
    pub hop: LevelFilter,
    /// The most verbose level for every other crate.
    pub others: LevelFilter,
}

impl Filter {
    /// What the agent writes: Hop at `info`, everything else at `warn`
    /// (quinn, rustls and mdns-sd are noisy below that, and the design caps
    /// them there).
    pub const AGENT: Filter = Filter {
        hop: LevelFilter::INFO,
        others: LevelFilter::WARN,
    };

    fn allows(&self, metadata: &Metadata<'_>) -> bool {
        let limit = if is_hop_target(metadata.target()) {
            self.hop
        } else {
            self.others
        };
        *metadata.level() <= limit
    }
}

fn is_hop_target(target: &str) -> bool {
    target == "hop" || target.starts_with("hop::") || target.starts_with("hop_")
}

/// Where the writer thread puts lines.
pub enum Destination {
    /// A log file (created with its folder if missing), plus stdout for
    /// status lines.
    File(PathBuf),
    /// Status lines on stdout and nothing else, for when the log file
    /// cannot be opened: status lines are the user's only view of what Hop
    /// is doing, so they must not go missing with the file.
    StatusOnly,
    /// Memory, for tests: every line, status lines included, in order.
    #[cfg(test)]
    Memory(Arc<Mutex<Vec<String>>>),
}

/// One item for the writer thread.
enum Entry {
    Line(String),
    /// A status line: `line` for the log, `text` for stdout.
    Status {
        line: String,
        text: String,
    },
    /// Answer once everything before this has been written.
    Flush(SyncSender<()>),
}

/// A running log: the writer thread and the queue to it. Cheap to clone.
#[derive(Clone)]
pub struct Log {
    tx: SyncSender<Entry>,
    dropped: Arc<AtomicU64>,
}

impl Log {
    /// Starts the writer thread.
    ///
    /// # Errors
    ///
    /// The log file or its folder could not be opened or created, or the
    /// thread could not start.
    pub fn start(destination: Destination) -> io::Result<Log> {
        let mut writer = Writer::open(destination)?;
        let (tx, rx) = mpsc::sync_channel(QUEUE_LINES);
        thread::Builder::new()
            .name("hop-log".into())
            .spawn(move || writer.run(rx))?;
        Ok(Log {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Makes this log the process-wide destination of `tracing` events,
    /// filtered by `filter`. Returns false if another subscriber was
    /// installed first (there can be only one per process).
    pub fn install(&self, filter: Filter) -> bool {
        let subscriber = LineSubscriber {
            log: self.clone(),
            filter,
            next_span: AtomicU64::new(1),
        };
        tracing::subscriber::set_global_default(subscriber).is_ok()
    }

    /// Waits up to `timeout` until every line queued so far is written.
    ///
    /// Called on the way out, where waiting a little is fine; a stuck disk
    /// still cannot hold Hop up for longer than `timeout`.
    pub fn flush(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let mut entry = Entry::Flush(done_tx);
        loop {
            match self.tx.try_send(entry) {
                Ok(()) => break,
                Err(TrySendError::Full(back)) if Instant::now() < deadline => {
                    entry = back;
                    thread::sleep(Duration::from_millis(2));
                }
                Err(_) => return,
            }
        }
        let _ = done_rx.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    }

    /// Queues `entry` without waiting. A full queue drops it and counts it,
    /// and the next entry that gets through is preceded by a note.
    fn send(&self, entry: Entry) {
        let lost = self.dropped.swap(0, Ordering::Relaxed);
        if lost > 0 {
            let note = format!(
                "{} WARN  hop::logging: {lost} log lines were dropped because the log \
                 writer fell behind",
                utc_timestamp(SystemTime::now())
            );
            if self.tx.try_send(Entry::Line(note)).is_err() {
                self.dropped.fetch_add(lost, Ordering::Relaxed);
            }
        }
        match self.tx.try_send(entry) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            // The writer thread is gone (it never stops on its own); there
            // is nowhere left to write.
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// Queues a line that does not come from `tracing`, such as the panic
    /// hook's report. Never blocks.
    pub fn write_status(&self, text: &str) {
        let text = clean(text);
        let line = format!(
            "{} WARN  {STATUS_TARGET}: {text}",
            utc_timestamp(SystemTime::now())
        );
        self.send(Entry::Status { line, text });
    }
}

/// The `tracing` subscriber: formats events into lines for a [`Log`].
struct LineSubscriber {
    log: Log,
    filter: Filter,
    next_span: AtomicU64,
}

impl Subscriber for LineSubscriber {
    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        // The filter never changes, so each call site is decided once.
        if self.filter.allows(metadata) {
            Interest::always()
        } else {
            Interest::never()
        }
    }

    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.filter.allows(metadata)
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(self.filter.hop.max(self.filter.others))
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        // Spans are not written; they only need distinct, non-zero ids.
        let id = self.next_span.fetch_add(1, Ordering::Relaxed);
        Id::from_u64(id.max(1))
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let metadata = event.metadata();
        let mut message = String::new();
        let mut fields = String::new();
        event.record(&mut LineVisitor {
            message: &mut message,
            fields: &mut fields,
        });
        let mut line = format!(
            "{} {:<5} {}: {message}{fields}",
            utc_timestamp(SystemTime::now()),
            level_name(*metadata.level()),
            metadata.target(),
        );
        truncate(&mut line, MAX_LINE_BYTES);
        if metadata.target() == STATUS_TARGET {
            truncate(&mut message, MAX_LINE_BYTES);
            self.log.send(Entry::Status {
                line,
                text: message,
            });
        } else {
            self.log.send(Entry::Line(line));
        }
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::ERROR => "ERROR",
        Level::WARN => "WARN",
        Level::INFO => "INFO",
        Level::DEBUG => "DEBUG",
        Level::TRACE => "TRACE",
    }
}

/// Collects an event's message and its other fields as ` name=value`.
struct LineVisitor<'a> {
    message: &'a mut String,
    fields: &'a mut String,
}

impl LineVisitor<'_> {
    fn add(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            push_clean(self.message, value);
        } else {
            let _ = write!(self.fields, " {}=", field.name());
            push_clean(self.fields, value);
        }
    }
}

impl Visit for LineVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.add(field, value);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.add(field, &format!("{value:?}"));
    }
}

/// Appends `text` with line breaks and other control characters escaped.
/// Hop's network layer already cleans text that came from peers; this is
/// the last line of defence against a forged log line.
fn push_clean(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push(' '),
            '\u{2028}' | '\u{2029}' => out.push(' '),
            c if c.is_control() => out.push('?'),
            c => out.push(c),
        }
    }
}

fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    push_clean(&mut out, text);
    truncate(&mut out, MAX_LINE_BYTES);
    out
}

/// Cuts `text` to at most `max` bytes, on a character boundary.
fn truncate(text: &mut String, max: usize) {
    if text.len() <= max {
        return;
    }
    let mut end = max.saturating_sub(3);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("...");
}

/// The writer thread's side.
struct Writer {
    target: Target,
}

enum Target {
    File(FileLog),
    StatusOnly,
    #[cfg(test)]
    Memory(Arc<Mutex<Vec<String>>>),
}

impl Writer {
    fn open(destination: Destination) -> io::Result<Writer> {
        let target = match destination {
            Destination::File(path) => Target::File(FileLog::open(path)?),
            Destination::StatusOnly => Target::StatusOnly,
            #[cfg(test)]
            Destination::Memory(lines) => Target::Memory(lines),
        };
        Ok(Writer { target })
    }

    fn run(&mut self, rx: Receiver<Entry>) {
        // `recv` fails only when every sender is gone; the subscriber keeps
        // one for the life of the process, so this runs until exit.
        while let Ok(entry) = rx.recv() {
            self.handle(entry);
            while let Ok(entry) = rx.try_recv() {
                self.handle(entry);
            }
            self.flush();
        }
        self.flush();
    }

    fn handle(&mut self, entry: Entry) {
        match entry {
            Entry::Line(line) => self.write(&line),
            Entry::Status { line, text } => {
                self.write(&line);
                if self.prints_status() {
                    // Only this thread writes stdout while the agent runs,
                    // so a terminal that stops reading can stall nothing
                    // but the log.
                    let mut out = io::stdout().lock();
                    let _ = writeln!(out, "{text}");
                    let _ = out.flush();
                }
            }
            Entry::Flush(done) => {
                self.flush();
                let _ = done.try_send(());
            }
        }
    }

    fn write(&mut self, line: &str) {
        match &mut self.target {
            Target::File(file) => file.write_line(line),
            Target::StatusOnly => {}
            #[cfg(test)]
            Target::Memory(lines) => lines
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(line.to_owned()),
        }
    }

    fn flush(&mut self) {
        match &mut self.target {
            Target::File(file) => file.flush(),
            Target::StatusOnly => {}
            #[cfg(test)]
            Target::Memory(_) => {}
        }
    }

    /// Status lines go to stdout too, except in tests.
    fn prints_status(&self) -> bool {
        match self.target {
            Target::File(_) | Target::StatusOnly => true,
            #[cfg(test)]
            Target::Memory(_) => false,
        }
    }
}

/// `hop.log` with its size cap.
struct FileLog {
    path: PathBuf,
    old: PathBuf,
    file: Option<BufWriter<File>>,
    size: u64,
}

impl FileLog {
    fn open(path: PathBuf) -> io::Result<FileLog> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let old = old_path(&path);
        let file = open_append(&path)?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        let mut log = FileLog {
            path,
            old,
            file: Some(BufWriter::new(file)),
            size,
        };
        if log.size >= MAX_LOG_BYTES {
            log.rotate();
        }
        Ok(log)
    }

    fn write_line(&mut self, line: &str) {
        let len = line.len() as u64 + 1;
        if self.size > 0 && self.size + len > MAX_LOG_BYTES {
            self.rotate();
        }
        if let Some(file) = &mut self.file
            && writeln!(file, "{line}").is_ok()
        {
            self.size += len;
        }
    }

    /// Moves the full log aside as the one old file and starts a new one.
    fn rotate(&mut self) {
        self.flush();
        // Closed first: Windows cannot rename a file that is still open.
        self.file = None;
        let _ = fs::rename(&self.path, &self.old);
        self.file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)
            .ok()
            .map(BufWriter::new);
        self.size = 0;
    }

    fn flush(&mut self) {
        if let Some(file) = &mut self.file {
            let _ = file.flush();
        }
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// `hop.log` → `hop.old.log`.
fn old_path(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "hop".to_owned());
    path.with_file_name(format!("{stem}.old.log"))
}

/// `2026-10-18 12:34:56.789Z`: UTC, since the agent has no time zone data.
pub fn utc_timestamp(now: SystemTime) -> String {
    let since = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = since.as_secs();
    let (year, month, day) = civil_from_days(secs / 86_400);
    let rest = secs % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}.{:03}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60,
        since.subsec_millis()
    )
}

/// The calendar date `days` days after 1970-01-01 (Howard Hinnant's
/// `civil_from_days`, for dates from 1970 on).
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::OnceLock;

    use super::*;

    /// The process-wide test log: every `tracing` event from every crate, at
    /// every level, in memory. Installed once; tests that look at it only
    /// check what must never appear, so other tests' lines do no harm.
    pub(crate) fn capture() -> &'static (Log, Arc<Mutex<Vec<String>>>) {
        static CAPTURE: OnceLock<(Log, Arc<Mutex<Vec<String>>>)> = OnceLock::new();
        CAPTURE.get_or_init(|| {
            let lines = Arc::new(Mutex::new(Vec::new()));
            let log = Log::start(Destination::Memory(lines.clone())).expect("log thread");
            assert!(
                log.install(Filter {
                    hop: LevelFilter::TRACE,
                    others: LevelFilter::TRACE,
                }),
                "no other subscriber in the test binary"
            );
            (log, lines)
        })
    }

    /// Everything captured so far, after waiting for the writer.
    pub(crate) fn captured_text() -> String {
        let (log, lines) = capture();
        log.flush(Duration::from_secs(5));
        lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .join("\n")
    }

    #[test]
    fn dates_are_right() {
        let at = |secs: u64| utc_timestamp(UNIX_EPOCH + Duration::from_secs(secs));
        assert_eq!(at(0), "1970-01-01 00:00:00.000Z");
        // 2000-02-29 (a leap day in a century year), 13:14:15.
        assert_eq!(at(951_830_055), "2000-02-29 13:14:15.000Z");
        assert_eq!(at(951_868_800), "2000-03-01 00:00:00.000Z");
        // 2026-10-18 23:59:59.
        assert_eq!(at(1_792_367_999), "2026-10-18 23:59:59.000Z");
        assert_eq!(
            utc_timestamp(UNIX_EPOCH + Duration::from_millis(1_500)),
            "1970-01-01 00:00:01.500Z"
        );
    }

    #[test]
    fn control_characters_cannot_forge_a_line() {
        assert_eq!(
            clean("PC\nWARN fake line\r\u{2028}x\u{7}"),
            "PC\\nWARN fake line\\r x?"
        );
    }

    #[test]
    fn long_lines_are_cut_on_a_character_boundary() {
        let mut text = "é".repeat(5000);
        truncate(&mut text, 101);
        assert!(text.len() <= 101);
        assert!(text.ends_with("..."));
        let mut short = "ok".to_owned();
        truncate(&mut short, 10);
        assert_eq!(short, "ok");
    }

    #[test]
    fn only_hop_crates_log_below_warn() {
        assert!(is_hop_target("hop"));
        assert!(is_hop_target("hop::agent"));
        assert!(is_hop_target("hop_net::link"));
        assert!(!is_hop_target("hopscotch"));
        assert!(!is_hop_target("quinn_proto::connection"));
    }

    #[test]
    fn the_file_is_capped_with_one_old_file() {
        let dir = std::env::temp_dir().join(format!(
            "hop-log-test-{}-{}",
            std::process::id(),
            utc_timestamp(SystemTime::now()).replace([' ', ':', '.'], "-")
        ));
        let path = dir.join("hop.log");
        let mut log = FileLog::open(path.clone()).expect("open");
        let line = "x".repeat(1023);
        // 12 MiB of 1 KiB lines: one rotation, and the new file holds the rest.
        for _ in 0..12 * 1024 {
            log.write_line(&line);
        }
        log.flush();
        let size = fs::metadata(&path).expect("log").len();
        let old = fs::metadata(dir.join("hop.old.log"))
            .expect("old log")
            .len();
        assert!(size <= MAX_LOG_BYTES, "{size}");
        assert!(old <= MAX_LOG_BYTES && old > MAX_LOG_BYTES - 2048, "{old}");
        assert_eq!(size + old, 12 * 1024 * 1024);
        drop(log);
        let _ = fs::remove_dir_all(&dir);
    }
}

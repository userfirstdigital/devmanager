//! Client startup phase machine and its trace log.
//!
//! The GPUI client can take tens of seconds to show task content while the host
//! itself boots in about a second. Nothing recorded where that time went, so the
//! slow phase could not be named. [`StartupTrace`] is the pure machine that
//! records the phases; [`SharedStartupTrace`] is the cheap cloneable handle the
//! shell, its bootstrap thread, and its controller worker all record through,
//! and the one that appends each transition to `client-startup.log`.
//!
//! The machine is deliberately free of I/O so it can be unit tested, and the
//! entries it keeps are the same values a later loading UI will render.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use time::{format_description, OffsetDateTime};

/// Phases of a client run, in the order they are reached.
///
/// The ordering is load bearing: [`SharedStartupTrace::advance_to`] only ever
/// moves forward, so a later observer (the render path, say) cannot rewind a
/// phase a background thread already passed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum StartupPhase {
    /// Isolated host process spawned, or a live one reused.
    HostSpawn,
    /// Transport connect attempts.
    HostConnect,
    /// Authenticated hello / negotiated parameters.
    Hello,
    /// Fleet resync / admission.
    Synchronize,
    /// Paging the snapshot.
    SnapshotPages,
    /// First `ClientModel` admitted by the shell.
    FirstProjection,
    /// Shell stage reached Cockpit with a selected task's surfaces requested.
    Ready,
}

impl StartupPhase {
    /// Every phase in reached order. Used by the summary line.
    pub const ALL: [StartupPhase; 7] = [
        StartupPhase::HostSpawn,
        StartupPhase::HostConnect,
        StartupPhase::Hello,
        StartupPhase::Synchronize,
        StartupPhase::SnapshotPages,
        StartupPhase::FirstProjection,
        StartupPhase::Ready,
    ];

    /// Stable log/UI label. Never localized: this string is grepped.
    pub fn label(self) -> &'static str {
        match self {
            StartupPhase::HostSpawn => "HostSpawn",
            StartupPhase::HostConnect => "HostConnect",
            StartupPhase::Hello => "Hello",
            StartupPhase::Synchronize => "Synchronize",
            StartupPhase::SnapshotPages => "SnapshotPages",
            StartupPhase::FirstProjection => "FirstProjection",
            StartupPhase::Ready => "Ready",
        }
    }
}

impl std::fmt::Display for StartupPhase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

/// One recorded transition, retry, or failure.
///
/// A retry and a failure both keep the phase they were recorded in; only the
/// attempt number and the detail text distinguish them, exactly as the log line
/// does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartupTraceEntry {
    phase: StartupPhase,
    entered_at_ms: u64,
    attempt: u32,
    detail: Option<String>,
}

impl StartupTraceEntry {
    pub fn phase(&self) -> StartupPhase {
        self.phase
    }

    /// Milliseconds since the trace started.
    pub fn entered_at_ms(&self) -> u64 {
        self.entered_at_ms
    }

    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    /// Render the trailing half of a log line: everything after the wall clock.
    fn render_suffix(&self) -> String {
        let minutes = self.entered_at_ms / 60_000;
        let seconds = (self.entered_at_ms % 60_000) / 1_000;
        let millis = self.entered_at_ms % 1_000;
        let mut line = format!(
            "+{minutes:02}:{seconds:02}.{millis:03} {} attempt={}",
            self.phase.label(),
            self.attempt
        );
        if let Some(detail) = self.detail.as_deref() {
            line.push_str(" detail=");
            line.push('"');
            line.push_str(&sanitize_detail(detail));
            line.push('"');
        }
        line
    }
}

/// Collapse anything that would break the one-line log format. Quotes become
/// apostrophes and control characters become spaces so a line is always one
/// line and always closes its quote.
fn sanitize_detail(detail: &str) -> String {
    detail
        .chars()
        .map(|character| match character {
            '"' => '\'',
            character if character.is_control() => ' ',
            character => character,
        })
        .collect()
}

/// The pure phase machine. No I/O, no clock beyond [`Instant`].
#[derive(Clone, Debug)]
pub struct StartupTrace {
    started_at: Instant,
    entries: Vec<StartupTraceEntry>,
    current: StartupPhase,
    attempt: u32,
    detail: Option<String>,
    failure_detail: Option<String>,
}

impl Default for StartupTrace {
    fn default() -> Self {
        Self::new()
    }
}

impl StartupTrace {
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            entries: Vec::new(),
            current: StartupPhase::HostSpawn,
            attempt: 1,
            detail: None,
            failure_detail: None,
        }
    }

    /// Enter `phase`, resetting the attempt counter. Records one entry.
    pub fn enter(&mut self, phase: StartupPhase, detail: Option<String>) -> StartupTraceEntry {
        self.current = phase;
        self.attempt = 1;
        self.detail = detail.clone();
        // Reaching a new phase is progress: whatever the previous one was
        // retrying is over, and a loading line that kept quoting it would be
        // naming a failure that no longer applies.
        self.failure_detail = None;
        self.push(detail)
    }

    /// Retry the current phase: same phase, attempt + 1. Records one entry.
    pub fn retry(&mut self, detail: Option<String>) -> StartupTraceEntry {
        self.attempt = self.attempt.saturating_add(1);
        self.detail = detail.clone();
        self.note_failure(detail.as_deref());
        self.push(detail)
    }

    /// Retry `phase` after a later phase has already been reached — the whole
    /// bootstrap unit going round again after a failure deep inside it. The
    /// attempt number continues that phase's own count rather than restarting.
    pub fn retry_in(&mut self, phase: StartupPhase, detail: Option<String>) -> StartupTraceEntry {
        if self.current == phase {
            return self.retry(detail);
        }
        let attempts_so_far = self
            .entries
            .iter()
            .rev()
            .find(|entry| entry.phase == phase)
            .map(|entry| entry.attempt)
            .unwrap_or(0);
        self.current = phase;
        self.attempt = attempts_so_far.saturating_add(1);
        self.detail = detail.clone();
        self.note_failure(detail.as_deref());
        self.push(detail)
    }

    /// Record an error without leaving the phase it happened in.
    pub fn fail(&mut self, detail: impl Into<String>) -> StartupTraceEntry {
        let detail = detail.into();
        self.detail = Some(detail.clone());
        self.failure_detail = Some(detail.clone());
        self.push(Some(detail))
    }

    /// Remember why a retry happened, so a loading line can say what is being
    /// retried rather than quoting whichever transition wrote `detail` last.
    fn note_failure(&mut self, detail: Option<&str>) {
        if let Some(detail) = detail {
            self.failure_detail = Some(detail.to_string());
        }
    }

    /// Record a sub-step inside the current phase without changing phase,
    /// attempt, or the failure detail: the timestamps of the work a phase
    /// spans are what attribute its duration.
    pub fn note(&mut self, detail: impl Into<String>) -> StartupTraceEntry {
        self.push(Some(detail.into()))
    }

    fn push(&mut self, detail: Option<String>) -> StartupTraceEntry {
        let entry = StartupTraceEntry {
            phase: self.current,
            entered_at_ms: duration_ms(self.started_at.elapsed()),
            attempt: self.attempt,
            detail,
        };
        self.entries.push(entry.clone());
        entry
    }

    pub fn current(&self) -> StartupPhase {
        self.current
    }

    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// The most recent detail text, whichever call recorded it.
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    /// The last failure this phase recorded, cleared when the next phase is
    /// entered. Unlike [`Self::detail`] this never carries an ordinary
    /// transition note, so a loading line built from it cannot claim that a
    /// successful step is being retried.
    pub fn failure_detail(&self) -> Option<&str> {
        self.failure_detail.as_deref()
    }

    pub fn entries(&self) -> &[StartupTraceEntry] {
        &self.entries
    }

    /// The instant the current phase was entered. Retries stay inside the same
    /// run, so this is the moment the user started waiting on THIS phase.
    pub fn phase_started_at(&self) -> Instant {
        self.started_at + Duration::from_millis(self.current_phase_started_at_ms())
    }

    /// Total time since the trace started.
    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// Time spent in the current phase, counting from the entry that first
    /// reached it (retries and failures stay inside the same run).
    pub fn elapsed_in_phase(&self) -> Duration {
        self.started_at
            .elapsed()
            .saturating_sub(Duration::from_millis(self.current_phase_started_at_ms()))
    }

    fn current_phase_started_at_ms(&self) -> u64 {
        let mut started_at_ms = 0;
        for entry in self.entries.iter().rev() {
            if entry.phase != self.current {
                break;
            }
            started_at_ms = entry.entered_at_ms;
        }
        started_at_ms
    }

    /// Duration of every phase that was actually reached, in reached order.
    /// The last phase runs to `now`.
    pub fn phase_durations(&self) -> Vec<(StartupPhase, Duration)> {
        let mut runs: Vec<(StartupPhase, u64)> = Vec::new();
        for entry in &self.entries {
            if runs.last().map(|(phase, _)| *phase) == Some(entry.phase) {
                continue;
            }
            runs.push((entry.phase, entry.entered_at_ms));
        }
        let total_ms = duration_ms(self.started_at.elapsed());
        runs.iter()
            .enumerate()
            .map(|(index, (phase, started_ms))| {
                let ended_ms = runs
                    .get(index + 1)
                    .map(|(_, next_ms)| *next_ms)
                    .unwrap_or(total_ms);
                (
                    *phase,
                    Duration::from_millis(ended_ms.saturating_sub(*started_ms)),
                )
            })
            .collect()
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The cloneable handle every recording site holds.
///
/// The phases are reached on three different threads — the bootstrap thread
/// (spawn/connect/hello), the controller worker (synchronize/snapshot pages),
/// and the shell itself (first projection/ready) — so the machine lives behind
/// a mutex rather than inside `NativeShell`. Recording takes the lock only on a
/// real transition; the render path uses [`Self::advance_to`], which is a no-op
/// once the phase has been reached.
#[derive(Clone)]
pub struct SharedStartupTrace {
    state: Arc<Mutex<TraceState>>,
}

struct TraceState {
    trace: StartupTrace,
    sink: Option<BufWriter<File>>,
}

static PROCESS_STARTUP_TRACE: OnceLock<SharedStartupTrace> = OnceLock::new();

impl SharedStartupTrace {
    /// A trace nobody else shares and that writes nowhere. Used by tests and by
    /// any shell that is not the process's real startup.
    pub fn detached() -> Self {
        Self {
            state: Arc::new(Mutex::new(TraceState {
                trace: StartupTrace::new(),
                sink: None,
            })),
        }
    }

    /// The one trace for this client run. Created on first use — which is the
    /// earliest startup site — so the elapsed clock starts as early as possible.
    pub fn process() -> Self {
        PROCESS_STARTUP_TRACE
            .get_or_init(SharedStartupTrace::detached)
            .clone()
    }

    fn lock(&self) -> MutexGuard<'_, TraceState> {
        // A poisoned trace must never take the client down with it.
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Point the trace at `<logs dir>/client-startup.log`, truncating it, and
    /// replay anything already recorded so no early phase is lost. I/O failure
    /// is reported once and never panics.
    pub fn attach_log_file(&self, path: &Path) {
        let file = match File::create(path) {
            Ok(file) => file,
            Err(error) => {
                eprintln!(
                    "devmanager: startup trace log cannot be opened {}: {error}",
                    path.display()
                );
                return;
            }
        };
        let mut state = self.lock();
        let mut sink = BufWriter::new(file);
        let replay: Vec<StartupTraceEntry> = state.trace.entries().to_vec();
        state.sink = if sink_after_replay(&mut sink, &replay) {
            Some(sink)
        } else {
            None
        };
    }

    /// Enter `phase` unconditionally.
    pub fn enter(&self, phase: StartupPhase, detail: Option<String>) {
        let mut state = self.lock();
        let entry = state.trace.enter(phase, detail);
        state.write(&entry);
    }

    /// Enter `phase` only when it is ahead of the current one. Safe to call
    /// from a render path or from any thread that may be behind another.
    pub fn advance_to(&self, phase: StartupPhase, detail: Option<String>) {
        let mut state = self.lock();
        if state.trace.current() >= phase {
            return;
        }
        let entry = state.trace.enter(phase, detail);
        state.write(&entry);
    }

    /// Retry the current phase.
    pub fn retry(&self, detail: Option<String>) {
        let mut state = self.lock();
        let entry = state.trace.retry(detail);
        state.write(&entry);
    }

    /// Retry `phase`, continuing that phase's own attempt count even when a
    /// later phase has since been reached.
    pub fn retry_in(&self, phase: StartupPhase, detail: Option<String>) {
        let mut state = self.lock();
        let entry = state.trace.retry_in(phase, detail);
        state.write(&entry);
    }

    /// Record an error in the current phase.
    pub fn fail(&self, detail: impl Into<String>) {
        let mut state = self.lock();
        let entry = state.trace.fail(detail);
        state.write(&entry);
    }

    /// See [`StartupTrace::note`].
    pub fn note(&self, detail: impl Into<String>) {
        let mut state = self.lock();
        let entry = state.trace.note(detail);
        state.write(&entry);
    }

    /// Record an error once per distinct text, so a render path that sees the
    /// same failure every frame writes one line.
    pub fn fail_once(&self, detail: impl Into<String>) {
        let detail = detail.into();
        let mut state = self.lock();
        if state
            .trace
            .entries()
            .last()
            .is_some_and(|entry| entry.detail() == Some(detail.as_str()))
        {
            return;
        }
        let entry = state.trace.fail(detail);
        state.write(&entry);
    }

    /// Read several values off the machine under ONE lock.
    ///
    /// A loading line renders phase, attempt, elapsed and detail together; four
    /// separate accessors would let a background thread move the phase between
    /// two of them and paint a line that never existed.
    pub fn with_trace<R>(&self, read: impl FnOnce(&StartupTrace) -> R) -> R {
        read(&self.lock().trace)
    }

    pub fn current(&self) -> StartupPhase {
        self.lock().trace.current()
    }

    pub fn attempt(&self) -> u32 {
        self.lock().trace.attempt()
    }

    pub fn elapsed_in_phase(&self) -> Duration {
        self.lock().trace.elapsed_in_phase()
    }

    pub fn entries(&self) -> Vec<StartupTraceEntry> {
        self.lock().trace.entries().to_vec()
    }

    /// A copy of the machine for a caller that wants to read several values
    /// without racing (a loading UI, for one).
    pub fn snapshot(&self) -> StartupTrace {
        self.lock().trace.clone()
    }

    /// Reach [`StartupPhase::Ready`] and write the one summary line that names
    /// every phase's duration, so "which phase took the 30 s" is one grep away.
    pub fn note_ready(&self, detail: Option<String>) {
        let mut state = self.lock();
        if state.trace.current() >= StartupPhase::Ready {
            return;
        }
        let entry = state.trace.enter(StartupPhase::Ready, detail);
        state.write(&entry);
        let summary = format!(
            "total={}ms {}",
            duration_ms(state.trace.elapsed()),
            state
                .trace
                .phase_durations()
                .into_iter()
                .map(|(phase, duration)| format!("{}={}ms", phase.label(), duration_ms(duration)))
                .collect::<Vec<_>>()
                .join(" ")
        );
        state.write_raw(&format!("{} summary {summary}", wall_clock_now()));
    }
}

impl std::fmt::Debug for SharedStartupTrace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SharedStartupTrace")
            .field("current", &self.current())
            .finish()
    }
}

impl TraceState {
    fn write(&mut self, entry: &StartupTraceEntry) {
        let line = format!("{} {}", wall_clock_now(), entry.render_suffix());
        self.write_raw(&line);
    }

    fn write_raw(&mut self, line: &str) {
        let Some(sink) = self.sink.as_mut() else {
            return;
        };
        if let Err(error) = writeln!(sink, "{line}").and_then(|()| sink.flush()) {
            eprintln!("devmanager: startup trace log write failed: {error}");
            // Report once: a failing handle would otherwise repeat per line.
            self.sink = None;
        }
    }
}

/// Replay the entries recorded before the log file existed. Returns false when
/// the sink is already unusable.
fn sink_after_replay(sink: &mut BufWriter<File>, replay: &[StartupTraceEntry]) -> bool {
    for entry in replay {
        let line = format!("{} {}", wall_clock_now(), entry.render_suffix());
        if let Err(error) = writeln!(sink, "{line}") {
            eprintln!("devmanager: startup trace log replay failed: {error}");
            return false;
        }
    }
    if let Err(error) = sink.flush() {
        eprintln!("devmanager: startup trace log replay flush failed: {error}");
        return false;
    }
    true
}

/// `2026-09-02 21:14:46.812` in local time, falling back to UTC when the
/// process cannot resolve its offset.
fn wall_clock_now() -> String {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    format_description::parse("[year]-[month]-[day] [hour]:[minute]:[second].[subsecond digits:3]")
        .ok()
        .and_then(|format| now.format(&format).ok())
        .unwrap_or_else(|| "0000-00-00 00:00:00.000".to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_note_records_detail_without_moving_phase_or_attempt() {
        let mut trace = super::StartupTrace::new();
        trace.enter(super::StartupPhase::SnapshotPages, None);
        trace.retry(None);
        let before = (trace.current(), trace.attempt());
        let entry = trace.note("synchronize returned in 12 ms");
        assert_eq!(entry.phase(), super::StartupPhase::SnapshotPages);
        assert_eq!(entry.attempt(), before.1);
        assert_eq!(entry.detail(), Some("synchronize returned in 12 ms"));
        assert_eq!((trace.current(), trace.attempt()), before);
        assert!(trace
            .entries()
            .iter()
            .any(|e| e.detail() == Some("synchronize returned in 12 ms")));
    }

    use super::*;

    #[test]
    fn enter_records_one_entry_per_phase_and_resets_the_attempt() {
        let mut trace = StartupTrace::new();
        assert_eq!(trace.current(), StartupPhase::HostSpawn);
        assert!(trace.entries().is_empty());

        trace.enter(StartupPhase::HostSpawn, Some("spawned".to_string()));
        trace.enter(StartupPhase::HostConnect, None);
        trace.retry(Some("pipe busy".to_string()));
        trace.enter(StartupPhase::Hello, None);

        assert_eq!(trace.current(), StartupPhase::Hello);
        assert_eq!(trace.attempt(), 1, "entering a phase resets the attempt");
        let phases: Vec<StartupPhase> = trace.entries().iter().map(|entry| entry.phase()).collect();
        assert_eq!(
            phases,
            vec![
                StartupPhase::HostSpawn,
                StartupPhase::HostConnect,
                StartupPhase::HostConnect,
                StartupPhase::Hello,
            ]
        );
        assert_eq!(trace.entries()[0].detail(), Some("spawned"));
    }

    #[test]
    fn retry_keeps_the_phase_and_counts_attempts() {
        let mut trace = StartupTrace::new();
        trace.enter(StartupPhase::Synchronize, None);
        trace.retry(Some("kernel store is temporarily unavailable".to_string()));
        trace.retry(Some("kernel store is temporarily unavailable".to_string()));

        assert_eq!(trace.current(), StartupPhase::Synchronize);
        assert_eq!(trace.attempt(), 3);
        let last = trace.entries().last().expect("retry entry");
        assert_eq!(last.phase(), StartupPhase::Synchronize);
        assert_eq!(last.attempt(), 3);
        assert_eq!(
            last.detail(),
            Some("kernel store is temporarily unavailable")
        );
    }

    #[test]
    fn retry_in_continues_the_earlier_phase_attempt_count() {
        let mut trace = StartupTrace::new();
        trace.enter(StartupPhase::Synchronize, None);
        trace.enter(StartupPhase::SnapshotPages, None);
        trace.fail("kernel store is temporarily unavailable");
        trace.retry_in(
            StartupPhase::Synchronize,
            Some("kernel store is temporarily unavailable".to_string()),
        );
        trace.enter(StartupPhase::SnapshotPages, None);
        trace.fail("kernel store is temporarily unavailable");
        trace.retry_in(StartupPhase::Synchronize, None);

        assert_eq!(trace.current(), StartupPhase::Synchronize);
        assert_eq!(
            trace.attempt(),
            3,
            "a bootstrap unit going round again is the third Synchronize attempt"
        );
        let synchronize_attempts: Vec<u32> = trace
            .entries()
            .iter()
            .filter(|entry| entry.phase() == StartupPhase::Synchronize)
            .map(|entry| entry.attempt())
            .collect();
        assert_eq!(synchronize_attempts, vec![1, 2, 3]);
    }

    #[test]
    fn fail_records_the_error_without_changing_the_phase_or_attempt() {
        let mut trace = StartupTrace::new();
        trace.enter(StartupPhase::Synchronize, None);
        trace.retry(None);
        trace.fail("fleet synchronize failed: broken pipe");

        assert_eq!(trace.current(), StartupPhase::Synchronize);
        assert_eq!(trace.attempt(), 2, "a failure is not a new attempt");
        assert_eq!(
            trace.detail(),
            Some("fleet synchronize failed: broken pipe")
        );
        assert_eq!(trace.entries().len(), 3);
    }

    #[test]
    fn phase_durations_cover_every_reached_phase_in_order() {
        let mut trace = StartupTrace::new();
        trace.enter(StartupPhase::HostSpawn, None);
        trace.enter(StartupPhase::HostConnect, None);
        trace.retry(None);
        trace.enter(StartupPhase::Synchronize, None);

        let durations = trace.phase_durations();
        let phases: Vec<StartupPhase> = durations.iter().map(|(phase, _)| *phase).collect();
        assert_eq!(
            phases,
            vec![
                StartupPhase::HostSpawn,
                StartupPhase::HostConnect,
                StartupPhase::Synchronize,
            ],
            "a retry must not open a second run of the same phase"
        );
        assert!(trace.elapsed_in_phase() <= trace.elapsed());
    }

    #[test]
    fn advance_to_never_rewinds_a_phase_a_background_thread_passed() {
        let trace = SharedStartupTrace::detached();
        trace.advance_to(StartupPhase::Synchronize, None);
        trace.advance_to(StartupPhase::Hello, None);

        assert_eq!(trace.current(), StartupPhase::Synchronize);
        assert_eq!(trace.entries().len(), 1);
    }

    #[test]
    fn the_log_line_carries_the_phase_attempt_and_detail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("client-startup.log");
        let trace = SharedStartupTrace::detached();
        trace.attach_log_file(&path);
        trace.enter(StartupPhase::Synchronize, None);
        trace.retry(Some("kernel store is temporarily unavailable".to_string()));

        let written = std::fs::read_to_string(&path).expect("startup log");
        let lines: Vec<&str> = written.lines().collect();
        assert_eq!(lines.len(), 2, "one line per transition: {written}");
        assert!(
            lines[0].contains(" Synchronize attempt=1"),
            "unexpected line: {}",
            lines[0]
        );
        assert!(
            lines[1].contains(
                " Synchronize attempt=2 detail=\"kernel store is temporarily unavailable\""
            ),
            "unexpected line: {}",
            lines[1]
        );
        // "<date> <time> +MM:SS.mmm ..."
        let elapsed = lines[1].split(' ').nth(2).expect("elapsed field");
        assert!(
            elapsed.starts_with('+') && elapsed.len() == "+00:00.000".len(),
            "unexpected elapsed field: {elapsed}"
        );
    }

    #[test]
    fn entries_recorded_before_the_log_existed_are_replayed_into_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("client-startup.log");
        let trace = SharedStartupTrace::detached();
        trace.enter(StartupPhase::HostSpawn, None);
        trace.enter(StartupPhase::HostConnect, None);
        trace.attach_log_file(&path);
        trace.enter(StartupPhase::Hello, None);

        let written = std::fs::read_to_string(&path).expect("startup log");
        assert_eq!(written.lines().count(), 3, "replayed + live: {written}");
        assert!(written.contains(" HostSpawn attempt=1"));
        assert!(written.contains(" Hello attempt=1"));
    }

    #[test]
    fn the_ready_summary_names_every_phase_duration() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("client-startup.log");
        let trace = SharedStartupTrace::detached();
        trace.attach_log_file(&path);
        trace.enter(StartupPhase::Synchronize, None);
        trace.note_ready(Some("cockpit".to_string()));
        trace.note_ready(None);

        assert_eq!(trace.current(), StartupPhase::Ready);
        let written = std::fs::read_to_string(&path).expect("startup log");
        let summary = written
            .lines()
            .find(|line| line.contains(" summary "))
            .expect("summary line");
        assert!(summary.contains("total="));
        assert!(summary.contains("Synchronize="));
        assert!(summary.contains("Ready="));
        assert_eq!(
            written
                .lines()
                .filter(|line| line.contains("Ready"))
                .count(),
            2,
            "Ready is entered once and summarized once: {written}"
        );
    }

    #[test]
    fn a_log_that_cannot_be_opened_neither_panics_nor_stops_the_machine() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A directory path can never be opened as a file.
        let trace = SharedStartupTrace::detached();
        trace.attach_log_file(dir.path());
        trace.enter(StartupPhase::HostConnect, None);
        trace.fail("connect refused");

        assert_eq!(trace.current(), StartupPhase::HostConnect);
        assert_eq!(trace.entries().len(), 2);
        assert_eq!(trace.entries()[1].detail(), Some("connect refused"));
    }

    #[test]
    fn fail_once_collapses_a_repeated_render_path_failure() {
        let trace = SharedStartupTrace::detached();
        trace.enter(StartupPhase::Synchronize, None);
        trace.fail_once("store recovery required");
        trace.fail_once("store recovery required");
        trace.fail_once("store recovery required");

        assert_eq!(trace.entries().len(), 2);
    }

    #[test]
    fn a_detail_with_quotes_or_newlines_stays_one_closed_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("client-startup.log");
        let trace = SharedStartupTrace::detached();
        trace.attach_log_file(&path);
        trace.enter(StartupPhase::Hello, None);
        trace.fail("bad \"handshake\"\nsecond line");

        let written = std::fs::read_to_string(&path).expect("startup log");
        assert_eq!(written.lines().count(), 2, "one line per entry: {written}");
        assert!(
            written.contains("detail=\"bad 'handshake' second line\""),
            "unexpected log: {written}"
        );
    }
}

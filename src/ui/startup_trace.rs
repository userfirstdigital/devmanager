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
use std::sync::atomic::{AtomicBool, Ordering};
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
    reached_ready: bool,
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
            reached_ready: false,
        }
    }

    /// Enter `phase`, resetting the attempt counter. Records one entry.
    pub fn enter(&mut self, phase: StartupPhase, detail: Option<String>) -> StartupTraceEntry {
        self.current = phase;
        // Sticky: a later reconnect rewinds `current` so the same loading line
        // can name the wait, but the session has still been Ready once and the
        // actions that startup disabled must not be taken away again.
        self.reached_ready |= phase == StartupPhase::Ready;
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

    /// Whether this session has EVER reached [`StartupPhase::Ready`].
    ///
    /// Not the same question as `current() >= Ready`: a reconnect rewinds the
    /// phase to name its wait, and that must not re-disable New task on a
    /// client whose tasks are already loaded.
    pub fn reached_ready(&self) -> bool {
        self.reached_ready
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
    /// Set when a reply is applied and cleared when the paint that showed it
    /// completes its record. `note_repaint` runs on the render path, so the
    /// common frame must cost one relaxed load rather than the trace mutex.
    awaiting_repaint: Arc<AtomicBool>,
}

struct TraceState {
    trace: StartupTrace,
    sink: Option<BufWriter<File>>,
    round_trips: RequestRoundTripLog,
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
                round_trips: RequestRoundTripLog::new(),
            })),
            awaiting_repaint: Arc::new(AtomicBool::new(false)),
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

    /// See [`StartupTrace::reached_ready`].
    pub fn reached_ready(&self) -> bool {
        self.lock().trace.reached_ready()
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

    /// Record a cockpit request entering the pending-action lane.
    ///
    /// `key` is the request id's raw bits and `task` the task id's; nothing is
    /// formatted until a line is actually written, so the enqueue path costs a
    /// lock and a push.
    pub fn note_request_enqueued(&self, key: u128, kind: &'static str, task: Option<u128>) {
        let mut state = self.lock();
        let at_ms = duration_ms(state.trace.elapsed());
        if let Some(displaced) = state.round_trips.begin(key, kind, task, at_ms) {
            // A request that never came back is the finding, so say so rather
            // than dropping it to make room in silence.
            let line = format!("{} {}", wall_clock_now(), displaced.render_suffix(at_ms));
            state.write_raw(&line);
        }
    }

    /// Record the host runtime accepting the request.
    pub fn note_request_handed_off(&self, key: u128) {
        self.note_request_stage(key, RequestStage::HandedOff);
    }

    /// Record the reply being published into the queue the controller drains.
    ///
    /// Called from the host worker thread. Everything after this stamp is the
    /// client's own scheduling, so this is what separates a slow host from a
    /// controller that did not wake.
    pub fn note_request_published(&self, key: u128) {
        self.note_request_stage(key, RequestStage::Published);
    }

    /// Record the controller draining the reply into the shell.
    pub fn note_request_replied(&self, key: u128) {
        self.note_request_stage(key, RequestStage::Replied);
    }

    /// Record the shell finishing with the reply. The record now waits only for
    /// the paint that shows it.
    pub fn note_request_applied(&self, key: u128) {
        if self.note_request_stage(key, RequestStage::Applied) {
            self.awaiting_repaint.store(true, Ordering::Release);
        }
    }

    fn note_request_stage(&self, key: u128, stage: RequestStage) -> bool {
        let mut state = self.lock();
        let at_ms = duration_ms(state.trace.elapsed());
        state.round_trips.note(key, stage, at_ms)
    }

    /// Complete every applied round trip at this paint and write its line.
    ///
    /// Called from the render path, so the ordinary frame — nothing applied
    /// since the last paint — costs one atomic load and never takes the trace
    /// mutex.
    pub fn note_repaint(&self) {
        if !self.awaiting_repaint.load(Ordering::Acquire) {
            return;
        }
        let mut state = self.lock();
        let at_ms = duration_ms(state.trace.elapsed());
        let repaint = state.round_trips.note_repaint(at_ms);
        let still_waiting = state.round_trips.awaiting_repaint();
        for record in &repaint.completed {
            let line = format!("{} {}", wall_clock_now(), record.render_suffix(at_ms));
            state.write_raw(&line);
        }
        if repaint.reached_cap {
            state.write_raw(&format!(
                "{} request-log capped after {MAX_LOGGED_REQUEST_ROUND_TRIPS} round trips",
                wall_clock_now()
            ));
        }
        self.awaiting_repaint
            .store(still_waiting, Ordering::Release);
    }

    /// Read the round-trip log under ONE lock. A caller that wants several
    /// values (a measurement harness, say) must not let another thread move a
    /// record between two accessors.
    pub fn with_round_trips<R>(&self, read: impl FnOnce(&RequestRoundTripLog) -> R) -> R {
        read(&self.lock().round_trips)
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

/// One cockpit request's round trip through the client, and the bounded log of
/// them.
///
/// The startup trace above answers "which startup phase took the time". It
/// cannot answer the next question, which is where a REPEATED cockpit request
/// spends its time once startup is over: a conversation that needs eight pages
/// pays whatever one page costs eight times, and nothing recorded which of the
/// four intervals — sitting in the pending-action lane, waiting on the host,
/// being admitted by the shell, or waiting for the paint that shows it — the
/// cost is in.
///
/// The machine is pure: every call carries the millisecond it happened at, so
/// tests are deterministic and the shell is the only thing that reads a clock.

/// How far a round trip has got. Ordered: [`RequestRoundTripLog::note`] only
/// ever moves a record forward, so a duplicate or out-of-order note from a
/// retry path cannot rewind a stage that already happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RequestStage {
    /// Captured into the shell's pending-action lane.
    Enqueued,
    /// Accepted by the host runtime and on its way to the worker.
    HandedOff,
    /// The reply was published into the queue the controller drains. Recorded
    /// by the worker, so this is the moment the answer became available to the
    /// client — everything after it is the client's own scheduling.
    Published,
    /// The controller drained the reply into the shell.
    Replied,
    /// The shell finished applying the reply.
    Applied,
}

impl RequestStage {
    /// Stable log/UI label. Never localized: this string is grepped.
    pub fn label(self) -> &'static str {
        match self {
            RequestStage::Enqueued => "Enqueued",
            RequestStage::HandedOff => "HandedOff",
            RequestStage::Published => "Published",
            RequestStage::Replied => "Replied",
            RequestStage::Applied => "Applied",
        }
    }
}

impl std::fmt::Display for RequestStage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

/// One request's timestamps. `key` is the request id; `task` is the task id the
/// request was captured against, both carried as their raw 128 bits so nothing
/// is allocated or formatted until a line is actually written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestRoundTrip {
    key: u128,
    kind: &'static str,
    task: Option<u128>,
    enqueued_at_ms: u64,
    handed_off_at_ms: Option<u64>,
    published_at_ms: Option<u64>,
    replied_at_ms: Option<u64>,
    applied_at_ms: Option<u64>,
    repainted_at_ms: Option<u64>,
}

impl RequestRoundTrip {
    pub fn key(&self) -> u128 {
        self.key
    }

    pub fn kind(&self) -> &'static str {
        self.kind
    }

    pub fn task(&self) -> Option<u128> {
        self.task
    }

    /// The furthest stage this record has reached.
    pub fn stage(&self) -> RequestStage {
        if self.applied_at_ms.is_some() {
            RequestStage::Applied
        } else if self.replied_at_ms.is_some() {
            RequestStage::Replied
        } else if self.published_at_ms.is_some() {
            RequestStage::Published
        } else if self.handed_off_at_ms.is_some() {
            RequestStage::HandedOff
        } else {
            RequestStage::Enqueued
        }
    }

    /// Milliseconds spent in the pending-action lane before the host runtime
    /// accepted it.
    pub fn lane_ms(&self) -> Option<u64> {
        self.handed_off_at_ms
            .map(|at| at.saturating_sub(self.enqueued_at_ms))
    }

    /// Milliseconds between hand-off and the reply reaching the shell.
    ///
    /// This spans two unrelated things — the worker's round trip to the host,
    /// and however long the controller took to notice the answer — which is
    /// why [`Self::worker_ms`] and [`Self::wake_ms`] exist. Reported on its own
    /// only for a reply that arrived without a publication stamp (the deferred
    /// and overflow paths, which do not go through the projection queue).
    pub fn host_ms(&self) -> Option<u64> {
        match (self.handed_off_at_ms, self.replied_at_ms) {
            (Some(handed_off), Some(replied)) => Some(replied.saturating_sub(handed_off)),
            _ => None,
        }
    }

    /// Milliseconds the host and its worker took to produce the answer. This is
    /// the only interval the host can be blamed for.
    pub fn worker_ms(&self) -> Option<u64> {
        match (self.handed_off_at_ms, self.published_at_ms) {
            (Some(handed_off), Some(published)) => Some(published.saturating_sub(handed_off)),
            _ => None,
        }
    }

    /// Milliseconds the answer sat in the projection queue before the client's
    /// controller drained it. This is pure client scheduling: a large value
    /// here with a small [`Self::worker_ms`] means the controller waited out a
    /// deadline instead of being woken by the publication.
    pub fn wake_ms(&self) -> Option<u64> {
        match (self.published_at_ms, self.replied_at_ms) {
            (Some(published), Some(replied)) => Some(replied.saturating_sub(published)),
            _ => None,
        }
    }

    /// Milliseconds the shell spent admitting and projecting the reply.
    pub fn admit_ms(&self) -> Option<u64> {
        match (self.replied_at_ms, self.applied_at_ms) {
            (Some(replied), Some(applied)) => Some(applied.saturating_sub(replied)),
            _ => None,
        }
    }

    /// Milliseconds between the reply being applied and the paint that showed
    /// it.
    pub fn paint_ms(&self) -> Option<u64> {
        match (self.applied_at_ms, self.repainted_at_ms) {
            (Some(applied), Some(repainted)) => Some(repainted.saturating_sub(applied)),
            _ => None,
        }
    }

    /// Enqueue to paint.
    pub fn total_ms(&self) -> Option<u64> {
        self.repainted_at_ms
            .map(|at| at.saturating_sub(self.enqueued_at_ms))
    }

    /// How long this record has been alive at `now_ms`. Used by the line that
    /// reports a request that never came back.
    pub fn waited_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.enqueued_at_ms)
    }

    /// Render the trailing half of a log line: everything after the wall clock.
    ///
    /// A complete record names all four intervals. An incomplete one names the
    /// stage it died at instead of printing zeros that would read as a fast
    /// round trip — a request that never returned is a finding, not a nil cost.
    fn render_suffix(&self, now_ms: u64) -> String {
        let started = self.enqueued_at_ms;
        let minutes = started / 60_000;
        let seconds = (started % 60_000) / 1_000;
        let millis = started % 1_000;
        let mut line = format!(
            "+{minutes:02}:{seconds:02}.{millis:03} request kind={} task={}",
            self.kind,
            render_short_id(self.task),
        );
        match (
            self.lane_ms(),
            self.host_ms(),
            self.admit_ms(),
            self.paint_ms(),
            self.total_ms(),
        ) {
            (Some(lane), Some(host), Some(admit), Some(paint), Some(total)) => {
                // Split the host interval whenever the publication was stamped:
                // "the host was slow" and "the controller did not wake" are
                // different findings and must never share one number.
                match (self.worker_ms(), self.wake_ms()) {
                    (Some(worker), Some(wake)) => {
                        line.push_str(&format!(
                            " lane={lane}ms worker={worker}ms wake={wake}ms"
                        ));
                        line.push_str(&format!(
                            " admit={admit}ms paint={paint}ms total={total}ms"
                        ));
                    }
                    _ => line.push_str(&format!(
                        " lane={lane}ms host={host}ms admit={admit}ms paint={paint}ms total={total}ms"
                    )),
                }
            }
            _ => {
                line.push_str(&format!(
                    " stage={} waited={}ms incomplete",
                    self.stage().label(),
                    self.waited_ms(now_ms)
                ));
            }
        }
        line
    }
}

/// The low 32 bits of an id, or `-` when there is none. Enough to tell one
/// task's requests from another's in a log without carrying a whole UUID.
fn render_short_id(id: Option<u128>) -> String {
    match id {
        Some(id) => format!("{:08x}", (id & 0xffff_ffff) as u32),
        None => "-".to_string(),
    }
}

/// How many round trips may be in flight before the oldest is reported
/// unfinished and dropped. The client's own action lane is far smaller than
/// this, so reaching it means requests are being lost, which the dropped
/// record's line says out loud.
pub const MAX_INFLIGHT_REQUEST_ROUND_TRIPS: usize = 64;

/// How many completed round trips are written before the log stops. A session
/// left open for a day must not grow the file without limit, and the cost being
/// measured shows up in the first few dozen.
pub const MAX_LOGGED_REQUEST_ROUND_TRIPS: u64 = 4096;

/// What one repaint completed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestRepaint {
    /// Records the repaint completed, in the order they were enqueued. Empty
    /// once the log has written [`MAX_LOGGED_REQUEST_ROUND_TRIPS`] lines.
    pub completed: Vec<RequestRoundTrip>,
    /// True on the single repaint that reached the cap, so the log can say it
    /// stopped rather than appearing to go quiet.
    pub reached_cap: bool,
}

/// The bounded round-trip machine.
#[derive(Clone, Debug, Default)]
pub struct RequestRoundTripLog {
    inflight: std::collections::VecDeque<RequestRoundTrip>,
    logged: u64,
    reported_cap: bool,
}

impl RequestRoundTripLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a request entering the pending-action lane.
    ///
    /// Returns whatever record had to leave to make room — an evicted oldest,
    /// or a previous record under the same key — so the caller can write its
    /// unfinished line instead of losing it silently.
    pub fn begin(
        &mut self,
        key: u128,
        kind: &'static str,
        task: Option<u128>,
        at_ms: u64,
    ) -> Option<RequestRoundTrip> {
        let displaced = self
            .inflight
            .iter()
            .position(|record| record.key == key)
            .and_then(|index| self.inflight.remove(index))
            .or_else(|| {
                (self.inflight.len() >= MAX_INFLIGHT_REQUEST_ROUND_TRIPS)
                    .then(|| self.inflight.pop_front())
                    .flatten()
            });
        self.inflight.push_back(RequestRoundTrip {
            key,
            kind,
            task,
            enqueued_at_ms: at_ms,
            handed_off_at_ms: None,
            published_at_ms: None,
            replied_at_ms: None,
            applied_at_ms: None,
            repainted_at_ms: None,
        });
        displaced
    }

    /// Advance `key` to `stage`. Never rewinds and never stamps a stage twice,
    /// so a retry path that hands the same record off again keeps the first
    /// hand-off's timestamp — the interval being measured is the wait the user
    /// actually sat through.
    pub fn note(&mut self, key: u128, stage: RequestStage, at_ms: u64) -> bool {
        let Some(record) = self.inflight.iter_mut().find(|record| record.key == key) else {
            return false;
        };
        let slot = match stage {
            RequestStage::Enqueued => return false,
            RequestStage::HandedOff => &mut record.handed_off_at_ms,
            RequestStage::Published => &mut record.published_at_ms,
            RequestStage::Replied => &mut record.replied_at_ms,
            RequestStage::Applied => &mut record.applied_at_ms,
        };
        if slot.is_some() {
            return false;
        }
        *slot = Some(at_ms);
        true
    }

    /// Whether any record is waiting only for the next paint. The shell reads
    /// this to keep [`Self::note_repaint`] off the per-frame path when there is
    /// nothing to complete.
    pub fn awaiting_repaint(&self) -> bool {
        self.inflight
            .iter()
            .any(|record| record.applied_at_ms.is_some())
    }

    /// Complete every applied record at this paint.
    pub fn note_repaint(&mut self, at_ms: u64) -> RequestRepaint {
        let mut completed = Vec::new();
        let mut remaining = std::collections::VecDeque::with_capacity(self.inflight.len());
        for mut record in self.inflight.drain(..) {
            if record.applied_at_ms.is_some() {
                record.repainted_at_ms = Some(at_ms);
                completed.push(record);
            } else {
                remaining.push_back(record);
            }
        }
        self.inflight = remaining;
        if completed.is_empty() {
            return RequestRepaint::default();
        }
        let room = MAX_LOGGED_REQUEST_ROUND_TRIPS.saturating_sub(self.logged);
        let admitted = usize::try_from(room)
            .unwrap_or(usize::MAX)
            .min(completed.len());
        let reached_cap = admitted < completed.len() && !self.reported_cap;
        self.reported_cap |= admitted < completed.len();
        completed.truncate(admitted);
        self.logged = self.logged.saturating_add(admitted as u64);
        RequestRepaint {
            completed,
            reached_cap,
        }
    }

    /// How many completed round trips have been handed out for logging.
    pub fn logged(&self) -> u64 {
        self.logged
    }

    pub fn inflight_len(&self) -> usize {
        self.inflight.len()
    }

    /// Every in-flight record, oldest first. Read by tests and by a caller that
    /// wants to name what is still outstanding.
    pub fn inflight(&self) -> impl Iterator<Item = &RequestRoundTrip> {
        self.inflight.iter()
    }
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

    fn begun(log: &mut RequestRoundTripLog, key: u128, at_ms: u64) {
        log.begin(key, "conversation", Some(0xabcd_1234), at_ms);
    }

    #[test]
    fn a_round_trip_reports_each_interval_separately() {
        let mut log = RequestRoundTripLog::new();
        begun(&mut log, 1, 100);
        assert!(log.note(1, RequestStage::HandedOff, 103));
        assert!(log.note(1, RequestStage::Replied, 115));
        assert!(log.note(1, RequestStage::Applied, 1_855));
        let repaint = log.note_repaint(1_858);

        let record = repaint.completed.first().expect("one completed round trip");
        assert_eq!(record.lane_ms(), Some(3));
        assert_eq!(record.host_ms(), Some(12));
        assert_eq!(record.admit_ms(), Some(1_740));
        assert_eq!(record.paint_ms(), Some(3));
        assert_eq!(record.total_ms(), Some(1_758));
        assert_eq!(log.inflight_len(), 0);
        // No publication stamp: the deferred and overflow reply paths never
        // reach the projection queue, so the two halves cannot be separated
        // and the line must say `host` rather than invent a zero for one.
        assert_eq!(record.worker_ms(), None);
        assert_eq!(record.wake_ms(), None);
        assert!(record
            .render_suffix(1_858)
            .contains("lane=3ms host=12ms admit=1740ms paint=3ms total=1758ms"));
    }

    #[test]
    fn a_publication_stamp_separates_the_host_from_the_controller() {
        let mut log = RequestRoundTripLog::new();
        begun(&mut log, 1, 100);
        log.note(1, RequestStage::HandedOff, 103);
        // The host answered in 40 ms and the answer then sat in the queue for
        // most of a second: that is a controller that was not woken, and the
        // line has to be able to say so.
        log.note(1, RequestStage::Published, 143);
        log.note(1, RequestStage::Replied, 1_090);
        log.note(1, RequestStage::Applied, 1_100);
        let repaint = log.note_repaint(1_103);

        let record = repaint.completed.first().expect("one completed round trip");
        assert_eq!(record.worker_ms(), Some(40));
        assert_eq!(record.wake_ms(), Some(947));
        assert_eq!(record.host_ms(), Some(987), "the two halves must still sum");
        let line = record.render_suffix(1_103);
        assert!(
            line.contains("worker=40ms wake=947ms"),
            "a split round trip must name both halves: {line}"
        );
        assert!(
            !line.contains("host="),
            "a split round trip must not also print the conflated interval: {line}"
        );
    }

    #[test]
    fn the_published_stage_sits_between_hand_off_and_the_reply() {
        assert!(RequestStage::HandedOff < RequestStage::Published);
        assert!(RequestStage::Published < RequestStage::Replied);
        let mut log = RequestRoundTripLog::new();
        begun(&mut log, 1, 0);
        log.note(1, RequestStage::HandedOff, 1);
        log.note(1, RequestStage::Published, 2);
        let published = log.inflight().next().expect("in flight");
        assert_eq!(published.stage(), RequestStage::Published);
        assert!(published
            .render_suffix(500)
            .contains("stage=Published waited=500ms incomplete"));
    }

    #[test]
    fn a_repaint_completes_only_records_that_were_applied() {
        let mut log = RequestRoundTripLog::new();
        begun(&mut log, 1, 0);
        begun(&mut log, 2, 5);
        log.note(1, RequestStage::HandedOff, 1);
        log.note(1, RequestStage::Replied, 2);
        log.note(1, RequestStage::Applied, 3);
        log.note(2, RequestStage::HandedOff, 6);

        assert!(log.awaiting_repaint());
        let repaint = log.note_repaint(10);
        assert_eq!(repaint.completed.len(), 1);
        assert_eq!(repaint.completed[0].key(), 1);
        // The unanswered request stays in flight rather than being completed
        // with a zero host interval it never earned.
        assert_eq!(log.inflight_len(), 1);
        assert!(!log.awaiting_repaint());
    }

    #[test]
    fn a_repaint_with_nothing_applied_writes_no_line() {
        let mut log = RequestRoundTripLog::new();
        begun(&mut log, 1, 0);
        log.note(1, RequestStage::HandedOff, 1);
        assert_eq!(log.note_repaint(50), RequestRepaint::default());
        assert_eq!(log.logged(), 0);
        assert_eq!(log.inflight_len(), 1);
    }

    #[test]
    fn a_stage_never_rewinds_and_never_stamps_twice() {
        let mut log = RequestRoundTripLog::new();
        begun(&mut log, 1, 0);
        assert!(log.note(1, RequestStage::HandedOff, 10));
        // A retry path handing the same record off again must keep the first
        // hand-off: the interval being measured is the wait the user sat
        // through, not the wait since the last internal retry.
        assert!(!log.note(1, RequestStage::HandedOff, 900));
        assert!(!log.note(1, RequestStage::Enqueued, 900));
        log.note(1, RequestStage::Replied, 20);
        log.note(1, RequestStage::Applied, 30);
        let repaint = log.note_repaint(40);
        assert_eq!(repaint.completed[0].lane_ms(), Some(10));
    }

    #[test]
    fn a_note_for_an_unknown_request_is_ignored() {
        let mut log = RequestRoundTripLog::new();
        assert!(!log.note(7, RequestStage::Replied, 5));
        assert_eq!(log.inflight_len(), 0);
    }

    #[test]
    fn the_inflight_ring_is_bounded_and_reports_what_it_drops() {
        let mut log = RequestRoundTripLog::new();
        for index in 0..MAX_INFLIGHT_REQUEST_ROUND_TRIPS {
            assert!(begun_returns_none(&mut log, index as u128, index as u64));
        }
        assert_eq!(log.inflight_len(), MAX_INFLIGHT_REQUEST_ROUND_TRIPS);

        let displaced = log
            .begin(9_999, "conversation", None, 1_000)
            .expect("the oldest record leaves to make room");
        assert_eq!(displaced.key(), 0);
        assert_eq!(displaced.stage(), RequestStage::Enqueued);
        assert_eq!(log.inflight_len(), MAX_INFLIGHT_REQUEST_ROUND_TRIPS);
        assert!(displaced
            .render_suffix(1_000)
            .contains("waited=1000ms incomplete"));
    }

    fn begun_returns_none(log: &mut RequestRoundTripLog, key: u128, at_ms: u64) -> bool {
        log.begin(key, "conversation", None, at_ms).is_none()
    }

    #[test]
    fn re_beginning_a_key_returns_the_record_it_replaced() {
        let mut log = RequestRoundTripLog::new();
        begun(&mut log, 1, 0);
        log.note(1, RequestStage::HandedOff, 5);
        let displaced = log
            .begin(1, "conversation", None, 100)
            .expect("the previous record under this key");
        assert_eq!(displaced.stage(), RequestStage::HandedOff);
        assert_eq!(log.inflight_len(), 1);
    }

    #[test]
    fn the_completed_log_is_capped_and_says_so_once() {
        let mut log = RequestRoundTripLog::new();
        let mut reached_cap_count = 0;
        for index in 0..(MAX_LOGGED_REQUEST_ROUND_TRIPS + 4) {
            let key = index as u128;
            begun(&mut log, key, index);
            log.note(key, RequestStage::HandedOff, index);
            log.note(key, RequestStage::Replied, index);
            log.note(key, RequestStage::Applied, index);
            let repaint = log.note_repaint(index);
            if repaint.reached_cap {
                reached_cap_count += 1;
            }
        }
        assert_eq!(log.logged(), MAX_LOGGED_REQUEST_ROUND_TRIPS);
        assert_eq!(
            reached_cap_count, 1,
            "the cap must be reported once, not on every later repaint"
        );
        // Records still complete after the cap; only the writing stops, so a
        // long session cannot leak in-flight records either.
        assert_eq!(log.inflight_len(), 0);
    }

    #[test]
    fn a_short_id_renders_the_low_bits_or_a_dash() {
        assert_eq!(render_short_id(Some(0x1122_3344_5566_7788)), "55667788");
        assert_eq!(render_short_id(None), "-");
    }

    #[test]
    fn the_shared_trace_completes_a_round_trip_only_on_the_next_paint() {
        let trace = SharedStartupTrace::detached();
        trace.note_request_enqueued(42, "conversation", Some(7));
        trace.note_request_handed_off(42);
        trace.note_request_replied(42);
        trace.with_round_trips(|log| {
            assert_eq!(log.inflight_len(), 1);
            assert!(!log.awaiting_repaint());
        });
        // A paint before the reply is applied must not complete the record.
        trace.note_repaint();
        trace.with_round_trips(|log| assert_eq!(log.inflight_len(), 1));

        trace.note_request_applied(42);
        trace.with_round_trips(|log| assert!(log.awaiting_repaint()));
        trace.note_repaint();
        trace.with_round_trips(|log| {
            assert_eq!(log.inflight_len(), 0);
            assert_eq!(log.logged(), 1);
        });
    }

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
    fn reaching_ready_is_sticky_across_a_reconnect_that_rewinds_the_phase() {
        let trace = SharedStartupTrace::detached();
        trace.enter(StartupPhase::SnapshotPages, None);
        assert!(!trace.reached_ready());

        trace.note_ready(None);
        assert!(trace.reached_ready());

        // A post-Ready resync goes round the synchronize unit again.
        trace.retry_in(StartupPhase::Synchronize, Some("fleet resync".to_string()));
        assert_eq!(trace.current(), StartupPhase::Synchronize);
        assert!(
            trace.reached_ready(),
            "a reconnect names its wait; it does not un-load the session"
        );
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

//! The one loading line the client paints while startup is still in progress.
//!
//! A real launch showed no task content for tens of seconds while the client
//! paged the snapshot, and the center read "Conversation is live; waiting for
//! messages." the whole time. Nothing in the UI could name the wait, because
//! nothing rendered the phase machine that already recorded it.
//!
//! Everything here is pure: [`StartupTraceSnapshot`] is plain data lifted off
//! [`StartupTrace`], and [`startup_status_line`] turns it into the exact strings
//! the GPUI code paints. The renderer holds no second copy of this logic, so a
//! test can assert the copy without a window.

use std::time::{Duration, Instant};

use crate::ui::startup_trace::{StartupPhase, StartupTrace};

/// How long a phase must run before the failure detail earns a second line.
///
/// Below this a retry is ordinary startup noise; above it the user is waiting
/// and deserves to be told what is being retried.
pub const STARTUP_SECONDARY_AFTER: Duration = Duration::from_secs(5);

/// Tooltip on every action that startup has disabled.
pub const STARTUP_LOADING_TOOLTIP: &str = "Loading…";

/// Named cause for a create-task gesture refused before startup finished.
///
/// A silent no-op is indistinguishable from a dead button, so the refusal says
/// which check failed rather than returning nothing.
pub const STARTUP_CREATE_REFUSAL: &str =
    "Not yet — still loading tasks. The New task button lights up when they land.";

/// What the center surface paints while startup is still in progress.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartupStatusLine {
    /// Phase copy, plus progress, attempt, and elapsed seconds when they apply.
    pub primary: String,
    /// The failure being retried, once the phase has run long enough that the
    /// user is owed an explanation.
    pub secondary: Option<String>,
}

/// Plain data lifted off the phase machine so the line is a pure function of it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartupTraceSnapshot {
    pub phase: StartupPhase,
    pub attempt: u32,
    /// When the current phase was entered. Retries stay inside the same run,
    /// so this is the moment the user started waiting on THIS phase.
    pub phase_started_at: Instant,
    /// The last failure the trace recorded in this phase, if any.
    pub failure_detail: Option<String>,
    /// Snapshot page number, when a note in this phase carried one.
    pub page: Option<u32>,
}

impl StartupTraceSnapshot {
    /// Read the machine once. Every field the line renders comes from this one
    /// read, so the copy cannot mix two different instants of the trace.
    pub fn from_trace(trace: &StartupTrace) -> Self {
        Self {
            phase: trace.current(),
            attempt: trace.attempt(),
            phase_started_at: trace.phase_started_at(),
            failure_detail: trace.failure_detail().map(str::to_string),
            page: current_phase_page(trace),
        }
    }
}

/// The last `page <n>` a note in the current phase carried.
///
/// The producer of the note is the paging lane, which may or may not be
/// recording page numbers yet; the brief is explicit that the page is rendered
/// only when the trace actually carries one. `items per page` is a phase label,
/// not a page number, so the word `per` before `page` is refused.
fn current_phase_page(trace: &StartupTrace) -> Option<u32> {
    let phase = trace.current();
    trace
        .entries()
        .iter()
        .rev()
        .take_while(|entry| entry.phase() == phase)
        .find_map(|entry| entry.detail().and_then(page_number))
}

fn page_number(detail: &str) -> Option<u32> {
    let words: Vec<&str> = detail.split_whitespace().collect();
    words.iter().enumerate().rev().find_map(|(index, word)| {
        if !word.eq_ignore_ascii_case("page") {
            return None;
        }
        if index > 0 && words[index - 1].eq_ignore_ascii_case("per") {
            return None;
        }
        words.get(index + 1).and_then(|next| {
            next.trim_matches(|c: char| !c.is_ascii_digit())
                .parse()
                .ok()
        })
    })
}

/// Phase copy, before any progress, attempt or elapsed suffix.
///
/// `Hello` is grouped with the connect phases deliberately: it is the
/// negotiated handshake with the same host the previous two phases were
/// reaching, and the approved copy names no separate state for it.
fn phase_copy(phase: StartupPhase, attempt: u32) -> Option<&'static str> {
    match phase {
        StartupPhase::HostSpawn | StartupPhase::HostConnect | StartupPhase::Hello => {
            Some(if attempt > 1 {
                "Connecting…"
            } else {
                "Starting the task host…"
            })
        }
        StartupPhase::Synchronize => Some("Synchronizing…"),
        StartupPhase::SnapshotPages => Some("Loading tasks"),
        StartupPhase::FirstProjection => Some("Building view…"),
        // Ready is not a wait. Nothing is painted.
        StartupPhase::Ready => None,
    }
}

/// The line the center surface paints, or `None` once startup is done.
///
/// `now` is passed rather than read so the whole rendering is testable: the
/// 5 s threshold and the elapsed suffix are the two things most likely to be
/// wrong, and neither can be exercised against a live clock.
pub fn startup_status_line(
    snapshot: &StartupTraceSnapshot,
    now: Instant,
) -> Option<StartupStatusLine> {
    let copy = phase_copy(snapshot.phase, snapshot.attempt)?;
    let elapsed = now.saturating_duration_since(snapshot.phase_started_at);
    let mut parts = vec![copy.to_string()];
    if snapshot.phase == StartupPhase::SnapshotPages {
        if let Some(page) = snapshot.page {
            parts.push(format!("page {page}"));
        }
    }
    if snapshot.attempt > 1 {
        parts.push(format!("attempt {}", snapshot.attempt));
    }
    let seconds = elapsed.as_secs();
    if seconds >= 1 {
        parts.push(format!("{seconds} s"));
    }
    let secondary = snapshot
        .failure_detail
        .as_deref()
        .filter(|_| elapsed >= STARTUP_SECONDARY_AFTER)
        .map(|detail| {
            if snapshot.attempt > 1 {
                format!("Retrying: {detail} (attempt {})", snapshot.attempt)
            } else {
                format!("Retrying: {detail}")
            }
        });
    Some(StartupStatusLine {
        primary: parts.join(" · "),
        secondary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(phase: StartupPhase, attempt: u32, waited: Duration) -> StartupTraceSnapshot {
        StartupTraceSnapshot {
            phase,
            attempt,
            phase_started_at: Instant::now() - waited,
            failure_detail: None,
            page: None,
        }
    }

    fn primary(snapshot: &StartupTraceSnapshot) -> String {
        startup_status_line(snapshot, Instant::now())
            .expect("a phase before Ready always paints a line")
            .primary
    }

    #[test]
    fn each_phase_before_ready_names_the_wait() {
        assert_eq!(
            primary(&snapshot(StartupPhase::HostSpawn, 1, Duration::ZERO)),
            "Starting the task host…"
        );
        assert_eq!(
            primary(&snapshot(StartupPhase::HostConnect, 1, Duration::ZERO)),
            "Starting the task host…"
        );
        assert_eq!(
            primary(&snapshot(StartupPhase::Hello, 1, Duration::ZERO)),
            "Starting the task host…"
        );
        assert_eq!(
            primary(&snapshot(StartupPhase::Synchronize, 1, Duration::ZERO)),
            "Synchronizing…"
        );
        assert_eq!(
            primary(&snapshot(StartupPhase::SnapshotPages, 1, Duration::ZERO)),
            "Loading tasks"
        );
        assert_eq!(
            primary(&snapshot(StartupPhase::FirstProjection, 1, Duration::ZERO)),
            "Building view…"
        );
    }

    #[test]
    fn ready_paints_nothing() {
        let ready = snapshot(StartupPhase::Ready, 1, Duration::from_secs(30));
        assert_eq!(startup_status_line(&ready, Instant::now()), None);
    }

    #[test]
    fn a_retried_connect_names_the_attempt() {
        let retrying = snapshot(StartupPhase::HostConnect, 12, Duration::ZERO);
        assert_eq!(primary(&retrying), "Connecting… · attempt 12");
    }

    #[test]
    fn the_paging_phase_carries_its_page_and_elapsed_seconds() {
        let mut paging = snapshot(StartupPhase::SnapshotPages, 1, Duration::from_secs(12));
        paging.page = Some(7);
        assert_eq!(primary(&paging), "Loading tasks · page 7 · 12 s");
    }

    #[test]
    fn a_page_the_trace_never_recorded_is_omitted() {
        let paging = snapshot(StartupPhase::SnapshotPages, 1, Duration::from_secs(3));
        assert_eq!(primary(&paging), "Loading tasks · 3 s");
    }

    #[test]
    fn a_wait_under_one_second_shows_no_seconds() {
        let starting = snapshot(StartupPhase::Synchronize, 1, Duration::from_millis(400));
        assert_eq!(primary(&starting), "Synchronizing…");
    }

    #[test]
    fn the_failure_detail_waits_five_seconds_and_then_names_the_attempt() {
        let mut retrying = snapshot(
            StartupPhase::Synchronize,
            12,
            STARTUP_SECONDARY_AFTER - Duration::from_millis(1),
        );
        retrying.failure_detail = Some("kernel store is temporarily unavailable".to_string());
        let early = startup_status_line(&retrying, Instant::now()).expect("line");
        assert_eq!(
            early.secondary, None,
            "a young phase is ordinary startup noise"
        );

        retrying.phase_started_at = Instant::now() - STARTUP_SECONDARY_AFTER;
        let late = startup_status_line(&retrying, Instant::now()).expect("line");
        assert_eq!(
            late.secondary.as_deref(),
            Some("Retrying: kernel store is temporarily unavailable (attempt 12)")
        );
    }

    #[test]
    fn a_long_wait_with_no_failure_detail_paints_no_second_line() {
        let waiting = snapshot(StartupPhase::SnapshotPages, 1, Duration::from_secs(30));
        let line = startup_status_line(&waiting, Instant::now()).expect("line");
        assert_eq!(line.secondary, None);
        assert_eq!(line.primary, "Loading tasks · 30 s");
    }

    #[test]
    fn a_first_attempt_failure_still_names_what_is_being_retried() {
        let mut waiting = snapshot(StartupPhase::Synchronize, 1, Duration::from_secs(9));
        waiting.failure_detail = Some("fleet synchronize failed: broken pipe".to_string());
        let line = startup_status_line(&waiting, Instant::now()).expect("line");
        assert_eq!(
            line.secondary.as_deref(),
            Some("Retrying: fleet synchronize failed: broken pipe"),
            "no attempt suffix when there has only been one attempt"
        );
    }

    #[test]
    fn the_snapshot_reads_the_page_out_of_a_note_and_never_out_of_the_phase_label() {
        let mut trace = StartupTrace::new();
        trace.enter(
            StartupPhase::SnapshotPages,
            Some("512 items per page".to_string()),
        );
        assert_eq!(
            StartupTraceSnapshot::from_trace(&trace).page,
            None,
            "'items per page' is a phase label, not a page number"
        );
        trace.note("snapshot page 7 admitted");
        assert_eq!(StartupTraceSnapshot::from_trace(&trace).page, Some(7));
    }

    #[test]
    fn a_page_recorded_in_an_earlier_phase_does_not_leak_into_a_later_one() {
        let mut trace = StartupTrace::new();
        trace.enter(StartupPhase::SnapshotPages, None);
        trace.note("snapshot page 7 admitted");
        trace.enter(StartupPhase::FirstProjection, None);
        assert_eq!(StartupTraceSnapshot::from_trace(&trace).page, None);
    }

    #[test]
    fn the_snapshot_carries_the_failure_the_phase_is_retrying() {
        let mut trace = StartupTrace::new();
        trace.enter(StartupPhase::Synchronize, None);
        trace.retry(Some("kernel store is temporarily unavailable".to_string()));
        let snapshot = StartupTraceSnapshot::from_trace(&trace);
        assert_eq!(snapshot.attempt, 2);
        assert_eq!(
            snapshot.failure_detail.as_deref(),
            Some("kernel store is temporarily unavailable")
        );

        // Entering the next phase is progress: the previous failure is over.
        trace.enter(
            StartupPhase::SnapshotPages,
            Some("512 items per page".to_string()),
        );
        assert_eq!(
            StartupTraceSnapshot::from_trace(&trace).failure_detail,
            None
        );
    }
}

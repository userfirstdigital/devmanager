//! Per-frame timing for the native shell, off unless `DEVMANAGER_FRAME_TRACE=1`.
//!
//! The window-drag complaint is a claim about the cost of one paint, and a
//! paint is the only thing here that is not measurable from the outside: the
//! shell builds its whole element tree inside `NativeShell::render`, so the
//! cost is spread across a hundred helpers and no profiler sample attributes
//! it to a section anybody can name. This records the sections by name.
//!
//! Two rules keep it honest:
//!
//! * A section records its INCLUSIVE time and its CALL COUNT. The count is the
//!   half that catches the defect this module was written for -- a helper that
//!   runs once per pane instead of once per frame is cheap in any single
//!   sample and quadratic on the screen the user actually has.
//! * Nothing is measured, allocated or printed when the env var is absent.
//!   `enabled()` is one relaxed atomic load, and every entry point returns
//!   before touching the thread-local.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

/// Set to `1` to print one line per frame on stderr.
pub const FRAME_TRACE_ENV: &str = "DEVMANAGER_FRAME_TRACE";

const UNKNOWN: u8 = 0;
const OFF: u8 = 1;
const ON: u8 = 2;

static STATE: AtomicU8 = AtomicU8::new(UNKNOWN);

/// Is the trace on? Reads the environment once and caches the answer.
#[inline]
pub fn enabled() -> bool {
    match STATE.load(Ordering::Relaxed) {
        ON => true,
        OFF => false,
        _ => {
            let on = std::env::var(FRAME_TRACE_ENV)
                .map(|value| {
                    let value = value.trim();
                    !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
                })
                .unwrap_or(false);
            STATE.store(if on { ON } else { OFF }, Ordering::Relaxed);
            on
        }
    }
}

/// Turn the trace on or off for a probe that does not want to depend on the
/// process environment. Test-only: a shipped build reads the env var and
/// nothing else.
#[cfg(test)]
pub fn set_enabled_for_test(on: bool) {
    STATE.store(if on { ON } else { OFF }, Ordering::Relaxed);
}

/// One frame's sections, in first-entered order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FrameReport {
    pub total: Duration,
    pub sections: Vec<SectionReport>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SectionReport {
    pub name: &'static str,
    /// Inclusive wall time: a section that contains another counts both.
    pub inclusive: Duration,
    pub calls: u32,
}

impl FrameReport {
    pub fn section(&self, name: &str) -> Option<&SectionReport> {
        self.sections.iter().find(|entry| entry.name == name)
    }

    /// How many times a named section ran in this frame. Absent is zero, which
    /// is the answer a "runs once per frame" assertion wants for a section
    /// that did not run at all.
    pub fn calls(&self, name: &str) -> u32 {
        self.section(name).map_or(0, |entry| entry.calls)
    }

    pub fn millis(&self, name: &str) -> f64 {
        self.section(name)
            .map_or(0.0, |entry| entry.inclusive.as_secs_f64() * 1_000.0)
    }

    pub fn total_millis(&self) -> f64 {
        self.total.as_secs_f64() * 1_000.0
    }
}

#[derive(Default)]
struct FrameState {
    started: Option<Instant>,
    sections: Vec<SectionReport>,
    last: Option<FrameReport>,
}

thread_local! {
    static FRAME: RefCell<FrameState> = RefCell::new(FrameState::default());
}

/// Open a frame. A second call without an intervening [`end_frame`] restarts
/// the frame rather than nesting -- the shell paints one tree per frame, and a
/// nested paint would be the bug rather than something to average over.
pub fn begin_frame() {
    if !enabled() {
        return;
    }
    FRAME.with(|frame| {
        let mut frame = frame.borrow_mut();
        frame.started = Some(Instant::now());
        frame.sections.clear();
    });
}

/// Close the frame, publish it to [`last_frame`], and print one line.
pub fn end_frame() {
    if !enabled() {
        return;
    }
    let report = FRAME.with(|frame| {
        let mut frame = frame.borrow_mut();
        let Some(started) = frame.started.take() else {
            return None;
        };
        let report = FrameReport {
            total: started.elapsed(),
            sections: std::mem::take(&mut frame.sections),
        };
        frame.last = Some(report.clone());
        Some(report)
    });
    let Some(report) = report else {
        return;
    };
    let mut line = format!("devmanager: frame={:.2}ms", report.total_millis());
    for section in &report.sections {
        line.push_str(&format!(
            " {}={:.2}ms/x{}",
            section.name,
            section.inclusive.as_secs_f64() * 1_000.0,
            section.calls
        ));
    }
    eprintln!("{line}");
}

/// The frame that [`end_frame`] most recently closed on this thread.
pub fn last_frame() -> Option<FrameReport> {
    if !enabled() {
        return None;
    }
    FRAME.with(|frame| frame.borrow().last.clone())
}

/// Time a named section until the returned guard drops. A no-op guard when the
/// trace is off.
#[inline]
pub fn section(name: &'static str) -> Section {
    if !enabled() {
        return Section { open: None };
    }
    Section {
        open: Some((name, Instant::now())),
    }
}

pub struct Section {
    open: Option<(&'static str, Instant)>,
}

impl Drop for Section {
    fn drop(&mut self) {
        let Some((name, started)) = self.open.take() else {
            return;
        };
        let elapsed = started.elapsed();
        FRAME.with(|frame| {
            let mut frame = frame.borrow_mut();
            // Sections outside a frame are still counted: `refresh_accessibility_tree`
            // runs from event handlers too, and a caller that only ever fires
            // between frames is exactly the finding this would otherwise hide.
            if let Some(entry) = frame.sections.iter_mut().find(|entry| entry.name == name) {
                entry.inclusive += elapsed;
                entry.calls = entry.calls.saturating_add(1);
            } else {
                frame.sections.push(SectionReport {
                    name,
                    inclusive: elapsed,
                    calls: 1,
                });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    /// The on/off switch is process-wide, so two of these running at once
    /// would each read the other's setting.
    fn trace_switch_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The whole point of the env gate: with the trace off nothing is
    /// recorded, so the shipped build pays one atomic load per section and
    /// keeps no state at all.
    #[test]
    fn the_trace_records_nothing_while_it_is_off() {
        let _switch = trace_switch_lock();
        set_enabled_for_test(false);
        begin_frame();
        {
            let _section = section("probe");
        }
        end_frame();
        assert!(
            last_frame().is_none(),
            "a disabled trace publishes no frame"
        );
        set_enabled_for_test(true);
        begin_frame();
        {
            let _section = section("probe");
        }
        end_frame();
        let report = last_frame().expect("an enabled trace publishes the frame");
        assert_eq!(report.calls("probe"), 1);
        set_enabled_for_test(false);
    }

    #[test]
    fn repeated_sections_accumulate_their_calls() {
        let _switch = trace_switch_lock();
        set_enabled_for_test(true);
        begin_frame();
        for _ in 0..3 {
            let _section = section("probe");
        }
        end_frame();
        let report = last_frame().expect("frame");
        assert_eq!(report.calls("probe"), 3, "each entry counts");
        assert_eq!(report.calls("absent"), 0, "an absent section is zero");
        set_enabled_for_test(false);
    }
}

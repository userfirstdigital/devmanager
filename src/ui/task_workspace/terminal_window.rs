//! The client's retained terminal scroll window.
//!
//! # Why this exists
//!
//! The host owns the alacritty grid and its viewport, so before this module a
//! wheel notch was a query: client -> host -> grid scroll -> screen encode ->
//! IPC -> admit -> paint. Measured on a real launch (2026-09-03, build
//! 98a9696c) over 184 terminal requests the median was `worker=52 wake=6.5
//! admit=13 paint=16.5 total=97 ms`, p90 156 ms, max 516 ms -- about 10 fps,
//! with nothing queuing. That is a remote-device design and it felt like one.
//!
//! Smooth scrolling needs one frame, so the wheel has to be served from
//! memory. The host now attaches a margin of scrollback rows above and below
//! the viewport to every terminal screen it sends
//! ([`crate::terminal::session::TERMINAL_MARGIN_ROWS`]); this type retains them
//! and answers a notch inside that window with no request at all.
//!
//! # The host stays authoritative
//!
//! This is a paint cache, never a second source of truth. Specifically:
//!
//! * **Every admitted screen REPLACES the window.** [`Self::capture`] is called
//!   from `admit_terminal` on every projection, so new output, a host-side
//!   viewport move, a resize and a reattach all rebase the retained rows rather
//!   than being merged into them. There is no path that edits a retained row.
//! * **A local scroll still tells the host.** The caller dispatches the ordinary
//!   `TerminalScroll` alongside the local repaint, so the host's viewport
//!   follows and its reply re-centres the window. The round trip is off the
//!   critical path instead of being removed.
//! * **Rows are never mixed across snapshots.** A window holds the rows of ONE
//!   admitted screen, so every row it can paint was on the terminal together,
//!   at one instant. The cache can therefore be up to one poll interval stale
//!   -- exactly as stale as the viewport the client is already painting -- but
//!   it can never show a line the terminal did not have.
//! * **`generation` pins identity.** [`Self::capture`] records the session,
//!   generations, epoch and grid size the rows belong to;
//!   [`Self::matches_identity`] refuses a window whose subject changed. A
//!   caller that somehow held a window across a reattach gets `None` and falls
//!   back to the synchronous path rather than painting another terminal's
//!   scrollback.
//!
//! Running past the retained window is not an error: [`Self::scrolled`] returns
//! `None` and the caller waits for the host, which is today's behaviour and
//! still correct, only not fast.

use crate::domain::cockpit::TaskTerminalProjection;
use crate::terminal::protocol::TerminalSessionId;

/// What a retained window's rows belong to. Any difference means the rows
/// describe a different terminal, or the same terminal reshaped, and they must
/// not be painted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalWindowIdentity {
    pub session_id: TerminalSessionId,
    pub runtime_generation: u64,
    pub resource_generation: u64,
    pub action_epoch: u64,
    pub cols: usize,
    pub rows: usize,
}

impl TerminalWindowIdentity {
    pub fn of(projection: &TaskTerminalProjection) -> Self {
        Self {
            session_id: projection.session_id,
            runtime_generation: projection.runtime_generation,
            resource_generation: projection.resource_generation,
            action_epoch: projection.action_epoch,
            cols: projection.screen.cols,
            rows: projection.screen.rows,
        }
    }
}

/// Rows retained around one admitted viewport, plus where the client is
/// currently looking inside them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedTerminalWindow {
    identity: TerminalWindowIdentity,
    /// Every retained row, oldest first: the host's margin above, then the
    /// viewport it sent, then the margin below.
    rows: Vec<String>,
    /// Buffer-line index of `rows[0]`. Buffer line 0 is the oldest line the
    /// host's history holds, so the viewport at `display_offset` starts at
    /// `history_size - display_offset`.
    top_buffer_line: usize,
    history_size: usize,
    viewport_rows: usize,
    /// The display offset the host itself sent. Painting this one is exact --
    /// it still carries the projection's styled cells.
    admitted_display_offset: usize,
    /// Where the client is looking now. Equal to `admitted_display_offset`
    /// until a notch is served locally.
    display_offset: usize,
}

impl RetainedTerminalWindow {
    /// Capture the window one admitted projection describes.
    ///
    /// This is the ONLY constructor, and it is called on every admission, so
    /// "what invalidates the cache" has one answer: the next screen from the
    /// host, whatever caused it.
    pub fn capture(projection: &TaskTerminalProjection) -> Self {
        let screen = &projection.screen;
        let viewport = if projection.text_lines.is_empty() {
            // Local consumers keep the styled grid instead of the wire's text.
            screen
                .lines
                .iter()
                .map(|cells| cells.iter().map(|cell| cell.character).collect::<String>())
                .collect::<Vec<_>>()
        } else {
            projection.text_lines.clone()
        };
        let viewport_rows = viewport.len();
        let display_offset = screen.display_offset.min(screen.history_size);
        // The viewport's first row, in buffer-line coordinates. `saturating_sub`
        // rather than a panic: a host that reports an offset past its own
        // history is wrong, but the right answer is to clamp to the top of the
        // buffer, not to lose the terminal.
        let viewport_top = screen.history_size.saturating_sub(display_offset);
        let above = screen.margin_above.len();
        let mut rows = Vec::with_capacity(above + viewport_rows + screen.margin_below.len());
        rows.extend(screen.margin_above.iter().cloned());
        rows.extend(viewport);
        rows.extend(screen.margin_below.iter().cloned());

        Self {
            identity: TerminalWindowIdentity::of(projection),
            rows,
            top_buffer_line: viewport_top.saturating_sub(above),
            history_size: screen.history_size,
            viewport_rows,
            admitted_display_offset: display_offset,
            display_offset,
        }
    }

    pub fn identity(&self) -> TerminalWindowIdentity {
        self.identity
    }

    pub fn matches_identity(&self, projection: &TaskTerminalProjection) -> bool {
        self.identity == TerminalWindowIdentity::of(projection)
    }

    /// Where the client is currently looking.
    pub fn display_offset(&self) -> usize {
        self.display_offset
    }

    /// The offset the host itself last sent. When the two are equal the painted
    /// screen is the host's own, styled cells and all.
    pub fn admitted_display_offset(&self) -> usize {
        self.admitted_display_offset
    }

    /// True when the client has scrolled away from the host's viewport, which
    /// is when the painted rows lose their styling until the host catches up.
    pub fn is_locally_scrolled(&self) -> bool {
        self.display_offset != self.admitted_display_offset
    }

    /// How many rows are retained beyond the viewport, above and below.
    ///
    /// Zero on both sides means an older host that sends no margin; the caller
    /// then never gets a local hit and behaves exactly as it did before.
    pub fn margins(&self) -> (usize, usize) {
        let above = (self
            .history_size
            .saturating_sub(self.admitted_display_offset))
        .saturating_sub(self.top_buffer_line);
        let below = self
            .rows
            .len()
            .saturating_sub(above + self.viewport_rows.min(self.rows.len()));
        (above, below)
    }

    /// The rows to paint for `display_offset`, or `None` when that viewport is
    /// not wholly inside the retained window.
    ///
    /// Wholly, deliberately: a half-served viewport would have to be completed
    /// from somewhere, and there is nowhere honest to complete it from.
    pub fn rows_at(&self, display_offset: usize) -> Option<&[String]> {
        if self.viewport_rows == 0 || display_offset > self.history_size {
            return None;
        }
        let top = self.history_size.checked_sub(display_offset)?;
        let start = top.checked_sub(self.top_buffer_line)?;
        let end = start.checked_add(self.viewport_rows)?;
        if end > self.rows.len() {
            return None;
        }
        Some(&self.rows[start..end])
    }

    /// The display offset one wheel gesture of `delta_lines` would reach, if
    /// the retained window can paint it.
    ///
    /// Positive values move toward older rows, matching `TerminalScroll`.
    /// `None` means the gesture leaves the window: the caller must fall back to
    /// the synchronous host query, which is slower and still correct.
    pub fn scrolled(&self, delta_lines: i32) -> Option<usize> {
        let target = i64::from(delta_lines).checked_add(self.display_offset as i64)?;
        let target = target.clamp(0, self.history_size as i64) as usize;
        if target == self.display_offset {
            // Already at the end of the scrollback in that direction. Report it
            // as served so the caller does not spend a round trip discovering
            // there is nothing there.
            return Some(target);
        }
        self.rows_at(target).is_some().then_some(target)
    }

    /// Move the client's view inside the window. Returns the rows now painted.
    ///
    /// Callers must have obtained `display_offset` from [`Self::scrolled`];
    /// an offset outside the window leaves the window untouched.
    pub fn seek(&mut self, display_offset: usize) -> Option<&[String]> {
        self.rows_at(display_offset)?;
        self.display_offset = display_offset;
        self.rows_at(display_offset)
    }
}

/// Rewrite a retained projection so every existing reader -- the paint path,
/// the scrollbar model, the cursor mapper -- sees the client's local scroll
/// position without knowing this module exists.
///
/// The styled cells are dropped whenever the position is not the host's own:
/// they are indexed by row within the ADMITTED viewport, so re-using them at a
/// different offset would paint one row's colours onto another's text. They
/// come back with the host's reply for the new position, one round trip later.
pub fn apply_local_scroll_to_projection(
    projection: &mut TaskTerminalProjection,
    window: &RetainedTerminalWindow,
) {
    let Some(rows) = window.rows_at(window.display_offset()) else {
        return;
    };
    projection.text_lines = rows.to_vec();
    projection.screen.display_offset = window.display_offset();
    projection.screen.lines.clear();
    if window.is_locally_scrolled() {
        projection.screen.cells.clear();
        projection.screen.cursor = None;
        // The margins belong to the admitted viewport's position. A reader that
        // recaptured a window from this rewritten projection would anchor it
        // wrongly, so leave nothing to recapture from.
        projection.screen.margin_above.clear();
        projection.screen.margin_below.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::cockpit::TaskTerminalProjection;
    use crate::terminal::session::TerminalScreenSnapshot;

    fn projection(
        history_size: usize,
        display_offset: usize,
        above: usize,
        below: usize,
    ) -> TaskTerminalProjection {
        let rows = 4;
        let viewport_top = history_size - display_offset;
        let mut screen = TerminalScreenSnapshot {
            rows,
            cols: 20,
            display_offset,
            history_size,
            total_lines: history_size + rows,
            ..TerminalScreenSnapshot::default()
        };
        screen.margin_above = (viewport_top - above..viewport_top)
            .map(|line| format!("line-{line}"))
            .collect();
        screen.margin_below = (viewport_top + rows..viewport_top + rows + below)
            .map(|line| format!("line-{line}"))
            .collect();
        TaskTerminalProjection {
            task_id: crate::domain::TaskId::new(),
            terminal_id: crate::domain::TerminalId::new(),
            session_id: TerminalSessionId::new(),
            agent_session_id: crate::domain::AgentSessionId::nil(),
            resource_id: crate::domain::ResourceId::new(),
            runtime_generation: 0,
            resource_generation: 1,
            action_epoch: 0,
            focus_epoch: crate::terminal::protocol::FocusEpoch::initial(),
            accepted_input_sequence: 0,
            accepts_input_without_conversation_id: false,
            sequence: 1,
            title: None,
            text_lines: (viewport_top..viewport_top + rows)
                .map(|line| format!("line-{line}"))
                .collect(),
            screen,
            is_provider: false,
            runtime_state: crate::domain::cockpit::TerminalRuntimeStateWire::Running,
        }
    }

    #[test]
    fn a_notch_inside_the_window_is_served_without_a_request() {
        let window = RetainedTerminalWindow::capture(&projection(100, 10, 8, 8));
        assert_eq!(window.display_offset(), 10);
        assert_eq!(window.margins(), (8, 8));
        // Toward older rows.
        let target = window.scrolled(3).expect("inside the window");
        assert_eq!(target, 13);
        let rows = window.rows_at(target).expect("rows");
        assert_eq!(rows[0], "line-87");
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn running_past_the_window_falls_back_to_the_host() {
        let window = RetainedTerminalWindow::capture(&projection(100, 10, 8, 8));
        assert_eq!(
            window.scrolled(9),
            None,
            "9 lines up leaves the 8-row margin"
        );
        assert_eq!(window.scrolled(-9), None, "9 lines down leaves the margin");
        assert_eq!(window.scrolled(8), Some(18));
        assert_eq!(window.scrolled(-8), Some(2));
    }

    #[test]
    fn a_host_that_sends_no_margin_never_serves_a_notch_locally() {
        let window = RetainedTerminalWindow::capture(&projection(100, 10, 0, 0));
        assert_eq!(window.margins(), (0, 0));
        assert_eq!(window.scrolled(1), None);
        assert_eq!(window.scrolled(-1), None);
        // The admitted position itself is still paintable.
        assert!(window.rows_at(10).is_some());
    }

    #[test]
    fn the_ends_of_the_scrollback_are_served_rather_than_requested() {
        let window = RetainedTerminalWindow::capture(&projection(100, 0, 8, 0));
        // Already at the live prompt: scrolling further down is a no-op, and
        // spending a round trip to learn that is exactly the old behaviour.
        assert_eq!(window.scrolled(-5), Some(0));

        let window = RetainedTerminalWindow::capture(&projection(100, 100, 0, 8));
        assert_eq!(window.scrolled(5), Some(100));
    }

    #[test]
    fn seeking_moves_the_view_and_marks_it_locally_scrolled() {
        let mut window = RetainedTerminalWindow::capture(&projection(100, 10, 8, 8));
        assert!(!window.is_locally_scrolled());
        let rows = window.seek(14).expect("rows").to_vec();
        assert_eq!(rows[0], "line-86");
        assert!(window.is_locally_scrolled());
        assert_eq!(window.admitted_display_offset(), 10);
        assert_eq!(window.display_offset(), 14);
        // An offset outside the window is refused rather than clamped.
        assert!(window.seek(90).is_none());
        assert_eq!(window.display_offset(), 14);
    }

    #[test]
    fn identity_changes_refuse_the_window() {
        let admitted = projection(100, 10, 8, 8);
        let window = RetainedTerminalWindow::capture(&admitted);
        assert!(window.matches_identity(&admitted));

        for mutate in [
            (|p: &mut TaskTerminalProjection| p.runtime_generation += 1) as fn(&mut _),
            |p: &mut TaskTerminalProjection| p.resource_generation += 1,
            |p: &mut TaskTerminalProjection| p.action_epoch += 1,
            |p: &mut TaskTerminalProjection| p.screen.cols += 1,
            |p: &mut TaskTerminalProjection| p.screen.rows += 1,
        ] {
            let mut changed = admitted.clone();
            mutate(&mut changed);
            assert!(
                !window.matches_identity(&changed),
                "a reshaped or regenerated terminal must not reuse retained rows"
            );
        }
    }

    #[test]
    fn a_local_scroll_rewrites_the_projection_and_drops_the_admitted_styling() {
        let mut admitted = projection(100, 10, 8, 8);
        admitted
            .screen
            .cells
            .push(crate::terminal::session::TerminalIndexedCellSnapshot {
                row: 0,
                column: 0,
                cell: crate::terminal::session::TerminalCellSnapshot {
                    character: 'x',
                    zero_width: Vec::new(),
                    foreground: 0,
                    background: 0,
                    bold: false,
                    dim: false,
                    italic: false,
                    underline: false,
                    undercurl: false,
                    strike: false,
                    hidden: false,
                    has_hyperlink: false,
                    default_background: true,
                    default_foreground: true,
                },
            });
        let mut window = RetainedTerminalWindow::capture(&admitted);

        // At the admitted position nothing is dropped: it IS the host's screen.
        let mut at_host = admitted.clone();
        apply_local_scroll_to_projection(&mut at_host, &window);
        assert_eq!(at_host.screen.display_offset, 10);
        assert_eq!(at_host.screen.cells.len(), 1);

        window.seek(13).expect("rows");
        let mut scrolled = admitted.clone();
        apply_local_scroll_to_projection(&mut scrolled, &window);
        assert_eq!(scrolled.screen.display_offset, 13);
        assert_eq!(scrolled.text_lines[0], "line-87");
        assert!(
            scrolled.screen.cells.is_empty(),
            "styled cells are indexed against the admitted viewport"
        );
        assert!(scrolled.screen.margin_above.is_empty());
        assert!(scrolled.screen.margin_below.is_empty());
    }

    /// The window must not be able to invent a row. Every row it paints has to
    /// be one the host sent in the same snapshot.
    #[test]
    fn every_paintable_row_came_from_the_admitted_screen() {
        let admitted = projection(100, 10, 8, 8);
        let sent: std::collections::BTreeSet<String> = admitted
            .text_lines
            .iter()
            .chain(admitted.screen.margin_above.iter())
            .chain(admitted.screen.margin_below.iter())
            .cloned()
            .collect();
        let window = RetainedTerminalWindow::capture(&admitted);
        for offset in 0..=100 {
            let Some(rows) = window.rows_at(offset) else {
                continue;
            };
            for row in rows {
                assert!(sent.contains(row), "{row} was never sent by the host");
            }
        }
    }

    /// Measurement, not an assertion: what one wheel notch costs now that it
    /// is served from the retained window.
    ///
    /// Ignored by default because a wall-clock number is not a gate -- a
    /// loaded machine would make it flake, and the point is the ORDER of
    /// magnitude against the 97 ms median round trip the lane replaced. Run it
    /// with `cargo test -- --ignored notch_cost`.
    #[test]
    #[ignore = "measurement, not a gate: prints the local notch cost"]
    fn notch_cost_measurement() {
        let admitted = projection(10_000, 5_000, 48, 48);
        let mut window = RetainedTerminalWindow::capture(&admitted);
        let mut projection_copy = admitted.clone();

        // Warm the allocator and the branch predictor the same way a real
        // burst would.
        for _ in 0..1_000 {
            if let Some(target) = window.scrolled(3) {
                window.seek(target);
                apply_local_scroll_to_projection(&mut projection_copy, &window);
            }
            if let Some(target) = window.scrolled(-3) {
                window.seek(target);
                apply_local_scroll_to_projection(&mut projection_copy, &window);
            }
        }

        let iterations = 10_000;
        let started = std::time::Instant::now();
        for index in 0..iterations {
            let delta = if index % 2 == 0 { 3 } else { -3 };
            if let Some(target) = window.scrolled(delta) {
                window.seek(target);
                apply_local_scroll_to_projection(&mut projection_copy, &window);
            }
        }
        let elapsed = started.elapsed();
        let per_notch_us = elapsed.as_secs_f64() * 1_000_000.0 / f64::from(iterations);
        println!(
            "local wheel notch: {per_notch_us:.3} us over {iterations} notches              (retained window {} rows, {} cols)",
            window.rows.len(),
            admitted.screen.cols
        );
        // The one thing worth failing on is a pathological regression: a notch
        // that costs a whole frame would put us back where we started.
        assert!(
            per_notch_us < 16_000.0,
            "a locally served notch must not cost a frame: {per_notch_us:.3} us"
        );
    }
}

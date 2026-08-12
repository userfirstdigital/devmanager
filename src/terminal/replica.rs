//! Client-space replica: apply snapshots and contiguous generation-fenced deltas.

use crate::domain::id::{ClientId, TerminalId};
use crate::terminal::protocol::{
    ReplicaApplyResult, TerminalDelta, TerminalDeltaOp, TerminalError, TerminalGeneration,
    TerminalSequence, TerminalSnapshot,
};
use crate::terminal::session::{TerminalCursorSnapshot, TerminalModeSnapshot, TerminalSearchMatch};
use crate::terminal::view::TerminalSelectionSnapshot;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientViewport {
    pub scroll_offset: usize,
    pub selection: Option<TerminalSelectionSnapshot>,
    pub search_query: Option<String>,
    pub search_matches: Vec<TerminalSearchMatch>,
    pub hover_row: Option<usize>,
    pub hover_column: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalReplica {
    terminal_id: Option<TerminalId>,
    generation: Option<TerminalGeneration>,
    sequence: TerminalSequence,
    size_cols: u16,
    size_rows: u16,
    cursor: Option<TerminalCursorSnapshot>,
    modes: TerminalModeSnapshot,
    title: Option<String>,
    rows: Vec<String>,
    history_rows: usize,
    truncated: bool,
    pending: Vec<TerminalDelta>,
    needs_snapshot: bool,
    viewport: ClientViewport,
    owner_client: Option<ClientId>,
}

impl Default for TerminalReplica {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalReplica {
    pub fn new() -> Self {
        Self {
            terminal_id: None,
            generation: None,
            sequence: TerminalSequence::ZERO,
            size_cols: 0,
            size_rows: 0,
            cursor: None,
            modes: TerminalModeSnapshot::default(),
            title: None,
            rows: Vec::new(),
            history_rows: 0,
            truncated: false,
            pending: Vec::new(),
            needs_snapshot: true,
            viewport: ClientViewport::default(),
            owner_client: None,
        }
    }

    pub fn bind_client(&mut self, client_id: ClientId) {
        self.owner_client = Some(client_id);
    }

    pub fn needs_snapshot(&self) -> bool {
        self.needs_snapshot
    }

    pub fn generation(&self) -> Option<TerminalGeneration> {
        self.generation
    }

    pub fn sequence(&self) -> TerminalSequence {
        self.sequence
    }

    pub fn rows(&self) -> &[String] {
        &self.rows
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn viewport(&self) -> &ClientViewport {
        &self.viewport
    }

    pub fn viewport_mut(&mut self) -> &mut ClientViewport {
        &mut self.viewport
    }

    pub fn apply_snapshot(&mut self, snapshot: TerminalSnapshot) {
        self.terminal_id = Some(snapshot.terminal_id);
        self.generation = Some(snapshot.generation);
        self.sequence = snapshot.sequence;
        self.size_cols = snapshot.size.cols;
        self.size_rows = snapshot.size.rows;
        self.cursor = snapshot.cursor;
        self.modes = snapshot.modes;
        self.title = snapshot.title;
        self.rows = snapshot.rows;
        self.history_rows = snapshot.history_rows;
        self.truncated = snapshot.truncated;
        self.pending.clear();
        self.needs_snapshot = false;
    }

    pub fn apply_delta(&mut self, delta: TerminalDelta) -> ReplicaApplyResult {
        if self.needs_snapshot {
            self.pending.clear();
            return ReplicaApplyResult::NeedSnapshot;
        }
        let Some(terminal_id) = self.terminal_id else {
            self.force_resync();
            return ReplicaApplyResult::NeedSnapshot;
        };
        let Some(generation) = self.generation else {
            self.force_resync();
            return ReplicaApplyResult::NeedSnapshot;
        };
        if delta.terminal_id != terminal_id || delta.generation != generation {
            self.force_resync();
            return ReplicaApplyResult::NeedSnapshot;
        }
        let expected = match self.sequence.next() {
            Ok(expected) => expected,
            Err(_) => {
                self.force_resync();
                return ReplicaApplyResult::NeedSnapshot;
            }
        };
        if delta.sequence != expected {
            self.force_resync();
            return ReplicaApplyResult::NeedSnapshot;
        }
        if self.apply_ops(&delta.ops).is_err() {
            self.force_resync();
            return ReplicaApplyResult::NeedSnapshot;
        }
        self.sequence = delta.sequence;
        ReplicaApplyResult::Applied
    }

    pub fn set_scroll_offset(&mut self, offset: usize) {
        self.viewport.scroll_offset = offset;
    }

    pub fn set_selection(&mut self, selection: Option<TerminalSelectionSnapshot>) {
        self.viewport.selection = selection;
    }

    pub fn set_search(&mut self, query: Option<String>, matches: Vec<TerminalSearchMatch>) {
        self.viewport.search_query = query;
        self.viewport.search_matches = matches;
    }

    pub fn set_hover(&mut self, row: Option<usize>, column: Option<usize>) {
        self.viewport.hover_row = row;
        self.viewport.hover_column = column;
    }

    fn apply_ops(&mut self, ops: &[TerminalDeltaOp]) -> Result<(), TerminalError> {
        for op in ops {
            match op {
                TerminalDeltaOp::RowsChanged { start_row, rows } => {
                    let start = usize::from(*start_row);
                    if start > self.rows.len() {
                        return Err(TerminalError::InvalidFence);
                    }
                    let end = start
                        .checked_add(rows.len())
                        .ok_or(TerminalError::BoundExceeded)?;
                    if end > self.rows.len() {
                        self.rows.resize(end, String::new());
                    }
                    self.rows[start..end].clone_from_slice(rows);
                }
                TerminalDeltaOp::Scroll { display_offset: _ } => {
                    // Shared PTY size/history is updated by snapshot/rows.
                    // Scroll offset stays client-local.
                }
                TerminalDeltaOp::Cursor { cursor } => {
                    self.cursor = *cursor;
                }
                TerminalDeltaOp::Mode { modes } => {
                    self.modes = *modes;
                }
                TerminalDeltaOp::Title { title } => {
                    self.title = title.clone();
                }
                TerminalDeltaOp::Truncated { dropped_rows } => {
                    self.truncated = true;
                    self.history_rows = self.history_rows.saturating_sub(*dropped_rows);
                }
                TerminalDeltaOp::Exit { .. } => {}
            }
        }
        Ok(())
    }

    fn force_resync(&mut self) {
        self.pending.clear();
        self.needs_snapshot = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::id::TerminalId;
    use crate::terminal::protocol::{TerminalGeneration, TerminalSequence, TerminalSize};

    fn snapshot(sequence: u64, rows: &[&str]) -> TerminalSnapshot {
        TerminalSnapshot {
            terminal_id: TerminalId::new(),
            generation: TerminalGeneration::initial(),
            sequence: TerminalSequence::from_raw(sequence),
            size: TerminalSize {
                cols: 20,
                rows: rows.len() as u16,
            },
            cursor: None,
            modes: TerminalModeSnapshot::default(),
            title: None,
            rows: rows.iter().map(|row| (*row).to_string()).collect(),
            history_rows: 0,
            truncated: false,
        }
    }

    #[test]
    fn gap_and_generation_mismatch_clear_pending_and_request_snapshot() {
        let snap = snapshot(1, &["one"]);
        let terminal_id = snap.terminal_id;
        let mut replica = TerminalReplica::new();
        replica.apply_snapshot(snap);
        replica.pending.push(TerminalDelta {
            terminal_id,
            generation: TerminalGeneration::initial(),
            sequence: TerminalSequence::from_raw(2),
            ops: Vec::new(),
        });
        let gap = TerminalDelta {
            terminal_id,
            generation: TerminalGeneration::initial(),
            sequence: TerminalSequence::from_raw(4),
            ops: vec![TerminalDeltaOp::Title {
                title: Some("gap".to_string()),
            }],
        };
        assert_eq!(replica.apply_delta(gap), ReplicaApplyResult::NeedSnapshot);
        assert!(replica.needs_snapshot());
        assert!(replica.pending.is_empty());

        replica.apply_snapshot(snapshot(1, &["one"]));
        let mismatch = TerminalDelta {
            terminal_id: replica.terminal_id.expect("id"),
            generation: TerminalGeneration::initial().next().expect("next"),
            sequence: TerminalSequence::from_raw(2),
            ops: Vec::new(),
        };
        assert_eq!(
            replica.apply_delta(mismatch),
            ReplicaApplyResult::NeedSnapshot
        );
    }

    #[test]
    fn viewport_state_is_client_local() {
        let mut replica = TerminalReplica::new();
        replica.apply_snapshot(snapshot(1, &["alpha"]));
        replica.set_scroll_offset(7);
        replica.set_selection(Some(TerminalSelectionSnapshot {
            start_row: 0,
            start_column: 1,
            end_row: 0,
            end_column: 3,
        }));
        replica.set_search(Some("al".to_string()), Vec::new());
        replica.set_hover(Some(0), Some(2));
        replica.apply_snapshot(snapshot(2, &["beta"]));
        assert_eq!(replica.viewport().scroll_offset, 7);
        assert_eq!(replica.viewport().hover_column, Some(2));
        assert_eq!(replica.rows(), &["beta".to_string()]);
    }
}

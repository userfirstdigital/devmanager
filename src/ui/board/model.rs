//! Pure board model: the state grouping, group ordering and per-group sort
//! order the board renders (spec 2026-09-03 section 4). No gpui types here.

use crate::client::HostTaskKey;
use crate::domain::id::ProjectId;
use crate::domain::task::VisibleTaskStatus;
use crate::ui::task_cockpit::inbox::PrimaryProviderIcon;

/// What a row is doing, as the board presents it. Narrower than
/// [`VisibleTaskStatus`]: the board collapses every stuck shape into `Blocked`
/// and every busy shape into `Working`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoardState {
    Question,
    Permission,
    Blocked,
    Working,
    Idle,
    Done,
}

impl BoardState {
    pub fn why_label(self) -> &'static str {
        match self {
            Self::Question => "Asked a question",
            Self::Permission => "Permission",
            Self::Blocked => "Blocked",
            Self::Working => "Working",
            Self::Idle => "Idle",
            Self::Done => "Done",
        }
    }

    pub fn needs_you(self) -> bool {
        matches!(self, Self::Question | Self::Permission | Self::Blocked)
    }
}

/// The four board sections, in the order they render.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoardGroup {
    NeedsYou,
    Working,
    Idle,
    Done,
}

impl BoardGroup {
    pub const ORDER: [Self; 4] = [Self::NeedsYou, Self::Working, Self::Idle, Self::Done];

    pub fn label(self) -> &'static str {
        match self {
            Self::NeedsYou => "Needs you",
            Self::Working => "Working",
            Self::Idle => "Idle",
            Self::Done => "Done",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoardProgress {
    pub completed: usize,
    pub total: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BoardRow {
    pub key: HostTaskKey,
    pub title: String,
    pub state: BoardState,
    /// Second-line left text.
    pub why: String,
    /// Time spent in the current state.
    pub state_age_ms: i64,
    pub progress: Option<BoardProgress>,
    pub provider: PrimaryProviderIcon,
    /// Palette index.
    pub project_colour: u8,
    /// The row's owning project, when the projection knows one. Carried so a
    /// consumer that needs the identity -- the accessibility tree, the rename
    /// gesture -- does not have to re-derive it from the fleet projection or,
    /// worse, mint a fresh one.
    pub project_id: Option<ProjectId>,
    /// Shown only in the hover tooltip; the stripe carries the project on the row.
    pub project_label: String,
    /// Shown only in the hover tooltip.
    pub branch: String,
    pub last_activity_ms: i64,
    /// The row's task has a panel in the workspace, and this is that panel's
    /// ordinal (1-based, in the workspace's reading order). `None` means the
    /// task is not open at all.
    ///
    /// Openness is a different axis from focus: with three panels on screen all
    /// three rows are open, and exactly one of them is [`Self::active`]. The
    /// board marking only the focused one is what made three visible panels
    /// read as one selection.
    pub open: Option<u8>,
    /// The row owns the focused panel. Implies [`Self::open`] is `Some`: a task
    /// cannot be the active panel without having one. Asserted in
    /// [`build_board_model`].
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BoardGroupModel {
    pub group: BoardGroup,
    pub rows: Vec<BoardRow>,
    pub collapsed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BoardModel {
    pub groups: Vec<BoardGroupModel>,
}

impl BoardModel {
    /// Whether the board has any task at all. Distinct from "no groups": Done
    /// is always present, so an empty board still renders one section, and a
    /// bare `DONE 0` is not an empty state -- it reads as a list that failed
    /// to load rather than as a board with nothing on it.
    pub fn has_rows(&self) -> bool {
        self.groups.iter().any(|group| !group.rows.is_empty())
    }
}

pub fn board_state_of(status: VisibleTaskStatus, done: bool) -> BoardState {
    if done {
        return BoardState::Done;
    }
    match status {
        VisibleTaskStatus::NeedsAnswer => BoardState::Question,
        VisibleTaskStatus::NeedsApproval => BoardState::Permission,
        VisibleTaskStatus::Failed
        | VisibleTaskStatus::Disconnected
        | VisibleTaskStatus::UncertainOutcome => BoardState::Blocked,
        VisibleTaskStatus::Working | VisibleTaskStatus::Settling => BoardState::Working,
        VisibleTaskStatus::ReadyForReview | VisibleTaskStatus::Idle => BoardState::Idle,
    }
}

pub fn group_of(state: BoardState) -> BoardGroup {
    match state {
        BoardState::Question | BoardState::Permission | BoardState::Blocked => BoardGroup::NeedsYou,
        BoardState::Working => BoardGroup::Working,
        BoardState::Idle => BoardGroup::Idle,
        BoardState::Done => BoardGroup::Done,
    }
}

/// Groups in fixed order. Empty live groups are omitted; Done is always
/// present so its count and disclosure have a home. Needs-you sorts oldest
/// ask first (it has waited longest); the rest sort most recent activity first.
pub fn build_board_model(rows: Vec<BoardRow>, done_expanded: bool) -> BoardModel {
    let mut buckets: [Vec<BoardRow>; 4] = Default::default();
    for row in rows {
        // The two axes are not independent in one direction: the active panel
        // is one of the open ones. A row that claims otherwise would paint the
        // active treatment with no ordinal to show, so it fails here rather
        // than rendering a chip with nothing in it.
        debug_assert!(
            !row.active || row.open.is_some(),
            "an active row must be open: {} has active=true with open=None",
            row.title
        );
        let index = BoardGroup::ORDER
            .iter()
            .position(|g| *g == group_of(row.state))
            .expect("every group is in ORDER");
        buckets[index].push(row);
    }
    let mut groups = Vec::with_capacity(BoardGroup::ORDER.len());
    for (index, group) in BoardGroup::ORDER.iter().copied().enumerate() {
        let mut rows = std::mem::take(&mut buckets[index]);
        match group {
            BoardGroup::NeedsYou => rows.sort_by(|a, b| b.state_age_ms.cmp(&a.state_age_ms)),
            _ => rows.sort_by(|a, b| b.last_activity_ms.cmp(&a.last_activity_ms)),
        }
        if rows.is_empty() && group != BoardGroup::Done {
            continue;
        }
        groups.push(BoardGroupModel {
            group,
            rows,
            collapsed: group == BoardGroup::Done && !done_expanded,
        });
    }
    BoardModel { groups }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{HostId, HostTaskKey};
    use crate::domain::id::TaskId;

    fn row(state: BoardState, state_age_ms: i64, last_activity_ms: i64) -> BoardRow {
        BoardRow {
            key: HostTaskKey::new(HostId::LocalProfile("p".into()), TaskId::new()),
            title: "t".into(),
            state,
            why: state.why_label().to_string(),
            state_age_ms,
            progress: None,
            provider: PrimaryProviderIcon::Claude,
            project_colour: 0,
            project_id: None,
            project_label: "p".into(),
            branch: "main".into(),
            last_activity_ms,
            open: None,
            active: false,
        }
    }

    /// The two axes survive grouping and sorting. They are carried per row, so
    /// a projection that drops one would show three open panels as one marked
    /// row -- exactly the finding this pair of fields exists to fix.
    #[test]
    fn build_board_model_preserves_open_and_active() {
        let mut first = row(BoardState::Working, 1, 300);
        first.open = Some(1);
        let mut second = row(BoardState::Working, 1, 200);
        second.open = Some(2);
        second.active = true;
        let third = row(BoardState::Working, 1, 100);
        let model = build_board_model(vec![first, second, third], false);
        let working = &model
            .groups
            .iter()
            .find(|group| group.group == BoardGroup::Working)
            .expect("working group")
            .rows;
        assert_eq!(
            working.iter().map(|r| r.open).collect::<Vec<_>>(),
            vec![Some(1), Some(2), None],
            "the ordinals ride the rows through the sort"
        );
        assert_eq!(
            working.iter().filter(|r| r.active).count(),
            1,
            "exactly one row is active"
        );
        assert!(
            working.iter().find(|r| r.active).expect("active").open == Some(2),
            "the active row keeps its own ordinal"
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "an active row must be open")]
    fn an_active_row_without_a_panel_is_a_bug() {
        let mut orphan = row(BoardState::Working, 1, 1);
        orphan.active = true;
        let _ = build_board_model(vec![orphan], false);
    }

    #[test]
    fn visible_status_maps_onto_board_states() {
        use VisibleTaskStatus as V;
        assert_eq!(board_state_of(V::NeedsAnswer, false), BoardState::Question);
        assert_eq!(
            board_state_of(V::NeedsApproval, false),
            BoardState::Permission
        );
        assert_eq!(board_state_of(V::Failed, false), BoardState::Blocked);
        assert_eq!(board_state_of(V::Disconnected, false), BoardState::Blocked);
        assert_eq!(
            board_state_of(V::UncertainOutcome, false),
            BoardState::Blocked
        );
        assert_eq!(board_state_of(V::Working, false), BoardState::Working);
        assert_eq!(board_state_of(V::Settling, false), BoardState::Working);
        assert_eq!(board_state_of(V::ReadyForReview, false), BoardState::Idle);
        assert_eq!(board_state_of(V::Idle, false), BoardState::Idle);
        assert_eq!(
            board_state_of(V::Working, true),
            BoardState::Done,
            "done wins"
        );
    }

    #[test]
    fn groups_come_in_fixed_order_and_done_is_collapsed_by_default() {
        let model = build_board_model(
            vec![
                row(BoardState::Idle, 1, 1),
                row(BoardState::Done, 1, 1),
                row(BoardState::Question, 1, 1),
                row(BoardState::Working, 1, 1),
            ],
            false,
        );
        let groups: Vec<_> = model.groups.iter().map(|g| g.group).collect();
        assert_eq!(
            groups,
            vec![
                BoardGroup::NeedsYou,
                BoardGroup::Working,
                BoardGroup::Idle,
                BoardGroup::Done
            ]
        );
        assert!(model.groups[3].collapsed);
        assert_eq!(
            model.groups[3].rows.len(),
            1,
            "collapsed groups keep their rows for the count"
        );
    }

    #[test]
    fn needs_you_sorts_oldest_ask_first_and_others_most_recent_first() {
        let model = build_board_model(
            vec![
                row(BoardState::Question, 5_000, 10),
                row(BoardState::Permission, 60_000, 20),
                row(BoardState::Working, 1, 100),
                row(BoardState::Working, 1, 300),
            ],
            true,
        );
        let needs: Vec<_> = model.groups[0]
            .rows
            .iter()
            .map(|r| r.state_age_ms)
            .collect();
        assert_eq!(needs, vec![60_000, 5_000]);
        let working: Vec<_> = model.groups[1]
            .rows
            .iter()
            .map(|r| r.last_activity_ms)
            .collect();
        assert_eq!(working, vec![300, 100]);
    }

    #[test]
    fn an_empty_board_reports_no_rows_even_though_done_always_renders() {
        let model = build_board_model(Vec::new(), false);
        assert_eq!(
            model.groups.len(),
            1,
            "Done is always present so its count has a home"
        );
        assert!(
            !model.has_rows(),
            "a lone empty Done section is not content"
        );
        assert!(build_board_model(vec![row(BoardState::Done, 1, 1)], false).has_rows());
        assert!(build_board_model(vec![row(BoardState::Idle, 1, 1)], false).has_rows());
    }

    #[test]
    fn empty_groups_are_omitted_except_done() {
        let model = build_board_model(vec![row(BoardState::Working, 1, 1)], false);
        let groups: Vec<_> = model.groups.iter().map(|g| g.group).collect();
        assert_eq!(groups, vec![BoardGroup::Working, BoardGroup::Done]);
    }

    #[test]
    fn labels_are_the_spec_strings() {
        assert_eq!(BoardGroup::NeedsYou.label(), "Needs you");
        assert_eq!(BoardState::Question.why_label(), "Asked a question");
        assert_eq!(BoardState::Permission.why_label(), "Permission");
        assert_eq!(BoardState::Blocked.why_label(), "Blocked");
    }
}

//! Pure panel chrome model (spec 2026-09-03 section 6). No gpui types here.
//!
//! The chrome is a projection of the board's [`BoardRow`]: the same title, the
//! same state age, the same progress. Nothing here re-derives task state, so a
//! panel can never disagree with the board row for the same task.
//!
//! The one thing the board does not carry is *why* a task wants a person right
//! now -- how many choices a question offers, whether a permission names a file
//! -- because the board row is one line and cannot show it. That arrives as
//! [`NeedsYou`], supplied by the shell alongside the row.

use crate::client::HostTaskKey;
use crate::ui::board::activity::bound;
use crate::ui::board::{format_age, BoardProgress, BoardRow, BoardState};
use crate::ui::task_cockpit::inbox::PrimaryProviderIcon;
use crate::ui::task_workspace::PaneView;

/// The blocked cause is a provider's own words and can be a paragraph. The
/// title row has one line for it, so it is bounded here rather than by the
/// painter: a model that says one thing and a painter that shows another is
/// exactly the drift a truncation rule in the painter would create.
pub const STATUS_CAUSE_MAX_CHARS: usize = 60;

/// Why this panel wants a person. Distinct from [`BoardState`]: the board
/// collapses these into one "needs you" group, and the panel is the surface
/// with room to say which one and how to answer it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NeedsYou {
    Question { choices: usize },
    Permission { names_a_file: bool },
    Blocked { cause: String },
}

/// The one button the title row spends its width on. Everything else lives
/// behind the menu.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrimaryAction {
    Done,
    Reopen,
}

/// How loud the status reads. Only the two states that want a person are
/// allowed to be saturated (spec 5.3), so this is the whole colour vocabulary
/// the title row has.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusTone {
    Neutral,
    Attention,
    Blocked,
}

/// The inline status folded into the title row (mockup `02-panel-chrome-2`,
/// chosen option 2).
#[derive(Clone, Debug, PartialEq)]
pub struct PanelStatus {
    pub icon: &'static str,
    pub text: String,
    pub age: String,
    pub progress: Option<BoardProgress>,
    pub tone: StatusTone,
}

/// Everything the panel chrome paints, and nothing else. The painter takes
/// this by reference and never reaches back to the row, the workspace or the
/// fleet projection.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelChrome {
    pub key: HostTaskKey,
    pub title: String,
    /// "Snake Game · Claude · main", shown only when zoomed: at one-of-eight
    /// width the title row has no room for it, and the board's stripe and mark
    /// already carry the project and the provider.
    pub crumb: String,
    pub provider: PrimaryProviderIcon,
    pub project_colour: u8,
    pub status: PanelStatus,
    pub needs_you: Option<NeedsYou>,
    pub primary: PrimaryAction,
    pub view: PaneView,
    pub focused: bool,
    pub zoomed: bool,
    pub minimised: bool,
}

/// Which parts of the inline status survive at a given panel width. The strip
/// goes first, then the text.
///
/// The icon and the age are not here because they are never dropped: "12s"
/// answers "is this stuck?" in three characters, and the painter reserves a
/// floor for them (and for a blocked panel's Retry) so no width can clip them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StatusLayout {
    pub show_segments: bool,
    pub show_text: bool,
}

pub fn status_layout(panel_width_px: f32) -> StatusLayout {
    StatusLayout {
        show_segments: panel_width_px >= 320.0,
        show_text: panel_width_px >= 260.0,
    }
}

/// Project one board row plus the shell's needs-you detail into panel chrome.
///
/// `focused`, `zoomed` and `minimised` are parameters rather than row fields on
/// purpose: they are facts about this *pane*, and the same task can be open in
/// more than one place. Reading them off the row would make the board's notion
/// of selection decide how a pane paints.
pub fn panel_chrome(
    row: &BoardRow,
    view: PaneView,
    focused: bool,
    zoomed: bool,
    minimised: bool,
    needs_you: Option<NeedsYou>,
    done: bool,
    crumb: String,
) -> PanelChrome {
    let (icon, text, tone) = match (&needs_you, row.state) {
        (Some(NeedsYou::Question { .. }), _) => {
            ("?", "Asked a question".to_string(), StatusTone::Attention)
        }
        (Some(NeedsYou::Permission { .. }), _) => {
            ("?", "Permission".to_string(), StatusTone::Attention)
        }
        (Some(NeedsYou::Blocked { cause }), _) => (
            "!",
            bound(cause, STATUS_CAUSE_MAX_CHARS),
            StatusTone::Blocked,
        ),
        (None, BoardState::Working) => ("▶", row.why.clone(), StatusTone::Neutral),
        (None, BoardState::Done) => ("✓", "Done".to_string(), StatusTone::Neutral),
        (None, _) => ("·", row.why.clone(), StatusTone::Neutral),
    };
    PanelChrome {
        key: row.key.clone(),
        title: row.title.clone(),
        crumb,
        provider: row.provider,
        project_colour: row.project_colour,
        status: PanelStatus {
            icon,
            text,
            age: format_age(row.state_age_ms),
            progress: row.progress,
            tone,
        },
        needs_you,
        primary: if done {
            PrimaryAction::Reopen
        } else {
            PrimaryAction::Done
        },
        view,
        focused,
        zoomed,
        minimised,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::HostId;
    use crate::domain::id::TaskId;
    use crate::ui::board::{BoardRow, BoardState};
    use crate::ui::task_cockpit::inbox::PrimaryProviderIcon;
    use crate::ui::task_workspace::PaneView;

    /// A fixed, valid UUID v7 so the fixture row is byte-identical on every
    /// run: the panel's element identity is a digest of this, and a random id
    /// would make that identity untestable.
    const TASK_ID_BYTES: [u8; 16] = [
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x01,
    ];

    fn row(state: BoardState) -> BoardRow {
        BoardRow {
            key: crate::client::HostTaskKey::new(
                HostId::LocalProfile("p".into()),
                TaskId::from_bytes(TASK_ID_BYTES).expect("task id"),
            ),
            title: "Snake Frontend".into(),
            state,
            why: state.why_label().into(),
            state_age_ms: 12_000,
            progress: None,
            provider: PrimaryProviderIcon::Claude,
            project_colour: 0,
            project_id: None,
            project_label: "Snake Game".into(),
            branch: "main".into(),
            last_activity_ms: 0,
            selected: false,
        }
    }

    #[test]
    fn question_panels_are_amber_with_the_asked_a_question_status() {
        let chrome = panel_chrome(
            &row(BoardState::Question),
            PaneView::Conversation,
            true,
            false,
            false,
            Some(NeedsYou::Question { choices: 3 }),
            false,
            String::new(),
        );
        assert_eq!(chrome.status.tone, StatusTone::Attention);
        assert_eq!(chrome.status.icon, "?");
        assert_eq!(chrome.status.text, "Asked a question");
        assert_eq!(chrome.primary, PrimaryAction::Done);
    }

    #[test]
    fn blocked_panels_are_red_and_name_the_cause_bounded() {
        let long = "x".repeat(200);
        let chrome = panel_chrome(
            &row(BoardState::Blocked),
            PaneView::Conversation,
            false,
            false,
            false,
            Some(NeedsYou::Blocked { cause: long }),
            false,
            String::new(),
        );
        assert_eq!(chrome.status.tone, StatusTone::Blocked);
        assert_eq!(chrome.status.icon, "!");
        assert_eq!(chrome.status.text.chars().count(), STATUS_CAUSE_MAX_CHARS);
    }

    #[test]
    fn working_panels_show_doing_now_and_done_tasks_offer_reopen() {
        let mut working = row(BoardState::Working);
        working.why = "cargo test".into();
        let chrome = panel_chrome(
            &working,
            PaneView::Terminal,
            false,
            false,
            false,
            None,
            false,
            String::new(),
        );
        assert_eq!(chrome.status.text, "cargo test");
        assert_eq!(chrome.status.icon, "▶");
        assert_eq!(chrome.status.tone, StatusTone::Neutral);
        assert_eq!(chrome.view, PaneView::Terminal);

        let done = panel_chrome(
            &row(BoardState::Done),
            PaneView::Conversation,
            false,
            false,
            false,
            None,
            true,
            String::new(),
        );
        assert_eq!(done.primary, PrimaryAction::Reopen);
        assert_eq!(done.status.icon, "✓");
        assert_eq!(done.status.text, "Done");
    }

    #[test]
    fn status_drops_segments_under_320_and_text_under_260() {
        assert_eq!(
            status_layout(470.0),
            StatusLayout {
                show_segments: true,
                show_text: true
            }
        );
        assert_eq!(
            status_layout(319.0),
            StatusLayout {
                show_segments: false,
                show_text: true
            }
        );
        assert_eq!(
            status_layout(259.0),
            StatusLayout {
                show_segments: false,
                show_text: false
            }
        );
    }

    /// The chrome carries the row's identity, its age and its progress
    /// unchanged: the painter must never have to reach back to the row.
    #[test]
    fn the_chrome_carries_the_rows_identity_age_and_progress() {
        let mut working = row(BoardState::Working);
        working.progress = Some(BoardProgress {
            completed: 5,
            total: 6,
        });
        let chrome = panel_chrome(
            &working,
            PaneView::Files,
            false,
            true,
            false,
            None,
            false,
            "Snake Game · Claude · main".into(),
        );
        assert_eq!(chrome.key, working.key);
        assert_eq!(chrome.title, "Snake Frontend");
        assert_eq!(chrome.crumb, "Snake Game · Claude · main");
        assert_eq!(chrome.status.age, format_age(12_000));
        assert_eq!(chrome.status.progress, working.progress);
        assert!(chrome.zoomed);
        assert!(!chrome.focused);
        assert!(!chrome.minimised);
    }

    /// A permission is the other amber state, and it says which word it is so
    /// the panel does not read as a question that has choices to number.
    #[test]
    fn permission_panels_are_amber_and_say_permission() {
        let chrome = panel_chrome(
            &row(BoardState::Permission),
            PaneView::Changes,
            false,
            false,
            true,
            Some(NeedsYou::Permission { names_a_file: true }),
            false,
            String::new(),
        );
        assert_eq!(chrome.status.tone, StatusTone::Attention);
        assert_eq!(chrome.status.icon, "?");
        assert_eq!(chrome.status.text, "Permission");
        assert!(chrome.minimised);
    }
}

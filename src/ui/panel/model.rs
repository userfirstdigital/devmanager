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
    Question {
        choices: usize,
    },
    Permission {
        names_a_file: bool,
    },
    Blocked {
        cause: String,
        recovery: BlockedRecovery,
    },
}

/// What a blocked panel's one recovery affordance actually DOES.
///
/// The label and the action are one decision, not two: a panel that says
/// "Retry" and starts a fresh conversation, or says "Start fresh" and retries,
/// is worse than either. So the painter reads its label from here and the
/// handler reads its action from the same value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockedRecovery {
    /// Re-select the task, which re-runs its cockpit refresh and re-attempts
    /// the provider restore. Right for every cause a retry can clear.
    Retry,
    /// Abandon the durable conversation the provider has forgotten and start a
    /// new one. The only way forward when the provider refuses to resume, and
    /// deliberately a person's decision -- it discards the conversation's
    /// durable identity.
    StartFresh,
}

/// Which recovery a blocked panel owes, from the one fact that decides it.
///
/// Pure and separate from the shell so BOTH directions can be exercised: a
/// mapping tested only where it answers `StartFresh` would not catch a version
/// that answers `StartFresh` always, and offering to discard a conversation on
/// an ordinary refusal is the expensive mistake here.
pub fn blocked_recovery_for(provider_conversation_missing: bool) -> BlockedRecovery {
    if provider_conversation_missing {
        // No retry can clear this: every attempt runs the same
        // `--resume <id>` against a conversation the provider does not have.
        BlockedRecovery::StartFresh
    } else {
        BlockedRecovery::Retry
    }
}

impl BlockedRecovery {
    /// The affordance's text where the row has room for it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Retry => "Retry",
            Self::StartFresh => "Start fresh",
        }
    }

    /// The form that fits at EVERY width, and therefore the one the status
    /// floor reserves.
    ///
    /// The floor cannot simply grow to the longest label: the title row is
    /// exactly paid for at every width
    /// (`CONTROLS_RESERVE + title_floor(w) + status_budget(w) == w`), and at
    /// 280 px the whole status budget is 73 px -- which is the floor itself.
    /// A floor sized to "Start fresh" would sit ABOVE its own budget there,
    /// and a `flex_none` child under an under-reserved floor is CLIPPED rather
    /// than moved. So the label yields instead, exactly as the status text
    /// does, and both forms are five characters wide at the floor.
    /// Measured, not guessed: at the 11 px status size "Fresh" is 29.5 px
    /// against the 28 px the floor reserves, so it does NOT fit and "New"
    /// (22.5 px) does. The floor has zero slack to give -- at 280 px the whole
    /// status budget IS the floor -- so the label is what had to be chosen to
    /// fit, and the render test asserts that it still does.
    pub fn short_label(self) -> &'static str {
        match self {
            Self::Retry => "Retry",
            Self::StartFresh => "New",
        }
    }

    /// The label to paint given the pixels the status group actually has.
    ///
    /// `spare_px` is what is left after everything the floor already reserved,
    /// so the long form is shown only where it costs nothing that was promised
    /// to something else.
    ///
    /// Deliberately NOT called `label_within`: `overlay_chrome::label_within`
    /// is the painter's truncating-label helper, a source guard counts its
    /// call sites by name to prove the title row has exactly two of them, and
    /// a third match here would have made that guard say the row grew a
    /// truncating label it did not grow. (It did fire, which is the only
    /// reason this is named differently.)
    pub fn label_for_room(self, spare_px: f32, extra_px: f32) -> &'static str {
        if spare_px >= extra_px {
            self.label()
        } else {
            self.short_label()
        }
    }
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
    /// The state glyph, or `None` for a state that has no verb of its own.
    ///
    /// Idle is the case the redesign found: its glyph was a middle dot, the
    /// same character the row already uses as its separator, so the status
    /// opened "· Idle · 4d" and read as a separator with nothing in front of
    /// it. Absence is the honest model -- there is no icon for "nothing is
    /// happening" -- and it is what the painter branches on.
    pub icon: Option<&'static str>,
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
    /// The panel number, 1-based, or `None` for a pane the workspace has no
    /// ordinal for. Read straight off the row's `open` field rather than from
    /// a second lookup, so a row's chip and its panel's chip are one number
    /// (Task 12).
    pub ordinal: Option<u8>,
    pub status: PanelStatus,
    pub needs_you: Option<NeedsYou>,
    pub primary: PrimaryAction,
    pub view: PaneView,
    pub focused: bool,
    pub zoomed: bool,
    pub minimised: bool,
}

/// Which parts of the title row's right-hand group survive at a given panel
/// width.
///
/// Two rules produce this, and they are different questions. This function
/// answers the first: below a width a part is simply too small to read, so the
/// plan strip goes at 320 px and the doing-now text at 260 px whatever else
/// the row contains. The painter then applies the second in
/// `render::title_row_layout`: the title is the panel's identity, so above
/// those floors the group keeps yielding -- the zoom glyph, then the age, then
/// the verb -- until the title has [`TITLE_MIN_SHARE`] of the row.
///
/// `show_zoom` and `show_age` open true here because neither has a legibility
/// floor of its own: "12s" answers "is this stuck?" in three characters and
/// the glyph is one character, so only the title's claim ever takes them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StatusLayout {
    pub show_zoom: bool,
    pub show_segments: bool,
    pub show_text: bool,
    pub show_age: bool,
    /// The primary action has given up its label and is a glyph.
    ///
    /// The last rung of the yield ladder and the only one that touches a
    /// control rather than the status: Done and the menu are how a panel is
    /// operated at all, so the button never DISAPPEARS -- it just stops
    /// spending 44 px on a word to buy the title enough room to name its task.
    pub primary_icon_only: bool,
}

/// The share of the title row the title may not be squeezed below while
/// anything in the status group is still droppable.
///
/// At 296 px the fix wave 2 render gave the title 66 px against roughly 200 px
/// of "Idle · 7d ⤢ Done ⋯", so three panels read "are there ...", "Build the
/// ..." and "Reply wit..." -- three panels that could not be told apart while
/// the row spent two thirds of itself on facts the board row already carries.
pub const TITLE_MIN_SHARE: f32 = 0.4;

/// The width above which nothing in the status group has to yield.
///
/// Roughly the width at which the whole group plus a 40% title fits, so above
/// it the ladder never fires and the rule costs nothing; below it the title is
/// under real pressure and the group pays for it.
pub const STATUS_YIELD_WIDTH: f32 = 420.0;

pub fn status_layout(panel_width_px: f32) -> StatusLayout {
    StatusLayout {
        show_zoom: true,
        show_segments: panel_width_px >= 320.0,
        show_text: panel_width_px >= 260.0,
        show_age: true,
        primary_icon_only: false,
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
        (Some(NeedsYou::Question { .. }), _) => (
            Some("?"),
            "Asked a question".to_string(),
            StatusTone::Attention,
        ),
        (Some(NeedsYou::Permission { .. }), _) => {
            (Some("?"), "Permission".to_string(), StatusTone::Attention)
        }
        (Some(NeedsYou::Blocked { cause, .. }), _) => (
            Some("!"),
            bound(cause, STATUS_CAUSE_MAX_CHARS),
            StatusTone::Blocked,
        ),
        (None, BoardState::Working) => (Some("▶"), row.why.clone(), StatusTone::Neutral),
        (None, BoardState::Done) => (Some("✓"), "Done".to_string(), StatusTone::Neutral),
        // Idle and every other quiet state: no verb, so no glyph. The row then
        // opens with the word, and the separator only ever sits BETWEEN two
        // things that are both there.
        (None, _) => (None, row.why.clone(), StatusTone::Neutral),
    };
    PanelChrome {
        key: row.key.clone(),
        title: row.title.clone(),
        crumb,
        provider: row.provider,
        project_colour: row.project_colour,
        ordinal: row.open,
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
            open: None,
            active: false,
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
        assert_eq!(chrome.status.icon, Some("?"));
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
            Some(NeedsYou::Blocked {
                cause: long,
                recovery: BlockedRecovery::Retry,
            }),
            false,
            String::new(),
        );
        assert_eq!(chrome.status.tone, StatusTone::Blocked);
        assert_eq!(chrome.status.icon, Some("!"));
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
        assert_eq!(chrome.status.icon, Some("▶"));
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
        assert_eq!(done.status.icon, Some("✓"));
        assert_eq!(done.status.text, "Done");
    }

    #[test]
    fn status_drops_segments_under_320_and_text_under_260() {
        assert_eq!(
            status_layout(470.0),
            StatusLayout {
                show_zoom: true,
                show_segments: true,
                show_text: true,
                show_age: true,
                primary_icon_only: false,
            }
        );
        assert_eq!(
            status_layout(319.0),
            StatusLayout {
                show_zoom: true,
                show_segments: false,
                show_text: true,
                show_age: true,
                primary_icon_only: false,
            }
        );
        assert_eq!(
            status_layout(259.0),
            StatusLayout {
                show_zoom: true,
                show_segments: false,
                show_text: false,
                show_age: true,
                primary_icon_only: false,
            }
        );
        // The legibility floors alone never take the zoom or the age: only the
        // title's claim does, and that is the painter's ladder.
        assert_eq!(TITLE_MIN_SHARE, 0.4);
        assert_eq!(STATUS_YIELD_WIDTH, 420.0);
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
        assert_eq!(chrome.status.icon, Some("?"));
        assert_eq!(chrome.status.text, "Permission");
        assert!(chrome.minimised);
    }
}

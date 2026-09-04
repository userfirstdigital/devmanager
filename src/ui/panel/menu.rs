//! The panel's ⋯ menu and its one-key vocabulary (spec 2026-09-03 section 6.4),
//! copied from the approved mockup `02-panel-chrome-2.html` — the open menu
//! beneath the zoomed panel is the authority for the order, the labels and the
//! shortcut column.
//!
//! Pure: the menu decides nothing, it only names what the shell may do. Two
//! separate vocabularies would be two places for a key to mean two things, so
//! [`panel_key_action`] answers for the whole panel and the shell dispatches.

use crate::ui::task_workspace::PaneView;

/// Everything the panel can do that is not the one primary button.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelMenuItem {
    AddAction,
    Commit,
    Zoom,
    PinSize,
    Move,
    Swap,
    /// The three views that do not get a tab: [`PaneView::MORE`].
    MoreViews,
    Rename,
    Archive,
    Delete,
}

/// One rendered menu row. `separator_before` is carried rather than derived so
/// the grouping is asserted in a test instead of being re-guessed by a painter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PanelMenuRow {
    pub item: PanelMenuItem,
    pub label: &'static str,
    /// The shortcut column. Empty when the item has no one-key form.
    pub key: &'static str,
    pub danger: bool,
    pub separator_before: bool,
}

/// What a key press on a focused panel means. `Done` is in the vocabulary
/// because the shell's keyboard model produces it for Ctrl+D; nothing here
/// returns it, so the panel and the shell cannot disagree about the modifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelKeyAction {
    Menu(PanelMenuItem),
    Done,
    Answer(u8),
    Allow,
    Deny,
    ViewDiff,
    Unzoom,
}

/// The menu rows in spec order. Zoom reads its current state so the row says
/// what pressing it will do, not what the panel already is.
pub fn panel_menu_rows(zoomed: bool) -> Vec<PanelMenuRow> {
    vec![
        PanelMenuRow {
            item: PanelMenuItem::AddAction,
            label: "Add action",
            key: "A",
            danger: false,
            separator_before: false,
        },
        PanelMenuRow {
            item: PanelMenuItem::Commit,
            label: "Commit",
            key: "C",
            danger: false,
            separator_before: false,
        },
        PanelMenuRow {
            item: PanelMenuItem::Zoom,
            label: if zoomed { "Unzoom" } else { "Zoom" },
            key: "Z",
            danger: false,
            separator_before: true,
        },
        PanelMenuRow {
            item: PanelMenuItem::PinSize,
            label: "Pin size",
            key: "P",
            danger: false,
            separator_before: false,
        },
        PanelMenuRow {
            item: PanelMenuItem::Move,
            label: "Move ← ↑ ↓ →",
            key: "⇧⌘arrows",
            danger: false,
            separator_before: false,
        },
        PanelMenuRow {
            item: PanelMenuItem::Swap,
            label: "Swap with…",
            key: "S",
            danger: false,
            separator_before: false,
        },
        PanelMenuRow {
            item: PanelMenuItem::MoreViews,
            label: "More views…",
            key: "",
            danger: false,
            separator_before: true,
        },
        PanelMenuRow {
            item: PanelMenuItem::Rename,
            label: "Rename",
            key: "",
            danger: false,
            separator_before: false,
        },
        PanelMenuRow {
            item: PanelMenuItem::Archive,
            label: "Archive",
            key: "",
            danger: false,
            separator_before: false,
        },
        PanelMenuRow {
            item: PanelMenuItem::Delete,
            label: "Delete…",
            key: "confirms",
            danger: true,
            separator_before: false,
        },
    ]
}

/// The views behind [`PanelMenuItem::MoreViews`]. One definition: the submenu
/// and the tab row read the same two constants, so a view can never be both
/// tabbed and hidden.
pub fn more_views() -> &'static [PaneView] {
    &PaneView::MORE
}

/// Map one key name to what the panel does with it.
///
/// The state-gated keys return `None` rather than a fallback when their state
/// is absent: a digit that means "answer choice 3" must not silently mean
/// something else on a panel with no question, and Escape is the only key with
/// a defined second meaning.
pub fn panel_key_action(
    key: &str,
    has_pending_question: bool,
    has_pending_permission: bool,
) -> Option<PanelKeyAction> {
    match key {
        "a" => Some(PanelKeyAction::Menu(PanelMenuItem::AddAction)),
        "c" => Some(PanelKeyAction::Menu(PanelMenuItem::Commit)),
        "z" => Some(PanelKeyAction::Menu(PanelMenuItem::Zoom)),
        "p" => Some(PanelKeyAction::Menu(PanelMenuItem::PinSize)),
        "s" => Some(PanelKeyAction::Menu(PanelMenuItem::Swap)),
        "d" => has_pending_permission.then_some(PanelKeyAction::ViewDiff),
        "enter" => has_pending_permission.then_some(PanelKeyAction::Allow),
        "escape" => Some(if has_pending_permission {
            PanelKeyAction::Deny
        } else {
            PanelKeyAction::Unzoom
        }),
        _ => {
            let digit = key
                .chars()
                .next()
                .filter(|_| key.chars().count() == 1)
                .and_then(|character| character.to_digit(10))
                .filter(|value| (1..=9).contains(value));
            match digit {
                Some(value) if has_pending_question => Some(PanelKeyAction::Answer(value as u8)),
                _ => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_rows_follow_the_spec_order_with_two_separators() {
        let rows = panel_menu_rows(false);
        let items: Vec<_> = rows.iter().map(|row| row.item).collect();
        assert_eq!(
            items,
            vec![
                PanelMenuItem::AddAction,
                PanelMenuItem::Commit,
                PanelMenuItem::Zoom,
                PanelMenuItem::PinSize,
                PanelMenuItem::Move,
                PanelMenuItem::Swap,
                PanelMenuItem::MoreViews,
                PanelMenuItem::Rename,
                PanelMenuItem::Archive,
                PanelMenuItem::Delete,
            ]
        );
        assert!(rows[2].separator_before && rows[6].separator_before);
        assert_eq!(
            rows.iter().filter(|row| row.separator_before).count(),
            2,
            "exactly two separators, or the menu is a list rather than three groups"
        );
        assert!(rows[9].danger);
        assert_eq!(
            rows.iter().filter(|row| row.danger).count(),
            1,
            "only Delete is destructive"
        );
        assert_eq!(rows[2].label, "Zoom");
        assert_eq!(panel_menu_rows(true)[2].label, "Unzoom");
    }

    #[test]
    fn letter_keys_map_only_when_the_matching_state_is_pending() {
        assert_eq!(
            panel_key_action("3", true, false),
            Some(PanelKeyAction::Answer(3))
        );
        assert_eq!(panel_key_action("3", false, false), None);
        assert_eq!(
            panel_key_action("enter", false, true),
            Some(PanelKeyAction::Allow)
        );
        assert_eq!(
            panel_key_action("escape", false, true),
            Some(PanelKeyAction::Deny)
        );
        assert_eq!(
            panel_key_action("escape", false, false),
            Some(PanelKeyAction::Unzoom)
        );
        assert_eq!(
            panel_key_action("d", false, true),
            Some(PanelKeyAction::ViewDiff)
        );
        assert_eq!(panel_key_action("d", false, false), None);
        assert_eq!(
            panel_key_action("z", false, false),
            Some(PanelKeyAction::Menu(PanelMenuItem::Zoom))
        );
    }

    /// The must-NOT-match half: a key with no meaning stays meaningless, and
    /// "0" is not a choice number, so it must not become `Answer(0)`.
    #[test]
    fn unmapped_keys_and_zero_stay_unmapped() {
        for key in ["0", "q", "10", "", "shift-3", "enter "] {
            assert_eq!(
                panel_key_action(key, true, true),
                None,
                "{key} must not map to a panel action"
            );
        }
        assert_eq!(
            panel_key_action("9", true, false),
            Some(PanelKeyAction::Answer(9))
        );
        assert_eq!(panel_key_action("enter", false, false), None);
    }

    /// The tab row and the menu's submenu read the same two constants, so a
    /// view can never be both tabbed and hidden behind the menu.
    #[test]
    fn the_more_views_submenu_is_exactly_the_views_without_a_tab() {
        assert_eq!(more_views(), PaneView::MORE.as_slice());
        for view in more_views() {
            assert!(!PaneView::TABS.contains(view));
        }
    }
}

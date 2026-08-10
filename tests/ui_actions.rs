use devmanager::client::action;
use devmanager::domain::id::TaskId;
use devmanager::ui::actions::{
    self, ActionAvailability, DockTool, KeyboardAction, KeyboardBinding, KeyboardModel,
    KeyboardShortcut, ShortcutKey,
};
use devmanager::ui::components::interaction::FocusEpochSource;
use devmanager::ui::components::{AccessibleRole, InteractionStateModel};
use serde::Deserialize;
use std::fs;

#[derive(Debug, Deserialize)]
struct ActionStateFixture {
    schema: String,
    states: Vec<ActionStateCase>,
}

#[derive(Debug, Deserialize)]
struct ActionStateCase {
    id: String,
    selected_task: bool,
    disabled_ids: Vec<String>,
}

fn fixture() -> ActionStateFixture {
    let path = format!(
        "{}/tests/fixtures/ui/task-list-states.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&fs::read_to_string(path).expect("task-list states fixture"))
        .expect("valid task-list states fixture")
}

fn fixed_task_id() -> TaskId {
    TaskId::from_bytes([
        0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x01,
    ])
    .expect("fixed UUIDv7 task id")
}

#[test]
fn action_projection_reuses_every_shared_id_and_adds_accessible_presentation() {
    let fixture = fixture();
    assert_eq!(fixture.schema, "devmanager.ui.task-cockpit.states/v1");

    let shared_ids: Vec<&str> = action::catalog()
        .iter()
        .map(|descriptor| descriptor.id)
        .collect();
    for state in fixture.states {
        let projected = actions::catalog(state.selected_task.then_some(fixed_task_id()));
        let projected_ids: Vec<&str> = projected.iter().map(|entry| entry.id()).collect();

        assert_eq!(
            projected_ids, shared_ids,
            "state {} changed the shared catalog",
            state.id
        );
        assert!(projected.iter().all(|entry| entry.shortcut().is_some()));
        assert!(projected
            .iter()
            .all(|entry| !entry.presentation_label().is_empty()));
        assert!(projected
            .iter()
            .all(|entry| !entry.accessibility().name().is_empty()));
        assert!(projected
            .iter()
            .all(|entry| entry.accessibility().role() == AccessibleRole::Button));

        for entry in &projected {
            let expected_disabled = state.disabled_ids.iter().any(|id| id == entry.id());
            assert_eq!(entry.disabled(), expected_disabled, "state {}", state.id);
            assert_eq!(
                entry.availability(),
                if expected_disabled {
                    ActionAvailability::Unavailable
                } else {
                    ActionAvailability::Available
                },
                "state {} action {}",
                state.id,
                entry.id()
            );
            if expected_disabled {
                let reason = entry
                    .disabled_reason()
                    .expect("disabled actions expose a reason");
                assert!(entry.accessibility().disabled());
                assert_eq!(entry.accessibility().description(), reason);
            } else {
                assert!(!entry.accessibility().disabled());
            }
        }
    }
}

#[test]
fn action_projection_does_not_add_ids_or_factories_to_the_shared_catalog() {
    let first = actions::catalog(None);
    let second = actions::catalog(None);

    assert_eq!(first, second, "presentation catalog must be deterministic");
    assert_eq!(first.len(), action::catalog().len());
    assert!(first.iter().all(|entry| {
        action::catalog()
            .iter()
            .any(|descriptor| descriptor.id == entry.id())
    }));
}

#[test]
fn keyboard_model_contains_only_the_planned_task_cockpit_shortcuts() {
    let model = KeyboardModel::default();
    let interaction = InteractionStateModel::default();

    assert_eq!(
        model.resolve(KeyboardShortcut::ctrl(ShortcutKey::Character('k'))),
        Some(KeyboardAction::OpenPalette)
    );
    assert_eq!(
        model.resolve(KeyboardShortcut::ctrl(ShortcutKey::Character('p'))),
        Some(KeyboardAction::OpenTaskSwitcher)
    );
    assert_eq!(
        model.resolve(KeyboardShortcut::ctrl_shift(ShortcutKey::Character('p'))),
        Some(KeyboardAction::OpenCommandPalette)
    );
    assert_eq!(
        model.resolve(KeyboardShortcut::alt(ShortcutKey::Digit(1))),
        Some(KeyboardAction::SelectDock(DockTool::Changes))
    );
    assert_eq!(
        model.resolve(KeyboardShortcut::alt(ShortcutKey::Digit(7))),
        Some(KeyboardAction::SelectDock(DockTool::Review))
    );
    assert_eq!(
        model.resolve(KeyboardShortcut::ctrl(ShortcutKey::Backtick)),
        Some(KeyboardAction::OpenTerminal)
    );
    assert_eq!(
        model.resolve(KeyboardShortcut::escape()),
        Some(KeyboardAction::DismissTransient)
    );
    assert_eq!(
        model.resolve(KeyboardShortcut::alt(ShortcutKey::Character('1'))),
        None
    );

    assert_eq!(
        model.activate(
            KeyboardShortcut::ctrl(ShortcutKey::Character('k')),
            &interaction,
            interaction.focus_epoch(),
        ),
        Some(KeyboardAction::OpenPalette)
    );
    let mut disabled = InteractionStateModel::default();
    disabled.set_disabled(true);
    assert_eq!(
        model.activate(
            KeyboardShortcut::ctrl(ShortcutKey::Character('k')),
            &disabled,
            disabled.focus_epoch(),
        ),
        None
    );
    assert_eq!(
        model.activate(
            KeyboardShortcut::escape(),
            &disabled,
            disabled.focus_epoch()
        ),
        Some(KeyboardAction::DismissTransient)
    );
    assert_eq!(
        model.activate(KeyboardShortcut::escape(), &disabled, {
            let mut source = FocusEpochSource::new();
            source.advance()
        }),
        None,
        "Escape may bypass disabled state but never a stale focus epoch"
    );
}

#[test]
fn custom_dismiss_shortcut_cannot_bypass_disabled_or_loading() {
    let shortcut = KeyboardShortcut::ctrl(ShortcutKey::Character('d'));
    let model = KeyboardModel::new(vec![KeyboardBinding {
        shortcut,
        action: KeyboardAction::DismissTransient,
    }])
    .expect("custom shortcut is conflict-free");

    let mut disabled = InteractionStateModel::default();
    disabled.set_disabled(true);
    assert_eq!(
        model.activate(shortcut, &disabled, disabled.focus_epoch()),
        None
    );

    let mut loading = InteractionStateModel::default();
    loading
        .set_loading(true)
        .expect("control can enter loading");
    assert_eq!(
        model.activate(shortcut, &loading, loading.focus_epoch()),
        None
    );
}

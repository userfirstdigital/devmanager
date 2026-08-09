use devmanager::ui::components::badge::Badge;
use devmanager::ui::components::button::{Button, ButtonVariant};
use devmanager::ui::components::empty_state::{EmptyState, RecoveryAction};
use devmanager::ui::components::error_boundary::ErrorBoundary;
use devmanager::ui::components::icon_button::{IconButton, TooltipContract};
use devmanager::ui::components::interaction::{
    AccessibilityMetadata, AccessibleRole, ActionEvent, ActionId, InteractionState,
    InteractionStateModel, InteractionTransition, KeyboardKey,
};
use devmanager::ui::components::status_light::StatusLight;
use devmanager::ui::components::text_field::{
    TextField, TextFieldError, TextFieldKey, TextFieldLimits,
};
use devmanager::ui::tokens::{theme, Density, Scale, StatusMeaning, ThemeMode};
use static_assertions::assert_not_impl_any;
use std::sync::{Arc, Mutex};

assert_not_impl_any!(InteractionStateModel: Clone);
assert_not_impl_any!(InteractionStateModel: Copy);

fn action_id(value: &str) -> ActionId {
    ActionId::new(value).expect("test action ids are stable and valid")
}

fn recording_callback() -> (
    Arc<Mutex<Vec<ActionEvent>>>,
    impl Fn(ActionEvent) + Send + Sync + 'static,
) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    (events, move |event| {
        sink.lock().expect("event sink lock").push(event)
    })
}

#[test]
fn interaction_transition_table_rejects_invalid_combinations_and_fails_closed() {
    let state = InteractionState::default()
        .transition(InteractionTransition::Hover)
        .expect("default may hover")
        .transition(InteractionTransition::Focus)
        .expect("hover may focus")
        .transition(InteractionTransition::Press)
        .expect("focused control may press");

    assert_eq!(
        state.visual_state(),
        devmanager::ui::components::interaction::VisualState::Pressed
    );
    assert!(state.pressed);
    assert!(state.focused);

    let invalid = InteractionState::disabled()
        .transition(InteractionTransition::Press)
        .expect_err("disabled controls cannot become pressed");
    assert!(invalid.to_string().contains("disabled"));
    let mut fail_closed = InteractionStateModel::default();
    fail_closed.set_disabled(true);
    let _ = fail_closed.transition(InteractionTransition::Press);
    assert_eq!(fail_closed.state(), InteractionState::disabled());

    let invalid_loading = InteractionState::loading()
        .transition(InteractionTransition::Hover)
        .expect_err("loading controls cannot become hovered");
    assert!(invalid_loading.to_string().contains("loading"));
}

#[test]
fn pointer_capture_and_keyboard_activation_reject_stale_focus_epochs() {
    let mut model = InteractionStateModel::default();
    model.set_focus_epoch(10);
    assert!(model.pointer_down(42, 10));
    model.set_focus_epoch(11);
    assert!(!model.pointer_up(42, 10));
    assert!(!model.state().pressed);

    assert!(model.key_activate(KeyboardKey::Enter, 11));
    assert!(model.key_activate(KeyboardKey::Space, 11));
    assert!(!model.key_activate(KeyboardKey::Escape, 11));
    model.set_disabled(true);
    assert!(!model.key_activate(KeyboardKey::Enter, 11));
}

#[test]
fn button_requires_accessible_metadata_and_never_dispatches_stale_or_blocked_input() {
    let (events, callback) = recording_callback();
    let mut button = Button::new("Save changes", action_id("task.save"), callback)
        .expect("button should be constructible");

    assert_eq!(button.accessibility().role, AccessibleRole::Button);
    assert_eq!(button.accessibility().name, "Save changes");
    assert!(!button.accessibility().disabled);
    assert_eq!(button.variant(), ButtonVariant::Primary);

    button.set_focus_epoch(4);
    assert!(button.pointer_down(7, 4));
    assert!(button.pointer_up(7, 4));
    assert_eq!(events.lock().expect("event sink lock").len(), 1);

    button.set_focus_epoch(5);
    assert!(button.pointer_down(8, 5));
    button.set_focus_epoch(6);
    assert!(!button.pointer_up(8, 5));
    assert_eq!(events.lock().expect("event sink lock").len(), 1);

    button
        .disable("saving is already in progress")
        .expect("reason is bounded");
    assert_eq!(
        button.disabled_reason(),
        Some("saving is already in progress")
    );
    assert_eq!(
        button.accessibility().description,
        "saving is already in progress"
    );
    assert!(!button.key_activate(KeyboardKey::Enter, 6));
    assert_eq!(events.lock().expect("event sink lock").len(), 1);

    button.enable().expect("button can be enabled");
    button.set_loading(true).expect("button can enter loading");
    assert!(!button.key_activate(KeyboardKey::Space, 6));
    assert_eq!(events.lock().expect("event sink lock").len(), 1);
}

#[test]
fn focused_button_exposes_a_visible_semantic_token_focus_ring() {
    let (_events, callback) = recording_callback();
    let mut button = Button::new("Open", action_id("task.open"), callback).expect("button");
    button.focus();
    let tokens = theme(ThemeMode::Dark, Density::Comfortable, Scale::Scale100);
    let style = button.presentation(tokens);
    let ring = style
        .focus_ring
        .expect("focused controls need a focus ring");
    assert_eq!(ring.color, tokens.borders.focus);
    assert!(ring.width > 0);
    assert!(ring.offset > 0);
}

#[test]
fn icon_button_requires_nonempty_accessible_label_and_tooltip_contract() {
    let (_events, callback) = recording_callback();
    let tooltip = TooltipContract::new("Open task details", 500).expect("tooltip");
    let icon_button = IconButton::new(
        "open-in-new",
        "Open task details",
        tooltip,
        action_id("task.open"),
        callback,
    )
    .expect("icon button");

    assert_eq!(icon_button.accessibility().role, AccessibleRole::Button);
    assert_eq!(icon_button.accessibility().name, "Open task details");
    assert_eq!(icon_button.tooltip().label, "Open task details");
    assert_eq!(icon_button.tooltip().delay_ms, 500);
}

#[test]
fn badges_and_status_lights_are_noninteractive_and_use_semantic_status_signals() {
    let badge = Badge::new(
        "Running",
        Some("The task is executing"),
        StatusMeaning::External,
    )
    .expect("badge");
    let light = StatusLight::new(StatusMeaning::Success, "Healthy", "The host is responding")
        .expect("status light");
    let tokens = theme(ThemeMode::Light, Density::Compact, Scale::Scale125);

    assert_eq!(badge.accessibility().role, AccessibleRole::Status);
    assert!(!badge.is_interactive());
    assert_eq!(badge.presentation(tokens).indicator, tokens.status.external);
    assert_eq!(light.accessibility().role, AccessibleRole::Status);
    assert!(!light.is_interactive());
    assert_eq!(light.presentation(tokens).indicator, tokens.status.success);
    assert_eq!(light.presentation(tokens).meaning, StatusMeaning::Success);
    assert!(!light.accessibility().description.is_empty());
}

#[test]
fn text_field_enforces_scalar_and_utf8_byte_bounds_and_keeps_paste_as_data() {
    let limits = TextFieldLimits::new(4, 8).expect("valid limits");
    let mut field = TextField::with_limits("Command text", limits).expect("field");
    field
        .set_description("Text is sent only after explicit user submission")
        .expect("description");
    assert!(!field
        .handle_key(TextFieldKey::Character('x'))
        .expect("unfocused input is ignored"));
    field.focus();
    assert!(field.handle_key(TextFieldKey::Character('界')).is_ok());
    assert!(field.paste("🙂").is_ok());
    assert_eq!(field.value(), "界🙂");
    assert_eq!(field.value().chars().count(), 2);
    assert_eq!(field.value().len(), "界🙂".len());
    assert_eq!(field.accessibility().role, AccessibleRole::TextField);
    assert_eq!(field.accessibility().name, "Command text");
    assert!(field.accessibility().description.contains("explicit"));
    field
        .set_error(Some("Use a safe task command"))
        .expect("error is bounded and accessible");
    assert!(field.accessibility().invalid);
    assert_eq!(
        field.accessibility().error.as_deref(),
        Some("Use a safe task command")
    );

    let error = field
        .set_value("12345")
        .expect_err("scalar bound must reject overflow");
    assert!(matches!(error, TextFieldError::ScalarLimitExceeded { .. }));
    let error = field
        .set_value("😀😀😀")
        .expect_err("byte bound must reject UTF-8 overflow");
    assert!(matches!(error, TextFieldError::ByteLimitExceeded { .. }));
    assert_eq!(field.value(), "界🙂");

    field.set_read_only(true);
    assert!(field.accessibility().read_only);
    assert!(!field
        .handle_key(TextFieldKey::Character('x'))
        .expect("read-only input is safe"));
    field.set_read_only(false);
    field.set_disabled(true);
    assert!(field.accessibility().disabled);
    assert!(!field.focus());
    assert!(!field
        .paste("never-execute-this")
        .expect("disabled paste is ignored"));
}

#[test]
fn empty_and_error_states_expose_only_explicit_typed_recovery_actions() {
    let (events, callback) = recording_callback();
    let action = RecoveryAction::new("Retry", action_id("task.retry"), callback).expect("action");
    let empty = EmptyState::new("No tasks", "Create a task to get started")
        .expect("empty state")
        .with_recovery_action(action)
        .expect("recovery action");
    assert_eq!(empty.accessibility().role, AccessibleRole::Region);
    assert_eq!(empty.recovery_actions().len(), 1);
    assert_eq!(
        empty.recovery_actions()[0].action_id().as_str(),
        "task.retry"
    );
    assert!(empty.activate_recovery(0, 0));
    assert_eq!(events.lock().expect("event sink lock").len(), 1);
    assert!(!empty.rendered_payload().contains("debug"));

    let (_events, callback) = recording_callback();
    let error = ErrorBoundary::new("Could not load tasks", "Try again or check the connection")
        .expect("error boundary")
        .with_recovery_action(
            RecoveryAction::new("Try again", action_id("task.retry"), callback).expect("action"),
        )
        .expect("recovery action");
    assert_eq!(error.accessibility().role, AccessibleRole::Alert);
    assert!(error.accessibility().invalid);
    assert_eq!(error.recovery_actions().len(), 1);
    assert!(!error.rendered_payload().contains("provider"));
}

#[test]
fn accessibility_metadata_is_bounded_and_rejects_blank_names() {
    let error = AccessibilityMetadata::new(AccessibleRole::Button, "   ")
        .expect_err("blank accessible names are not useful");
    assert!(error.to_string().contains("name"));

    let long_name = "x".repeat(257);
    let error = AccessibilityMetadata::new(AccessibleRole::Button, long_name)
        .expect_err("accessible names are bounded");
    assert!(error.to_string().contains("256"));
}

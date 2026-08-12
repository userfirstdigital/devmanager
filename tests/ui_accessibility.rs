use devmanager::client::action::ActionRequest;
use devmanager::ui::components::badge::Badge;
use devmanager::ui::components::button::{Button, ButtonVariant};
use devmanager::ui::components::empty_state::{EmptyState, RecoveryAction};
use devmanager::ui::components::error_boundary::{
    ErrorBoundary, SafeErrorCode, SafeErrorProjection,
};
use devmanager::ui::components::icon_button::{IconButton, TooltipContract};
use devmanager::ui::components::interaction::{
    AccessibilityMetadata, AccessibleRole, ActionEvent, ActionId, ActivationSource, ComponentError,
    FocusEpochSource, InteractionState, InteractionStateModel, InteractionTransition, KeyboardKey,
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
    let mut source = FocusEpochSource::new();
    let first = source.current();
    let mut model = InteractionStateModel::default();
    model.set_focus_epoch(first);
    assert!(model.pointer_down(42, first));
    let second = source.advance();
    model.set_focus_epoch(second);
    assert!(!model.pointer_up(42, first));
    assert!(!model.state().pressed);

    assert!(!model.key_activate(KeyboardKey::Enter, second));
    assert!(model.focus());
    assert!(model.key_activate(KeyboardKey::Enter, second));
    assert!(model.key_activate(KeyboardKey::Space, second));
    assert!(!model.key_activate(KeyboardKey::Escape, second));
    model.set_disabled(true);
    assert!(!model.key_activate(KeyboardKey::Enter, second));
}

#[test]
fn button_requires_accessible_metadata_and_never_dispatches_stale_or_blocked_input() {
    let mut button = Button::new("Save changes", ActionRequest::TaskList)
        .expect("button should be constructible");

    assert_eq!(button.accessibility().role, AccessibleRole::Button);
    assert_eq!(button.accessibility().name, "Save changes");
    assert!(!button.accessibility().disabled);
    assert_eq!(button.variant(), ButtonVariant::Primary);

    let mut source = FocusEpochSource::new();
    let first = source.current();
    button.set_focus_epoch(first);
    assert!(button.pointer_down(7, first));
    let event = button.pointer_up(7, first).expect("pointer activation");
    assert_eq!(event.request, ActionRequest::TaskList);

    let second = source.advance();
    button.set_focus_epoch(second);
    assert!(button.pointer_down(8, second));
    let third = source.advance();
    button.set_focus_epoch(third);
    assert!(button.pointer_up(8, second).is_none());

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
    assert!(button.key_activate(KeyboardKey::Enter, third).is_none());

    button.enable().expect("button can be enabled");
    button.set_loading(true).expect("button can enter loading");
    assert!(button.key_activate(KeyboardKey::Space, third).is_none());
}

#[test]
fn focused_button_exposes_a_visible_semantic_token_focus_ring() {
    let mut button = Button::new("Open", ActionRequest::TaskList).expect("button");
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
    let tooltip = TooltipContract::new("Open task details", 500).expect("tooltip");
    let icon_button = IconButton::new(
        "open-in-new",
        "Open task details",
        tooltip,
        ActionRequest::TaskList,
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
        .handle_key(TextFieldKey::Character('x'), field.focus_epoch())
        .expect("unfocused input is ignored"));
    field.focus();
    assert!(field
        .handle_key(TextFieldKey::Character('界'), field.focus_epoch())
        .is_ok());
    assert!(field.paste("🙂", field.focus_epoch()).is_ok());
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
        .handle_key(TextFieldKey::Character('x'), field.focus_epoch())
        .expect("read-only input is safe"));
    field.set_read_only(false);
    field.set_disabled(true);
    assert!(field.accessibility().disabled);
    assert!(!field.focus());
    assert!(!field
        .paste("never-execute-this", field.focus_epoch())
        .expect("disabled paste is ignored"));
}

#[test]
fn empty_and_error_states_expose_only_explicit_typed_recovery_actions() {
    let source = FocusEpochSource::new();
    let epoch = source.current();
    let action = RecoveryAction::new("Retry", ActionRequest::TaskList).expect("action");
    let mut empty = EmptyState::new("No tasks", "Create a task to get started")
        .expect("empty state")
        .with_recovery_action(action)
        .expect("recovery action");
    assert_eq!(empty.accessibility().role, AccessibleRole::Region);
    assert_eq!(empty.recovery_actions().len(), 1);
    assert_eq!(
        empty.recovery_actions()[0].action_request(),
        &ActionRequest::TaskList
    );
    assert!(empty.activate_recovery(0, epoch).is_none());
    empty.set_focus_epoch(epoch);
    assert!(empty.focus_recovery(0));
    let keyboard_event = empty
        .key_activate_recovery(0, KeyboardKey::Enter, epoch)
        .expect("focused recovery action accepts Enter");
    assert_eq!(keyboard_event.request, ActionRequest::TaskList);
    assert_eq!(keyboard_event.focus_epoch(), epoch);
    assert!(matches!(
        keyboard_event.source,
        ActivationSource::Keyboard {
            key: KeyboardKey::Enter
        }
    ));
    assert!(empty.pointer_down_recovery(0, 7, epoch));
    let pointer_event = empty
        .pointer_up_recovery(0, 7, epoch)
        .expect("matching recovery pointer release activates");
    assert_eq!(pointer_event.request, ActionRequest::TaskList);
    assert!(matches!(
        pointer_event.source,
        ActivationSource::Pointer { pointer_id: 7 }
    ));
    assert!(!empty.rendered_payload().contains("debug"));

    let mut error = ErrorBoundary::new(
        SafeErrorProjection::new(
            SafeErrorCode::HostUnavailable,
            "Could not load tasks",
            "Try again or check the connection",
        )
        .expect("safe projection"),
    )
    .expect("error boundary")
    .with_recovery_action(
        RecoveryAction::new("Try again", ActionRequest::TaskList).expect("action"),
    )
    .expect("recovery action");
    assert_eq!(error.accessibility().role, AccessibleRole::Alert);
    assert!(error.accessibility().invalid);
    assert_eq!(error.recovery_actions().len(), 1);
    error.set_focus_epoch(epoch);
    assert!(error.focus_recovery(0));
    let delegated = error
        .key_activate_recovery(0, KeyboardKey::Space, epoch)
        .expect("error recovery action delegates keyboard activation");
    assert_eq!(delegated.request, ActionRequest::TaskList);
    assert!(error.pointer_down_recovery(0, 9, epoch));
    assert!(error.pointer_up_recovery(0, 9, epoch).is_some());
    error.blur_recovery(0);
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

#[test]
fn presentational_button_emits_only_a_typed_catalog_request() {
    let mut button = Button::new("List tasks", ActionRequest::TaskList).expect("button");
    let source = FocusEpochSource::new();
    let epoch = source.current();
    button.set_focus_epoch(epoch);
    button.focus();

    let event = button
        .key_activate(KeyboardKey::Enter, epoch)
        .expect("focused button should emit an event");
    assert_eq!(event.request, ActionRequest::TaskList);
    assert_eq!(event.request.id(), "task.list");
}

#[test]
fn keyboard_activation_requires_current_focus_and_epoch_changes_clear_focus() {
    let mut source = FocusEpochSource::new();
    let first = source.current();
    let second = source.advance();
    let mut model = InteractionStateModel::default();
    model.set_focus_epoch(first);
    assert!(!model.key_activate(KeyboardKey::Enter, first));

    assert!(model.focus());
    assert!(model.key_activate(KeyboardKey::Enter, first));

    model.set_focus_epoch(second);
    assert!(!model.state().focused);
    assert!(!model.key_activate(KeyboardKey::Space, second));

    assert!(model.focus());
    assert!(!model.set_focus_epoch(first));
    assert_eq!(
        model.focus_epoch(),
        second,
        "stale host epochs must be rejected"
    );
    assert!(
        model.state().focused,
        "rejected epochs must not clear focus"
    );
    assert!(model.key_activate(KeyboardKey::Enter, second));
    assert!(matches!(
        model.try_set_focus_epoch(first),
        Err(ComponentError::StaleFocusEpoch { .. })
    ));
}

#[test]
fn text_field_limits_reject_oversized_public_limits() {
    assert!(TextFieldLimits::new(4_097, 16_384).is_err());
    assert!(TextFieldLimits::new(4_096, 16_385).is_err());
    assert!(TextField::with_limits(
        "Prompt",
        TextFieldLimits {
            max_scalars: 4_097,
            max_bytes: 16_384,
        },
    )
    .is_err());
}

#[test]
fn text_field_keyboard_input_requires_the_current_focus_epoch() {
    let mut source = FocusEpochSource::new();
    let stale = source.current();
    let current = source.advance();
    let mut field = TextField::new("Prompt").expect("field");
    field.set_focus_epoch(current);
    assert!(field.focus());

    assert!(!field
        .handle_key(TextFieldKey::Character('x'), stale)
        .expect("stale keyboard input is ignored"));
    assert!(field
        .handle_key(TextFieldKey::Character('x'), current)
        .expect("current keyboard input is accepted"));
    assert_eq!(field.value(), "x");
}

#[test]
fn paste_requires_current_focus_and_preflights_both_limits() {
    let mut source = FocusEpochSource::new();
    let first = source.current();
    let second = source.advance();
    let limits = TextFieldLimits::new(3, 5).expect("valid limits");
    let mut field = TextField::with_limits("Prompt", limits).expect("field");

    assert!(!field.paste("x", first).expect("unfocused paste is ignored"));
    field.set_focus_epoch(first);
    field.focus();
    assert!(field.paste("界", first).expect("focused paste is accepted"));

    field.set_focus_epoch(second);
    assert!(!field.paste("x", first).expect("stale paste is ignored"));
    assert_eq!(field.value(), "界");

    field.focus();
    let scalar_error = field
        .paste("xyz", second)
        .expect_err("scalar bound must be checked before insertion");
    assert!(matches!(
        scalar_error,
        TextFieldError::ScalarLimitExceeded { .. }
    ));
    assert_eq!(field.value(), "界");

    let byte_error = field
        .paste("🙂", second)
        .expect_err("byte bound must be checked before insertion");
    assert!(matches!(
        byte_error,
        TextFieldError::ByteLimitExceeded { .. }
    ));
    assert_eq!(field.value(), "界");
}

#[test]
fn error_boundary_accepts_only_safe_redacted_projection() {
    const SECRET_SENTINEL: &str = "UI_ERROR_BOUNDARY_SECRET_SENTINEL";
    let projection = SafeErrorProjection::new(
        SafeErrorCode::RendererFailure,
        "Could not render task",
        format!("token={SECRET_SENTINEL}"),
    )
    .expect("safe projection");
    let error = ErrorBoundary::new(projection).expect("error boundary");

    assert!(!error.rendered_payload().contains(SECRET_SENTINEL));
    assert!(error.rendered_payload().contains("***"));
}

#[test]
fn every_renderable_error_and_recovery_label_is_bounded_and_redacted() {
    const API_KEY: &str = "UI_API_KEY_SENTINEL";
    const AWS_ACCESS: &str = "UI_AWS_ACCESS_SENTINEL";
    const AWS_SECRET: &str = "UI_AWS_SECRET_SENTINEL";
    const BASIC: &str = "UI_BASIC_AUTH_SENTINEL";
    const BEARER: &str = "UI_BEARER_AUTH_SENTINEL";

    let empty = EmptyState::new(
        format!("AWS_ACCESS_KEY_ID={AWS_ACCESS}"),
        format!("api_key={API_KEY}"),
    )
    .expect("empty state labels are bounded")
    .with_recovery_action(
        RecoveryAction::new(
            format!("Authorization: Basic {BASIC}"),
            ActionRequest::TaskList,
        )
        .expect("recovery label is bounded"),
    )
    .expect("recovery action");
    let empty_payload = empty.rendered_payload();

    let error = ErrorBoundary::new(
        SafeErrorProjection::new(
            SafeErrorCode::RendererFailure,
            format!("AWS_SECRET_ACCESS_KEY={AWS_SECRET}"),
            format!("Authorization: Bearer {BEARER}"),
        )
        .expect("error labels are bounded"),
    )
    .expect("error boundary")
    .with_recovery_action(
        RecoveryAction::new(format!("api_key={API_KEY}"), ActionRequest::TaskList)
            .expect("recovery label is bounded"),
    )
    .expect("recovery action");
    let error_payload = error.rendered_payload();

    for secret in [API_KEY, AWS_ACCESS, AWS_SECRET, BASIC, BEARER] {
        assert!(
            !empty_payload.contains(secret),
            "empty payload leaked {secret}"
        );
        assert!(
            !error_payload.contains(secret),
            "error payload leaked {secret}"
        );
    }
    assert!(empty_payload.contains("***") || empty_payload.contains("[redacted]"));
    assert!(error_payload.contains("***") || error_payload.contains("[redacted]"));

    let mut metadata =
        AccessibilityMetadata::new(AccessibleRole::Alert, "Error").expect("metadata");
    metadata
        .set_error(Some(format!("api_key={API_KEY}")))
        .expect("metadata error is bounded");
    assert!(!metadata.error.as_deref().unwrap().contains(API_KEY));
}

#[test]
fn focus_tokens_are_minted_by_one_monotonic_source_and_stale_tokens_cannot_activate() {
    let mut source = FocusEpochSource::new();
    let first = source.current();
    let mut model = InteractionStateModel::default();

    assert!(model.set_focus_epoch(first));
    assert!(model.focus());
    assert!(model.key_activate(KeyboardKey::Enter, first));

    let second = source.advance();
    assert_ne!(first, second);
    assert!(model.set_focus_epoch(second));
    assert!(!model.key_activate(KeyboardKey::Enter, first));
    assert!(!model.state().focused);

    let mut foreign_source = FocusEpochSource::new();
    let foreign = foreign_source.advance();
    assert!(!model.set_focus_epoch(foreign));
}

#[test]
fn error_label_redaction_normalizes_key_separators_without_redacting_unrelated_prose() {
    const ACCESS: &str = "UI_ACCESS_KEY_ID_VARIANT_SENTINEL";
    const SECRET: &str = "UI_SECRET_ACCESS_KEY_VARIANT_SENTINEL";
    const CREDENTIAL_EQUALS: &str = "UI_CREDENTIAL_EQUALS_VARIANT_SENTINEL";
    const CREDENTIAL_COLON: &str = "UI_CREDENTIAL_COLON_VARIANT_SENTINEL";
    const BASIC: &str = "UI_AUTH_BASIC_VARIANT_SENTINEL";
    const BEARER: &str = "UI_AUTH_BEARER_VARIANT_SENTINEL";

    let mut metadata =
        AccessibilityMetadata::new(AccessibleRole::Alert, "Error").expect("metadata");
    metadata
        .set_error(Some(format!(
            "AccessKeyId : {ACCESS}\nsecret-access-key={SECRET}\ncredential= {CREDENTIAL_EQUALS}\ncredential : {CREDENTIAL_COLON}\nAUTHORIZATION : Basic {BASIC}\nauthorization : bearer {BEARER}\nThe AccessKeyId field is documented here."
        )))
        .expect("error label is bounded and redacted");

    let rendered = metadata.error.expect("error label");
    for secret in [
        ACCESS,
        SECRET,
        CREDENTIAL_EQUALS,
        CREDENTIAL_COLON,
        BASIC,
        BEARER,
    ] {
        assert!(!rendered.contains(secret), "rendered error leaked {secret}");
    }
    assert!(rendered.contains("The AccessKeyId field is documented here."));
}

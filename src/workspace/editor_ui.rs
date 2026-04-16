use super::{EditorActions, EditorField, EditorPaneModel, EditorPanel};
use crate::{terminal, theme};
use gpui::{
    canvas, div, fill, point, px, rgb, size, AnyElement, App, Bounds, CursorStyle, DispatchPhase,
    Hitbox, HitboxBehavior, Hsla, InteractiveElement, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, ParentElement, Pixels, SharedString, StatefulInteractiveElement, Styled,
    TextRun, Window,
};

pub(super) type ClickHandler = Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>;

const EDITOR_FIELD_FONT_SIZE_PX: f32 = 12.0;
const EDITOR_FIELD_LINE_HEIGHT_PX: f32 = 18.0;
const EDITOR_FIELD_CURSOR_WIDTH_PX: f32 = 2.0;

#[derive(Clone)]
struct EditableFieldLine {
    display_text: SharedString,
    editable_text: String,
    char_start: usize,
    cursor_col: Option<usize>,
    selection_cols: Option<(usize, usize)>,
    placeholder: bool,
}

struct EditableFieldLinePaintState {
    hitbox: Hitbox,
    shaped_line: gpui::ShapedLine,
    editable_text: String,
    char_start: usize,
    cursor_x: Option<Pixels>,
    selection_x: Option<(Pixels, Pixels)>,
}

#[derive(Clone, Copy)]
pub(super) enum SurfaceActionButtonStyle {
    Primary,
    Danger,
    Ghost,
}

#[derive(Clone, Copy)]
pub(super) enum SurfaceTone {
    Accent,
    Muted,
    Success,
    Warning,
    Danger,
}

#[derive(Clone)]
pub(super) struct SurfaceBadge {
    pub label: String,
    pub tone: SurfaceTone,
}

impl SurfaceBadge {
    pub fn new(label: impl Into<String>, tone: SurfaceTone) -> Self {
        Self {
            label: label.into(),
            tone,
        }
    }
}

pub(super) struct FormSection {
    pub title: String,
    pub hint: Option<String>,
    pub fields: Vec<FormField>,
}

impl FormSection {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            hint: None,
            fields: Vec::new(),
        }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn field(mut self, field: FormField) -> Self {
        self.fields.push(field);
        self
    }

    pub fn fields(mut self, fields: Vec<FormField>) -> Self {
        self.fields.extend(fields);
        self
    }
}

pub(super) enum FormField {
    Text(FormTextField),
    Multiline(FormTextField),
    Choice(FormChoiceField),
    Toggle(FormToggleField),
    Info(FormInfoField),
    Notice(FormNotice),
    Action(FormAction),
    ActionGroup(FormActionGroup),
    SelectionList(FormSelectionList),
    EmptyState(FormEmptyState),
    Custom(AnyElement),
}

impl FormField {
    pub fn text(
        label: impl Into<String>,
        hint: impl Into<String>,
        value: impl Into<String>,
        field: EditorField,
    ) -> Self {
        Self::Text(FormTextField::new(label, hint, value, field))
    }

    pub fn multiline(
        label: impl Into<String>,
        hint: impl Into<String>,
        value: impl Into<String>,
        field: EditorField,
    ) -> Self {
        Self::Multiline(FormTextField::new(label, hint, value, field))
    }

    pub fn multiline_sized(
        label: impl Into<String>,
        hint: impl Into<String>,
        value: impl Into<String>,
        field: EditorField,
        height: f32,
    ) -> Self {
        Self::Multiline(FormTextField::new(label, hint, value, field).height(height))
    }

    pub fn choice(
        label: impl Into<String>,
        value: impl Into<String>,
        hint: Option<String>,
        on_click: ClickHandler,
    ) -> Self {
        Self::Choice(FormChoiceField::new(label, value, hint, on_click))
    }

    pub fn toggle(
        label: impl Into<String>,
        value: bool,
        hint: impl Into<String>,
        on_click: ClickHandler,
    ) -> Self {
        Self::Toggle(FormToggleField::new(label, value, hint, on_click))
    }

    pub fn info(label: impl Into<String>, value: impl Into<String>, hint: Option<String>) -> Self {
        Self::Info(FormInfoField::new(label, value, hint))
    }

    pub fn notice(message: impl Into<String>, tone: SurfaceTone) -> Self {
        Self::Notice(FormNotice::new(message, tone))
    }

    pub fn action(action: FormAction) -> Self {
        Self::Action(action)
    }

    pub fn action_group(group: FormActionGroup) -> Self {
        Self::ActionGroup(group)
    }

    pub fn selection_list(list: FormSelectionList) -> Self {
        Self::SelectionList(list)
    }

    pub fn empty_state(
        title: impl Into<String>,
        detail: impl Into<String>,
        tone: SurfaceTone,
    ) -> Self {
        Self::EmptyState(FormEmptyState::new(title, detail, tone))
    }

    pub fn custom(element: AnyElement) -> Self {
        Self::Custom(element)
    }
}

pub(super) struct FormTextField {
    pub label: String,
    pub hint: String,
    pub value: String,
    pub field: EditorField,
    pub height: Option<f32>,
}

impl FormTextField {
    pub fn new(
        label: impl Into<String>,
        hint: impl Into<String>,
        value: impl Into<String>,
        field: EditorField,
    ) -> Self {
        Self {
            label: label.into(),
            hint: hint.into(),
            value: value.into(),
            field,
            height: None,
        }
    }

    pub fn height(mut self, height: f32) -> Self {
        self.height = Some(height);
        self
    }
}

pub(super) struct FormChoiceField {
    pub label: String,
    pub value: String,
    pub hint: Option<String>,
    pub on_click: ClickHandler,
}

impl FormChoiceField {
    pub fn new(
        label: impl Into<String>,
        value: impl Into<String>,
        hint: Option<String>,
        on_click: ClickHandler,
    ) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            hint,
            on_click,
        }
    }
}

pub(super) struct FormToggleField {
    pub label: String,
    pub value: bool,
    pub hint: String,
    pub on_click: ClickHandler,
}

impl FormToggleField {
    pub fn new(
        label: impl Into<String>,
        value: bool,
        hint: impl Into<String>,
        on_click: ClickHandler,
    ) -> Self {
        Self {
            label: label.into(),
            value,
            hint: hint.into(),
            on_click,
        }
    }
}

pub(super) struct FormInfoField {
    pub label: String,
    pub value: String,
    pub hint: Option<String>,
    pub badge: Option<SurfaceBadge>,
    pub actions: Vec<FormAction>,
}

impl FormInfoField {
    pub fn new(label: impl Into<String>, value: impl Into<String>, hint: Option<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            hint,
            badge: None,
            actions: Vec::new(),
        }
    }

    pub fn badge(mut self, badge: SurfaceBadge) -> Self {
        self.badge = Some(badge);
        self
    }

    pub fn action(mut self, action: FormAction) -> Self {
        self.actions.push(action);
        self
    }
}

pub(super) struct FormNotice {
    pub message: String,
    pub tone: SurfaceTone,
}

impl FormNotice {
    pub fn new(message: impl Into<String>, tone: SurfaceTone) -> Self {
        Self {
            message: message.into(),
            tone,
        }
    }
}

pub(super) struct FormAction {
    pub title: String,
    pub value: String,
    pub description: Option<String>,
    pub badge: Option<SurfaceBadge>,
    pub style: SurfaceActionButtonStyle,
    pub on_click: ClickHandler,
}

impl FormAction {
    pub fn new(title: impl Into<String>, value: impl Into<String>, on_click: ClickHandler) -> Self {
        Self {
            title: title.into(),
            value: value.into(),
            description: None,
            badge: None,
            style: SurfaceActionButtonStyle::Ghost,
            on_click,
        }
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn badge(mut self, badge: SurfaceBadge) -> Self {
        self.badge = Some(badge);
        self
    }

    pub fn style(mut self, style: SurfaceActionButtonStyle) -> Self {
        self.style = style;
        self
    }
}

pub(super) struct FormActionGroup {
    pub title: Option<String>,
    pub hint: Option<String>,
    pub actions: Vec<FormAction>,
}

impl FormActionGroup {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: Some(title.into()),
            hint: None,
            actions: Vec::new(),
        }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn action(mut self, action: FormAction) -> Self {
        self.actions.push(action);
        self
    }
}

pub(super) struct FormSelectionList {
    pub title: Option<String>,
    pub hint: Option<String>,
    pub rows: Vec<FormSelectionRow>,
}

impl FormSelectionList {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: Some(title.into()),
            hint: None,
            rows: Vec::new(),
        }
    }

    pub fn untitled() -> Self {
        Self {
            title: None,
            hint: None,
            rows: Vec::new(),
        }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn row(mut self, row: FormSelectionRow) -> Self {
        self.rows.push(row);
        self
    }
}

pub(super) struct FormSelectionRow {
    pub label: String,
    pub detail: Option<String>,
    pub selected: bool,
    pub on_click: ClickHandler,
}

impl FormSelectionRow {
    pub fn new(
        label: impl Into<String>,
        detail: Option<String>,
        selected: bool,
        on_click: ClickHandler,
    ) -> Self {
        Self {
            label: label.into(),
            detail,
            selected,
            on_click,
        }
    }
}

pub(super) struct FormEmptyState {
    pub title: String,
    pub detail: String,
    pub tone: SurfaceTone,
}

impl FormEmptyState {
    pub fn new(title: impl Into<String>, detail: impl Into<String>, tone: SurfaceTone) -> Self {
        Self {
            title: title.into(),
            detail: detail.into(),
            tone,
        }
    }
}

pub(super) struct PreviewStory {
    pub title: String,
    pub description: String,
    pub states: Vec<PreviewState>,
}

impl PreviewStory {
    pub fn new(title: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            description: description.into(),
            states: Vec::new(),
        }
    }

    pub fn state(mut self, state: PreviewState) -> Self {
        self.states.push(state);
        self
    }
}

pub(super) struct PreviewState {
    pub label: String,
    pub note: Option<String>,
    pub badges: Vec<SurfaceBadge>,
    pub body: AnyElement,
}

impl PreviewState {
    pub fn new(label: impl Into<String>, body: AnyElement) -> Self {
        Self {
            label: label.into(),
            note: None,
            badges: Vec::new(),
            body,
        }
    }

    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    pub fn badge(mut self, badge: SurfaceBadge) -> Self {
        self.badges.push(badge);
        self
    }
}

pub(super) fn render_editor_toolbar(
    title: &str,
    subtitle: &str,
    accent: u32,
    save_label: Option<&str>,
    on_save: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    on_delete: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    on_close: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    div()
        .flex_none()
        .px(px(12.0))
        .py(px(10.0))
        .bg(rgb(theme::TOPBAR_BG))
        .border_b_1()
        .border_color(rgb(theme::BORDER_PRIMARY))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(16.0))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .gap(px(12.0))
                        .child(div().size(px(10.0)).rounded_full().bg(rgb(accent)))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(2.0))
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(rgb(theme::TEXT_PRIMARY))
                                        .child(SharedString::from(title.to_string())),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(theme::TEXT_SUBTLE))
                                        .child(SharedString::from(subtitle.to_string())),
                                ),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .children(save_label.zip(on_save).map(|(save_label, on_save)| {
                            render_surface_action_button(
                                save_label,
                                SurfaceActionButtonStyle::Primary,
                                on_save,
                            )
                            .into_any_element()
                        }))
                        .children(on_delete.map(|on_delete| {
                            render_surface_action_button(
                                "Delete",
                                SurfaceActionButtonStyle::Danger,
                                on_delete,
                            )
                            .into_any_element()
                        }))
                        .child(render_surface_action_button(
                            "Close",
                            SurfaceActionButtonStyle::Ghost,
                            on_close,
                        )),
                ),
        )
}

pub(super) fn render_surface_action_button(
    label: &str,
    style: SurfaceActionButtonStyle,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    let (bg, border, text, hover_bg) = surface_action_button_colors(style);

    div()
        .px(px(12.0))
        .py(px(6.0))
        .rounded_sm()
        .bg(rgb(bg))
        .border_1()
        .border_color(rgb(border))
        .text_xs()
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(rgb(text))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(hover_bg)))
        .child(SharedString::from(label.to_string()))
        .on_mouse_down(MouseButton::Left, on_click)
}

fn render_compact_surface_action_button(
    label: &str,
    style: SurfaceActionButtonStyle,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    let (bg, border, text, hover_bg) = surface_action_button_colors(style);

    div()
        .px(px(9.0))
        .py(px(4.0))
        .rounded_sm()
        .bg(rgb(bg))
        .border_1()
        .border_color(rgb(border))
        .text_xs()
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(rgb(text))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(hover_bg)))
        .child(SharedString::from(label.to_string()))
        .on_mouse_down(MouseButton::Left, on_click)
}

fn surface_action_button_colors(style: SurfaceActionButtonStyle) -> (u32, u32, u32, u32) {
    match style {
        SurfaceActionButtonStyle::Primary => (
            theme::PRIMARY,
            theme::PRIMARY,
            theme::SELECTION_TEXT,
            theme::PRIMARY_HOVER,
        ),
        SurfaceActionButtonStyle::Danger => (
            theme::EDITOR_CARD_BG,
            0x5a2630,
            theme::DANGER_TEXT,
            0x382029,
        ),
        SurfaceActionButtonStyle::Ghost => (
            theme::EDITOR_CARD_BG,
            theme::BORDER_SECONDARY,
            theme::TEXT_MUTED,
            theme::ROW_HOVER_BG,
        ),
    }
}

pub(super) fn render_display_field(
    label: &str,
    hint: &str,
    value: &str,
    placeholder: &str,
    focused: bool,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    let display_value = if value.is_empty() && !focused {
        placeholder.to_string()
    } else {
        value.to_string()
    };

    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(12.0))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .child(
                            div()
                                .text_xs()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(rgb(theme::TEXT_PRIMARY))
                                .child(SharedString::from(label.to_string())),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_SUBTLE))
                                .child(SharedString::from(hint.to_string())),
                        ),
                )
                .children(focused.then(|| {
                    render_inline_state_badge("Editing", theme::PRIMARY).into_any_element()
                })),
        )
        .child(
            div()
                .px(px(12.0))
                .py(px(10.0))
                .rounded_md()
                .bg(rgb(theme::EDITOR_FIELD_BG))
                .border_1()
                .border_color(rgb(if focused {
                    theme::PRIMARY
                } else {
                    theme::BORDER_PRIMARY
                }))
                .text_sm()
                .text_color(rgb(if value.is_empty() && !focused {
                    theme::TEXT_DIM
                } else {
                    theme::TEXT_PRIMARY
                }))
                .cursor_text()
                .child(SharedString::from(display_value))
                .on_mouse_down(MouseButton::Left, on_click),
        )
}

pub(super) fn render_editor_intro(panel: &EditorPanel) -> impl IntoElement {
    let accent = panel.accent_color();
    let summary_items = panel.summary_items();

    div()
        .flex()
        .flex_col()
        .gap(px(14.0))
        .p(px(18.0))
        .rounded_md()
        .bg(rgb(theme::EDITOR_CARD_BG))
        .border_1()
        .border_color(rgb(theme::BORDER_PRIMARY))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(div().size(px(8.0)).rounded_full().bg(rgb(accent)))
                .child(
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(SharedString::from(panel.title().to_string())),
                ),
        )
        .child(
            div()
                .text_sm()
                .font_weight(gpui::FontWeight::BOLD)
                .text_color(rgb(theme::TEXT_PRIMARY))
                .child(SharedString::from(panel.headline())),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(panel.subtitle().to_string())),
        )
        .children(panel.context_line().map(|context| {
            div()
                .px(px(12.0))
                .py(px(10.0))
                .rounded_md()
                .bg(rgb(theme::EDITOR_FIELD_BG))
                .border_1()
                .border_color(rgb(theme::BORDER_SECONDARY))
                .text_xs()
                .text_color(rgb(theme::TEXT_PRIMARY))
                .child(SharedString::from(context))
                .into_any_element()
        }))
        .children((!summary_items.is_empty()).then(|| {
            div()
                .flex()
                .gap(px(10.0))
                .children(summary_items.into_iter().map(|(label, value)| {
                    render_editor_summary_item(label, value).into_any_element()
                }))
                .into_any_element()
        }))
}

fn render_editor_summary_item(label: String, value: String) -> impl IntoElement {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .p(px(12.0))
        .rounded_md()
        .bg(rgb(theme::EDITOR_FIELD_BG))
        .border_1()
        .border_color(rgb(theme::BORDER_SECONDARY))
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_SUBTLE))
                .child(SharedString::from(label)),
        )
        .child(
            div()
                .text_xs()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(rgb(theme::TEXT_PRIMARY))
                .child(SharedString::from(value)),
        )
}

pub(super) fn render_editor_section(
    label: &str,
    hint: Option<&str>,
    body: AnyElement,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(14.0))
        .p(px(16.0))
        .rounded_md()
        .bg(rgb(theme::EDITOR_CARD_BG))
        .border_1()
        .border_color(rgb(theme::BORDER_PRIMARY))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(10.0))
                .child(div().size(px(8.0)).rounded_full().bg(rgb(theme::PRIMARY)))
                .children(hint.map(|hint| {
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(
                            div()
                                .text_xs()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(rgb(theme::TEXT_PRIMARY))
                                .child(SharedString::from(label.to_string())),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_SUBTLE))
                                .child(SharedString::from(hint.to_string())),
                        )
                        .into_any_element()
                }))
                .children(hint.is_none().then(|| {
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::TEXT_PRIMARY))
                        .child(SharedString::from(label.to_string()))
                        .into_any_element()
                })),
        )
        .child(body)
}

pub(super) fn render_form_sections(
    sections: Vec<FormSection>,
    model: &EditorPaneModel,
    actions: &EditorActions,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(18.0))
        .children(
            sections
                .into_iter()
                .map(|section| render_form_section(section, model, actions).into_any_element()),
        )
        .into_any_element()
}

pub(super) fn render_static_form_sections(sections: Vec<FormSection>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(18.0))
        .children(
            sections
                .into_iter()
                .map(|section| render_static_form_section(section).into_any_element()),
        )
        .into_any_element()
}

pub(super) fn render_form_fields(
    fields: Vec<FormField>,
    model: &EditorPaneModel,
    actions: &EditorActions,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(14.0))
        .children(
            fields
                .into_iter()
                .map(|field| render_form_field(field, model, actions)),
        )
        .into_any_element()
}

pub(super) fn render_static_form_fields(fields: Vec<FormField>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(14.0))
        .children(fields.into_iter().map(render_static_form_field))
        .into_any_element()
}

pub(super) fn render_preview_stories(stories: Vec<PreviewStory>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(18.0))
        .children(
            stories
                .into_iter()
                .map(|story| render_preview_story(story).into_any_element()),
        )
        .into_any_element()
}

fn render_form_section(
    section: FormSection,
    model: &EditorPaneModel,
    actions: &EditorActions,
) -> impl IntoElement {
    render_editor_section(
        section.title.as_str(),
        section.hint.as_deref(),
        render_form_fields(section.fields, model, actions),
    )
}

fn render_static_form_section(section: FormSection) -> impl IntoElement {
    render_editor_section(
        section.title.as_str(),
        section.hint.as_deref(),
        render_static_form_fields(section.fields),
    )
}

fn render_form_field(
    field: FormField,
    model: &EditorPaneModel,
    actions: &EditorActions,
) -> AnyElement {
    match field {
        FormField::Text(field) => render_text_field(
            field.label.as_str(),
            field.hint.as_str(),
            field.value.as_str(),
            field.field,
            model,
            actions,
        )
        .into_any_element(),
        FormField::Multiline(field) => render_multiline_field(
            field.label.as_str(),
            field.hint.as_str(),
            field.value.as_str(),
            field.field,
            field.height,
            model,
            actions,
        )
        .into_any_element(),
        FormField::Choice(field) => render_choice_row(
            field.label.as_str(),
            field.value.as_str(),
            field.hint.as_deref(),
            field.on_click,
        )
        .into_any_element(),
        FormField::Toggle(field) => render_toggle_row_with_hint(
            field.label.as_str(),
            field.value,
            field.hint.as_str(),
            field.on_click,
        )
        .into_any_element(),
        FormField::Info(field) => render_info_row_with_badge(
            field.label.as_str(),
            field.value.as_str(),
            field.hint,
            field.badge,
            field.actions,
        )
        .into_any_element(),
        FormField::Notice(field) => {
            render_notice_row_with_tone(field.message.as_str(), field.tone).into_any_element()
        }
        FormField::Action(action) => render_form_action(action).into_any_element(),
        FormField::ActionGroup(group) => render_form_action_group(group).into_any_element(),
        FormField::SelectionList(list) => render_form_selection_list(list).into_any_element(),
        FormField::EmptyState(state) => render_empty_state(state).into_any_element(),
        FormField::Custom(element) => element,
    }
}

fn render_static_form_field(field: FormField) -> AnyElement {
    match field {
        FormField::Text(field) => render_static_text_field(
            field.label.as_str(),
            field.hint.as_str(),
            field.value.as_str(),
            false,
            field.height,
        )
        .into_any_element(),
        FormField::Multiline(field) => render_static_text_field(
            field.label.as_str(),
            field.hint.as_str(),
            field.value.as_str(),
            true,
            field.height,
        )
        .into_any_element(),
        FormField::Choice(field) => render_choice_row(
            field.label.as_str(),
            field.value.as_str(),
            field.hint.as_deref(),
            field.on_click,
        )
        .into_any_element(),
        FormField::Toggle(field) => render_toggle_row_with_hint(
            field.label.as_str(),
            field.value,
            field.hint.as_str(),
            field.on_click,
        )
        .into_any_element(),
        FormField::Info(field) => render_info_row_with_badge(
            field.label.as_str(),
            field.value.as_str(),
            field.hint,
            field.badge,
            field.actions,
        )
        .into_any_element(),
        FormField::Notice(field) => {
            render_notice_row_with_tone(field.message.as_str(), field.tone).into_any_element()
        }
        FormField::Action(action) => render_form_action(action).into_any_element(),
        FormField::ActionGroup(group) => render_form_action_group(group).into_any_element(),
        FormField::SelectionList(list) => render_form_selection_list(list).into_any_element(),
        FormField::EmptyState(state) => render_empty_state(state).into_any_element(),
        FormField::Custom(element) => element,
    }
}

fn render_preview_story(story: PreviewStory) -> impl IntoElement {
    render_editor_section(
        story.title.as_str(),
        Some(story.description.as_str()),
        div()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .children(
                story
                    .states
                    .into_iter()
                    .map(|state| render_preview_state(state).into_any_element()),
            )
            .into_any_element(),
    )
}

fn render_static_text_field(
    label: &str,
    hint: &str,
    value: &str,
    multiline: bool,
    height: Option<f32>,
) -> impl IntoElement {
    let hint = (!hint.trim().is_empty()).then_some(hint);
    let height = height.unwrap_or(140.0);
    let display_value = SharedString::from(if value.is_empty() {
        "Not set".to_string()
    } else {
        value.to_string()
    });
    let text_color = rgb(if value.is_empty() {
        theme::TEXT_DIM
    } else {
        theme::TEXT_PRIMARY
    });
    let surface: AnyElement = if multiline {
        div()
            .id(("static-multiline", label.as_ptr() as usize))
            .h(px(height))
            .overflow_y_scroll()
            .scrollbar_width(px(6.0))
            .px(px(12.0))
            .py(px(10.0))
            .rounded_md()
            .bg(rgb(theme::EDITOR_FIELD_BG))
            .border_1()
            .border_color(rgb(theme::BORDER_PRIMARY))
            .text_sm()
            .text_color(text_color)
            .child(display_value.clone())
            .into_any_element()
    } else {
        div()
            .px(px(12.0))
            .py(px(10.0))
            .rounded_md()
            .bg(rgb(theme::EDITOR_FIELD_BG))
            .border_1()
            .border_color(rgb(theme::BORDER_PRIMARY))
            .text_sm()
            .text_color(text_color)
            .child(display_value)
            .into_any_element()
    };

    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .child(
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgb(theme::TEXT_PRIMARY))
                        .child(SharedString::from(label.to_string())),
                )
                .children(hint.map(|hint| {
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_SUBTLE))
                        .child(SharedString::from(hint.to_string()))
                        .into_any_element()
                })),
        )
        .child(surface)
}

fn render_preview_state(state: PreviewState) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(12.0))
        .p(px(14.0))
        .rounded_md()
        .bg(rgb(theme::EDITOR_FIELD_BG))
        .border_1()
        .border_color(rgb(theme::BORDER_SECONDARY))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(12.0))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .child(
                            div()
                                .text_xs()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(rgb(theme::TEXT_PRIMARY))
                                .child(SharedString::from(state.label)),
                        )
                        .children(state.note.map(|note| {
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_SUBTLE))
                                .child(SharedString::from(note))
                                .into_any_element()
                        })),
                )
                .child(
                    div().flex().items_center().gap(px(8.0)).children(
                        state
                            .badges
                            .into_iter()
                            .map(|badge| render_surface_badge(badge).into_any_element()),
                    ),
                ),
        )
        .child(state.body)
}

fn render_form_action(action: FormAction) -> impl IntoElement {
    let (bg, border, value_color, hover_bg) = match action.style {
        SurfaceActionButtonStyle::Primary => (
            theme::EDITOR_NOTICE_BG,
            theme::PRIMARY,
            theme::PRIMARY,
            theme::ROW_HOVER_BG,
        ),
        SurfaceActionButtonStyle::Danger => (0x2b161c, 0x5a2630, theme::DANGER_TEXT, 0x382029),
        SurfaceActionButtonStyle::Ghost => (
            theme::EDITOR_FIELD_BG,
            theme::BORDER_PRIMARY,
            theme::TEXT_PRIMARY,
            theme::ROW_HOVER_BG,
        ),
    };

    div()
        .px(px(12.0))
        .py(px(10.0))
        .rounded_md()
        .bg(rgb(bg))
        .border_1()
        .border_color(rgb(border))
        .cursor_pointer()
        .hover(|surface| {
            surface
                .bg(rgb(hover_bg))
                .border_color(rgb(if border == theme::BORDER_PRIMARY {
                    theme::PRIMARY
                } else {
                    border
                }))
        })
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(12.0))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .child(
                            div()
                                .text_xs()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(rgb(theme::TEXT_PRIMARY))
                                .child(SharedString::from(action.title)),
                        )
                        .children(action.description.map(|description| {
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_SUBTLE))
                                .child(SharedString::from(description))
                                .into_any_element()
                        })),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .children(
                            action
                                .badge
                                .into_iter()
                                .map(|badge| render_surface_badge(badge).into_any_element()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(rgb(value_color))
                                .child(SharedString::from(action.value)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_SUBTLE))
                                .child(">"),
                        ),
                ),
        )
        .on_mouse_down(MouseButton::Left, action.on_click)
}

fn render_form_action_group(group: FormActionGroup) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .children(group.title.map(|title| {
            div()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .child(
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgb(theme::TEXT_PRIMARY))
                        .child(SharedString::from(title)),
                )
                .children(group.hint.map(|hint| {
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_SUBTLE))
                        .child(SharedString::from(hint))
                        .into_any_element()
                }))
                .into_any_element()
        }))
        .children(
            group
                .actions
                .into_iter()
                .map(|action| render_form_action(action).into_any_element()),
        )
}

fn render_form_selection_list(list: FormSelectionList) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .children(list.title.map(|title| {
            div()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .child(
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(SharedString::from(title)),
                )
                .children(list.hint.map(|hint| {
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_SUBTLE))
                        .child(SharedString::from(hint))
                        .into_any_element()
                }))
                .into_any_element()
        }))
        .children(list.rows.into_iter().map(|row| {
            render_selection_row(row.label, row.detail, row.selected, row.on_click)
                .into_any_element()
        }))
}

fn render_empty_state(state: FormEmptyState) -> impl IntoElement {
    let (bg, border, accent) = tone_colors(state.tone);
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .p(px(14.0))
        .rounded_md()
        .bg(rgb(bg))
        .border_1()
        .border_color(rgb(border))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(div().size(px(8.0)).rounded_full().bg(rgb(accent)))
                .child(
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::TEXT_PRIMARY))
                        .child(SharedString::from(state.title)),
                ),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(state.detail)),
        )
}

pub(super) fn render_surface_badge(badge: SurfaceBadge) -> impl IntoElement {
    let (bg, border, text) = tone_colors(badge.tone);
    div()
        .px(px(8.0))
        .py(px(4.0))
        .rounded_full()
        .bg(rgb(bg))
        .border_1()
        .border_color(rgb(border))
        .text_xs()
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(rgb(text))
        .child(SharedString::from(badge.label))
}

pub(super) fn render_text_field(
    label: &str,
    hint: &str,
    value: &str,
    field: EditorField,
    model: &EditorPaneModel,
    actions: &EditorActions,
) -> impl IntoElement {
    let hint = (!hint.trim().is_empty()).then_some(hint);
    let focused = model.active_field == Some(field);

    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(12.0))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .child(
                            div()
                                .text_xs()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(rgb(theme::TEXT_PRIMARY))
                                .child(SharedString::from(label.to_string())),
                        )
                        .children(hint.map(|hint| {
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_SUBTLE))
                                .child(SharedString::from(hint.to_string()))
                                .into_any_element()
                        })),
                )
                .children(focused.then(|| {
                    render_inline_state_badge("Editing", theme::PRIMARY).into_any_element()
                })),
        )
        .child(render_text_input_surface(
            value, field, model, actions, "Not set", false, None,
        ))
}

pub(super) fn render_multiline_field(
    label: &str,
    hint: &str,
    value: &str,
    field: EditorField,
    height: Option<f32>,
    model: &EditorPaneModel,
    actions: &EditorActions,
) -> impl IntoElement {
    let hint = (!hint.trim().is_empty()).then_some(hint);
    let height = height.unwrap_or(140.0);
    let focused = model.active_field == Some(field);

    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(12.0))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .child(
                            div()
                                .text_xs()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(rgb(theme::TEXT_PRIMARY))
                                .child(SharedString::from(label.to_string())),
                        )
                        .children(hint.map(|hint| {
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_SUBTLE))
                                .child(SharedString::from(hint.to_string()))
                                .into_any_element()
                        })),
                )
                .children(focused.then(|| {
                    render_inline_state_badge("Editing", theme::PRIMARY).into_any_element()
                })),
        )
        .child(render_text_input_surface(
            value,
            field,
            model,
            actions,
            "Not set",
            true,
            Some(height),
        ))
}

pub(super) fn render_compact_text_input(
    label: &str,
    hint: &str,
    value: &str,
    field: EditorField,
    model: &EditorPaneModel,
    actions: &EditorActions,
    width: Option<f32>,
    placeholder: &str,
) -> impl IntoElement {
    let mut input =
        render_text_input_surface(value, field, model, actions, placeholder, false, None);
    if let Some(width) = width {
        input = input.w(px(width));
    } else {
        input = input.w_full();
    }

    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(
            div()
                .text_xs()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(rgb(theme::TEXT_PRIMARY))
                .child(SharedString::from(label.to_string())),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_SUBTLE))
                .child(SharedString::from(hint.to_string())),
        )
        .child(input)
}

fn render_text_input_surface(
    value: &str,
    field: EditorField,
    model: &EditorPaneModel,
    actions: &EditorActions,
    placeholder: &str,
    multiline: bool,
    min_height: Option<f32>,
) -> gpui::Div {
    let focused = model.active_field == Some(field);
    let text_color = if value.is_empty() && !focused {
        theme::TEXT_DIM
    } else {
        theme::TEXT_PRIMARY
    };
    let lines = editable_field_lines(
        value,
        model.cursor,
        model.selection_anchor,
        focused,
        multiline,
        placeholder,
    );

    let mut surface = div()
        .px(px(12.0))
        .py(px(10.0))
        .rounded_md()
        .bg(rgb(theme::EDITOR_FIELD_BG))
        .border_1()
        .border_color(rgb(if focused {
            theme::PRIMARY
        } else {
            theme::BORDER_PRIMARY
        }))
        .overflow_hidden()
        .cursor_text()
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .children(lines.into_iter().map(|line| {
                    render_editable_field_line(line, field, actions, text_color).into_any_element()
                })),
        );

    if multiline {
        surface = surface.min_h(px(min_height.unwrap_or(140.0)));
    }

    surface
}

fn render_editable_field_line(
    line: EditableFieldLine,
    field: EditorField,
    actions: &EditorActions,
    text_color: u32,
) -> impl IntoElement {
    let on_focus_at = actions.on_focus_at.clone();
    let on_drag_to = actions.on_drag_to.clone();
    let display_text = line.display_text;
    let editable_text = line.editable_text;
    let char_start = line.char_start;
    let cursor_col = line.cursor_col;
    let selection_cols = line.selection_cols;
    let placeholder = line.placeholder;

    canvas(
        move |bounds, window, _cx| {
            let paint_text: SharedString = if display_text.is_empty() {
                SharedString::from(" ")
            } else {
                display_text.clone()
            };
            let shaped_line = window.text_system().shape_line(
                paint_text,
                px(EDITOR_FIELD_FONT_SIZE_PX),
                &[TextRun {
                    len: if display_text.is_empty() {
                        1
                    } else {
                        display_text.len()
                    },
                    font: terminal::terminal_font(),
                    color: field_text_color(text_color, placeholder),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                None,
            );
            let cursor_x = cursor_col.map(|cursor_col| {
                let cursor_byte = byte_index_for_char(editable_text.as_str(), cursor_col);
                shaped_line.x_for_index(cursor_byte)
            });
            let selection_x = selection_cols.map(|(start_col, end_col)| {
                let start_byte = byte_index_for_char(editable_text.as_str(), start_col);
                let end_byte = byte_index_for_char(editable_text.as_str(), end_col);
                (
                    shaped_line.x_for_index(start_byte),
                    shaped_line.x_for_index(end_byte),
                )
            });

            EditableFieldLinePaintState {
                hitbox: window.insert_hitbox(bounds, HitboxBehavior::Normal),
                shaped_line,
                editable_text,
                char_start,
                cursor_x,
                selection_x,
            }
        },
        move |bounds, state, window, cx| {
            let hitbox = state.hitbox.clone();
            let shaped_line = state.shaped_line.clone();
            let editable_text = state.editable_text.clone();
            let char_start = state.char_start;
            let on_focus_at = on_focus_at.clone();

            // Mouse down → position cursor
            {
                let hitbox = hitbox.clone();
                let shaped_line = shaped_line.clone();
                let editable_text = editable_text.clone();
                window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                    if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                        let cursor_index = if editable_text.is_empty() {
                            char_start
                        } else {
                            let local_x =
                                (event.position.x - hitbox.bounds.origin.x).max(Pixels::ZERO);
                            let byte_index = shaped_line.closest_index_for_x(local_x);
                            char_start + char_count_for_byte(editable_text.as_str(), byte_index)
                        };
                        let on_focus = on_focus_at(field, cursor_index);
                        on_focus(event, window, cx);
                    }
                });
            }

            // Mouse move → drag selection
            {
                let hitbox = hitbox.clone();
                let shaped_line = shaped_line.clone();
                let editable_text = editable_text.clone();
                let on_drag_to = on_drag_to.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                    if phase == DispatchPhase::Bubble
                        && hitbox.is_hovered(window)
                        && event.dragging()
                    {
                        let cursor_index = if editable_text.is_empty() {
                            char_start
                        } else {
                            let local_x =
                                (event.position.x - hitbox.bounds.origin.x).max(Pixels::ZERO);
                            let byte_index = shaped_line.closest_index_for_x(local_x);
                            char_start + char_count_for_byte(editable_text.as_str(), byte_index)
                        };
                        let on_drag = on_drag_to(field, cursor_index);
                        on_drag(event, window, cx);
                    }
                });
            }

            if state.hitbox.is_hovered(window) {
                window.set_cursor_style(CursorStyle::IBeam, &state.hitbox);
            }

            // Paint selection highlight
            if let Some((sel_start_x, sel_end_x)) = state.selection_x {
                let sel_bounds = Bounds::new(
                    point(bounds.origin.x + sel_start_x, bounds.origin.y),
                    size(sel_end_x - sel_start_x, px(EDITOR_FIELD_LINE_HEIGHT_PX)),
                );
                window.paint_quad(fill(sel_bounds, rgb(theme::SELECTION_BG)));
            }

            let _ =
                state
                    .shaped_line
                    .paint(bounds.origin, px(EDITOR_FIELD_LINE_HEIGHT_PX), window, cx);

            if let Some(cursor_x) = state.cursor_x {
                let cursor_bounds = Bounds::new(
                    point(bounds.origin.x + cursor_x, bounds.origin.y + px(1.0)),
                    size(
                        px(EDITOR_FIELD_CURSOR_WIDTH_PX),
                        px((EDITOR_FIELD_LINE_HEIGHT_PX - 2.0).max(1.0)),
                    ),
                );
                window.paint_quad(fill(cursor_bounds, rgb(theme::TEXT_PRIMARY)));
            }
        },
    )
    .w_full()
    .h(px(EDITOR_FIELD_LINE_HEIGHT_PX))
}

fn line_selection_cols(
    sel_start: usize,
    sel_end: usize,
    char_start: usize,
    line_char_count: usize,
) -> Option<(usize, usize)> {
    if sel_start == sel_end {
        return None;
    }
    let line_end = char_start + line_char_count;
    let s = sel_start.max(char_start).min(line_end);
    let e = sel_end.max(char_start).min(line_end);
    if s < e {
        Some((s - char_start, e - char_start))
    } else {
        None
    }
}

fn editable_field_lines(
    value: &str,
    cursor: usize,
    selection_anchor: Option<usize>,
    focused: bool,
    multiline: bool,
    placeholder: &str,
) -> Vec<EditableFieldLine> {
    let sel_range = if focused {
        selection_anchor.and_then(|anchor| {
            let (s, e) = if anchor < cursor {
                (anchor, cursor)
            } else {
                (cursor, anchor)
            };
            if s == e {
                None
            } else {
                Some((s, e))
            }
        })
    } else {
        None
    };

    if !focused && value.is_empty() {
        return vec![EditableFieldLine {
            display_text: SharedString::from(placeholder.to_string()),
            editable_text: String::new(),
            char_start: 0,
            cursor_col: None,
            selection_cols: None,
            placeholder: true,
        }];
    }

    if !multiline {
        let char_count = value.chars().count();
        return vec![EditableFieldLine {
            display_text: SharedString::from(value.to_string()),
            editable_text: value.to_string(),
            char_start: 0,
            cursor_col: focused.then_some(cursor.min(char_count)),
            selection_cols: sel_range.and_then(|(s, e)| line_selection_cols(s, e, 0, char_count)),
            placeholder: false,
        }];
    }

    let mut lines = Vec::new();
    let mut char_start = 0usize;
    let (cursor_line, cursor_column) = cursor_line_and_column(value, cursor);
    let mut split_lines: Vec<&str> = value.split('\n').collect();
    if split_lines.is_empty() {
        split_lines.push("");
    }

    for (line_index, line) in split_lines.into_iter().enumerate() {
        let line_char_count = line.chars().count();
        lines.push(EditableFieldLine {
            display_text: SharedString::from(line.to_string()),
            editable_text: line.to_string(),
            char_start,
            cursor_col: (focused && line_index == cursor_line).then_some(cursor_column),
            selection_cols: sel_range
                .and_then(|(s, e)| line_selection_cols(s, e, char_start, line_char_count)),
            placeholder: false,
        });
        char_start += line_char_count + 1;
    }

    if lines.is_empty() {
        lines.push(EditableFieldLine {
            display_text: SharedString::new(""),
            editable_text: String::new(),
            char_start: 0,
            cursor_col: focused.then_some(0),
            selection_cols: None,
            placeholder: false,
        });
    }

    lines
}

fn cursor_line_and_column(value: &str, cursor: usize) -> (usize, usize) {
    let mut remaining = cursor.min(value.chars().count());
    for (line_index, line) in value.split('\n').enumerate() {
        let line_len = line.chars().count();
        if remaining <= line_len {
            return (line_index, remaining);
        }
        remaining = remaining.saturating_sub(line_len + 1);
    }
    (value.split('\n').count().saturating_sub(1), remaining)
}

fn byte_index_for_char(value: &str, char_index: usize) -> usize {
    if char_index == 0 {
        return 0;
    }
    value
        .char_indices()
        .nth(char_index)
        .map(|(index, _)| index)
        .unwrap_or(value.len())
}

fn char_count_for_byte(value: &str, byte_index: usize) -> usize {
    value[..byte_index.min(value.len())].chars().count()
}

fn field_text_color(text_color: u32, placeholder: bool) -> Hsla {
    if placeholder {
        rgb(theme::TEXT_DIM).into()
    } else {
        rgb(text_color).into()
    }
}

pub(super) fn render_choice_row(
    label: &str,
    value: &str,
    hint: Option<&str>,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .child(
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(rgb(theme::TEXT_PRIMARY))
                        .child(SharedString::from(label.to_string())),
                )
                .children(hint.map(|hint| {
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_SUBTLE))
                        .child(SharedString::from(hint.to_string()))
                        .into_any_element()
                })),
        )
        .child(
            div()
                .px(px(12.0))
                .py(px(10.0))
                .rounded_md()
                .bg(rgb(theme::EDITOR_FIELD_BG))
                .border_1()
                .border_color(rgb(theme::BORDER_PRIMARY))
                .cursor_pointer()
                .hover(|s| {
                    s.bg(rgb(theme::ROW_HOVER_BG))
                        .border_color(rgb(theme::PRIMARY))
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(12.0))
                        .child(
                            div()
                                .flex_1()
                                .text_sm()
                                .text_color(rgb(theme::TEXT_PRIMARY))
                                .child(SharedString::from(value.to_string())),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_SUBTLE))
                                .child(">"),
                        ),
                )
                .on_mouse_down(MouseButton::Left, on_click),
        )
}

pub(super) fn render_toggle_row_with_hint(
    label: &str,
    value: bool,
    hint: &str,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    let hint = (!hint.trim().is_empty()).then_some(hint);
    let mut toggle = div()
        .w(px(38.0))
        .h(px(22.0))
        .p(px(2.0))
        .rounded_full()
        .flex()
        .items_center()
        .bg(rgb(if value {
            theme::PRIMARY
        } else {
            theme::BORDER_SECONDARY
        }));
    toggle = if value {
        toggle.justify_end()
    } else {
        toggle.justify_start()
    };

    div()
        .px(px(12.0))
        .py(px(10.0))
        .rounded_md()
        .bg(rgb(theme::EDITOR_FIELD_BG))
        .border_1()
        .border_color(rgb(theme::BORDER_PRIMARY))
        .cursor_pointer()
        .hover(|s| {
            s.bg(rgb(theme::ROW_HOVER_BG))
                .border_color(rgb(theme::PRIMARY))
        })
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(12.0))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .child(
                            div()
                                .text_xs()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(rgb(theme::TEXT_PRIMARY))
                                .child(SharedString::from(label.to_string())),
                        )
                        .children(hint.map(|hint| {
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_SUBTLE))
                                .child(SharedString::from(hint.to_string()))
                                .into_any_element()
                        })),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            div()
                                .text_xs()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(rgb(if value {
                                    theme::PRIMARY
                                } else {
                                    theme::TEXT_SUBTLE
                                }))
                                .child(if value { "On" } else { "Off" }),
                        )
                        .child(toggle.child(div().size(px(16.0)).rounded_full().bg(rgb(0xffffff)))),
                ),
        )
        .on_mouse_down(MouseButton::Left, on_click)
}

fn render_inline_state_badge(label: &str, color: u32) -> impl IntoElement {
    div()
        .px(px(8.0))
        .py(px(4.0))
        .rounded_full()
        .bg(rgb(theme::APP_BG))
        .border_1()
        .border_color(rgb(color))
        .text_xs()
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(rgb(color))
        .child(SharedString::from(label.to_string()))
}

pub(super) fn render_notice_row(message: &str) -> impl IntoElement {
    render_notice_row_with_tone(message, SurfaceTone::Accent)
}

fn render_notice_row_with_tone(message: &str, tone: SurfaceTone) -> impl IntoElement {
    let (bg, border, accent) = tone_colors(tone);
    div()
        .flex()
        .items_center()
        .gap(px(10.0))
        .px(px(12.0))
        .py(px(10.0))
        .rounded_md()
        .bg(rgb(bg))
        .border_1()
        .border_color(rgb(border))
        .child(div().size(px(8.0)).rounded_full().bg(rgb(accent)))
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(message.to_string())),
        )
}

pub(super) fn render_selection_row(
    label: String,
    detail: Option<String>,
    selected: bool,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    div()
        .px(px(12.0))
        .py(px(10.0))
        .rounded_md()
        .bg(rgb(if selected {
            theme::EDITOR_NOTICE_BG
        } else {
            theme::EDITOR_FIELD_BG
        }))
        .border_1()
        .border_color(rgb(if selected {
            theme::PRIMARY
        } else {
            theme::BORDER_PRIMARY
        }))
        .cursor_pointer()
        .hover(|s| {
            s.bg(rgb(theme::ROW_HOVER_BG))
                .border_color(rgb(theme::PRIMARY))
        })
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(10.0))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .text_xs()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(rgb(theme::TEXT_PRIMARY))
                                .child(SharedString::from(label)),
                        )
                        .children(detail.map(|detail| {
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_SUBTLE))
                                .child(SharedString::from(detail))
                        })),
                )
                .children(selected.then(|| {
                    div()
                        .text_xs()
                        .text_color(rgb(theme::PRIMARY))
                        .child("Selected")
                        .into_any_element()
                })),
        )
        .on_mouse_down(MouseButton::Left, on_click)
}

pub(super) fn render_info_row(label: &str, value: &str, hint: Option<&str>) -> impl IntoElement {
    render_info_row_with_badge(
        label,
        value,
        hint.map(|value| value.to_string()),
        Some(SurfaceBadge::new("Detected", SurfaceTone::Muted)),
        Vec::new(),
    )
}

fn render_info_row_with_badge(
    label: &str,
    value: &str,
    hint: Option<String>,
    badge: Option<SurfaceBadge>,
    actions: Vec<FormAction>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(12.0))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .child(
                            div()
                                .text_xs()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(rgb(theme::TEXT_PRIMARY))
                                .child(SharedString::from(label.to_string())),
                        )
                        .children(hint.map(|hint| {
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_SUBTLE))
                                .child(SharedString::from(hint))
                                .into_any_element()
                        })),
                )
                .children(badge.map(|badge| render_surface_badge(badge).into_any_element())),
        )
        .child(
            div()
                .px(px(12.0))
                .py(px(10.0))
                .rounded_md()
                .bg(rgb(theme::EDITOR_FIELD_BG))
                .border_1()
                .border_color(rgb(theme::BORDER_PRIMARY))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.0))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .text_sm()
                                .text_color(rgb(theme::TEXT_PRIMARY))
                                .child(SharedString::from(value.to_string())),
                        )
                        .children((!actions.is_empty()).then(|| {
                            div()
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .flex_shrink_0()
                                .children(actions.into_iter().map(|action| {
                                    render_compact_surface_action_button(
                                        action.value.as_str(),
                                        action.style,
                                        action.on_click,
                                    )
                                    .into_any_element()
                                }))
                                .into_any_element()
                        })),
                ),
        )
}

fn tone_colors(tone: SurfaceTone) -> (u32, u32, u32) {
    match tone {
        SurfaceTone::Accent => (
            theme::EDITOR_NOTICE_BG,
            theme::BORDER_ACCENT,
            theme::PRIMARY,
        ),
        SurfaceTone::Muted => (theme::APP_BG, theme::BORDER_SECONDARY, theme::TEXT_MUTED),
        SurfaceTone::Success => (theme::SUCCESS_BG, 0x1c3b27, theme::SUCCESS_TEXT),
        SurfaceTone::Warning => (0x2a2211, 0x4f3b0d, theme::WARNING_TEXT),
        SurfaceTone::Danger => (0x2b161c, 0x5a2630, theme::DANGER_TEXT),
    }
}

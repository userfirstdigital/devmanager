mod editor_ui;

use self::editor_ui::{
    render_choice_row, render_compact_text_input, render_display_field, render_editor_intro,
    render_editor_section, render_editor_toolbar, render_form_fields, render_form_sections,
    render_info_row, render_notice_row, render_preview_stories, render_selection_row,
    render_static_form_fields, render_static_form_sections, render_surface_action_button,
    FormAction, FormActionGroup, FormField, FormInfoField, FormSection, FormSelectionList,
    FormSelectionRow, PreviewState, PreviewStory, SurfaceActionButtonStyle, SurfaceBadge,
    SurfaceTone,
};
use crate::models::{
    DefaultTerminal, DependencyStatus, MacTerminalProfile, RootScanEntry, ScanResult, ScannedPort,
    ScannedScript,
};
use crate::remote::{
    KnownRemoteHost, PairedRemoteClient, PairedWebClient, RemoteAccessActivityEvent,
    RemoteAccessActivityKind, RemoteAccessSource,
};
use crate::theme;
use crate::updater::{UpdaterSnapshot, UpdaterStage};
use gpui::{
    anchored, deferred, div, px, rgb, AnyElement, App, Corner, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, MouseMoveEvent, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window,
};
use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};
use time::{format_description, OffsetDateTime, UtcOffset};

// ── Add Project Wizard ──────────────────────────────────────────────────────

const PROJECT_COLOR_PRESETS: &[(u32, &str)] = &[
    (0x6366f1, "#6366f1"), // indigo
    (0xec4899, "#ec4899"), // pink
    (0xf59e0b, "#f59e0b"), // amber
    (0x10b981, "#10b981"), // emerald
    (0x3b82f6, "#3b82f6"), // blue
    (0xef4444, "#ef4444"), // red
    (0xa855f7, "#a855f7"), // purple
    (0x14b8a6, "#14b8a6"), // teal
];

#[derive(Debug, Clone)]
pub struct AddProjectWizard {
    pub name: String,
    pub color: String,
    pub root_path: String,
    pub cursor: usize,
    pub name_focused: bool,
    pub step: u8,
    pub scan_message: Option<String>,
    pub scan_entries: Vec<RootScanEntry>,
    pub selected_folders: std::collections::BTreeSet<String>,
    pub selected_scripts: HashMap<String, BTreeSet<String>>,
    pub selected_port_variables: HashMap<String, Option<String>>,
}

impl Default for AddProjectWizard {
    fn default() -> Self {
        Self {
            name: String::new(),
            color: PROJECT_COLOR_PRESETS[0].1.to_string(),
            root_path: String::new(),
            cursor: 0,
            name_focused: false,
            step: 1,
            scan_message: None,
            scan_entries: Vec::new(),
            selected_folders: Default::default(),
            selected_scripts: Default::default(),
            selected_port_variables: Default::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum WizardAction {
    Cancel,
    Create,
    Configure,
    Back,
    ClickName,
    SelectColor(String),
    PickRootFolder,
    ToggleFolder(String),
    ToggleScript {
        folder_path: String,
        script_name: String,
    },
    SelectPortVariable {
        folder_path: String,
        variable: Option<String>,
    },
}

pub struct WizardActions<'a> {
    pub on_action: &'a dyn Fn(WizardAction) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
}

pub fn render_add_project_wizard(
    wizard: &AddProjectWizard,
    actions: WizardActions<'_>,
) -> AnyElement {
    match wizard.step {
        2 => render_wizard_step2(wizard, actions).into_any_element(),
        _ => render_wizard_step1(wizard, actions).into_any_element(),
    }
}

fn render_wizard_shell(backdrop_id: &'static str, frame: AnyElement) -> impl IntoElement {
    deferred(
        anchored().snap_to_window().anchor(Corner::TopLeft).child(
            div()
                .id(backdrop_id)
                .occlude()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(frame),
        ),
    )
    .with_priority(2)
}

fn render_wizard_frame(
    scroll_id: &'static str,
    width: f32,
    title: &str,
    description: &str,
    on_close: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    body: AnyElement,
    footer: AnyElement,
) -> impl IntoElement {
    div()
        .w(px(width))
        .max_h(px(720.0))
        .rounded_md()
        .bg(rgb(theme::EDITOR_CARD_BG))
        .border_1()
        .border_color(rgb(theme::BORDER_PRIMARY))
        .flex()
        .flex_col()
        .overflow_hidden()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(16.0))
                .px(px(18.0))
                .py(px(14.0))
                .bg(rgb(theme::TOPBAR_BG))
                .border_b_1()
                .border_color(rgb(theme::BORDER_PRIMARY))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .gap(px(12.0))
                        .child(div().size(px(10.0)).rounded_full().bg(rgb(theme::PRIMARY)))
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
                                        .child(SharedString::from(description.to_string())),
                                ),
                        ),
                )
                .child(render_surface_action_button(
                    "Close",
                    SurfaceActionButtonStyle::Ghost,
                    on_close,
                )),
        )
        .child(
            div()
                .flex_1()
                .id(scroll_id)
                .overflow_y_scroll()
                .scrollbar_width(px(6.0))
                .child(
                    div()
                        .px(px(20.0))
                        .py(px(20.0))
                        .flex()
                        .flex_col()
                        .gap(px(18.0))
                        .child(body),
                ),
        )
        .child(
            div()
                .px(px(18.0))
                .py(px(14.0))
                .border_t_1()
                .border_color(rgb(theme::BORDER_PRIMARY))
                .child(footer),
        )
}

fn render_wizard_step1(wizard: &AddProjectWizard, actions: WizardActions<'_>) -> impl IntoElement {
    render_wizard_shell(
        "wizard-backdrop",
        render_wizard_step1_frame("add-project-step1-scroll", wizard, &actions).into_any_element(),
    )
}

fn render_wizard_step1_frame(
    scroll_id: &'static str,
    wizard: &AddProjectWizard,
    actions: &WizardActions<'_>,
) -> impl IntoElement {
    let on_cancel = (actions.on_action)(WizardAction::Cancel);
    let on_configure = (actions.on_action)(WizardAction::Configure);

    render_wizard_frame(
        scroll_id,
        760.0,
        "Add Project",
        "Define the project identity, pick a root folder, and review the detected apps.",
        on_cancel,
        render_wizard_step1_content(wizard, actions).into_any_element(),
        div()
            .flex()
            .items_center()
            .justify_end()
            .gap(px(10.0))
            .child(render_surface_action_button(
                "Cancel",
                SurfaceActionButtonStyle::Ghost,
                (actions.on_action)(WizardAction::Cancel),
            ))
            .child(render_surface_action_button(
                "Configure Folders",
                SurfaceActionButtonStyle::Primary,
                on_configure,
            ))
            .into_any_element(),
    )
}

fn wizard_scan_detail(entry: &RootScanEntry) -> String {
    let scripts = entry.scripts.len();
    let has_env = entry.has_env;
    let mut parts = Vec::new();

    if !entry.project_type.trim().is_empty() {
        parts.push(scan_project_type_label(entry.project_type.as_str()));
    }

    match (scripts, has_env) {
        (0, false) => {}
        (0, true) => parts.push(".env".to_string()),
        (1, false) => parts.push("1 script".to_string()),
        (n, false) => parts.push(format!("{n} scripts")),
        (1, true) => {
            parts.push("1 script".to_string());
            parts.push(".env".to_string());
        }
        (n, true) => {
            parts.push(format!("{n} scripts"));
            parts.push(".env".to_string());
        }
    }

    parts.join(" | ")
}

fn scan_project_type_label(project_type: &str) -> String {
    match project_type.trim().to_ascii_lowercase().as_str() {
        "node" | "npm" => "Node".to_string(),
        "cargo" | "rust" => "Rust".to_string(),
        "python" => "Python".to_string(),
        other if other.is_empty() => String::new(),
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
                None => String::new(),
            }
        }
    }
}

fn render_wizard_step1_content(
    wizard: &AddProjectWizard,
    actions: &WizardActions<'_>,
) -> impl IntoElement {
    let on_pick_root = (actions.on_action)(WizardAction::PickRootFolder);
    let on_click_name = (actions.on_action)(WizardAction::ClickName);
    let display_name = if wizard.name_focused {
        display_text_with_cursor(wizard.name.as_str(), wizard.cursor)
    } else {
        wizard.name.clone()
    };

    let scan_notice = wizard
        .scan_message
        .as_ref()
        .filter(|message| wizard.scan_entries.is_empty() || !message.starts_with("Discovered "));

    let mut sections = vec![FormSection::new("Project").fields(vec![
        FormField::custom(
            render_display_field(
                "Name",
                "Shown in the sidebar.",
                display_name.as_str(),
                "My App",
                wizard.name_focused,
                on_click_name,
            )
            .into_any_element(),
        ),
        FormField::custom(render_wizard_color_picker(wizard, actions).into_any_element()),
        FormField::choice(
            "Root folder",
            if wizard.root_path.is_empty() {
                "Choose root folder".to_string()
            } else {
                wizard.root_path.clone()
            },
            Some(if wizard.root_path.is_empty() {
                "Pick the repo root to detect app folders.".to_string()
            } else {
                "Change the folder DevManager scans.".to_string()
            }),
            on_pick_root,
        ),
    ])];

    if wizard.root_path.is_empty() {
        sections.push(FormSection::new("Scan").field(FormField::empty_state(
            "Pick a root folder",
            "DevManager will look for app folders, scripts, and env ports there.",
            SurfaceTone::Muted,
        )));
    } else if let Some(message) = scan_notice {
        sections.push(
            FormSection::new("Scan").field(FormField::notice(message.clone(), SurfaceTone::Accent)),
        );
    }

    if !wizard.scan_entries.is_empty() {
        let count = wizard.scan_entries.len();
        let mut list = FormSelectionList::untitled();
        for entry in &wizard.scan_entries {
            let selected = wizard.selected_folders.contains(&entry.path);
            let on_toggle = (actions.on_action)(WizardAction::ToggleFolder(entry.path.clone()));
            let detail = wizard_scan_detail(entry);
            list = list.row(FormSelectionRow::new(
                entry.name.clone(),
                (!detail.is_empty()).then_some(detail),
                selected,
                on_toggle,
            ));
        }
        sections.push(
            FormSection::new("Folders to Add")
                .hint(format!(
                    "{count} found. Clear anything you do not want in the new project."
                ))
                .field(FormField::selection_list(list)),
        );
    }

    render_static_form_sections(sections)
}

fn render_wizard_color_picker(
    wizard: &AddProjectWizard,
    actions: &WizardActions<'_>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
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
                        .child("Accent color"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_SUBTLE))
                        .child("Sidebar marker color."),
                ),
        )
        .child(div().flex().items_center().gap(px(10.0)).children(
            PROJECT_COLOR_PRESETS.iter().map(|(hex, name)| {
                let selected = wizard.color == *name;
                let on_select = (actions.on_action)(WizardAction::SelectColor(name.to_string()));
                div()
                    .size(px(34.0))
                    .rounded_full()
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgb(theme::EDITOR_FIELD_BG))
                    .border_1()
                    .border_color(rgb(if selected {
                        theme::PRIMARY
                    } else {
                        theme::BORDER_PRIMARY
                    }))
                    .hover(|s| s.border_color(rgb(theme::PRIMARY)))
                    .child(div().size(px(22.0)).rounded_full().bg(rgb(*hex)))
                    .on_mouse_down(MouseButton::Left, on_select)
                    .into_any_element()
            }),
        ))
}

fn render_wizard_step2(wizard: &AddProjectWizard, actions: WizardActions<'_>) -> impl IntoElement {
    render_wizard_shell(
        "wizard-step2-backdrop",
        render_wizard_step2_frame("add-project-step2-scroll", wizard, &actions).into_any_element(),
    )
}

fn render_wizard_step2_frame(
    scroll_id: &'static str,
    wizard: &AddProjectWizard,
    actions: &WizardActions<'_>,
) -> impl IntoElement {
    let on_cancel = (actions.on_action)(WizardAction::Cancel);
    let on_back = (actions.on_action)(WizardAction::Back);
    let on_create = (actions.on_action)(WizardAction::Create);

    render_wizard_frame(
        scroll_id,
        820.0,
        "Add Project",
        "Choose which scripts and default port variables should seed the new project folders.",
        on_cancel,
        render_wizard_step2_content(wizard, actions).into_any_element(),
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(10.0))
            .child(render_surface_action_button(
                "Back",
                SurfaceActionButtonStyle::Ghost,
                on_back,
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(render_surface_action_button(
                        "Cancel",
                        SurfaceActionButtonStyle::Ghost,
                        (actions.on_action)(WizardAction::Cancel),
                    ))
                    .child(render_surface_action_button(
                        "Create Project",
                        SurfaceActionButtonStyle::Primary,
                        on_create,
                    )),
            )
            .into_any_element(),
    )
}

fn render_wizard_step2_content(
    wizard: &AddProjectWizard,
    actions: &WizardActions<'_>,
) -> impl IntoElement {
    let selected_entries: Vec<&RootScanEntry> = wizard
        .scan_entries
        .iter()
        .filter(|entry| wizard.selected_folders.contains(&entry.path))
        .collect();

    if selected_entries.is_empty() {
        return render_static_form_sections(vec![FormSection::new("Folders").field(
            FormField::empty_state(
                "No folders selected",
                "Go back and pick at least one folder to configure.",
                SurfaceTone::Warning,
            ),
        )]);
    }

    let mut sections = Vec::new();
    for entry in selected_entries {
        sections.push(render_wizard_folder_config(entry, wizard, actions));
    }

    render_static_form_sections(sections)
}

#[allow(unreachable_code)]
fn render_wizard_folder_config(
    entry: &RootScanEntry,
    wizard: &AddProjectWizard,
    actions: &WizardActions<'_>,
) -> FormSection {
    let selected_scripts = wizard.selected_scripts.get(&entry.path);
    let selected_port = wizard
        .selected_port_variables
        .get(&entry.path)
        .cloned()
        .flatten();

    let detail = wizard_scan_detail(entry);
    let section_hint = if detail.is_empty() {
        entry.path.clone()
    } else {
        format!("{} | {}", detail, entry.path)
    };

    let mut fields = Vec::new();

    if !entry.scripts.is_empty() {
        let mut list = FormSelectionList::new("Create commands")
            .hint("Selected scripts become commands in the new folder.");
        for script in &entry.scripts {
            let is_selected = selected_scripts
                .map(|scripts| scripts.contains(&script.name))
                .unwrap_or(false);
            let on_toggle = (actions.on_action)(WizardAction::ToggleScript {
                folder_path: entry.path.clone(),
                script_name: script.name.clone(),
            });
            list = list.row(FormSelectionRow::new(
                script.name.clone(),
                Some(script.command.clone()),
                is_selected,
                on_toggle,
            ));
        }
        fields.push(FormField::selection_list(list));
    }

    if !entry.ports.is_empty() {
        let mut list = FormSelectionList::new("Default port")
            .hint("Choose which env var should fill the folder port setting.");
        list = list.row(FormSelectionRow::new(
            "None",
            Some("Do not set a default port.".to_string()),
            selected_port.is_none(),
            (actions.on_action)(WizardAction::SelectPortVariable {
                folder_path: entry.path.clone(),
                variable: None,
            }),
        ));
        for port in &entry.ports {
            let is_selected = selected_port.as_deref() == Some(port.variable.as_str());
            let on_select = (actions.on_action)(WizardAction::SelectPortVariable {
                folder_path: entry.path.clone(),
                variable: Some(port.variable.clone()),
            });
            list = list.row(FormSelectionRow::new(
                format!("{} = {}", port.variable, port.port),
                Some(port.source.clone()),
                is_selected,
                on_select,
            ));
        }
        fields.push(FormField::selection_list(list));
    }

    if fields.is_empty() {
        fields.push(FormField::empty_state(
            "No detected scripts or ports",
            "This folder can still be added, but the scan did not find anything to prefill.",
            SurfaceTone::Muted,
        ));
    }

    return FormSection::new(entry.name.clone())
        .hint(section_hint)
        .fields(fields);

    let _ = render_editor_section(
        entry.name.as_str(),
        Some(section_hint.as_str()),
        /*
                format!("{} · {}", entry.path, detail).as_str()
            },
        ),
        */
        div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .children((!entry.scripts.is_empty()).then(|| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child("Scripts"),
                    )
                    .children(entry.scripts.iter().map(|script| {
                        let is_selected = selected_scripts
                            .map(|scripts| scripts.contains(&script.name))
                            .unwrap_or(false);
                        let on_toggle = (actions.on_action)(WizardAction::ToggleScript {
                            folder_path: entry.path.clone(),
                            script_name: script.name.clone(),
                        });
                        render_selection_row(
                            script.name.clone(),
                            Some(script.command.clone()),
                            is_selected,
                            on_toggle,
                        )
                        .into_any_element()
                    }))
                    .into_any_element()
            }))
            .children((!entry.ports.is_empty()).then(|| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_xs()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(rgb(theme::TEXT_MUTED))
                            .child("Default port variable"),
                    )
                    .child(render_selection_row(
                        "None".to_string(),
                        Some("Do not set a default port variable for this folder.".to_string()),
                        selected_port.is_none(),
                        (actions.on_action)(WizardAction::SelectPortVariable {
                            folder_path: entry.path.clone(),
                            variable: None,
                        }),
                    ))
                    .children(entry.ports.iter().map(|port| {
                        let is_selected = selected_port.as_deref() == Some(port.variable.as_str());
                        let on_select = (actions.on_action)(WizardAction::SelectPortVariable {
                            folder_path: entry.path.clone(),
                            variable: Some(port.variable.clone()),
                        });
                        render_selection_row(
                            format!("{} = {}", port.variable, port.port),
                            Some(port.source.clone()),
                            is_selected,
                            on_select,
                        )
                        .into_any_element()
                    }))
                    .into_any_element()
            }))
            .into_any_element(),
    );
}

#[derive(Debug, Clone)]
pub enum EditorPanel {
    Settings(SettingsDraft),
    UiPreview(UiPreviewDraft),
    Project(ProjectDraft),
    Folder(FolderDraft),
    Command(CommandDraft),
    Ssh(SshDraft),
}

impl EditorPanel {
    pub fn title(&self) -> &'static str {
        match self {
            Self::Settings(draft) => {
                if draft.remote_focus_only {
                    "Remote"
                } else {
                    "Settings"
                }
            }
            Self::UiPreview(_) => "UI Preview",
            Self::Project(_) => "Edit Project",
            Self::Folder(draft) => {
                if draft.existing_id.is_some() {
                    "Edit Folder"
                } else {
                    "Add Folder"
                }
            }
            Self::Command(draft) => {
                if draft.existing_id.is_some() {
                    "Edit Command"
                } else {
                    "Add Command"
                }
            }
            Self::Ssh(draft) => {
                if draft.existing_id.is_some() {
                    "Edit SSH Connection"
                } else {
                    "Add SSH Connection"
                }
            }
        }
    }

    pub fn subtitle(&self) -> &'static str {
        match self {
            Self::Settings(draft) => {
                if draft.remote_focus_only {
                    "Connect to other devices or host this machine for desktop and browser access."
                } else {
                    "Workspace defaults for terminals, AI tools, and data handling."
                }
            }
            Self::UiPreview(_) => {
                "Read-only stories for iterating on native UI without touching live data."
            }
            Self::Project(_) => "Project identity, notes, and workspace defaults.",
            Self::Folder(_) => "Folder path, env settings, and detected commands.",
            Self::Command(_) => "Process command, env overrides, and restart behavior.",
            Self::Ssh(_) => "Connection details for remote terminal sessions.",
        }
    }

    pub fn accent_color(&self) -> u32 {
        match self {
            Self::Settings(_) => theme::PRIMARY,
            Self::UiPreview(_) => 0x14b8a6,
            Self::Project(draft) => {
                theme::parse_hex_color(Some(draft.color.as_str()), theme::PROJECT_DOT)
            }
            Self::Folder(_) => theme::PROJECT_DOT,
            Self::Command(_) => theme::AI_DOT,
            Self::Ssh(_) => theme::SSH_DOT,
        }
    }

    pub fn headline(&self) -> String {
        match self {
            Self::Settings(draft) => {
                if draft.remote_focus_only {
                    "Remote access".to_string()
                } else {
                    "Workspace settings".to_string()
                }
            }
            Self::UiPreview(_) => "Native UI preview lab".to_string(),
            Self::Project(draft) => fallback_editor_label(draft.name.as_str(), "Untitled project"),
            Self::Folder(draft) => fallback_editor_label(
                if draft.name.trim().is_empty() {
                    path_leaf(draft.folder_path.as_str())
                } else {
                    draft.name.as_str()
                },
                "Folder",
            ),
            Self::Command(draft) => fallback_editor_label(
                if draft.label.trim().is_empty() {
                    draft.command.as_str()
                } else {
                    draft.label.as_str()
                },
                "Command",
            ),
            Self::Ssh(draft) => fallback_editor_label(
                if draft.label.trim().is_empty() {
                    draft.host.as_str()
                } else {
                    draft.label.as_str()
                },
                "SSH connection",
            ),
        }
    }

    pub fn context_line(&self) -> Option<String> {
        match self {
            Self::Settings(_) => None,
            Self::UiPreview(_) => Some(
                "Seeded states only. Actions are intentionally disabled in this surface."
                    .to_string(),
            ),
            Self::Project(draft) => non_empty_value(draft.root_path.as_str()),
            Self::Folder(draft) => non_empty_value(draft.folder_path.as_str()),
            Self::Command(draft) => {
                let command = draft.command.trim();
                if command.is_empty() {
                    None
                } else if draft.args_text.trim().is_empty() {
                    Some(command.to_string())
                } else {
                    Some(format!("{command} {}", draft.args_text.trim()))
                }
            }
            Self::Ssh(draft) => {
                let host = draft.host.trim();
                let username = draft.username.trim();
                if host.is_empty() {
                    None
                } else if username.is_empty() {
                    Some(host.to_string())
                } else {
                    Some(format!("{username}@{host}"))
                }
            }
        }
    }

    pub fn summary_items(&self) -> Vec<(String, String)> {
        match self {
            Self::Settings(_) => Vec::new(),
            Self::UiPreview(_) => vec![
                ("Stories".to_string(), "6".to_string()),
                ("Mode".to_string(), "Design".to_string()),
                ("Safety".to_string(), "Read-only".to_string()),
            ],
            Self::Project(draft) => vec![
                (
                    "Accent".to_string(),
                    if draft.color.trim().is_empty() {
                        "Default".to_string()
                    } else {
                        draft.color.trim().to_string()
                    },
                ),
                (
                    "Sidebar".to_string(),
                    if draft.pinned {
                        "Pinned".to_string()
                    } else {
                        "Standard".to_string()
                    },
                ),
                (
                    "Logs".to_string(),
                    if draft.save_log_files {
                        "Saved".to_string()
                    } else {
                        "Off".to_string()
                    },
                ),
            ],
            Self::Folder(draft) => vec![
                (
                    "Visibility".to_string(),
                    if draft.hidden {
                        "Hidden".to_string()
                    } else {
                        "Visible".to_string()
                    },
                ),
                (
                    "Port".to_string(),
                    if draft.port_variable.trim().is_empty() {
                        "Not set".to_string()
                    } else {
                        draft.port_variable.trim().to_string()
                    },
                ),
                (
                    "Git".to_string(),
                    draft
                        .git_branch
                        .clone()
                        .unwrap_or_else(|| "No repo".to_string()),
                ),
            ],
            Self::Command(draft) => vec![
                (
                    "Port".to_string(),
                    if draft.port_text.trim().is_empty() {
                        "Not set".to_string()
                    } else {
                        draft.port_text.trim().to_string()
                    },
                ),
                (
                    "Restart".to_string(),
                    if draft.auto_restart {
                        "Auto".to_string()
                    } else {
                        "Manual".to_string()
                    },
                ),
                (
                    "Logs".to_string(),
                    if draft.clear_logs_on_restart {
                        "Clear on restart".to_string()
                    } else {
                        "Keep history".to_string()
                    },
                ),
            ],
            Self::Ssh(draft) => vec![
                (
                    "Host".to_string(),
                    if draft.host.trim().is_empty() {
                        "Not set".to_string()
                    } else {
                        draft.host.trim().to_string()
                    },
                ),
                (
                    "Port".to_string(),
                    if draft.port_text.trim().is_empty() {
                        "22".to_string()
                    } else {
                        draft.port_text.trim().to_string()
                    },
                ),
                (
                    "Password".to_string(),
                    if draft.password.trim().is_empty() {
                        "Not saved".to_string()
                    } else {
                        "Saved".to_string()
                    },
                ),
            ],
        }
    }

    pub fn save_label(&self) -> &'static str {
        match self {
            Self::Settings(_) => "Close",
            Self::UiPreview(_) => "Close",
            Self::Project(_) => "Save Project",
            Self::Folder(draft) => {
                if draft.existing_id.is_some() {
                    "Save Folder"
                } else {
                    "Create Folder"
                }
            }
            Self::Command(draft) => {
                if draft.existing_id.is_some() {
                    "Save Command"
                } else {
                    "Create Command"
                }
            }
            Self::Ssh(draft) => {
                if draft.existing_id.is_some() {
                    "Save SSH"
                } else {
                    "Create SSH"
                }
            }
        }
    }

    pub fn show_delete(&self) -> bool {
        match self {
            Self::Settings(_) => false,
            Self::UiPreview(_) => false,
            Self::Project(draft) => draft.existing_id.is_some(),
            Self::Folder(draft) => draft.existing_id.is_some(),
            Self::Command(draft) => draft.existing_id.is_some(),
            Self::Ssh(draft) => draft.existing_id.is_some(),
        }
    }

    pub fn text_value(&self, field: EditorField) -> Option<&String> {
        match (self, field) {
            (Self::Settings(draft), EditorField::Settings(SettingsField::Theme)) => {
                Some(&draft.theme)
            }
            (Self::UiPreview(_), _) => None,
            (Self::Settings(draft), EditorField::Settings(SettingsField::LogBufferSize)) => {
                Some(&draft.log_buffer_size)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::ClaudeCommand)) => {
                Some(&draft.claude_command)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::CodexCommand)) => {
                Some(&draft.codex_command)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::TerminalFontSize)) => {
                Some(&draft.terminal_font_size)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::GitHubToken)) => {
                Some(&draft.github_token)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::RemoteBindAddress)) => {
                Some(&draft.remote_bind_address)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::RemotePort)) => {
                Some(&draft.remote_port)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::RemoteWebPort)) => {
                Some(&draft.remote_web_port)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::RemoteConnectAddress)) => {
                Some(&draft.remote_connect_address)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::RemoteConnectPort)) => {
                Some(&draft.remote_connect_port)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::RemoteConnectToken)) => {
                Some(&draft.remote_connect_token)
            }
            (Self::Project(draft), EditorField::Project(ProjectField::Name)) => Some(&draft.name),
            (Self::Project(draft), EditorField::Project(ProjectField::RootPath)) => {
                Some(&draft.root_path)
            }
            (Self::Project(draft), EditorField::Project(ProjectField::Color)) => Some(&draft.color),
            (Self::Project(draft), EditorField::Project(ProjectField::Notes)) => Some(&draft.notes),
            (Self::Folder(draft), EditorField::Folder(FolderField::Name)) => Some(&draft.name),
            (Self::Folder(draft), EditorField::Folder(FolderField::FolderPath)) => {
                Some(&draft.folder_path)
            }
            (Self::Folder(draft), EditorField::Folder(FolderField::EnvFilePath)) => {
                Some(&draft.env_file_path)
            }
            (Self::Folder(draft), EditorField::Folder(FolderField::PortVariable)) => {
                Some(&draft.port_variable)
            }
            (Self::Folder(draft), EditorField::Folder(FolderField::EnvContents)) => {
                Some(&draft.env_file_contents)
            }
            (Self::Command(draft), EditorField::Command(CommandField::Label)) => Some(&draft.label),
            (Self::Command(draft), EditorField::Command(CommandField::Command)) => {
                Some(&draft.command)
            }
            (Self::Command(draft), EditorField::Command(CommandField::Args)) => {
                Some(&draft.args_text)
            }
            (Self::Command(draft), EditorField::Command(CommandField::Env)) => {
                Some(&draft.env_text)
            }
            (Self::Command(draft), EditorField::Command(CommandField::Port)) => {
                Some(&draft.port_text)
            }
            (Self::Ssh(draft), EditorField::Ssh(SshField::Label)) => Some(&draft.label),
            (Self::Ssh(draft), EditorField::Ssh(SshField::Host)) => Some(&draft.host),
            (Self::Ssh(draft), EditorField::Ssh(SshField::Port)) => Some(&draft.port_text),
            (Self::Ssh(draft), EditorField::Ssh(SshField::Username)) => Some(&draft.username),
            (Self::Ssh(draft), EditorField::Ssh(SshField::Password)) => Some(&draft.password),
            _ => None,
        }
    }

    pub fn text_value_mut(&mut self, field: EditorField) -> Option<&mut String> {
        match (self, field) {
            (Self::Settings(draft), EditorField::Settings(SettingsField::Theme)) => {
                Some(&mut draft.theme)
            }
            (Self::UiPreview(_), _) => None,
            (Self::Settings(draft), EditorField::Settings(SettingsField::LogBufferSize)) => {
                Some(&mut draft.log_buffer_size)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::ClaudeCommand)) => {
                Some(&mut draft.claude_command)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::CodexCommand)) => {
                Some(&mut draft.codex_command)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::TerminalFontSize)) => {
                Some(&mut draft.terminal_font_size)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::GitHubToken)) => {
                Some(&mut draft.github_token)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::RemoteBindAddress)) => {
                Some(&mut draft.remote_bind_address)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::RemotePort)) => {
                Some(&mut draft.remote_port)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::RemoteWebPort)) => {
                Some(&mut draft.remote_web_port)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::RemoteConnectAddress)) => {
                Some(&mut draft.remote_connect_address)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::RemoteConnectPort)) => {
                Some(&mut draft.remote_connect_port)
            }
            (Self::Settings(draft), EditorField::Settings(SettingsField::RemoteConnectToken)) => {
                Some(&mut draft.remote_connect_token)
            }
            (Self::Project(draft), EditorField::Project(ProjectField::Name)) => {
                Some(&mut draft.name)
            }
            (Self::Project(draft), EditorField::Project(ProjectField::RootPath)) => {
                Some(&mut draft.root_path)
            }
            (Self::Project(draft), EditorField::Project(ProjectField::Color)) => {
                Some(&mut draft.color)
            }
            (Self::Project(draft), EditorField::Project(ProjectField::Notes)) => {
                Some(&mut draft.notes)
            }
            (Self::Folder(draft), EditorField::Folder(FolderField::Name)) => Some(&mut draft.name),
            (Self::Folder(draft), EditorField::Folder(FolderField::FolderPath)) => {
                Some(&mut draft.folder_path)
            }
            (Self::Folder(draft), EditorField::Folder(FolderField::EnvFilePath)) => {
                Some(&mut draft.env_file_path)
            }
            (Self::Folder(draft), EditorField::Folder(FolderField::PortVariable)) => {
                Some(&mut draft.port_variable)
            }
            (Self::Folder(draft), EditorField::Folder(FolderField::EnvContents)) => {
                Some(&mut draft.env_file_contents)
            }
            (Self::Command(draft), EditorField::Command(CommandField::Label)) => {
                Some(&mut draft.label)
            }
            (Self::Command(draft), EditorField::Command(CommandField::Command)) => {
                Some(&mut draft.command)
            }
            (Self::Command(draft), EditorField::Command(CommandField::Args)) => {
                Some(&mut draft.args_text)
            }
            (Self::Command(draft), EditorField::Command(CommandField::Env)) => {
                Some(&mut draft.env_text)
            }
            (Self::Command(draft), EditorField::Command(CommandField::Port)) => {
                Some(&mut draft.port_text)
            }
            (Self::Ssh(draft), EditorField::Ssh(SshField::Label)) => Some(&mut draft.label),
            (Self::Ssh(draft), EditorField::Ssh(SshField::Host)) => Some(&mut draft.host),
            (Self::Ssh(draft), EditorField::Ssh(SshField::Port)) => Some(&mut draft.port_text),
            (Self::Ssh(draft), EditorField::Ssh(SshField::Username)) => Some(&mut draft.username),
            (Self::Ssh(draft), EditorField::Ssh(SshField::Password)) => Some(&mut draft.password),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum RemoteTopTab {
    Connect,
    Host,
    Log,
}

impl RemoteTopTab {
    fn label(self) -> &'static str {
        match self {
            Self::Connect => "Connect",
            Self::Host => "Host",
            Self::Log => "Log",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SettingsDraft {
    pub remote_focus_only: bool,
    pub remote_active_tab: RemoteTopTab,
    pub default_terminal: DefaultTerminal,
    pub mac_terminal_profile: MacTerminalProfile,
    pub theme: String,
    pub log_buffer_size: String,
    pub claude_command: String,
    pub codex_command: String,
    pub notification_sound: String,
    pub confirm_on_close: bool,
    pub minimize_to_tray: bool,
    pub restore_session_on_start: bool,
    pub terminal_font_size: String,
    pub option_as_meta: bool,
    pub copy_on_select: bool,
    pub keep_selection_on_copy: bool,
    pub show_terminal_scrollbar: bool,
    pub shell_integration_enabled: bool,
    pub terminal_mouse_override: bool,
    pub terminal_read_only: bool,
    pub github_token: String,
    pub remote_host_enabled: bool,
    pub remote_bind_address: String,
    pub remote_port: String,
    pub remote_web_port: String,
    pub remote_pairing_token: String,
    pub remote_connect_address: String,
    pub remote_connect_port: String,
    pub remote_connect_token: String,
    pub remote_connect_in_flight: bool,
    pub remote_connect_status: Option<String>,
    pub remote_connect_status_is_error: bool,
    pub remote_connected_label: Option<String>,
    pub remote_connected_endpoint: Option<String>,
    pub remote_connected_server_id: Option<String>,
    pub remote_connected_fingerprint: Option<String>,
    pub remote_latency_summary: Option<String>,
    pub remote_reconnect_attempts: u32,
    pub remote_reconnect_last_error: Option<String>,
    pub remote_has_control: bool,
    pub remote_connected: bool,
    pub remote_host_clients: usize,
    pub remote_host_controller_client_id: Option<String>,
    pub remote_host_listening: bool,
    pub remote_host_error: Option<String>,
    pub remote_host_last_note: Option<String>,
    pub remote_host_last_note_is_error: bool,
    pub remote_host_latency_summary: Option<String>,
    pub remote_host_server_id: String,
    pub remote_host_fingerprint: String,
    pub remote_port_forwards: Vec<RemotePortForwardDraft>,
    pub remote_known_hosts: Vec<KnownRemoteHost>,
    pub remote_paired_clients: Vec<PairedRemoteClient>,
    pub remote_web_enabled: bool,
    pub remote_web_pairing_token: String,
    pub remote_web_listener_url: Option<String>,
    pub remote_web_listener_error: Option<String>,
    pub remote_web_paired_clients: usize,
    pub remote_web_paired_clients_detail: Vec<PairedWebClient>,
    pub remote_access_activity_log: Vec<RemoteAccessActivityEvent>,
    pub open_picker: Option<SettingsPicker>,
}

#[derive(Debug, Clone)]
pub struct RemotePortForwardDraft {
    pub label: String,
    pub status: String,
    pub detail: Option<String>,
    pub is_error: bool,
}

#[derive(Debug, Clone, Default)]
pub struct UiPreviewDraft;

#[derive(Debug, Clone)]
pub struct ProjectDraft {
    pub existing_id: Option<String>,
    pub name: String,
    pub root_path: String,
    pub color: String,
    pub pinned: bool,
    pub save_log_files: bool,
    pub notes: String,
}

#[derive(Debug, Clone)]
pub struct FolderDraft {
    pub project_id: String,
    pub existing_id: Option<String>,
    pub name: String,
    pub folder_path: String,
    pub env_file_path: String,
    pub env_file_contents: String,
    pub env_file_loaded: bool,
    pub port_variable: String,
    pub hidden: bool,
    pub git_branch: Option<String>,
    pub dependency_status: Option<DependencyStatus>,
    pub scan_result: Option<ScanResult>,
    pub selected_scanned_scripts: BTreeSet<String>,
    pub selected_scanned_port_variable: Option<String>,
    pub scan_message: Option<String>,
    pub is_scanning: bool,
}

#[derive(Debug, Clone)]
pub struct CommandDraft {
    pub project_id: String,
    pub folder_id: String,
    pub existing_id: Option<String>,
    pub label: String,
    pub command: String,
    pub args_text: String,
    pub env_text: String,
    pub port_text: String,
    pub auto_restart: bool,
    pub clear_logs_on_restart: bool,
}

#[derive(Debug, Clone)]
pub struct SshDraft {
    pub existing_id: Option<String>,
    pub label: String,
    pub host: String,
    pub port_text: String,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorField {
    Settings(SettingsField),
    Project(ProjectField),
    Folder(FolderField),
    Command(CommandField),
    Ssh(SshField),
}

impl EditorField {
    pub fn accepts_text(self) -> bool {
        true
    }

    pub fn allows_newlines(self) -> bool {
        matches!(
            self,
            Self::Project(ProjectField::Notes) | Self::Folder(FolderField::EnvContents)
        )
    }

    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            Self::Settings(
                SettingsField::LogBufferSize
                    | SettingsField::TerminalFontSize
                    | SettingsField::RemotePort
                    | SettingsField::RemoteWebPort
                    | SettingsField::RemoteConnectPort
            ) | Self::Command(CommandField::Port)
                | Self::Ssh(SshField::Port)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsField {
    Theme,
    LogBufferSize,
    ClaudeCommand,
    CodexCommand,
    TerminalFontSize,
    GitHubToken,
    RemoteBindAddress,
    RemotePort,
    RemoteWebPort,
    RemoteConnectAddress,
    RemoteConnectPort,
    RemoteConnectToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsPicker {
    Terminal,
    NotificationSound,
    DataActions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectField {
    Name,
    RootPath,
    Color,
    SaveLogFiles,
    Notes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderField {
    Name,
    FolderPath,
    EnvFilePath,
    EnvContents,
    PortVariable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandField {
    Label,
    Command,
    Args,
    Env,
    Port,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshField {
    Label,
    Host,
    Port,
    Username,
    Password,
}

#[derive(Debug, Clone)]
pub struct EditorPaneModel {
    pub panel: EditorPanel,
    pub active_field: Option<EditorField>,
    pub cursor: usize,
    pub selection_anchor: Option<usize>,
    pub notice: Option<String>,
    pub updater: UpdaterSnapshot,
    pub allow_mutation: bool,
}

#[derive(Debug, Clone)]
pub enum EditorAction {
    FocusField(EditorField),
    Save,
    Delete,
    Close,
    PickFolderPath,
    ScanFolderPath,
    ToggleFolderScanScript(String),
    SelectFolderPortVariable(Option<String>),
    LoadFolderEnvFile,
    OpenFolderExternalTerminal,
    ExportConfig,
    ImportConfigMerge,
    ImportConfigReplace,
    CheckForUpdates,
    DownloadUpdate,
    InstallUpdate,
    OpenUiPreview,
    CycleDefaultTerminal,
    CycleMacTerminalProfile,
    CycleNotificationSound,
    PreviewNotificationSound,
    ToggleSettingsPicker(SettingsPicker),
    SelectDefaultTerminal(DefaultTerminal),
    SelectMacTerminalProfile(MacTerminalProfile),
    SelectNotificationSound(String),
    SetTerminalFontSize(u16),
    ToggleConfirmOnClose,
    ToggleMinimizeToTray,
    ToggleRestoreSession,
    ToggleOptionAsMeta,
    ToggleCopyOnSelect,
    ToggleKeepSelectionOnCopy,
    ToggleShowTerminalScrollbar,
    ToggleShellIntegrationEnabled,
    ToggleTerminalMouseOverride,
    ToggleTerminalReadOnly,
    SelectRemoteTopTab(RemoteTopTab),
    ToggleRemoteHosting,
    RegenerateRemotePairingToken,
    ToggleRemoteWebHosting,
    RegenerateRemoteWebPairingToken,
    ResetRemoteWebAccess,
    CopyRemoteWebPairingToken,
    CopyRemoteWebInviteLink,
    CopyRemotePairingToken,
    ConnectRemoteHost,
    DisconnectRemoteHost,
    TakeRemoteControl,
    ReleaseRemoteControl,
    TakeHostControl,
    UseKnownRemoteHost(String),
    ForgetKnownRemoteHost(String),
    RevokeRemoteClient(String),
    RevokeRemoteWebClient(String),
    ToggleProjectPinned,
    ToggleProjectSaveLogs,
    ToggleFolderHidden,
    ToggleCommandAutoRestart,
    ToggleCommandClearLogs,
}

pub struct EditorActions {
    pub on_action: Arc<dyn Fn(EditorAction) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_focus_at:
        Arc<dyn Fn(EditorField, usize) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_drag_to:
        Arc<dyn Fn(EditorField, usize) -> Box<dyn Fn(&MouseMoveEvent, &mut Window, &mut App)>>,
}

pub fn render_editor_surface(model: &EditorPaneModel, actions: EditorActions) -> AnyElement {
    if let EditorPanel::Settings(draft) = &model.panel {
        return render_settings_editor_surface(draft, model, &actions).into_any_element();
    }

    let title = model.panel.title();
    let subtitle = model.panel.subtitle();
    let save_label = model.allow_mutation.then(|| model.panel.save_label());
    let accent = model.panel.accent_color();

    let body: AnyElement = match &model.panel {
        EditorPanel::Settings(draft) => {
            render_settings_panel(draft, model, &actions).into_any_element()
        }
        EditorPanel::UiPreview(draft) => {
            render_ui_preview_panel(draft, model, &actions).into_any_element()
        }
        EditorPanel::Project(draft) => {
            render_project_panel(draft, model, &actions).into_any_element()
        }
        EditorPanel::Folder(draft) => {
            render_folder_panel(draft, model, &actions).into_any_element()
        }
        EditorPanel::Command(draft) => {
            render_command_panel(draft, model, &actions).into_any_element()
        }
        EditorPanel::Ssh(draft) => render_ssh_panel(draft, model, &actions).into_any_element(),
    };

    let on_close = (actions.on_action)(EditorAction::Close);
    let on_save = model
        .allow_mutation
        .then(|| (actions.on_action)(EditorAction::Save));
    let on_delete = (model.allow_mutation && model.panel.show_delete())
        .then(|| (actions.on_action)(EditorAction::Delete));
    let show_intro = matches!(model.panel, EditorPanel::UiPreview(_));

    div()
        .flex_1()
        .h_full()
        .flex()
        .flex_col()
        .bg(rgb(theme::APP_BG))
        .child(render_editor_toolbar(
            title, subtitle, accent, save_label, on_save, on_delete, on_close,
        ))
        .child(
            div()
                .flex_1()
                .id("editor-panel-scroll")
                .overflow_y_scroll()
                .scrollbar_width(px(6.0))
                .child(
                    div().w_full().flex().justify_center().child(
                        div()
                            .w_full()
                            .max_w(px(760.0))
                            .pt(px(if show_intro { 28.0 } else { 20.0 }))
                            .pb(px(56.0))
                            .px(px(24.0))
                            .flex()
                            .flex_col()
                            .gap(px(18.0))
                            .children(model.notice.as_ref().map(|notice| {
                                render_notice_row(notice.as_str()).into_any_element()
                            }))
                            .children(
                                show_intro
                                    .then(|| render_editor_intro(&model.panel).into_any_element()),
                            )
                            .child(body),
                    ),
                ),
        )
        .into_any_element()
}

pub fn next_default_terminal(current: DefaultTerminal) -> DefaultTerminal {
    match current {
        DefaultTerminal::Bash => DefaultTerminal::Powershell,
        DefaultTerminal::Powershell => DefaultTerminal::Cmd,
        DefaultTerminal::Cmd => DefaultTerminal::Bash,
    }
}

pub fn default_terminal_label(value: &DefaultTerminal) -> &'static str {
    match value {
        DefaultTerminal::Bash => "Bash (Git Bash)",
        DefaultTerminal::Powershell => "PowerShell",
        DefaultTerminal::Cmd => "CMD",
    }
}

pub fn next_mac_terminal_profile(current: MacTerminalProfile) -> MacTerminalProfile {
    match current {
        MacTerminalProfile::System => MacTerminalProfile::Zsh,
        MacTerminalProfile::Zsh => MacTerminalProfile::Bash,
        MacTerminalProfile::Bash => MacTerminalProfile::System,
    }
}

pub fn mac_terminal_profile_label(value: &MacTerminalProfile) -> &'static str {
    match value {
        MacTerminalProfile::System => "User shell",
        MacTerminalProfile::Zsh => "zsh",
        MacTerminalProfile::Bash => "bash",
    }
}

pub fn notification_sound_options() -> &'static [&'static str] {
    &[
        "glass", "chord", "glisten", "polite", "calm", "sharp", "jinja", "cloud", "none",
    ]
}

pub fn notification_sound_label(value: &str) -> &'static str {
    match value {
        "glass" => "Glass",
        "chord" => "Chord",
        "glisten" => "Glisten",
        "polite" => "Polite",
        "calm" => "Calm",
        "sharp" => "Sharp",
        "jinja" => "Jinja",
        "cloud" => "Cloud",
        "none" => "None",
        _ => "Glass",
    }
}

pub fn next_notification_sound(current: &str) -> String {
    let options = notification_sound_options();
    let index = options
        .iter()
        .position(|option| *option == current)
        .unwrap_or(0);
    options[(index + 1) % options.len()].to_string()
}

fn render_settings_editor_surface(
    draft: &SettingsDraft,
    model: &EditorPaneModel,
    actions: &EditorActions,
) -> impl IntoElement {
    let on_close = (actions.on_action)(EditorAction::Close);

    div()
        .flex_1()
        .h_full()
        .flex()
        .flex_col()
        .bg(rgb(theme::APP_BG))
        .child(
            div()
                .h(px(22.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_between()
                .px(px(6.0))
                .bg(rgb(theme::TOPBAR_BG))
                .border_b_1()
                .border_color(rgb(theme::BORDER_PRIMARY))
                .child(
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::TEXT_PRIMARY))
                        .child(if draft.remote_focus_only {
                            "Remote"
                        } else {
                            "Settings"
                        }),
                )
                .child(render_settings_close_button(on_close)),
        )
        .child(
            div()
                .flex_1()
                .id("settings-panel-scroll")
                .overflow_y_scroll()
                .scrollbar_width(px(6.0))
                .child(
                    div().w_full().flex().justify_center().child(
                        div()
                            .w_full()
                            .max_w(px(540.0))
                            .pt(px(24.0))
                            .pb(px(40.0))
                            .px(px(20.0))
                            .child(render_settings_panel(draft, model, actions)),
                    ),
                ),
        )
}

fn render_settings_panel(
    draft: &SettingsDraft,
    model: &EditorPaneModel,
    actions: &EditorActions,
) -> impl IntoElement {
    let is_mac = cfg!(target_os = "macos");
    let on_toggle_terminal_picker =
        (actions.on_action)(EditorAction::ToggleSettingsPicker(SettingsPicker::Terminal));
    let on_toggle_sound_picker = (actions.on_action)(EditorAction::ToggleSettingsPicker(
        SettingsPicker::NotificationSound,
    ));
    let on_preview_sound = (actions.on_action)(EditorAction::PreviewNotificationSound);
    let on_toggle_confirm = (actions.on_action)(EditorAction::ToggleConfirmOnClose);
    let on_toggle_tray = (actions.on_action)(EditorAction::ToggleMinimizeToTray);
    let on_toggle_restore = (actions.on_action)(EditorAction::ToggleRestoreSession);
    let on_export = (actions.on_action)(EditorAction::ExportConfig);
    let on_import_merge = (actions.on_action)(EditorAction::ImportConfigMerge);
    let on_import_replace = (actions.on_action)(EditorAction::ImportConfigReplace);
    let on_check_updates = (actions.on_action)(EditorAction::CheckForUpdates);
    let on_install_update = matches!(model.updater.stage, UpdaterStage::ReadyToInstall)
        .then(|| (actions.on_action)(EditorAction::InstallUpdate));
    let on_toggle_data_picker = (actions.on_action)(EditorAction::ToggleSettingsPicker(
        SettingsPicker::DataActions,
    ));
    let on_open_ui_preview = (actions.on_action)(EditorAction::OpenUiPreview);
    let terminal_options: Vec<AnyElement> = if is_mac {
        [
            MacTerminalProfile::System,
            MacTerminalProfile::Zsh,
            MacTerminalProfile::Bash,
        ]
        .into_iter()
        .map(|profile| {
            render_settings_dropdown_option(
                mac_terminal_profile_label(&profile).to_string(),
                draft.mac_terminal_profile == profile,
                (actions.on_action)(EditorAction::SelectMacTerminalProfile(profile)),
            )
            .into_any_element()
        })
        .collect()
    } else {
        [
            DefaultTerminal::Bash,
            DefaultTerminal::Powershell,
            DefaultTerminal::Cmd,
        ]
        .into_iter()
        .map(|terminal| {
            render_settings_dropdown_option(
                default_terminal_label(&terminal).to_string(),
                draft.default_terminal == terminal,
                (actions.on_action)(EditorAction::SelectDefaultTerminal(terminal)),
            )
            .into_any_element()
        })
        .collect()
    };

    let sound_options: Vec<AnyElement> = notification_sound_options()
        .iter()
        .map(|sound_id| {
            render_settings_dropdown_option(
                notification_sound_label(sound_id).to_string(),
                draft.notification_sound.eq_ignore_ascii_case(sound_id),
                (actions.on_action)(EditorAction::SelectNotificationSound(
                    (*sound_id).to_string(),
                )),
            )
            .into_any_element()
        })
        .collect();

    let mut sections = Vec::new();

    if let Some(notice) = model.notice.as_ref() {
        sections.push(
            FormSection::new("Status")
                .field(FormField::notice(notice.clone(), SurfaceTone::Accent)),
        );
    }

    if !draft.remote_focus_only {
        sections.push(FormSection::new("App").fields(vec![
            FormField::toggle(
                "Confirm before closing",
                draft.confirm_on_close,
                "Warn when servers are still running.",
                on_toggle_confirm,
            ),
            FormField::toggle(
                "Minimize instead of close",
                draft.minimize_to_tray,
                minimize_to_tray_hint(),
                on_toggle_tray,
            ),
            FormField::toggle(
                "Restore previous session",
                draft.restore_session_on_start,
                "Reopen tabs and sidebar state on launch.",
                on_toggle_restore,
            ),
        ]));
    }

    let mut terminal_fields = vec![
        FormField::custom(
            render_settings_select_row(
                if is_mac {
                    "Default terminal shell"
                } else {
                    "Default terminal"
                },
                if is_mac {
                    "Shell used for interactive and AI terminals on macOS."
                } else {
                    "Shell used for interactive and AI terminals."
                },
                if is_mac {
                    mac_terminal_profile_label(&draft.mac_terminal_profile)
                } else {
                    default_terminal_label(&draft.default_terminal)
                },
                draft.open_picker == Some(SettingsPicker::Terminal),
                on_toggle_terminal_picker,
                None,
                Some(220.0),
                terminal_options,
            )
            .into_any_element(),
        ),
        FormField::custom(render_settings_font_size_row(draft, actions).into_any_element()),
    ];

    if is_mac {
        terminal_fields.push(FormField::toggle(
            "Option acts as Meta",
            draft.option_as_meta,
            "Use Option as terminal Meta/Alt instead of character input.",
            (actions.on_action)(EditorAction::ToggleOptionAsMeta),
        ));
    }

    terminal_fields.extend([
        FormField::custom(
            render_settings_text_input(
                "Log history",
                "Max lines kept per terminal.",
                draft.log_buffer_size.as_str(),
                EditorField::Settings(SettingsField::LogBufferSize),
                model,
                actions,
                Some(140.0),
                "10000",
            )
            .into_any_element(),
        ),
        FormField::toggle(
            "Copy on select",
            draft.copy_on_select,
            "Copy terminal selections to the clipboard.",
            (actions.on_action)(EditorAction::ToggleCopyOnSelect),
        ),
        FormField::toggle(
            "Keep selection after copy",
            draft.keep_selection_on_copy,
            "Leave the current selection highlighted after copy.",
            (actions.on_action)(EditorAction::ToggleKeepSelectionOnCopy),
        ),
        FormField::toggle(
            "Show terminal scrollbar",
            draft.show_terminal_scrollbar,
            "Keep a visible scroll indicator on terminal tabs.",
            (actions.on_action)(EditorAction::ToggleShowTerminalScrollbar),
        ),
        FormField::toggle(
            "Enable shell integration",
            draft.shell_integration_enabled,
            "Allow Ghostty-style shell markers for supported shells.",
            (actions.on_action)(EditorAction::ToggleShellIntegrationEnabled),
        ),
        FormField::toggle(
            "Override app mouse capture",
            draft.terminal_mouse_override,
            "Prefer selection and scrolling over terminal mouse reporting.",
            (actions.on_action)(EditorAction::ToggleTerminalMouseOverride),
        ),
        FormField::toggle(
            "Read-only terminal",
            draft.terminal_read_only,
            "Block accidental typing and pasting into terminal tabs.",
            (actions.on_action)(EditorAction::ToggleTerminalReadOnly),
        ),
        FormField::custom(
            render_settings_select_row(
                "Notification sound",
                "Played when an AI terminal finishes a long task.",
                notification_sound_label(&draft.notification_sound),
                draft.open_picker == Some(SettingsPicker::NotificationSound),
                on_toggle_sound_picker,
                Some(
                    render_settings_inline_button("Test", false, on_preview_sound)
                        .into_any_element(),
                ),
                Some(180.0),
                sound_options,
            )
            .into_any_element(),
        ),
    ]);

    if !draft.remote_focus_only {
        sections.push(FormSection::new("Terminal").fields(terminal_fields));

        sections.push(FormSection::new("AI").fields(vec![
                FormField::custom(
                    render_settings_text_input(
                        "Claude command",
                        "Command used for Claude terminals.",
                        draft.claude_command.as_str(),
                        EditorField::Settings(SettingsField::ClaudeCommand),
                        model,
                        actions,
                        None,
                        "npx -y @anthropic-ai/claude-code@latest --dangerously-skip-permissions",
                    )
                    .into_any_element(),
                ),
                FormField::custom(
                    render_settings_text_input(
                        "Codex command",
                        "Command used for Codex terminals.",
                        draft.codex_command.as_str(),
                        EditorField::Settings(SettingsField::CodexCommand),
                        model,
                        actions,
                        None,
                        "npx -y @openai/codex@latest --dangerously-bypass-approvals-and-sandbox",
                    )
                    .into_any_element(),
                ),
                FormField::custom(
                    render_settings_text_input(
                        "GitHub token",
                        "Personal access token for AI commit messages and GitHub API.",
                        draft.github_token.as_str(),
                        EditorField::Settings(SettingsField::GitHubToken),
                        model,
                        actions,
                        None,
                        "ghp_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
                    )
                    .into_any_element(),
                ),
            ]));

        sections.push(
            FormSection::new("Updates").field(FormField::custom(
                render_updater_panel(&model.updater, on_check_updates, None, on_install_update)
                    .into_any_element(),
            )),
        );

        sections.push(FormSection::new("Configuration").fields({
            let mut fields = vec![FormField::action(
                FormAction::new(
                    "Configuration tools",
                    if draft.open_picker == Some(SettingsPicker::DataActions) {
                        "Hide"
                    } else {
                        "Show"
                    },
                    on_toggle_data_picker,
                )
                .description("Import or export config.json."),
            )];

            if draft.open_picker == Some(SettingsPicker::DataActions) {
                fields.push(FormField::action_group(
                    FormActionGroup::new("Config actions")
                        .action(
                            FormAction::new("Export current config", "Export", on_export)
                                .description("Write the current config to disk."),
                        )
                        .action(
                            FormAction::new("Import and merge", "Merge", on_import_merge)
                                .description(
                                    "Merge imported projects and settings into the current config.",
                                ),
                        )
                        .action(
                            FormAction::new("Import and replace", "Replace", on_import_replace)
                                .description("Replace the current config with the imported file.")
                                .style(SurfaceActionButtonStyle::Danger),
                        ),
                ));
            }

            fields
        }));
    }

    if draft.remote_focus_only {
        return render_remote_tabbed_panel(draft, model, actions);
    }

    if !draft.remote_focus_only {
        sections.push(
            FormSection::new("Developer").field(FormField::action(
                FormAction::new("Open UI preview", "Open", on_open_ui_preview)
                    .description(
                        "Inspect seeded editor and settings states without touching live data.",
                    )
                    .badge(SurfaceBadge::new("Read-only", SurfaceTone::Muted)),
            )),
        );
    }

    render_form_sections(sections, model, actions)
}

#[cfg(test)]
fn build_remote_dashboard_sections(
    draft: &SettingsDraft,
    model: &EditorPaneModel,
    actions: &EditorActions,
) -> Vec<FormSection> {
    let mut sections = build_remote_tab_sections(draft, RemoteTopTab::Connect, model, actions);
    sections.extend(build_remote_tab_sections(
        draft,
        RemoteTopTab::Host,
        model,
        actions,
    ));
    sections.extend(build_remote_tab_sections(
        draft,
        RemoteTopTab::Log,
        model,
        actions,
    ));
    sections
}

fn build_remote_tab_sections(
    draft: &SettingsDraft,
    tab: RemoteTopTab,
    model: &EditorPaneModel,
    actions: &EditorActions,
) -> Vec<FormSection> {
    match tab {
        RemoteTopTab::Connect => build_remote_current_session_sections(draft, model, actions),
        RemoteTopTab::Host => {
            let mut sections = vec![
                build_remote_this_device_section(draft, model, actions),
                build_remote_browser_access_section(draft, model, actions),
            ];
            if let Some(section) = build_remote_advanced_section(draft) {
                sections.push(section);
            }
            sections
        }
        RemoteTopTab::Log => vec![build_remote_access_log_section(draft, actions)],
    }
}

fn render_remote_tabbed_panel(
    draft: &SettingsDraft,
    model: &EditorPaneModel,
    actions: &EditorActions,
) -> AnyElement {
    let sections =
        build_remote_tab_sections(draft, draft.remote_active_tab.clone(), model, actions);
    div()
        .flex()
        .flex_col()
        .gap(px(18.0))
        .child(render_remote_top_tab_row(draft, actions))
        .child(render_remote_status_summary(draft, actions))
        .child(render_form_sections(sections, model, actions))
        .into_any_element()
}

fn render_remote_top_tab_row(draft: &SettingsDraft, actions: &EditorActions) -> impl IntoElement {
    div()
        .flex()
        .gap(px(8.0))
        .child(render_surface_action_button(
            RemoteTopTab::Connect.label(),
            if matches!(draft.remote_active_tab, RemoteTopTab::Connect) {
                SurfaceActionButtonStyle::Primary
            } else {
                SurfaceActionButtonStyle::Ghost
            },
            (actions.on_action)(EditorAction::SelectRemoteTopTab(RemoteTopTab::Connect)),
        ))
        .child(render_surface_action_button(
            RemoteTopTab::Host.label(),
            if matches!(draft.remote_active_tab, RemoteTopTab::Host) {
                SurfaceActionButtonStyle::Primary
            } else {
                SurfaceActionButtonStyle::Ghost
            },
            (actions.on_action)(EditorAction::SelectRemoteTopTab(RemoteTopTab::Host)),
        ))
        .child(render_surface_action_button(
            RemoteTopTab::Log.label(),
            if matches!(draft.remote_active_tab, RemoteTopTab::Log) {
                SurfaceActionButtonStyle::Primary
            } else {
                SurfaceActionButtonStyle::Ghost
            },
            (actions.on_action)(EditorAction::SelectRemoteTopTab(RemoteTopTab::Log)),
        ))
}

fn render_remote_status_summary(
    draft: &SettingsDraft,
    actions: &EditorActions,
) -> impl IntoElement {
    div()
        .flex()
        .gap(px(10.0))
        .child(render_remote_status_summary_info(
            "Connection",
            remote_connection_summary_value(draft),
        ))
        .child(render_remote_status_summary_toggle(
            "Desktop host",
            draft.remote_host_enabled,
            remote_host_summary_value(draft),
            (actions.on_action)(EditorAction::ToggleRemoteHosting),
        ))
        .child(render_remote_status_summary_toggle(
            "Browser access",
            draft.remote_web_enabled,
            remote_browser_summary_value(draft),
            (actions.on_action)(EditorAction::ToggleRemoteWebHosting),
        ))
}

fn render_remote_status_summary_info(label: &str, value: String) -> impl IntoElement {
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
                .child(SharedString::from(label.to_string())),
        )
        .child(
            div()
                .text_xs()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(rgb(theme::TEXT_PRIMARY))
                .child(SharedString::from(value)),
        )
}

fn render_remote_status_summary_toggle(
    label: &str,
    enabled: bool,
    value: String,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    let mut toggle = div()
        .w(px(32.0))
        .h(px(18.0))
        .p(px(2.0))
        .rounded_full()
        .flex()
        .items_center()
        .bg(rgb(if enabled {
            theme::PRIMARY
        } else {
            theme::BORDER_SECONDARY
        }));
    toggle = if enabled {
        toggle.justify_end()
    } else {
        toggle.justify_start()
    };

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
        .cursor_pointer()
        .hover(|s| {
            s.bg(rgb(theme::ROW_HOVER_BG))
                .border_color(rgb(theme::PRIMARY))
        })
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_SUBTLE))
                .child(SharedString::from(label.to_string())),
        )
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .child(
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(if enabled {
                            theme::PRIMARY
                        } else {
                            theme::TEXT_PRIMARY
                        }))
                        .child(SharedString::from(value)),
                )
                .child(toggle.child(div().size(px(14.0)).rounded_full().bg(rgb(0xffffff)))),
        )
        .on_mouse_down(MouseButton::Left, on_click)
}

fn remote_connection_summary_value(draft: &SettingsDraft) -> String {
    if draft.remote_connected {
        draft
            .remote_connected_label
            .clone()
            .unwrap_or_else(|| "Connected".to_string())
    } else if draft.remote_connect_in_flight {
        "Connecting...".to_string()
    } else {
        "Not connected".to_string()
    }
}

fn remote_host_summary_value(draft: &SettingsDraft) -> String {
    if draft.remote_host_enabled && draft.remote_host_listening {
        "Ready".to_string()
    } else if draft.remote_host_enabled {
        "Attention".to_string()
    } else {
        "Off".to_string()
    }
}

fn remote_browser_summary_value(draft: &SettingsDraft) -> String {
    if draft.remote_web_enabled && draft.remote_web_listener_error.is_none() {
        "On".to_string()
    } else if draft.remote_web_enabled {
        "Attention".to_string()
    } else {
        "Off".to_string()
    }
}

fn build_remote_current_session_sections(
    draft: &SettingsDraft,
    model: &EditorPaneModel,
    actions: &EditorActions,
) -> Vec<FormSection> {
    let mut fields = Vec::new();

    if draft.remote_connected {
        if let Some(status) = draft.remote_connect_status.as_ref() {
            fields.push(FormField::notice(
                status.clone(),
                if draft.remote_connect_status_is_error {
                    SurfaceTone::Danger
                } else if status.starts_with("Reconnecting to ") {
                    SurfaceTone::Warning
                } else {
                    SurfaceTone::Accent
                },
            ));
        }

        fields.push(FormField::Info(
            FormInfoField::new(
                "Connected host",
                draft
                    .remote_connected_label
                    .clone()
                    .unwrap_or_else(|| "Unknown host".to_string()),
                Some(
                    "This window is showing the workspace, terminals, and servers from that machine."
                        .to_string(),
                ),
            )
            .badge(SurfaceBadge::new(
                remote_session_mode_label(draft),
                remote_session_mode_tone(draft),
            )),
        ));

        if let Some(latency_summary) = draft.remote_latency_summary.as_ref() {
            fields.push(FormField::info(
                "Latency",
                latency_summary.clone(),
                Some("Recent transport and paint timings from this remote client.".to_string()),
            ));
        }

        if let Some(endpoint) = draft.remote_connected_endpoint.as_ref() {
            fields.push(FormField::info(
                "Endpoint",
                endpoint.clone(),
                Some("Address this client is currently connected to.".to_string()),
            ));
        }

        if let Some(server_id) = draft.remote_connected_server_id.as_ref() {
            fields.push(FormField::info(
                "Connected host id",
                server_id.clone(),
                Some("Stable identity for the connected DevManager host.".to_string()),
            ));
        }

        if let Some(fingerprint) = draft.remote_connected_fingerprint.as_ref() {
            fields.push(FormField::info(
                "Connected host fingerprint",
                fingerprint.clone(),
                Some("Pinned TLS fingerprint for the connected host.".to_string()),
            ));
        }

        let forwarded_ports_value = if draft.remote_port_forwards.is_empty() {
            "None".to_string()
        } else if draft.remote_port_forwards.len() == 1 {
            "1 forwarded port".to_string()
        } else {
            format!("{} forwarded ports", draft.remote_port_forwards.len())
        };
        fields.push(FormField::info(
            "Forwarded ports",
            forwarded_ports_value,
            Some("Remote host servers mirrored onto this client's localhost.".to_string()),
        ));

        fields.extend(draft.remote_port_forwards.iter().map(|forward| {
            if forward.is_error {
                FormField::notice(
                    format!("{} — {}", forward.label, forward.status),
                    SurfaceTone::Warning,
                )
            } else {
                FormField::info(
                    forward.label.clone(),
                    forward.status.clone(),
                    forward.detail.clone(),
                )
            }
        }));

        if draft.remote_reconnect_attempts > 0 {
            fields.push(FormField::info(
                "Reconnect attempts",
                draft.remote_reconnect_attempts.to_string(),
                Some(
                    "Automatic reconnect retries since the last successful connection.".to_string(),
                ),
            ));
        }

        if let Some(error) = draft.remote_reconnect_last_error.as_ref() {
            fields.push(FormField::notice(
                format!("Last transient error: {error}"),
                SurfaceTone::Warning,
            ));
        }

        fields.push(FormField::action_group(
            FormActionGroup::new("Session actions")
                .action(
                    FormAction::new(
                        "Disconnect from the current remote host",
                        "Disconnect",
                        (actions.on_action)(EditorAction::DisconnectRemoteHost),
                    )
                    .description("Return this window to its local workspace.")
                    .style(SurfaceActionButtonStyle::Danger),
                )
                .action(if draft.remote_has_control {
                    FormAction::new(
                        "Release control back to viewers",
                        "Release control",
                        (actions.on_action)(EditorAction::ReleaseRemoteControl),
                    )
                    .description("Stay connected, but stop typing and mutating the host.")
                } else {
                    FormAction::new(
                        "Take control of the connected host",
                        "Take control",
                        (actions.on_action)(EditorAction::TakeRemoteControl),
                    )
                    .description("Enable typing, editing, and process management on this client.")
                    .style(SurfaceActionButtonStyle::Primary)
                }),
        ));
    } else {
        if let Some(status) = draft.remote_connect_status.clone() {
            fields.push(FormField::notice(
                status,
                if draft.remote_connect_in_flight {
                    SurfaceTone::Accent
                } else if draft.remote_connect_status_is_error {
                    SurfaceTone::Danger
                } else {
                    SurfaceTone::Muted
                },
            ));
        } else if draft.remote_connect_in_flight {
            fields.push(FormField::notice("Connecting...", SurfaceTone::Accent));
        }

        let host_input = render_settings_text_input(
            "Host or IP",
            "",
            draft.remote_connect_address.as_str(),
            EditorField::Settings(SettingsField::RemoteConnectAddress),
            model,
            actions,
            None,
            "192.168.0.20",
        )
        .into_any_element();
        let port_input = render_settings_text_input(
            "Port",
            "",
            draft.remote_connect_port.as_str(),
            EditorField::Settings(SettingsField::RemoteConnectPort),
            model,
            actions,
            None,
            "43871",
        )
        .into_any_element();

        fields.push(FormField::custom(
            div()
                .flex()
                .gap(px(10.0))
                .items_end()
                .child(div().flex_1().min_w(px(0.0)).child(host_input))
                .child(div().w(px(110.0)).child(port_input))
                .into_any_element(),
        ));
        let token_input = render_settings_text_input(
            "Desktop pair token",
            "Only needed the first time this device pairs with a host.",
            draft.remote_connect_token.as_str(),
            EditorField::Settings(SettingsField::RemoteConnectToken),
            model,
            actions,
            None,
            "ABC123",
        )
        .into_any_element();
        let connect_button = render_surface_action_button(
            if draft.remote_connect_in_flight {
                "Connecting..."
            } else {
                "Connect"
            },
            SurfaceActionButtonStyle::Primary,
            (actions.on_action)(EditorAction::ConnectRemoteHost),
        )
        .into_any_element();
        fields.push(FormField::custom(
            div()
                .flex()
                .gap(px(10.0))
                .items_end()
                .child(div().flex_1().min_w(px(0.0)).child(token_input))
                .child(connect_button)
                .into_any_element(),
        ));
    }

    let mut sections = vec![FormSection::new("Current Session")
        .hint("Connect to another DevManager host or manage the remote session you already have.")
        .fields(fields)];

    if !draft.remote_connected {
        sections.push(build_remote_saved_hosts_section(draft, actions));
    }

    sections
}

fn build_remote_saved_hosts_section(draft: &SettingsDraft, actions: &EditorActions) -> FormSection {
    let mut fields: Vec<FormField> = Vec::new();
    if draft.remote_known_hosts.is_empty() {
        fields.push(FormField::empty_state(
            "No saved hosts yet",
            "After you connect once, trusted hosts will appear here for quick reconnects.",
            SurfaceTone::Muted,
        ));
    } else {
        fields.extend(draft.remote_known_hosts.iter().map(|host| {
            FormField::Info(
                FormInfoField::new("", format_saved_host_hint(host), None)
                    .action(
                        FormAction::new(
                            "Connect",
                            "Connect",
                            (actions.on_action)(EditorAction::UseKnownRemoteHost(
                                host.server_id.clone(),
                            )),
                        )
                        .style(SurfaceActionButtonStyle::Primary),
                    )
                    .action(
                        FormAction::new(
                            "Forget",
                            "Forget",
                            (actions.on_action)(EditorAction::ForgetKnownRemoteHost(
                                host.server_id.clone(),
                            )),
                        )
                        .style(SurfaceActionButtonStyle::Danger),
                    ),
            )
        }));
    }

    FormSection::new("Saved hosts")
        .hint("Quick reconnects for hosts this device already trusts.")
        .fields(fields)
}

fn build_remote_this_device_section(
    draft: &SettingsDraft,
    model: &EditorPaneModel,
    actions: &EditorActions,
) -> FormSection {
    let mut fields: Vec<FormField> = Vec::new();

    if let Some(error) = draft.remote_host_error.as_ref() {
        fields.push(FormField::notice(error.clone(), SurfaceTone::Danger));
    } else if draft.remote_host_enabled && !draft.remote_host_listening {
        fields.push(FormField::notice(
            "Sharing is enabled, but this device is not currently listening. Check the bind address, port, or firewall state."
                .to_string(),
            SurfaceTone::Danger,
        ));
    }

    fields.push(FormField::custom(
        render_settings_text_input(
            "Bind address",
            "Address the desktop listener binds to on this machine.",
            draft.remote_bind_address.as_str(),
            EditorField::Settings(SettingsField::RemoteBindAddress),
            model,
            actions,
            Some(200.0),
            "0.0.0.0",
        )
        .into_any_element(),
    ));

    fields.push(FormField::custom(
        render_settings_text_input(
            "Desktop port",
            "TCP port used by DevManager desktop sharing.",
            draft.remote_port.as_str(),
            EditorField::Settings(SettingsField::RemotePort),
            model,
            actions,
            Some(120.0),
            "43871",
        )
        .into_any_element(),
    ));

    fields.push(FormField::Info(
        FormInfoField::new(
            "Desktop pairing token",
            remote_pairing_token_display(draft),
            Some("First-time desktop clients enter this after the host and port.".to_string()),
        )
        .action(
            FormAction::new(
                "Copy the desktop pairing token",
                "Copy",
                (actions.on_action)(EditorAction::CopyRemotePairingToken),
            )
            .style(SurfaceActionButtonStyle::Primary),
        )
        .action(FormAction::new(
            "Generate a new desktop pairing token",
            "New token",
            (actions.on_action)(EditorAction::RegenerateRemotePairingToken),
        )),
    ));

    if let Some(latency_summary) = draft.remote_host_latency_summary.as_ref() {
        fields.push(FormField::info(
            "Host activity",
            latency_summary.clone(),
            Some("Recent terminal timing observed by the host.".to_string()),
        ));
    }

    if let Some(note) = draft.remote_host_last_note.as_ref() {
        fields.push(FormField::notice(
            note.clone(),
            if draft.remote_host_last_note_is_error {
                SurfaceTone::Danger
            } else {
                SurfaceTone::Muted
            },
        ));
    }

    if draft.remote_host_controller_client_id.is_some() {
        fields.push(FormField::action(
            FormAction::new(
                "Take local host control back from the active remote client",
                "Take local control",
                (actions.on_action)(EditorAction::TakeHostControl),
            )
            .description("Reclaim keyboard and mutation control for this machine.")
            .style(SurfaceActionButtonStyle::Primary),
        ));
    }

    FormSection::new("Native App Hosting")
        .hint("Share this machine with another desktop DevManager app.")
        .fields(fields)
}

fn build_remote_browser_access_section(
    draft: &SettingsDraft,
    model: &EditorPaneModel,
    actions: &EditorActions,
) -> FormSection {
    let mut fields: Vec<FormField> = Vec::new();

    if let Some(error) = draft.remote_web_listener_error.as_ref() {
        fields.push(FormField::notice(error.clone(), SurfaceTone::Danger));
    }

    let browser_ready = draft.remote_web_enabled && draft.remote_web_listener_error.is_none();

    fields.push(FormField::custom(
        render_settings_text_input(
            "Browser port",
            "TCP port the browser listener binds to on your local network.",
            draft.remote_web_port.as_str(),
            EditorField::Settings(SettingsField::RemoteWebPort),
            model,
            actions,
            Some(120.0),
            "43872",
        )
        .into_any_element(),
    ));

    fields.push(FormField::Info(
        FormInfoField::new(
            "Invite link",
            if browser_ready {
                draft
                    .remote_web_listener_url
                    .clone()
                    .unwrap_or_else(|| "Starting browser access...".to_string())
            } else if draft.remote_web_enabled {
                "Resolve the browser access error before sharing an invite.".to_string()
            } else {
                "Enable browser access to generate a link.".to_string()
            },
            Some(
                "Copy invite adds the one-time browser token automatically for first-time pairing."
                    .to_string(),
            ),
        )
        .action(
            FormAction::new(
                "Copy the browser invite link",
                "Copy invite",
                (actions.on_action)(EditorAction::CopyRemoteWebInviteLink),
            )
            .style(SurfaceActionButtonStyle::Primary),
        ),
    ));

    fields.push(FormField::Info(
        FormInfoField::new(
            "Manual pair token",
            remote_web_pairing_token_display(draft),
            Some("Use this only when you cannot open the invite link directly.".to_string()),
        )
        .action(
            FormAction::new(
                "Copy the browser pair token",
                "Copy token",
                (actions.on_action)(EditorAction::CopyRemoteWebPairingToken),
            )
            .style(SurfaceActionButtonStyle::Primary),
        )
        .action(FormAction::new(
            "Generate a new browser pairing token",
            "New token",
            (actions.on_action)(EditorAction::RegenerateRemoteWebPairingToken),
        )),
    ));

    FormSection::new("Browser Access")
        .hint("Let browsers open this DevManager without installing the desktop app.")
        .fields(fields)
}

fn build_remote_access_log_section(draft: &SettingsDraft, actions: &EditorActions) -> FormSection {
    let mut fields = vec![FormField::Info(
        FormInfoField::new(
            "Recent access activity",
            remote_access_activity_summary(draft.remote_access_activity_log.len()),
            Some(
                "Recent browser pairing plus browser and desktop connection history for this machine."
                    .to_string(),
            ),
        )
        .action(
            FormAction::new(
                "Reset all remembered browser access",
                "Reset access",
                (actions.on_action)(EditorAction::ResetRemoteWebAccess),
            )
            .description(
                "Disconnect all browsers, forget saved browser cookies, and require pairing again.",
            )
            .style(SurfaceActionButtonStyle::Danger),
        ),
    )];

    if draft.remote_access_activity_log.is_empty() {
        fields.push(FormField::empty_state(
            "No recent access activity",
            "Pair a browser or connect a desktop app to build a connection history here.",
            SurfaceTone::Muted,
        ));
    } else {
        fields.extend(draft.remote_access_activity_log.iter().rev().map(|event| {
            FormField::info(
                event.label.clone(),
                format_remote_access_value(event),
                remote_access_hint(event),
            )
        }));
    }

    FormSection::new("Remote Access Log")
        .hint("Review recent browser and desktop connection activity for this machine.")
        .fields(fields)
}

fn build_remote_advanced_section(draft: &SettingsDraft) -> Option<FormSection> {
    let mut fields = Vec::new();
    if let Some(error) = draft.remote_host_error.as_ref() {
        fields.push(FormField::notice(error.clone(), SurfaceTone::Danger));
    }
    if let Some(error) = draft.remote_web_listener_error.as_ref() {
        fields.push(FormField::notice(error.clone(), SurfaceTone::Danger));
    }
    if let Some(note) = draft.remote_host_last_note.as_ref() {
        fields.push(FormField::notice(
            note.clone(),
            if draft.remote_host_last_note_is_error {
                SurfaceTone::Danger
            } else {
                SurfaceTone::Muted
            },
        ));
    }

    if fields.is_empty() {
        None
    } else {
        Some(
            FormSection::new("Advanced Network and Security")
                .hint("Networking, pairing, and low-level host details for expert troubleshooting.")
                .fields(fields),
        )
    }
}

fn remote_session_mode_label(draft: &SettingsDraft) -> &'static str {
    if draft
        .remote_connect_status
        .as_ref()
        .is_some_and(|status| status.starts_with("Reconnecting to "))
    {
        "Reconnecting"
    } else if draft.remote_has_control {
        "Controller"
    } else {
        "Viewer"
    }
}

fn remote_session_mode_tone(draft: &SettingsDraft) -> SurfaceTone {
    if draft
        .remote_connect_status
        .as_ref()
        .is_some_and(|status| status.starts_with("Reconnecting to "))
    {
        SurfaceTone::Warning
    } else if draft.remote_has_control {
        SurfaceTone::Accent
    } else {
        SurfaceTone::Muted
    }
}

fn remote_pairing_token_display(draft: &SettingsDraft) -> String {
    if draft.remote_pairing_token.trim().is_empty() {
        "Unavailable".to_string()
    } else {
        draft.remote_pairing_token.clone()
    }
}

fn remote_web_pairing_token_display(draft: &SettingsDraft) -> String {
    if draft.remote_web_pairing_token.trim().is_empty() {
        "Unavailable".to_string()
    } else {
        draft.remote_web_pairing_token.clone()
    }
}

fn remote_access_activity_summary(count: usize) -> String {
    if count == 0 {
        "No recent activity".to_string()
    } else if count == 1 {
        "1 recent event".to_string()
    } else {
        format!("{count} recent events")
    }
}

fn remote_access_source_label(source: &RemoteAccessSource) -> &'static str {
    match source {
        RemoteAccessSource::Browser => "Browser",
        RemoteAccessSource::NativeApp => "Desktop app",
    }
}

fn remote_access_kind_label(kind: &RemoteAccessActivityKind) -> &'static str {
    match kind {
        RemoteAccessActivityKind::Paired => "Paired",
        RemoteAccessActivityKind::Connected => "Connected",
        RemoteAccessActivityKind::Reconnected => "Reconnected",
    }
}

fn format_remote_access_timestamp(epoch_ms: Option<u64>) -> Option<String> {
    let epoch_ms = epoch_ms?;
    let timestamp =
        OffsetDateTime::from_unix_timestamp_nanos((epoch_ms as i128) * 1_000_000).ok()?;
    let local_offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let local_timestamp = timestamp.to_offset(local_offset);
    let format = format_description::parse(
        "[year]-[month]-[day] [hour repr:12]:[minute] [period case:lower]",
    )
    .expect("valid remote access timestamp format");
    local_timestamp.format(&format).ok()
}

fn format_remote_access_value(event: &RemoteAccessActivityEvent) -> String {
    let prefix = format!(
        "{} {}",
        remote_access_source_label(&event.source),
        remote_access_kind_label(&event.event_kind)
    );
    match format_remote_access_timestamp(event.event_at_epoch_ms) {
        Some(timestamp) => format!("{prefix} • {timestamp}"),
        None => prefix,
    }
}

fn remote_access_hint(event: &RemoteAccessActivityEvent) -> Option<String> {
    let mut parts = vec![remote_access_source_label(&event.source).to_string()];
    if let Some(ip_address) = event
        .ip_address
        .as_ref()
        .filter(|value| !value.trim().is_empty())
    {
        parts.push(format!("IP {ip_address}"));
    }
    if let Some(browser_family) = event
        .browser_family
        .as_ref()
        .filter(|value| !value.trim().is_empty())
    {
        if let Some(browser_version) = event
            .browser_version
            .as_ref()
            .filter(|value| !value.trim().is_empty())
        {
            parts.push(format!("{browser_family} {browser_version}"));
        } else {
            parts.push(browser_family.clone());
        }
    }
    if let Some(os_family) = event
        .os_family
        .as_ref()
        .filter(|value| !value.trim().is_empty())
    {
        parts.push(os_family.clone());
    }
    if let Some(device_class) = event
        .device_class
        .as_ref()
        .filter(|value| !value.trim().is_empty())
    {
        parts.push(device_class.clone());
    }
    (parts.len() > 1).then(|| parts.join(" • "))
}

fn render_settings_text_input(
    label: &str,
    hint: &str,
    value: &str,
    field: EditorField,
    model: &EditorPaneModel,
    actions: &EditorActions,
    width: Option<f32>,
    placeholder: &str,
) -> impl IntoElement {
    render_compact_text_input(
        label,
        hint,
        value,
        field,
        model,
        actions,
        width,
        placeholder,
    )
}

fn render_settings_select_row(
    label: &str,
    hint: &str,
    value: &str,
    expanded: bool,
    on_toggle: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    accessory: Option<AnyElement>,
    field_width: Option<f32>,
    options: Vec<AnyElement>,
) -> impl IntoElement {
    let mut select = div()
        .px(px(10.0))
        .py(px(6.0))
        .rounded_sm()
        .bg(rgb(theme::PANEL_HEADER_BG))
        .border_1()
        .border_color(rgb(if expanded {
            theme::PRIMARY
        } else {
            theme::BORDER_SECONDARY
        }))
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, on_toggle)
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_PRIMARY))
                        .child(SharedString::from(value.to_string())),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_SUBTLE))
                        .child(if expanded { "^" } else { "v" }),
                ),
        );
    select = if let Some(field_width) = field_width {
        select.w(px(field_width))
    } else {
        select.w_full()
    };

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
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(select)
                .children(accessory),
        )
        .children(expanded.then(|| {
            div()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .p(px(4.0))
                .rounded_sm()
                .bg(rgb(theme::PANEL_HEADER_BG))
                .border_1()
                .border_color(rgb(theme::BORDER_SECONDARY))
                .children(options)
                .into_any_element()
        }))
}

fn render_settings_dropdown_option(
    label: String,
    selected: bool,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    div()
        .px(px(10.0))
        .py(px(5.0))
        .rounded_sm()
        .bg(rgb(if selected {
            0x1e1e22
        } else {
            theme::PANEL_HEADER_BG
        }))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(theme::ROW_HOVER_BG)))
        .on_mouse_down(MouseButton::Left, on_click)
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_PRIMARY))
                        .child(SharedString::from(label)),
                )
                .children(
                    selected.then(|| div().text_xs().text_color(rgb(theme::PRIMARY)).child("*")),
                ),
        )
}

fn render_settings_font_size_row(
    draft: &SettingsDraft,
    actions: &EditorActions,
) -> impl IntoElement {
    let current = settings_font_size_value(draft);

    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(
            div()
                .text_xs()
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(rgb(theme::TEXT_PRIMARY))
                .child(SharedString::from(format!(
                    "Terminal font size: {current}px"
                ))),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_SUBTLE))
                .child("Default text size for all terminals"),
        )
        .child(
            div()
                .pt(px(2.0))
                .flex()
                .items_center()
                .gap(px(1.0))
                .children((8u16..=24).map(|size| {
                    let on_select = (actions.on_action)(EditorAction::SetTerminalFontSize(size));
                    let is_current = size == current;
                    let in_range = size <= current;
                    div()
                        .flex_1()
                        .h(px(if is_current { 14.0 } else { 6.0 }))
                        .rounded_sm()
                        .cursor_pointer()
                        .bg(rgb(if is_current {
                            theme::PRIMARY
                        } else if in_range {
                            theme::BORDER_PRIMARY
                        } else {
                            theme::BORDER_SECONDARY
                        }))
                        .on_mouse_down(MouseButton::Left, on_select)
                        .into_any_element()
                })),
        )
}

fn render_settings_inline_button(
    label: &str,
    primary: bool,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    div()
        .px(px(if primary { 14.0 } else { 10.0 }))
        .py(px(6.0))
        .rounded_sm()
        .bg(rgb(if primary {
            theme::PRIMARY
        } else {
            theme::PANEL_HEADER_BG
        }))
        .border_1()
        .border_color(rgb(if primary {
            theme::PRIMARY
        } else {
            theme::BORDER_SECONDARY
        }))
        .text_xs()
        .text_color(rgb(if primary {
            theme::APP_BG
        } else {
            theme::TEXT_PRIMARY
        }))
        .cursor_pointer()
        .hover(|s| {
            s.bg(rgb(if primary {
                theme::PRIMARY_HOVER
            } else {
                theme::ROW_HOVER_BG
            }))
        })
        .child(SharedString::from(label.to_string()))
        .on_mouse_down(MouseButton::Left, on_click)
}

fn render_settings_close_button(
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    div()
        .px(px(6.0))
        .py(px(2.0))
        .rounded_sm()
        .text_xs()
        .text_color(rgb(theme::TEXT_MUTED))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(theme::ROW_HOVER_BG)))
        .child("close")
        .on_mouse_down(MouseButton::Left, on_click)
}

fn settings_font_size_value(draft: &SettingsDraft) -> u16 {
    draft
        .terminal_font_size
        .trim()
        .parse::<u16>()
        .ok()
        .unwrap_or(13)
        .clamp(8, 24)
}

fn fallback_editor_label(value: &str, fallback: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

fn non_empty_value(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn path_leaf(path: &str) -> &str {
    path.rsplit(['\\', '/'])
        .find(|segment| !segment.trim().is_empty())
        .unwrap_or(path)
}

fn remote_preview_story(model: &EditorPaneModel) -> PreviewStory {
    let preview_actions = EditorActions {
        on_action: Arc::new(preview_editor_action_handler),
        on_focus_at: Arc::new(preview_editor_focus_handler),
        on_drag_to: Arc::new(preview_editor_drag_handler),
    };
    let disconnected = disconnected_remote_draft();
    let mut host = browser_access_enabled_remote_draft();
    host.remote_web_enabled = false;
    host.remote_web_paired_clients = 0;
    host.remote_web_paired_clients_detail.clear();
    host.remote_access_activity_log.clear();
    let browser = browser_access_enabled_remote_draft();
    let disconnected_preview =
        preview_editor_model(EditorPanel::Settings(disconnected.clone()), model);
    let host_preview = preview_editor_model(EditorPanel::Settings(host.clone()), model);
    let browser_preview = preview_editor_model(EditorPanel::Settings(browser.clone()), model);

    PreviewStory::new(
        "Remote Access",
        "Tabbed states for connecting to another device and hosting desktop or browser access.",
    )
    .state(PreviewState::new(
        "Connect tab",
        render_settings_panel(&disconnected, &disconnected_preview, &preview_actions)
            .into_any_element(),
    ))
    .state(PreviewState::new(
        "Host tab",
        render_settings_panel(&host, &host_preview, &preview_actions).into_any_element(),
    ))
    .state(PreviewState::new(
        "Host tab with browser access",
        render_settings_panel(&browser, &browser_preview, &preview_actions).into_any_element(),
    ))
}

fn render_ui_preview_panel(
    _: &UiPreviewDraft,
    model: &EditorPaneModel,
    _: &EditorActions,
) -> impl IntoElement {
    let preview_actions = EditorActions {
        on_action: Arc::new(preview_editor_action_handler),
        on_focus_at: Arc::new(preview_editor_focus_handler),
        on_drag_to: Arc::new(preview_editor_drag_handler),
    };
    let preview_wizard_actions = WizardActions {
        on_action: &preview_wizard_action_handler,
    };

    let project_draft = sample_project_draft();
    let project_empty_draft = sample_project_draft_empty();
    let folder_draft = sample_folder_draft();
    let folder_scanning_draft = sample_folder_draft_scanning();
    let folder_minimal_draft = sample_folder_draft_minimal();
    let command_draft = sample_command_draft();
    let ssh_draft = sample_ssh_draft();
    let settings_default = sample_settings_draft(None);
    let settings_terminal = sample_settings_draft(Some(SettingsPicker::Terminal));
    let settings_sound = sample_settings_draft(Some(SettingsPicker::NotificationSound));
    let project_preview = preview_editor_model(EditorPanel::Project(project_draft.clone()), model);
    let project_notes_preview = preview_editor_model_with_state(
        EditorPanel::Project(project_draft.clone()),
        model,
        Some(EditorField::Project(ProjectField::Notes)),
        project_draft.notes.chars().count(),
        Some("Preview mode: edits here are visual only.".to_string()),
    );
    let project_empty_preview =
        preview_editor_model(EditorPanel::Project(project_empty_draft.clone()), model);
    let folder_preview = preview_editor_model(EditorPanel::Folder(folder_draft.clone()), model);
    let folder_scanning_preview =
        preview_editor_model(EditorPanel::Folder(folder_scanning_draft.clone()), model);
    let folder_minimal_preview =
        preview_editor_model(EditorPanel::Folder(folder_minimal_draft.clone()), model);
    let command_preview = preview_editor_model(EditorPanel::Command(command_draft.clone()), model);
    let ssh_preview = preview_editor_model(EditorPanel::Ssh(ssh_draft.clone()), model);
    let settings_preview =
        preview_editor_model(EditorPanel::Settings(settings_default.clone()), model);
    let settings_terminal_preview =
        preview_editor_model(EditorPanel::Settings(settings_terminal.clone()), model);
    let settings_sound_preview =
        preview_editor_model(EditorPanel::Settings(settings_sound.clone()), model);
    let wizard_initial = sample_wizard_initial();
    let wizard_step1 = sample_wizard(1);
    let wizard_step2 = sample_wizard(2);

    let stories = vec![
        PreviewStory::new(
            "Add Project Flow",
            "Reference states for onboarding a repo into DevManager without touching live project data.",
        )
        .state(
            PreviewState::new(
                "Fresh start",
                render_wizard_step1_frame(
                    "preview-step1-initial-scroll",
                    &wizard_initial,
                    &preview_wizard_actions,
                )
                .into_any_element(),
            )
            .note("No root selected yet, so the wizard explains the next action instead of showing scan results.")
            .badge(SurfaceBadge::new("Empty", SurfaceTone::Muted)),
        )
        .state(
            PreviewState::new(
                "Detected folders",
                render_wizard_step1_frame(
                    "preview-step1-scroll",
                    &wizard_step1,
                    &preview_wizard_actions,
                )
                .into_any_element(),
            )
            .note("A scanned repo with multiple candidate folders and explicit selection choices.")
            .badge(SurfaceBadge::new("Workflow", SurfaceTone::Accent)),
        )
        .state(
            PreviewState::new(
                "Folder configuration",
                render_wizard_step2_frame(
                    "preview-step2-scroll",
                    &wizard_step2,
                    &preview_wizard_actions,
                )
                .into_any_element(),
            )
            .note("Second step for default scripts and primary port variables.")
            .badge(SurfaceBadge::new("Configured", SurfaceTone::Success)),
        ),
        PreviewStory::new(
            "Project Editor",
            "Project-level editing states for identity, notes, and defaults.",
        )
        .state(
            PreviewState::new(
                "Default state",
                render_project_panel(&project_draft, &project_preview, &preview_actions)
                    .into_any_element(),
            )
            .note("A healthy saved project with notes and defaults already in place.")
            .badge(SurfaceBadge::new("Saved", SurfaceTone::Success)),
        )
        .state(
            PreviewState::new(
                "Focused text field",
                render_project_panel(&project_draft, &project_notes_preview, &preview_actions)
                    .into_any_element(),
            )
            .note("Used for checking focus rings, cursor placement, and dense text content.")
            .badge(SurfaceBadge::new("Editing", SurfaceTone::Accent)),
        )
        .state(
            PreviewState::new(
                "Minimal project",
                render_project_panel(&project_empty_draft, &project_empty_preview, &preview_actions)
                    .into_any_element(),
            )
            .note("Useful for testing empty-field readability and onboarding copy.")
            .badge(SurfaceBadge::new("Empty", SurfaceTone::Muted)),
        ),
        PreviewStory::new(
            "Folder Editor",
            "Operational folder states covering discovery, loading, and manual setup.",
        )
        .state(
            PreviewState::new(
                "Scanned and healthy",
                render_folder_panel(&folder_draft, &folder_preview, &preview_actions)
                    .into_any_element(),
            )
            .note("A fully scanned frontend folder with scripts, env contents, and dependency status.")
            .badge(SurfaceBadge::new("Ready", SurfaceTone::Success)),
        )
        .state(
            PreviewState::new(
                "Scanning",
                render_folder_panel(
                    &folder_scanning_draft,
                    &folder_scanning_preview,
                    &preview_actions,
                )
                .into_any_element(),
            )
            .note("Use this state to inspect busy affordances, notices, and reduced metadata.")
            .badge(SurfaceBadge::new("Busy", SurfaceTone::Accent)),
        )
        .state(
            PreviewState::new(
                "Manual setup",
                render_folder_panel(
                    &folder_minimal_draft,
                    &folder_minimal_preview,
                    &preview_actions,
                )
                .into_any_element(),
            )
            .note("Manual folder setup before scans or env loading have happened.")
            .badge(SurfaceBadge::new("Manual", SurfaceTone::Warning)),
        ),
        PreviewStory::new(
            "Command and SSH",
            "Secondary editor surfaces that should still read like part of the same system.",
        )
        .state(
            PreviewState::new(
                "Command editor",
                render_command_panel(&command_draft, &command_preview, &preview_actions)
                    .into_any_element(),
            )
            .note("Checks command naming, runtime metadata, and restart toggles.")
            .badge(SurfaceBadge::new("Runtime", SurfaceTone::Accent)),
        )
        .state(
            PreviewState::new(
                "SSH editor",
                render_ssh_panel(&ssh_draft, &ssh_preview, &preview_actions).into_any_element(),
            )
            .note("Connection information and credentials should feel as clear as project editing.")
            .badge(SurfaceBadge::new("Remote", SurfaceTone::Muted)),
        ),
        PreviewStory::new(
            "Settings Surface",
            "Non-editor surfaces should share the same section and action language.",
        )
        .state(
            PreviewState::new(
                "Default settings",
                render_settings_panel(&settings_default, &settings_preview, &preview_actions)
                    .into_any_element(),
            )
            .note("The baseline settings experience using the same card and action system.")
            .badge(SurfaceBadge::new("Default", SurfaceTone::Success)),
        )
        .state(
            PreviewState::new(
                "Terminal picker open",
                render_settings_panel(
                    &settings_terminal,
                    &settings_terminal_preview,
                    &preview_actions,
                )
                .into_any_element(),
            )
            .note("Dropdown and picker states need to remain legible inside dense settings screens.")
            .badge(SurfaceBadge::new("Picker", SurfaceTone::Accent)),
        )
        .state(
            PreviewState::new(
                "Notification picker open",
                render_settings_panel(&settings_sound, &settings_sound_preview, &preview_actions)
                    .into_any_element(),
            )
            .note("Use this state when tuning accessory buttons and dropdown density.")
            .badge(SurfaceBadge::new("Audio", SurfaceTone::Warning)),
        ),
        PreviewStory::new(
            "Component Kit",
            "Atomic stories for the reusable rows, callouts, and empty states that power the app shell.",
        )
        .state(
            PreviewState::new(
                "Actions and notices",
                render_static_form_fields(vec![
                    FormField::action(
                        FormAction::new("Primary action", "Run task", preview_click_handler())
                            .description("Use when the next step is the obvious forward action.")
                            .style(SurfaceActionButtonStyle::Primary)
                            .badge(SurfaceBadge::new("Primary", SurfaceTone::Accent)),
                    ),
                    FormField::action(
                        FormAction::new("Destructive action", "Delete", preview_click_handler())
                            .description("Use for irreversible flows that need stronger contrast.")
                            .style(SurfaceActionButtonStyle::Danger)
                            .badge(SurfaceBadge::new("Danger", SurfaceTone::Danger)),
                    ),
                    FormField::notice(
                        "Accent notices call attention to the most relevant guidance for the current surface.",
                        SurfaceTone::Accent,
                    ),
                    FormField::notice(
                        "Warning notices are available for risky or incomplete states.",
                        SurfaceTone::Warning,
                    ),
                ])
                .into_any_element(),
            )
            .note("Helps tune row density, contrast, and tone semantics in isolation.")
            .badge(SurfaceBadge::new("Kit", SurfaceTone::Muted)),
        )
        .state(
            PreviewState::new(
                "Selection and empty states",
                render_static_form_fields(vec![
                    FormField::selection_list(
                        FormSelectionList::new("Selectable rows")
                            .hint("Use for scripts, ports, and scan-driven choices.")
                            .row(FormSelectionRow::new(
                                "Selected item",
                                Some("Shows the active state and detail copy.".to_string()),
                                true,
                                preview_click_handler(),
                            ))
                            .row(FormSelectionRow::new(
                                "Available item",
                                Some("Hover and selection styling stay consistent across editors.".to_string()),
                                false,
                                preview_click_handler(),
                            )),
                    ),
                    FormField::empty_state(
                        "Empty state",
                        "Use this when the user has not scanned, loaded, or configured anything yet.",
                        SurfaceTone::Muted,
                    ),
                ])
                .into_any_element(),
            )
            .note("These are the fallback states that usually break visual consistency first.")
            .badge(SurfaceBadge::new("Fallback", SurfaceTone::Muted)),
        ),
        remote_preview_story(model),
    ];

    div()
        .flex()
        .flex_col()
        .gap(px(18.0))
        .child(render_notice_row(
            "This surface is read-only and seeded with sample data so UI work can happen without mutating real projects.",
        ))
        .child(render_preview_stories(stories))
}

fn preview_editor_model(panel: EditorPanel, model: &EditorPaneModel) -> EditorPaneModel {
    EditorPaneModel {
        panel,
        active_field: None,
        cursor: 0,
        selection_anchor: None,
        notice: None,
        updater: model.updater.clone(),
        allow_mutation: true,
    }
}

fn preview_editor_model_with_state(
    panel: EditorPanel,
    model: &EditorPaneModel,
    active_field: Option<EditorField>,
    cursor: usize,
    notice: Option<String>,
) -> EditorPaneModel {
    EditorPaneModel {
        panel,
        active_field,
        cursor,
        selection_anchor: None,
        notice,
        updater: model.updater.clone(),
        allow_mutation: true,
    }
}

fn preview_editor_action_handler(
    _: EditorAction,
) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
    Box::new(|_, _, _| {})
}

fn preview_editor_focus_handler(
    _: EditorField,
    _: usize,
) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
    Box::new(|_, _, _| {})
}

fn preview_editor_drag_handler(
    _: EditorField,
    _: usize,
) -> Box<dyn Fn(&MouseMoveEvent, &mut Window, &mut App)> {
    Box::new(|_, _, _| {})
}

fn preview_wizard_action_handler(
    _: WizardAction,
) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
    Box::new(|_, _, _| {})
}

fn preview_click_handler() -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)> {
    Box::new(|_, _, _| {})
}

fn sample_project_draft() -> ProjectDraft {
    ProjectDraft {
        existing_id: Some("project-preview".to_string()),
        name: "House Hunter".to_string(),
        root_path: "C:\\Code\\personal\\househunter".to_string(),
        color: "#ef4444".to_string(),
        pinned: true,
        save_log_files: true,
        notes: "Main app.\nUse the API folder for backend work and the web folder for UI changes."
            .to_string(),
    }
}

fn sample_project_draft_empty() -> ProjectDraft {
    ProjectDraft {
        existing_id: None,
        name: String::new(),
        root_path: String::new(),
        color: "#6366f1".to_string(),
        pinned: false,
        save_log_files: false,
        notes: String::new(),
    }
}

fn sample_folder_draft() -> FolderDraft {
    FolderDraft {
        project_id: "project-preview".to_string(),
        existing_id: Some("folder-preview".to_string()),
        name: "web".to_string(),
        folder_path: "C:\\Code\\personal\\househunter\\web".to_string(),
        env_file_path: ".env.local".to_string(),
        env_file_contents: "VITE_DEV_PORT=5173\nVITE_API_ORIGIN=http://localhost:4555\n"
            .to_string(),
        env_file_loaded: true,
        port_variable: "VITE_DEV_PORT".to_string(),
        hidden: false,
        git_branch: Some("feature/native-ui".to_string()),
        dependency_status: Some(DependencyStatus {
            status: "up to date".to_string(),
            message: "package-lock.json matches node_modules metadata.".to_string(),
        }),
        scan_result: Some(ScanResult {
            scripts: vec![
                ScannedScript {
                    name: "dev".to_string(),
                    command: "vite".to_string(),
                },
                ScannedScript {
                    name: "build".to_string(),
                    command: "vite build".to_string(),
                },
            ],
            ports: vec![
                ScannedPort {
                    variable: "VITE_DEV_PORT".to_string(),
                    port: 5173,
                    source: ".env.local".to_string(),
                },
                ScannedPort {
                    variable: "PORT".to_string(),
                    port: 3000,
                    source: "package.json".to_string(),
                },
            ],
            has_package_json: true,
            has_cargo_toml: false,
            has_env_file: true,
        }),
        selected_scanned_scripts: ["dev".to_string()].into_iter().collect(),
        selected_scanned_port_variable: Some("VITE_DEV_PORT".to_string()),
        scan_message: Some("Discovered 2 scripts and 2 port variables.".to_string()),
        is_scanning: false,
    }
}

fn sample_folder_draft_scanning() -> FolderDraft {
    FolderDraft {
        project_id: "project-preview".to_string(),
        existing_id: Some("folder-scanning".to_string()),
        name: "api".to_string(),
        folder_path: "C:\\Code\\personal\\househunter\\api".to_string(),
        env_file_path: ".env".to_string(),
        env_file_contents: String::new(),
        env_file_loaded: false,
        port_variable: String::new(),
        hidden: false,
        git_branch: Some("feature/onboarding".to_string()),
        dependency_status: None,
        scan_result: None,
        selected_scanned_scripts: BTreeSet::new(),
        selected_scanned_port_variable: None,
        scan_message: Some(
            "Scanning folder for scripts, env files, and default ports.".to_string(),
        ),
        is_scanning: true,
    }
}

fn sample_folder_draft_minimal() -> FolderDraft {
    FolderDraft {
        project_id: "project-preview".to_string(),
        existing_id: None,
        name: String::new(),
        folder_path: "C:\\Code\\personal\\househunter\\worker".to_string(),
        env_file_path: ".env".to_string(),
        env_file_contents: String::new(),
        env_file_loaded: false,
        port_variable: String::new(),
        hidden: false,
        git_branch: None,
        dependency_status: None,
        scan_result: None,
        selected_scanned_scripts: BTreeSet::new(),
        selected_scanned_port_variable: None,
        scan_message: None,
        is_scanning: false,
    }
}

fn sample_command_draft() -> CommandDraft {
    CommandDraft {
        project_id: "project-preview".to_string(),
        folder_id: "folder-preview".to_string(),
        existing_id: Some("command-preview".to_string()),
        label: "web dev".to_string(),
        command: "npm".to_string(),
        args_text: "run dev -- --host".to_string(),
        env_text: "NODE_ENV=development;FORCE_COLOR=1".to_string(),
        port_text: "5173".to_string(),
        auto_restart: true,
        clear_logs_on_restart: false,
    }
}

fn sample_ssh_draft() -> SshDraft {
    SshDraft {
        existing_id: Some("ssh-preview".to_string()),
        label: "Prod Box".to_string(),
        host: "prod.example.com".to_string(),
        port_text: "22".to_string(),
        username: "deploy".to_string(),
        password: String::new(),
    }
}

fn sample_settings_draft(open_picker: Option<SettingsPicker>) -> SettingsDraft {
    SettingsDraft {
        remote_focus_only: false,
        remote_active_tab: RemoteTopTab::Connect,
        default_terminal: DefaultTerminal::Powershell,
        mac_terminal_profile: MacTerminalProfile::Zsh,
        theme: "dark".to_string(),
        log_buffer_size: "10000".to_string(),
        claude_command: "npx -y @anthropic-ai/claude-code@latest --dangerously-skip-permissions"
            .to_string(),
        codex_command: "npx -y @openai/codex@latest --dangerously-bypass-approvals-and-sandbox"
            .to_string(),
        notification_sound: "glass".to_string(),
        confirm_on_close: true,
        minimize_to_tray: false,
        restore_session_on_start: true,
        terminal_font_size: "13".to_string(),
        option_as_meta: false,
        copy_on_select: true,
        keep_selection_on_copy: false,
        show_terminal_scrollbar: true,
        shell_integration_enabled: true,
        terminal_mouse_override: false,
        terminal_read_only: false,
        github_token: String::new(),
        remote_host_enabled: false,
        remote_bind_address: "0.0.0.0".to_string(),
        remote_port: "43871".to_string(),
        remote_web_port: "43872".to_string(),
        remote_pairing_token: "ABC123".to_string(),
        remote_connect_address: "192.168.0.20".to_string(),
        remote_connect_port: "43871".to_string(),
        remote_connect_token: String::new(),
        remote_connect_in_flight: false,
        remote_connect_status: Some("Connected to studio-pc.".to_string()),
        remote_connect_status_is_error: false,
        remote_connected_label: Some("studio-pc".to_string()),
        remote_connected_endpoint: Some("192.168.0.20:43871".to_string()),
        remote_connected_server_id: Some("host-studio".to_string()),
        remote_connected_fingerprint: Some("fingerprint".to_string()),
        remote_reconnect_attempts: 1,
        remote_reconnect_last_error: Some(
            "Connection reset during sleep recovery; reconnected automatically.".to_string(),
        ),
        remote_latency_summary: Some("host 2 ms • paint 1 ms".to_string()),
        remote_has_control: true,
        remote_connected: true,
        remote_host_clients: 1,
        remote_host_controller_client_id: Some("client-studio".to_string()),
        remote_host_listening: true,
        remote_host_error: None,
        remote_host_last_note: None,
        remote_host_last_note_is_error: false,
        remote_host_latency_summary: Some("write 1 ms".to_string()),
        remote_host_server_id: "host-studio".to_string(),
        remote_host_fingerprint: "fingerprint".to_string(),
        remote_port_forwards: vec![
            RemotePortForwardDraft {
                label: "localhost:5173".to_string(),
                status: "Forwarded".to_string(),
                detail: Some("Open URL uses this local mirror.".to_string()),
                is_error: false,
            },
            RemotePortForwardDraft {
                label: "localhost:4000".to_string(),
                status: "Local port busy".to_string(),
                detail: Some("Local port 4000 is already in use on this machine.".to_string()),
                is_error: true,
            },
        ],
        remote_known_hosts: vec![KnownRemoteHost {
            label: "studio-pc".to_string(),
            address: "192.168.0.20".to_string(),
            port: 43871,
            server_id: "host-studio".to_string(),
            certificate_fingerprint: "fingerprint".to_string(),
            client_id: "client-laptop".to_string(),
            auth_token: "token".to_string(),
            last_connected_epoch_ms: Some(1_710_000_000_000),
        }],
        remote_paired_clients: vec![PairedRemoteClient {
            client_id: "client-studio".to_string(),
            label: "studio-laptop".to_string(),
            auth_token: "token".to_string(),
            last_seen_epoch_ms: Some(1_710_000_000_000),
        }],
        remote_web_enabled: false,
        remote_web_pairing_token: "AB23CD45".to_string(),
        remote_web_listener_url: Some("http://192.168.0.20:43872".to_string()),
        remote_web_listener_error: None,
        remote_web_paired_clients: 0,
        remote_web_paired_clients_detail: Vec::new(),
        remote_access_activity_log: Vec::new(),
        open_picker,
    }
}

fn sample_remote_access_log() -> Vec<RemoteAccessActivityEvent> {
    vec![
        RemoteAccessActivityEvent {
            client_id: "browser-ipad".to_string(),
            source: RemoteAccessSource::Browser,
            event_kind: RemoteAccessActivityKind::Reconnected,
            label: "iPad Safari".to_string(),
            ip_address: Some("192.168.0.15".to_string()),
            event_at_epoch_ms: Some(1_710_000_120_000),
            browser_family: Some("Safari".to_string()),
            browser_version: Some("17.4".to_string()),
            os_family: Some("iOS".to_string()),
            device_class: Some("tablet".to_string()),
        },
        RemoteAccessActivityEvent {
            client_id: "native-studio".to_string(),
            source: RemoteAccessSource::NativeApp,
            event_kind: RemoteAccessActivityKind::Connected,
            label: "Studio MacBook".to_string(),
            ip_address: Some("192.168.0.22".to_string()),
            event_at_epoch_ms: Some(1_710_000_090_000),
            browser_family: None,
            browser_version: None,
            os_family: Some("macOS".to_string()),
            device_class: Some("desktop".to_string()),
        },
        RemoteAccessActivityEvent {
            client_id: "browser-work".to_string(),
            source: RemoteAccessSource::Browser,
            event_kind: RemoteAccessActivityKind::Connected,
            label: "Windows Chrome".to_string(),
            ip_address: Some("192.168.0.44".to_string()),
            event_at_epoch_ms: Some(1_710_000_060_000),
            browser_family: Some("Chrome".to_string()),
            browser_version: Some("135".to_string()),
            os_family: Some("Windows".to_string()),
            device_class: Some("desktop".to_string()),
        },
        RemoteAccessActivityEvent {
            client_id: "browser-ipad".to_string(),
            source: RemoteAccessSource::Browser,
            event_kind: RemoteAccessActivityKind::Paired,
            label: "iPad Safari".to_string(),
            ip_address: Some("192.168.0.15".to_string()),
            event_at_epoch_ms: Some(1_710_000_000_000),
            browser_family: Some("Safari".to_string()),
            browser_version: Some("17.4".to_string()),
            os_family: Some("iOS".to_string()),
            device_class: Some("tablet".to_string()),
        },
    ]
}

fn connected_remote_draft() -> SettingsDraft {
    let mut draft = sample_settings_draft(None);
    draft.remote_focus_only = true;
    draft.remote_active_tab = RemoteTopTab::Connect;
    draft.remote_web_enabled = true;
    draft.remote_web_paired_clients = 1;
    draft.remote_web_paired_clients_detail = vec![PairedWebClient {
        client_id: "browser-ipad".to_string(),
        browser_install_id: "browser-install-ipad".to_string(),
        nickname: None,
        label: "Safari on iPad".to_string(),
        issued_at_epoch_ms: Some(1_710_000_000_000),
        last_seen_epoch_ms: Some(1_710_000_000_000),
        last_seen_ip: Some("192.168.0.15".to_string()),
        user_agent: Some("Safari".to_string()),
        browser_family: Some("Safari".to_string()),
        browser_version: Some("17.4".to_string()),
        os_family: Some("iOS".to_string()),
        device_class: Some("tablet".to_string()),
    }];
    draft.remote_access_activity_log = sample_remote_access_log();
    draft
}

fn disconnected_remote_draft() -> SettingsDraft {
    let mut draft = connected_remote_draft();
    draft.remote_active_tab = RemoteTopTab::Connect;
    draft.remote_connected = false;
    draft.remote_has_control = false;
    draft.remote_connected_label = None;
    draft.remote_connected_endpoint = None;
    draft.remote_connected_server_id = None;
    draft.remote_connected_fingerprint = None;
    draft.remote_latency_summary = None;
    draft.remote_reconnect_attempts = 0;
    draft.remote_reconnect_last_error = None;
    draft.remote_port_forwards.clear();
    draft.remote_host_enabled = false;
    draft.remote_web_enabled = false;
    draft.remote_web_paired_clients = 0;
    draft.remote_web_paired_clients_detail.clear();
    draft.remote_access_activity_log.clear();
    draft.remote_connect_status = Some("Ready to connect to another device.".to_string());
    draft
}

fn browser_access_enabled_remote_draft() -> SettingsDraft {
    let mut draft = disconnected_remote_draft();
    draft.remote_active_tab = RemoteTopTab::Host;
    draft.remote_host_enabled = true;
    draft.remote_host_listening = true;
    draft.remote_web_enabled = true;
    draft.remote_web_paired_clients = 1;
    draft.remote_web_paired_clients_detail = vec![PairedWebClient {
        client_id: "browser-ipad".to_string(),
        browser_install_id: "browser-install-ipad".to_string(),
        nickname: None,
        label: "Safari on iPad".to_string(),
        issued_at_epoch_ms: Some(1_710_000_000_000),
        last_seen_epoch_ms: Some(1_710_000_000_000),
        last_seen_ip: Some("192.168.0.15".to_string()),
        user_agent: Some("Safari".to_string()),
        browser_family: Some("Safari".to_string()),
        browser_version: Some("17.4".to_string()),
        os_family: Some("iOS".to_string()),
        device_class: Some("tablet".to_string()),
    }];
    draft.remote_access_activity_log = sample_remote_access_log();
    draft.remote_host_error = None;
    draft
}

fn format_saved_host_hint(host: &KnownRemoteHost) -> String {
    let mut hint = format!("{}:{}", host.address, host.port);
    if host.last_connected_epoch_ms.is_some() {
        hint.push_str(" • previously connected");
    }
    hint
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_preview_story_uses_connect_and_host_states() {
        let model = settings_model(sample_settings_draft(None));
        let story = remote_preview_story(&model);
        assert_eq!(story.title, "Remote Access");
        let labels: Vec<&str> = story.states.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["Connect tab", "Host tab", "Host tab with browser access"]
        );
    }

    #[test]
    fn remote_panel_uses_tabbed_remote_access_language() {
        let mut draft = disconnected_remote_draft();
        draft.remote_known_hosts.clear();
        let model = settings_model(draft.clone());
        let actions = dummy_editor_actions();

        let sections = build_remote_tab_sections(&draft, RemoteTopTab::Connect, &model, &actions);
        let panel = EditorPanel::Settings(draft);

        assert_eq!(panel.headline(), "Remote access");
        assert_eq!(
            panel.subtitle(),
            "Connect to other devices or host this machine for desktop and browser access."
        );
        let current = section(&sections, "Current Session");
        assert!(contains_notice(
            current,
            "Ready to connect to another device."
        ));
    }

    #[test]
    fn remote_tabs_group_sections_by_user_intent() {
        let draft = disconnected_remote_draft();
        let model = settings_model(draft.clone());
        let actions = dummy_editor_actions();

        let connect_sections =
            build_remote_tab_sections(&draft, RemoteTopTab::Connect, &model, &actions);
        let host_sections = build_remote_tab_sections(&draft, RemoteTopTab::Host, &model, &actions);
        let log_sections = build_remote_tab_sections(&draft, RemoteTopTab::Log, &model, &actions);

        let connect_titles: Vec<&str> = connect_sections
            .iter()
            .map(|section| section.title.as_str())
            .collect();
        let host_titles: Vec<&str> = host_sections
            .iter()
            .map(|section| section.title.as_str())
            .collect();
        let log_titles: Vec<&str> = log_sections
            .iter()
            .map(|section| section.title.as_str())
            .collect();

        assert_eq!(connect_titles, vec!["Current Session", "Saved hosts"]);
        assert_eq!(host_titles, vec!["Native App Hosting", "Browser Access"]);
        assert_eq!(log_titles, vec!["Remote Access Log"]);
    }

    #[test]
    fn remote_dashboard_uses_stable_primary_sections_when_disconnected() {
        let draft = disconnected_remote_draft();
        let model = settings_model(draft.clone());
        let actions = dummy_editor_actions();

        let sections = build_remote_dashboard_sections(&draft, &model, &actions);
        let titles: Vec<&str> = sections
            .iter()
            .map(|section| section.title.as_str())
            .collect();

        assert_eq!(
            titles,
            vec![
                "Current Session",
                "Saved hosts",
                "Native App Hosting",
                "Browser Access",
                "Remote Access Log",
            ]
        );
    }

    #[test]
    fn remote_dashboard_rejects_legacy_remote_section_titles() {
        let draft = disconnected_remote_draft();
        let model = settings_model(draft.clone());
        let actions = dummy_editor_actions();

        let sections = build_remote_dashboard_sections(&draft, &model, &actions);
        let titles: Vec<&str> = sections
            .iter()
            .map(|section| section.title.as_str())
            .collect();

        assert_eq!(
            titles,
            vec![
                "Current Session",
                "Saved hosts",
                "Native App Hosting",
                "Browser Access",
                "Remote Access Log",
            ]
        );

        for legacy in [
            "Connect To Another Device",
            "Host This Device",
            "Browser Web UI",
            "Paired Clients",
        ] {
            assert!(
                !titles.contains(&legacy),
                "legacy remote section title must not appear: {legacy}"
            );
        }
    }

    #[test]
    fn remote_dashboard_uses_stable_primary_sections_when_connected() {
        let draft = connected_remote_draft();
        let model = settings_model(draft.clone());
        let actions = dummy_editor_actions();

        let sections = build_remote_dashboard_sections(&draft, &model, &actions);
        let titles: Vec<&str> = sections
            .iter()
            .map(|section| section.title.as_str())
            .collect();

        assert_eq!(
            titles,
            vec![
                "Current Session",
                "Native App Hosting",
                "Browser Access",
                "Remote Access Log",
            ]
        );
    }

    #[test]
    fn remote_dashboard_uses_inline_actions_for_token_and_invite_rows() {
        let draft = connected_remote_draft();
        let model = settings_model(draft.clone());
        let actions = dummy_editor_actions();
        let sections = build_remote_dashboard_sections(&draft, &model, &actions);

        let device_section = section(&sections, "Native App Hosting");
        let browser_section = section(&sections, "Browser Access");

        let desktop_token = info_field(device_section, "Desktop pairing token");
        let invite_link = info_field(browser_section, "Invite link");
        let manual_pair_token = info_field(browser_section, "Manual pair token");

        assert_eq!(
            desktop_token
                .actions
                .iter()
                .map(|action| action.value.as_str())
                .collect::<Vec<_>>(),
            vec!["Copy", "New token"]
        );
        assert_eq!(
            invite_link
                .actions
                .iter()
                .map(|action| action.value.as_str())
                .collect::<Vec<_>>(),
            vec!["Copy invite"]
        );
        assert_eq!(
            manual_pair_token
                .actions
                .iter()
                .map(|action| action.value.as_str())
                .collect::<Vec<_>>(),
            vec!["Copy token", "New token"]
        );
    }

    #[test]
    fn log_tab_shows_recent_remote_access_activity_and_host_tab_keeps_access_controls() {
        let draft = browser_access_enabled_remote_draft();
        let model = settings_model(draft.clone());
        let actions = dummy_editor_actions();
        let host_sections = build_remote_tab_sections(&draft, RemoteTopTab::Host, &model, &actions);
        let log_sections = build_remote_tab_sections(&draft, RemoteTopTab::Log, &model, &actions);

        let browser_section = section(&host_sections, "Browser Access");
        let log_section = section(&log_sections, "Remote Access Log");
        let summary = info_field(log_section, "Recent access activity");

        assert_eq!(summary.value, "4 recent events");
        assert_eq!(
            summary
                .actions
                .iter()
                .map(|action| action.value.as_str())
                .collect::<Vec<_>>(),
            vec!["Reset access"]
        );
        assert!(contains_info_field(log_section, "iPad Safari"));
        assert!(contains_info_field(log_section, "Studio MacBook"));
        assert!(contains_info_field(log_section, "Windows Chrome"));
        assert!(!contains_info_field(
            browser_section,
            "Recent access activity"
        ));
        assert!(!contains_info_field(browser_section, "Paired browsers"));
    }

    #[test]
    fn remote_dashboard_surfaces_browser_errors_in_summary_and_advanced_sections() {
        let mut draft = connected_remote_draft();
        draft.remote_web_listener_error = Some("Browser listener failed to bind".to_string());
        let model = settings_model(draft.clone());
        let actions = dummy_editor_actions();
        let sections = build_remote_dashboard_sections(&draft, &model, &actions);

        let browser_section = section(&sections, "Browser Access");
        let advanced_section = section(&sections, "Advanced Network and Security");
        let invite_link = info_field(browser_section, "Invite link");

        assert!(contains_notice(
            browser_section,
            "Browser listener failed to bind"
        ));
        assert!(contains_notice(
            advanced_section,
            "Browser listener failed to bind"
        ));
        assert_eq!(
            invite_link.value,
            "Resolve the browser access error before sharing an invite."
        );
    }

    #[test]
    fn host_tab_hides_legacy_keep_sharing_in_background_section_when_idle() {
        let mut draft = browser_access_enabled_remote_draft();
        draft.remote_host_last_note = None;
        let model = settings_model(draft.clone());
        let actions = dummy_editor_actions();
        let sections = build_remote_dashboard_sections(&draft, &model, &actions);

        assert!(sections
            .iter()
            .all(|section| section.title != "Advanced Network and Security"));
    }

    fn settings_model(draft: SettingsDraft) -> EditorPaneModel {
        EditorPaneModel {
            panel: EditorPanel::Settings(draft),
            active_field: None,
            cursor: 0,
            selection_anchor: None,
            notice: None,
            updater: UpdaterSnapshot {
                configured: false,
                current_version: "0.0.0".to_string(),
                endpoints: Vec::new(),
                stage: UpdaterStage::Idle,
                target_version: None,
                detail: String::new(),
                release_notes: None,
                last_checked_at: None,
                downloaded_bytes: 0,
                total_bytes: None,
            },
            allow_mutation: true,
        }
    }

    fn dummy_editor_actions() -> EditorActions {
        EditorActions {
            on_action: Arc::new(|_| Box::new(|_, _, _| {})),
            on_focus_at: Arc::new(|_, _| Box::new(|_, _, _| {})),
            on_drag_to: Arc::new(|_, _| Box::new(|_, _, _| {})),
        }
    }

    fn section<'a>(sections: &'a [FormSection], title: &str) -> &'a FormSection {
        sections
            .iter()
            .find(|section| section.title == title)
            .unwrap_or_else(|| panic!("missing section {title}"))
    }

    fn info_field<'a>(section: &'a FormSection, label: &str) -> &'a FormInfoField {
        section
            .fields
            .iter()
            .find_map(|field| match field {
                FormField::Info(field) if field.label == label => Some(field),
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing info field {label}"))
    }

    fn contains_notice(section: &FormSection, message: &str) -> bool {
        section.fields.iter().any(|field| match field {
            FormField::Notice(notice) => notice.message == message,
            _ => false,
        })
    }

    fn contains_info_field(section: &FormSection, label: &str) -> bool {
        section.fields.iter().any(|field| match field {
            FormField::Info(field) => field.label == label,
            _ => false,
        })
    }
}

fn sample_wizard_initial() -> AddProjectWizard {
    AddProjectWizard {
        name: String::new(),
        color: "#6366f1".to_string(),
        root_path: String::new(),
        cursor: 0,
        name_focused: false,
        step: 1,
        scan_message: Some(
            "Pick a repository root to scan for package.json files, Cargo projects, and env ports."
                .to_string(),
        ),
        scan_entries: Vec::new(),
        selected_folders: BTreeSet::new(),
        selected_scripts: HashMap::new(),
        selected_port_variables: HashMap::new(),
    }
}

fn sample_wizard(step: u8) -> AddProjectWizard {
    let api_entry = RootScanEntry {
        path: "C:\\Code\\personal\\househunter\\api".to_string(),
        name: "api".to_string(),
        has_env: true,
        project_type: "node".to_string(),
        scripts: vec![
            ScannedScript {
                name: "dev".to_string(),
                command: "tsx watch src/server.ts".to_string(),
            },
            ScannedScript {
                name: "migrate".to_string(),
                command: "drizzle-kit migrate".to_string(),
            },
        ],
        ports: vec![
            ScannedPort {
                variable: "PORT".to_string(),
                port: 4555,
                source: ".env".to_string(),
            },
            ScannedPort {
                variable: "SMTP_PORT".to_string(),
                port: 1025,
                source: ".env".to_string(),
            },
        ],
    };
    let web_entry = RootScanEntry {
        path: "C:\\Code\\personal\\househunter\\web".to_string(),
        name: "web".to_string(),
        has_env: true,
        project_type: "node".to_string(),
        scripts: vec![
            ScannedScript {
                name: "dev".to_string(),
                command: "vite".to_string(),
            },
            ScannedScript {
                name: "build".to_string(),
                command: "vite build".to_string(),
            },
        ],
        ports: vec![ScannedPort {
            variable: "VITE_DEV_PORT".to_string(),
            port: 5173,
            source: ".env.local".to_string(),
        }],
    };

    let mut wizard = AddProjectWizard {
        name: "House Hunter".to_string(),
        color: "#ef4444".to_string(),
        root_path: "C:\\Code\\personal\\househunter".to_string(),
        cursor: "House Hunter".chars().count(),
        name_focused: false,
        step,
        scan_message: Some(
            "Discovered 2 folder(s). Review scripts and ports before creating the project."
                .to_string(),
        ),
        scan_entries: vec![api_entry.clone(), web_entry.clone()],
        selected_folders: [api_entry.path.clone(), web_entry.path.clone()]
            .into_iter()
            .collect(),
        selected_scripts: HashMap::new(),
        selected_port_variables: HashMap::new(),
    };

    wizard.selected_scripts.insert(
        api_entry.path.clone(),
        ["dev".to_string(), "migrate".to_string()]
            .into_iter()
            .collect(),
    );
    wizard.selected_scripts.insert(
        web_entry.path.clone(),
        ["dev".to_string()].into_iter().collect(),
    );
    wizard
        .selected_port_variables
        .insert(api_entry.path.clone(), Some("PORT".to_string()));
    wizard
        .selected_port_variables
        .insert(web_entry.path.clone(), Some("VITE_DEV_PORT".to_string()));

    wizard
}

fn render_project_panel(
    draft: &ProjectDraft,
    model: &EditorPaneModel,
    actions: &EditorActions,
) -> impl IntoElement {
    let on_toggle_pinned = (actions.on_action)(EditorAction::ToggleProjectPinned);
    let on_toggle_save_logs = (actions.on_action)(EditorAction::ToggleProjectSaveLogs);

    render_form_sections(
        vec![
            FormSection::new("Project").fields(vec![
                FormField::text(
                    "Name",
                    "Shown in the sidebar.",
                    draft.name.clone(),
                    EditorField::Project(ProjectField::Name),
                ),
                FormField::text(
                    "Root folder",
                    "Main repo or workspace path.",
                    draft.root_path.clone(),
                    EditorField::Project(ProjectField::RootPath),
                ),
                FormField::text(
                    "Accent color",
                    "Sidebar marker color, for example #6366f1.",
                    draft.color.clone(),
                    EditorField::Project(ProjectField::Color),
                ),
            ]),
            FormSection::new("Notes").field(FormField::multiline(
                "Notes",
                "Optional setup notes or reminders.",
                draft.notes.clone(),
                EditorField::Project(ProjectField::Notes),
            )),
            FormSection::new("Behavior").fields(vec![
                FormField::toggle(
                    "Save logs",
                    draft.save_log_files,
                    "Write command output to disk.",
                    on_toggle_save_logs,
                ),
                FormField::toggle(
                    "Pin in sidebar",
                    draft.pinned,
                    "Keep this project near the top.",
                    on_toggle_pinned,
                ),
            ]),
        ],
        model,
        actions,
    )
}

fn render_folder_panel(
    draft: &FolderDraft,
    model: &EditorPaneModel,
    actions: &EditorActions,
) -> impl IntoElement {
    let on_toggle_hidden = (actions.on_action)(EditorAction::ToggleFolderHidden);
    let on_pick_folder = (actions.on_action)(EditorAction::PickFolderPath);
    let on_scan_folder = (actions.on_action)(EditorAction::ScanFolderPath);
    let on_load_env = (actions.on_action)(EditorAction::LoadFolderEnvFile);
    let on_open_terminal = (actions.on_action)(EditorAction::OpenFolderExternalTerminal);
    let scan_notice = draft
        .scan_message
        .as_ref()
        .filter(|message| draft.scan_result.is_none() || !message.starts_with("Discovered "));

    let mut sections = vec![FormSection::new("Folder").fields(vec![
        FormField::text(
            "Name",
            "Shown in the workspace.",
            draft.name.clone(),
            EditorField::Folder(FolderField::Name),
        ),
        FormField::choice(
            "Path",
            if draft.folder_path.is_empty() {
                "Choose folder".to_string()
            } else {
                draft.folder_path.clone()
            },
            Some(if draft.folder_path.is_empty() {
                "Click to choose the folder on disk.".to_string()
            } else {
                "Click to replace the current folder.".to_string()
            }),
            on_pick_folder,
        ),
        FormField::toggle(
            "Show in sidebar",
            !draft.hidden,
            "Turn this off to keep the folder without showing it in the list.",
            on_toggle_hidden,
        ),
        FormField::action_group(
            FormActionGroup::new("Actions")
                .action(
                    FormAction::new(
                        "Scan folder",
                        if draft.is_scanning {
                            "Scanning..."
                        } else {
                            "Scan"
                        },
                        on_scan_folder,
                    )
                    .description("Refresh commands, ports, and repo status.")
                    .style(SurfaceActionButtonStyle::Primary)
                    .badge(if draft.is_scanning {
                        SurfaceBadge::new("Busy", SurfaceTone::Accent)
                    } else {
                        SurfaceBadge::new("Ready", SurfaceTone::Muted)
                    }),
                )
                .action(
                    FormAction::new(
                        "Open terminal",
                        if draft.folder_path.is_empty() {
                            "Pick folder first"
                        } else {
                            "Open"
                        },
                        on_open_terminal,
                    )
                    .description("Open this folder in your system terminal."),
                ),
        ),
    ])];

    let mut detected_fields = Vec::new();
    if let Some(message) = scan_notice {
        detected_fields.push(FormField::notice(message.clone(), SurfaceTone::Accent));
    }
    if let Some(branch) = draft.git_branch.as_ref() {
        detected_fields.push(FormField::info("Branch", branch.clone(), None));
    }
    if let Some(status) = draft.dependency_status.as_ref() {
        detected_fields.push(FormField::info(
            "Dependencies",
            status.status.clone(),
            Some(status.message.clone()),
        ));
    }
    if !detected_fields.is_empty() {
        sections.push(FormSection::new("Detected").fields(detected_fields));
    }

    if let Some(scan_result) = draft.scan_result.as_ref() {
        sections.push(
            FormSection::new("Commands and Port").field(FormField::custom(
                render_folder_scan_panel(draft, scan_result, model, actions).into_any_element(),
            )),
        );
    }

    let mut environment_fields = vec![
        FormField::text(
            "Env file",
            "Relative path inside this folder, for example .env.local.",
            draft.env_file_path.clone(),
            EditorField::Folder(FolderField::EnvFilePath),
        ),
        FormField::action_group({
            let actions_group = FormActionGroup::new("Env file actions").action(
                FormAction::new(
                    "Load env file",
                    if draft.env_file_loaded {
                        "Reload"
                    } else {
                        "Load"
                    },
                    on_load_env,
                )
                .description("Load the env file for inline editing."),
            );
            actions_group
        }),
        FormField::text(
            "Default port env var",
            "Env var used for the folder port.",
            draft.port_variable.clone(),
            EditorField::Folder(FolderField::PortVariable),
        ),
    ];

    if draft.env_file_loaded || !draft.env_file_contents.is_empty() {
        environment_fields.push(FormField::multiline_sized(
            "Env contents",
            "Edit inline. Comments and blank lines are preserved when you save the folder.",
            draft.env_file_contents.clone(),
            EditorField::Folder(FolderField::EnvContents),
            320.0,
        ));
    } else {
        environment_fields.push(FormField::notice(
            "Load the env file to edit it inline.",
            SurfaceTone::Muted,
        ));
    }

    sections.push(FormSection::new("Environment").fields(environment_fields));

    render_form_sections(sections, model, actions)
}

fn render_command_panel(
    draft: &CommandDraft,
    model: &EditorPaneModel,
    actions: &EditorActions,
) -> impl IntoElement {
    let on_toggle_restart = (actions.on_action)(EditorAction::ToggleCommandAutoRestart);
    let on_toggle_clear_logs = (actions.on_action)(EditorAction::ToggleCommandClearLogs);

    render_form_sections(
        vec![
            FormSection::new("Command").fields(vec![
                FormField::text(
                    "Name",
                    "Shown in the sidebar and tabs.",
                    draft.label.clone(),
                    EditorField::Command(CommandField::Label),
                ),
                FormField::text(
                    "Run",
                    "Program or script to launch.",
                    draft.command.clone(),
                    EditorField::Command(CommandField::Command),
                ),
                FormField::text(
                    "Arguments",
                    "Space-separated arguments.",
                    draft.args_text.clone(),
                    EditorField::Command(CommandField::Args),
                ),
            ]),
            FormSection::new("Runtime").fields(vec![
                FormField::text(
                    "Env overrides",
                    "Semicolon-separated KEY=VALUE pairs.",
                    draft.env_text.clone(),
                    EditorField::Command(CommandField::Env),
                ),
                FormField::text(
                    "Known port",
                    "Numeric port, if this command owns one.",
                    draft.port_text.clone(),
                    EditorField::Command(CommandField::Port),
                ),
            ]),
            FormSection::new("Restart").fields(vec![
                FormField::toggle(
                    "Restart automatically",
                    draft.auto_restart,
                    "Restart the command after it exits.",
                    on_toggle_restart,
                ),
                FormField::toggle(
                    "Clear output on restart",
                    draft.clear_logs_on_restart,
                    "Clear previous output before starting again.",
                    on_toggle_clear_logs,
                ),
            ]),
        ],
        model,
        actions,
    )
}

fn render_ssh_panel(
    draft: &SshDraft,
    model: &EditorPaneModel,
    actions: &EditorActions,
) -> impl IntoElement {
    render_form_sections(
        vec![
            FormSection::new("Connection").fields(vec![
                FormField::text(
                    "Name",
                    "Shown in the sidebar.",
                    draft.label.clone(),
                    EditorField::Ssh(SshField::Label),
                ),
                FormField::text(
                    "Host",
                    "Hostname or IP address.",
                    draft.host.clone(),
                    EditorField::Ssh(SshField::Host),
                ),
                FormField::text(
                    "Username",
                    "Remote account name.",
                    draft.username.clone(),
                    EditorField::Ssh(SshField::Username),
                ),
                FormField::text(
                    "Port",
                    "Leave blank to use 22.",
                    draft.port_text.clone(),
                    EditorField::Ssh(SshField::Port),
                ),
            ]),
            FormSection::new("Authentication").field(FormField::text(
                "Password",
                "Leave blank if you use keys or an agent.",
                draft.password.clone(),
                EditorField::Ssh(SshField::Password),
            )),
        ],
        model,
        actions,
    )
}

fn minimize_to_tray_hint() -> &'static str {
    "Keep DevManager running in the taskbar when the window close button is used"
}

fn render_folder_scan_panel(
    draft: &FolderDraft,
    scan_result: &ScanResult,
    model: &EditorPaneModel,
    actions: &EditorActions,
) -> impl IntoElement {
    let script_summary = format!(
        "{} commands found, {} selected",
        scan_result.scripts.len(),
        draft.selected_scanned_scripts.len()
    );

    let mut fields = vec![FormField::info(
        "Summary",
        script_summary,
        Some("Only missing commands are added when you save.".to_string()),
    )];

    if !scan_result.scripts.is_empty() {
        let mut list = FormSelectionList::new("Add commands")
            .hint("Detected scripts that can become commands.");
        for script in &scan_result.scripts {
            let on_toggle_script =
                (actions.on_action)(EditorAction::ToggleFolderScanScript(script.name.clone()));
            list = list.row(FormSelectionRow::new(
                script.name.clone(),
                Some(script.command.clone()),
                draft.selected_scanned_scripts.contains(&script.name),
                on_toggle_script,
            ));
        }
        fields.push(FormField::selection_list(list));
    }

    if !scan_result.ports.is_empty() {
        let mut list = FormSelectionList::new("Default port")
            .hint("Choose which env var should fill the folder port setting.");
        list = list.row(FormSelectionRow::new(
            "None",
            Some("Do not set a default port.".to_string()),
            draft.selected_scanned_port_variable.is_none(),
            (actions.on_action)(EditorAction::SelectFolderPortVariable(None)),
        ));
        for port in &scan_result.ports {
            let on_select_port = (actions.on_action)(EditorAction::SelectFolderPortVariable(Some(
                port.variable.clone(),
            )));
            list = list.row(FormSelectionRow::new(
                format!("{} = {}", port.variable, port.port),
                Some(port.source.clone()),
                draft.selected_scanned_port_variable.as_deref() == Some(port.variable.as_str()),
                on_select_port,
            ));
        }
        fields.push(FormField::selection_list(list));
    }

    render_form_fields(fields, model, actions)
}

fn render_updater_panel(
    updater: &UpdaterSnapshot,
    on_check: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
    on_download: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    on_install: Option<Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(render_info_row(
            "Updater status",
            updater_stage_label(&updater.stage),
            Some(updater.detail.as_str()),
        ))
        .children(updater.target_version.as_ref().map(|version| {
            render_info_row(
                "Latest available version",
                version.as_str(),
                Some("From the signed release manifest."),
            )
        }))
        .children(updater.endpoints.first().map(|endpoint| {
            render_info_row(
                "Manifest endpoint",
                endpoint.as_str(),
                Some("Where update checks run."),
            )
        }))
        .child(render_choice_row(
            "Check for updates",
            "Check now",
            Some("Check in the background."),
            on_check,
        ))
        .children(on_download.map(|on_download| {
            render_choice_row(
                "Download update",
                "Download now",
                Some("Download and verify the installer."),
                on_download,
            )
        }))
        .children(on_install.map(|on_install| {
            render_choice_row(
                "Restart to update",
                "Install and close DevManager",
                Some("Close DevManager and launch the installer."),
                on_install,
            )
        }))
        .children(updater.release_notes.as_ref().map(|notes| {
            render_info_row(
                "Release notes",
                notes.as_str(),
                Some("From the signed release manifest."),
            )
        }))
}

fn updater_stage_label(stage: &UpdaterStage) -> &'static str {
    match stage {
        UpdaterStage::Disabled => "disabled",
        UpdaterStage::Idle => "idle",
        UpdaterStage::Checking => "checking",
        UpdaterStage::UpToDate => "up to date",
        UpdaterStage::UpdateAvailable => "update found",
        UpdaterStage::Downloading => "downloading",
        UpdaterStage::ReadyToInstall => "ready to install",
        UpdaterStage::Installing => "installing",
        UpdaterStage::Error => "error",
    }
}

fn display_text_with_cursor(value: &str, cursor: usize) -> String {
    let chars: Vec<char> = value.chars().collect();
    let cursor = cursor.min(chars.len());
    let mut display = String::new();
    for (index, character) in chars.iter().enumerate() {
        if index == cursor {
            display.push('|');
        }
        display.push(*character);
    }
    if cursor == chars.len() {
        display.push('|');
    }
    if display.is_empty() {
        display.push('|');
    }
    display
}

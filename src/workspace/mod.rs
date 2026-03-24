use crate::models::{
    DefaultTerminal, DependencyStatus, MacTerminalProfile, RootScanEntry, ScanResult,
};
use crate::theme;
use crate::updater::{UpdaterSnapshot, UpdaterStage};
use gpui::{
    anchored, deferred, div, px, rgb, AnyElement, App, Corner, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, SharedString, StatefulInteractiveElement, Styled,
    Window,
};
use std::collections::{BTreeSet, HashMap};

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

fn render_wizard_step1(wizard: &AddProjectWizard, actions: WizardActions<'_>) -> impl IntoElement {
    let on_cancel = (actions.on_action)(WizardAction::Cancel);
    let on_configure = (actions.on_action)(WizardAction::Configure);
    let on_pick_root = (actions.on_action)(WizardAction::PickRootFolder);
    let on_click_name = (actions.on_action)(WizardAction::ClickName);

    let focused = wizard.name_focused;
    let display_name = if focused || !wizard.name.is_empty() {
        let text = if wizard.name.is_empty() {
            ""
        } else {
            &wizard.name
        };
        display_text_with_cursor(text, wizard.cursor)
    } else {
        "My App".to_string()
    };
    let name_is_placeholder = wizard.name.is_empty() && !focused;

    deferred(
        anchored()
            .snap_to_window()
            .anchor(Corner::TopLeft)
            .child(
                // Backdrop
                div()
                    .id("wizard-backdrop")
                    .occlude()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        // Modal card
                        div()
                            .w(px(420.0))
                            .rounded_md()
                            .bg(rgb(theme::PANEL_HEADER_BG))
                            .border_1()
                            .border_color(rgb(theme::BORDER_PRIMARY))
                            .flex()
                            .flex_col()
                            .overflow_hidden()
                            // Header
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .px(px(16.0))
                                    .py(px(12.0))
                                    .border_b_1()
                                    .border_color(rgb(theme::BORDER_PRIMARY))
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(gpui::FontWeight::BOLD)
                                            .text_color(rgb(theme::TEXT_PRIMARY))
                                            .child("Add Project"),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(theme::TEXT_MUTED))
                                            .cursor_pointer()
                                            .hover(|s| s.text_color(rgb(theme::TEXT_PRIMARY)))
                                            .child("\u{2715}")
                                            .on_mouse_down(MouseButton::Left, on_cancel),
                                    ),
                            )
                            // Body
                            .child(
                                div()
                                    .px(px(16.0))
                                    .py(px(16.0))
                                    .flex()
                                    .flex_col()
                                    .gap(px(16.0))
                                    // Name field
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap(px(6.0))
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(rgb(theme::TEXT_MUTED))
                                                    .child("Project Name"),
                                            )
                                            .child(
                                                div()
                                                    .w_full()
                                                    .px(px(10.0))
                                                    .py(px(8.0))
                                                    .rounded_sm()
                                                    .bg(rgb(theme::APP_BG))
                                                    .border_1()
                                                    .border_color(rgb(if focused {
                                                        theme::PRIMARY
                                                    } else {
                                                        theme::BORDER_SECONDARY
                                                    }))
                                                    .text_sm()
                                                    .text_color(rgb(if name_is_placeholder {
                                                        theme::TEXT_SUBTLE
                                                    } else {
                                                        theme::TEXT_PRIMARY
                                                    }))
                                                    .cursor_text()
                                                    .child(SharedString::from(display_name))
                                                    .on_mouse_down(MouseButton::Left, on_click_name),
                                            ),
                                    )
                                    // Color picker
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap(px(6.0))
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(rgb(theme::TEXT_MUTED))
                                                    .child("Color"),
                                            )
                                            .child(
                                                div()
                                                    .flex()
                                                    .items_center()
                                                    .gap(px(8.0))
                                                    .children(
                                                        PROJECT_COLOR_PRESETS.iter().map(
                                                            |(hex, name)| {
                                                                let selected =
                                                                    wizard.color == *name;
                                                                let on_select = (actions.on_action)(
                                                                    WizardAction::SelectColor(
                                                                        name.to_string(),
                                                                    ),
                                                                );
                                                                div()
                                                                    .size(px(28.0))
                                                                    .rounded_full()
                                                                    .cursor_pointer()
                                                                    .flex()
                                                                    .items_center()
                                                                    .justify_center()
                                                                    .border_2()
                                                                    .border_color(rgb(if selected {
                                                                        0xffffff
                                                                    } else {
                                                                        theme::PANEL_HEADER_BG
                                                                    }))
                                                                    .child(
                                                                        div()
                                                                            .size(px(20.0))
                                                                            .rounded_full()
                                                                            .bg(rgb(*hex)),
                                                                    )
                                                                    .on_mouse_down(
                                                                        MouseButton::Left,
                                                                        on_select,
                                                                    )
                                                                    .into_any_element()
                                                            },
                                                        ),
                                                    ),
                                            ),
                                    )
                                    // Root folder
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap(px(6.0))
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(rgb(theme::TEXT_MUTED))
                                                    .child("Root Folder"),
                                            )
                                            .child(
                                                div()
                                                    .w_full()
                                                    .px(px(10.0))
                                                    .py(px(8.0))
                                                    .rounded_sm()
                                                    .bg(rgb(theme::APP_BG))
                                                    .border_1()
                                                    .border_color(rgb(theme::BORDER_SECONDARY))
                                                    .text_sm()
                                                    .text_color(rgb(if wizard.root_path.is_empty() {
                                                        theme::TEXT_SUBTLE
                                                    } else {
                                                        theme::TEXT_PRIMARY
                                                    }))
                                                    .cursor_pointer()
                                                    .hover(|s| {
                                                        s.border_color(rgb(theme::TEXT_SUBTLE))
                                                    })
                                                    .child(SharedString::from(
                                                        if wizard.root_path.is_empty() {
                                                            "Select root folder\u{2026}".to_string()
                                                        } else {
                                                            wizard.root_path.clone()
                                                        },
                                                    ))
                                                    .on_mouse_down(MouseButton::Left, on_pick_root),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(rgb(theme::TEXT_SUBTLE))
                                                    .child("Sub-folders with package.json or Cargo.toml will be discovered automatically"),
                                            ),
                                    )
                                    .children(wizard.scan_message.as_ref().map(|message| {
                                        div()
                                            .text_xs()
                                            .text_color(rgb(theme::TEXT_MUTED))
                                            .child(SharedString::from(message.clone()))
                                    }))
                                    // Discovered folders
                                    .children(
                                        (!wizard.scan_entries.is_empty()).then(|| {
                                            let count = wizard.scan_entries.len();
                                            div()
                                                .flex()
                                                .flex_col()
                                                .gap(px(6.0))
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(rgb(theme::TEXT_MUTED))
                                                        .child(SharedString::from(format!(
                                                            "Discovered folders ({count})"
                                                        ))),
                                                )
                                                .children(wizard.scan_entries.iter().map(|entry| {
                                                    let selected = wizard
                                                        .selected_folders
                                                        .contains(&entry.path);
                                                    let on_toggle = (actions.on_action)(
                                                        WizardAction::ToggleFolder(
                                                            entry.path.clone(),
                                                        ),
                                                    );
                                                    let detail = wizard_scan_detail(entry);
                                                    div()
                                                        .flex()
                                                        .items_center()
                                                        .justify_between()
                                                        .gap(px(8.0))
                                                        .px(px(10.0))
                                                        .py(px(6.0))
                                                        .rounded_sm()
                                                        .bg(rgb(theme::APP_BG))
                                                        .cursor_pointer()
                                                        .hover(|s| {
                                                            s.bg(rgb(theme::ROW_HOVER_BG))
                                                        })
                                                        .child(
                                                            div()
                                                                .flex()
                                                                .items_center()
                                                                .gap(px(8.0))
                                                                .child(
                                                                    div()
                                                                        .size(px(16.0))
                                                                        .rounded_sm()
                                                                        .flex()
                                                                        .items_center()
                                                                        .justify_center()
                                                                        .bg(rgb(if selected {
                                                                            theme::PRIMARY
                                                                        } else {
                                                                            theme::BORDER_SECONDARY
                                                                        }))
                                                                        .child(
                                                                            div()
                                                                                .text_xs()
                                                                                .text_color(rgb(
                                                                                    0xffffff,
                                                                                ))
                                                                                .child(
                                                                                    if selected {
                                                                                        "\u{2713}"
                                                                                    } else {
                                                                                        ""
                                                                                    },
                                                                                ),
                                                                        ),
                                                                )
                                                                .child(
                                                                    div()
                                                                        .text_sm()
                                                                        .text_color(rgb(
                                                                            theme::TEXT_PRIMARY,
                                                                        ))
                                                                        .child(SharedString::from(
                                                                            entry.name.clone(),
                                                                        )),
                                                                ),
                                                        )
                                                        .child(
                                                            div()
                                                                .text_xs()
                                                                .text_color(rgb(
                                                                    theme::TEXT_SUBTLE,
                                                                ))
                                                                .child(SharedString::from(
                                                                    detail,
                                                                )),
                                                        )
                                                        .on_mouse_down(
                                                            MouseButton::Left,
                                                            on_toggle,
                                                        )
                                                        .into_any_element()
                                                }))
                                                .into_any_element()
                                        }),
                                    ),
                            )
                            // Footer
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_end()
                                    .gap(px(8.0))
                                    .px(px(16.0))
                                    .py(px(12.0))
                                    .border_t_1()
                                    .border_color(rgb(theme::BORDER_PRIMARY))
                                    .child(
                                        div()
                                            .px(px(12.0))
                                            .py(px(6.0))
                                            .rounded_sm()
                                            .text_xs()
                                            .text_color(rgb(theme::TEXT_MUTED))
                                            .cursor_pointer()
                                            .hover(|s| {
                                                s.text_color(rgb(theme::TEXT_PRIMARY))
                                                    .bg(rgb(theme::ROW_HOVER_BG))
                                            })
                                            .child("Cancel")
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                (actions.on_action)(WizardAction::Cancel),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .px(px(14.0))
                                            .py(px(6.0))
                                            .rounded_sm()
                                            .bg(rgb(theme::PRIMARY))
                                            .text_xs()
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(rgb(theme::SELECTION_TEXT))
                                            .cursor_pointer()
                                            .hover(|s| s.bg(rgb(theme::PRIMARY_HOVER)))
                                            .child("Configure \u{2192}")
                                            .on_mouse_down(MouseButton::Left, on_configure),
                                    ),
                            ),
                    ),
            ),
    )
    .with_priority(2)
}

fn wizard_scan_detail(entry: &RootScanEntry) -> String {
    let scripts = entry.scripts.len();
    let has_env = entry.has_env;
    match (scripts, has_env) {
        (0, false) => String::new(),
        (0, true) => ".env".to_string(),
        (n, false) => format!("{n} scripts"),
        (n, true) => format!("{n} scripts + .env"),
    }
}

fn render_wizard_step2(wizard: &AddProjectWizard, actions: WizardActions<'_>) -> impl IntoElement {
    let on_cancel = (actions.on_action)(WizardAction::Cancel);
    let on_back = (actions.on_action)(WizardAction::Back);
    let on_create = (actions.on_action)(WizardAction::Create);

    let selected_entries: Vec<&RootScanEntry> = wizard
        .scan_entries
        .iter()
        .filter(|e| wizard.selected_folders.contains(&e.path))
        .collect();

    deferred(
        anchored().snap_to_window().anchor(Corner::TopLeft).child(
            div()
                .id("wizard-step2-backdrop")
                .occlude()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .w(px(520.0))
                        .max_h(px(600.0))
                        .rounded_md()
                        .bg(rgb(theme::PANEL_HEADER_BG))
                        .border_1()
                        .border_color(rgb(theme::BORDER_PRIMARY))
                        .flex()
                        .flex_col()
                        .overflow_hidden()
                        // Header
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .px(px(16.0))
                                .py(px(12.0))
                                .border_b_1()
                                .border_color(rgb(theme::BORDER_PRIMARY))
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(gpui::FontWeight::BOLD)
                                        .text_color(rgb(theme::TEXT_PRIMARY))
                                        .child("Add Project \u{2014} Configure Folders"),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(theme::TEXT_MUTED))
                                        .cursor_pointer()
                                        .hover(|s| s.text_color(rgb(theme::TEXT_PRIMARY)))
                                        .child("\u{2715}")
                                        .on_mouse_down(MouseButton::Left, on_cancel),
                                ),
                        )
                        // Body (scrollable)
                        .child(
                            div()
                                .flex_1()
                                .id("wizard-step2-scroll")
                                .overflow_y_scroll()
                                .scrollbar_width(px(6.0))
                                .child(
                                    div()
                                        .px(px(16.0))
                                        .py(px(16.0))
                                        .flex()
                                        .flex_col()
                                        .gap(px(20.0))
                                        .children(selected_entries.iter().map(|entry| {
                                            render_wizard_folder_config(entry, wizard, &actions)
                                                .into_any_element()
                                        })),
                                ),
                        )
                        // Footer
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .px(px(16.0))
                                .py(px(12.0))
                                .border_t_1()
                                .border_color(rgb(theme::BORDER_PRIMARY))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(rgb(theme::TEXT_MUTED))
                                        .cursor_pointer()
                                        .hover(|s| s.text_color(rgb(theme::TEXT_PRIMARY)))
                                        .child("\u{2190} Back")
                                        .on_mouse_down(MouseButton::Left, on_back),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(8.0))
                                        .child(
                                            div()
                                                .px(px(12.0))
                                                .py(px(6.0))
                                                .rounded_sm()
                                                .text_xs()
                                                .text_color(rgb(theme::TEXT_MUTED))
                                                .cursor_pointer()
                                                .hover(|s| {
                                                    s.text_color(rgb(theme::TEXT_PRIMARY))
                                                        .bg(rgb(theme::ROW_HOVER_BG))
                                                })
                                                .child("Cancel")
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    (actions.on_action)(WizardAction::Cancel),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .px(px(14.0))
                                                .py(px(6.0))
                                                .rounded_sm()
                                                .bg(rgb(theme::PRIMARY))
                                                .text_xs()
                                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                                .text_color(rgb(theme::SELECTION_TEXT))
                                                .cursor_pointer()
                                                .hover(|s| s.bg(rgb(theme::PRIMARY_HOVER)))
                                                .child("Create Project")
                                                .on_mouse_down(MouseButton::Left, on_create),
                                        ),
                                ),
                        ),
                ),
        ),
    )
    .with_priority(2)
}

fn render_wizard_folder_config(
    entry: &RootScanEntry,
    wizard: &AddProjectWizard,
    actions: &WizardActions<'_>,
) -> impl IntoElement {
    let selected_scripts = wizard.selected_scripts.get(&entry.path);
    let selected_port = wizard
        .selected_port_variables
        .get(&entry.path)
        .cloned()
        .flatten();

    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        // Folder header
        .child(
            div()
                .flex()
                .items_baseline()
                .gap(px(8.0))
                .child(
                    div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(rgb(theme::TEXT_PRIMARY))
                        .child(SharedString::from(entry.name.clone())),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_SUBTLE))
                        .child(SharedString::from(entry.path.clone())),
                ),
        )
        // Scripts
        .children((!entry.scripts.is_empty()).then(|| {
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .children(entry.scripts.iter().map(|script| {
                    let is_selected = selected_scripts
                        .map(|s| s.contains(&script.name))
                        .unwrap_or(false);
                    let on_toggle = (actions.on_action)(WizardAction::ToggleScript {
                        folder_path: entry.path.clone(),
                        script_name: script.name.clone(),
                    });
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(8.0))
                        .px(px(6.0))
                        .py(px(4.0))
                        .rounded_sm()
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(theme::ROW_HOVER_BG)))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .child(
                                    div()
                                        .size(px(16.0))
                                        .rounded_sm()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .bg(rgb(if is_selected {
                                            theme::PRIMARY
                                        } else {
                                            theme::BORDER_SECONDARY
                                        }))
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(rgb(0xffffff))
                                                .child(if is_selected { "\u{2713}" } else { "" }),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(rgb(theme::TEXT_PRIMARY))
                                        .child(SharedString::from(script.name.clone())),
                                ),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_SUBTLE))
                                .child(SharedString::from(script.command.clone())),
                        )
                        .on_mouse_down(MouseButton::Left, on_toggle)
                        .into_any_element()
                }))
                .into_any_element()
        }))
        // Port variables
        .children((!entry.ports.is_empty()).then(|| {
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .pt(px(4.0))
                        .child("Port Variable (select one)"),
                )
                .children(entry.ports.iter().map(|port| {
                    let is_selected = selected_port.as_deref() == Some(port.variable.as_str());
                    let on_select = (actions.on_action)(WizardAction::SelectPortVariable {
                        folder_path: entry.path.clone(),
                        variable: Some(port.variable.clone()),
                    });
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap(px(8.0))
                        .px(px(6.0))
                        .py(px(6.0))
                        .rounded_sm()
                        .cursor_pointer()
                        .bg(rgb(if is_selected {
                            theme::APP_BG
                        } else {
                            theme::PANEL_HEADER_BG
                        }))
                        .border_1()
                        .border_color(rgb(if is_selected {
                            theme::PRIMARY
                        } else {
                            theme::PANEL_HEADER_BG
                        }))
                        .hover(|s| s.bg(rgb(theme::ROW_HOVER_BG)))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .child(
                                    div()
                                        .size(px(14.0))
                                        .rounded_full()
                                        .border_2()
                                        .border_color(rgb(if is_selected {
                                            theme::PRIMARY
                                        } else {
                                            theme::TEXT_SUBTLE
                                        }))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .children(is_selected.then(|| {
                                            div()
                                                .size(px(6.0))
                                                .rounded_full()
                                                .bg(rgb(theme::PRIMARY))
                                        })),
                                )
                                .child(div().text_sm().text_color(rgb(theme::TEXT_PRIMARY)).child(
                                    SharedString::from(format!(
                                        "{}  = {}",
                                        port.variable, port.port
                                    )),
                                )),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_SUBTLE))
                                .child(SharedString::from(format!("({})", port.source))),
                        )
                        .on_mouse_down(MouseButton::Left, on_select)
                        .into_any_element()
                }))
                .into_any_element()
        }))
}

#[derive(Debug, Clone)]
pub enum EditorPanel {
    Settings(SettingsDraft),
    Project(ProjectDraft),
    Folder(FolderDraft),
    Command(CommandDraft),
    Ssh(SshDraft),
}

impl EditorPanel {
    pub fn title(&self) -> &'static str {
        match self {
            Self::Settings(_) => "Settings",
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
            Self::Settings(_) => "Click a field to type. Settings persist as you change them.",
            Self::Project(_) => "Project metadata and notes are persisted to config.json.",
            Self::Folder(_) => "Folders own command definitions and env helpers.",
            Self::Command(_) => "Args use space-separated tokens. Env uses KEY=VALUE;KEY2=VALUE2.",
            Self::Ssh(_) => "Saved SSH entries can now open native terminal sessions.",
        }
    }

    pub fn accent_color(&self) -> u32 {
        match self {
            Self::Settings(_) => theme::PRIMARY,
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
            Self::Settings(_) => "Workspace settings".to_string(),
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
pub struct SettingsDraft {
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
    pub open_picker: Option<SettingsPicker>,
}

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
            Self::Settings(SettingsField::LogBufferSize | SettingsField::TerminalFontSize)
                | Self::Command(CommandField::Port)
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
    pub notice: Option<String>,
    pub updater: UpdaterSnapshot,
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
    ToggleProjectPinned,
    ToggleProjectSaveLogs,
    ToggleFolderHidden,
    ToggleCommandAutoRestart,
    ToggleCommandClearLogs,
}

pub struct EditorActions<'a> {
    pub on_action: &'a dyn Fn(EditorAction) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
}

pub fn render_editor_surface(model: &EditorPaneModel, actions: EditorActions<'_>) -> AnyElement {
    if let EditorPanel::Settings(draft) = &model.panel {
        return render_settings_editor_surface(draft, model, &actions).into_any_element();
    }

    let title = model.panel.title();
    let subtitle = model.panel.subtitle();
    let save_label = model.panel.save_label();
    let accent = model.panel.accent_color();

    let body: AnyElement = match &model.panel {
        EditorPanel::Settings(draft) => {
            render_settings_panel(draft, model, &actions).into_any_element()
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
    let on_save = (actions.on_action)(EditorAction::Save);
    let on_delete = model
        .panel
        .show_delete()
        .then(|| (actions.on_action)(EditorAction::Delete));

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
                            .pt(px(28.0))
                            .pb(px(56.0))
                            .px(px(24.0))
                            .flex()
                            .flex_col()
                            .gap(px(18.0))
                            .children(model.notice.as_ref().map(|notice| {
                                render_notice_row(notice.as_str()).into_any_element()
                            }))
                            .child(render_editor_intro(&model.panel))
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
    actions: &EditorActions<'_>,
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
                        .child("Settings"),
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
    actions: &EditorActions<'_>,
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

    div()
        .flex()
        .flex_col()
        .gap(px(20.0))
        .children(
            model
                .notice
                .as_ref()
                .map(|notice| render_notice_row(notice.as_str()).into_any_element()),
        )
        // — General section
        .child(render_settings_section(
            "General",
            div()
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(render_settings_toggle_row(
                    "Confirm on close",
                    "Show confirmation dialog when closing with running servers",
                    draft.confirm_on_close,
                    on_toggle_confirm,
                ))
                .child(render_settings_toggle_row(
                    "Minimize to tray",
                    minimize_to_tray_hint(),
                    draft.minimize_to_tray,
                    on_toggle_tray,
                ))
                .child(render_settings_toggle_row(
                    "Resume previous session on startup",
                    "Restore open tabs and sidebar state on launch",
                    draft.restore_session_on_start,
                    on_toggle_restore,
                ))
                .child(render_settings_text_input(
                    "Log buffer size",
                    "Maximum log lines per command (100 - 100,000)",
                    draft.log_buffer_size.as_str(),
                    EditorField::Settings(SettingsField::LogBufferSize),
                    model,
                    actions,
                    Some(140.0),
                    "10000",
                ))
                .into_any_element(),
        ))
        // — Terminal section
        .child(render_settings_section(
            "Terminal",
            div()
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(render_settings_select_row(
                    if is_mac {
                        "Default terminal shell"
                    } else {
                        "Default terminal"
                    },
                    if is_mac {
                        "Shell used for Claude Code and interactive terminals on macOS"
                    } else {
                        "Shell used for Claude Code and interactive terminals"
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
                ))
                .child(render_settings_font_size_row(draft, actions))
                .children(is_mac.then(|| {
                    render_settings_toggle_row(
                        "Option acts as Meta",
                        "On macOS, treat Option as terminal Meta/Alt instead of character input",
                        draft.option_as_meta,
                        (actions.on_action)(EditorAction::ToggleOptionAsMeta),
                    )
                }))
                .child(render_settings_toggle_row(
                    "Copy on select",
                    "Copy terminal selections to the clipboard when you release the mouse",
                    draft.copy_on_select,
                    (actions.on_action)(EditorAction::ToggleCopyOnSelect),
                ))
                .child(render_settings_toggle_row(
                    "Keep selection after copy",
                    "Preserve the current terminal selection after a copy action",
                    draft.keep_selection_on_copy,
                    (actions.on_action)(EditorAction::ToggleKeepSelectionOnCopy),
                ))
                .child(render_settings_select_row(
                    "Notification sound",
                    "Sound played when an AI terminal finishes a long task",
                    notification_sound_label(&draft.notification_sound),
                    draft.open_picker == Some(SettingsPicker::NotificationSound),
                    on_toggle_sound_picker,
                    Some(render_settings_icon_button("♪", on_preview_sound).into_any_element()),
                    Some(180.0),
                    sound_options,
                ))
                .into_any_element(),
        ))
        // — AI section
        .child(render_settings_section(
            "AI Commands",
            div()
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(render_settings_text_input(
                    "Claude command",
                    "Command launched when opening a Claude terminal",
                    draft.claude_command.as_str(),
                    EditorField::Settings(SettingsField::ClaudeCommand),
                    model,
                    actions,
                    None,
                    "npx -y @anthropic-ai/claude-code@latest --dangerously-skip-permissions",
                ))
                .child(render_settings_text_input(
                    "Codex command",
                    "Command launched when opening a Codex terminal",
                    draft.codex_command.as_str(),
                    EditorField::Settings(SettingsField::CodexCommand),
                    model,
                    actions,
                    None,
                    "npx -y @openai/codex@latest --dangerously-bypass-approvals-and-sandbox",
                ))
                .into_any_element(),
        ))
        .child(render_settings_section(
            "Updates",
            render_updater_panel(&model.updater, on_check_updates, None, on_install_update)
                .into_any_element(),
        ))
        // — Data section
        .child(render_settings_section(
            "Data",
            div()
                .flex()
                .flex_col()
                .gap(px(10.0))
                .child(render_settings_inline_button(
                    "Import / Export Configuration",
                    false,
                    on_toggle_data_picker,
                ))
                .children(
                    (draft.open_picker == Some(SettingsPicker::DataActions)).then(|| {
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.0))
                            .child(render_settings_dropdown_option(
                                "Export config".to_string(),
                                false,
                                on_export,
                            ))
                            .child(render_settings_dropdown_option(
                                "Import config (merge)".to_string(),
                                false,
                                on_import_merge,
                            ))
                            .child(render_settings_dropdown_option(
                                "Import config (replace)".to_string(),
                                false,
                                on_import_replace,
                            ))
                            .into_any_element()
                    }),
                )
                .into_any_element(),
        ))
}

fn render_settings_toggle_row(
    label: &str,
    description: &str,
    checked: bool,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    let mut toggle = div()
        .w(px(36.0))
        .h(px(20.0))
        .p(px(2.0))
        .rounded_full()
        .flex()
        .items_center()
        .cursor_pointer()
        .bg(rgb(if checked {
            theme::PRIMARY
        } else {
            theme::BORDER_SECONDARY
        }))
        .on_mouse_down(MouseButton::Left, on_click);
    toggle = if checked {
        toggle.justify_end()
    } else {
        toggle.justify_start()
    };

    div()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(16.0))
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
                        .child(SharedString::from(label.to_string())),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_SUBTLE))
                        .child(SharedString::from(description.to_string())),
                ),
        )
        .child(toggle.child(div().size(px(14.0)).rounded_full().bg(rgb(0xffffff))))
}

fn render_settings_text_input(
    label: &str,
    hint: &str,
    value: &str,
    field: EditorField,
    model: &EditorPaneModel,
    actions: &EditorActions<'_>,
    width: Option<f32>,
    placeholder: &str,
) -> impl IntoElement {
    let focused = model.active_field == Some(field);
    let display_value = if focused {
        display_text_with_cursor(value, model.cursor)
    } else if value.is_empty() {
        placeholder.to_string()
    } else {
        value.to_string()
    };
    let on_focus = (actions.on_action)(EditorAction::FocusField(field));

    let mut input = div()
        .px(px(10.0))
        .py(px(6.0))
        .rounded_sm()
        .bg(rgb(if focused {
            0x1e1e22
        } else {
            theme::PANEL_HEADER_BG
        }))
        .border_1()
        .border_color(rgb(if focused {
            theme::PRIMARY
        } else {
            theme::BORDER_SECONDARY
        }))
        .text_xs()
        .text_color(rgb(if value.is_empty() && !focused {
            theme::TEXT_SUBTLE
        } else {
            theme::TEXT_PRIMARY
        }))
        .overflow_hidden()
        .whitespace_nowrap()
        .cursor_text()
        .child(SharedString::from(display_value))
        .on_mouse_down(MouseButton::Left, on_focus);
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
    actions: &EditorActions<'_>,
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

fn render_settings_section(label: &str, body: AnyElement) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap(px(12.0))
        .child(
            div()
                .pb(px(4.0))
                .border_b_1()
                .border_color(rgb(theme::BORDER_SECONDARY))
                .child(
                    div()
                        .text_xs()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child(SharedString::from(label.to_string())),
                ),
        )
        .child(body)
}

fn render_editor_section(label: &str, hint: Option<&str>, body: AnyElement) -> impl IntoElement {
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

fn render_settings_icon_button(
    label: &str,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    div()
        .w(px(26.0))
        .h(px(26.0))
        .rounded_sm()
        .flex()
        .items_center()
        .justify_center()
        .bg(rgb(theme::PANEL_HEADER_BG))
        .border_1()
        .border_color(rgb(theme::BORDER_SECONDARY))
        .text_xs()
        .text_color(rgb(theme::TEXT_MUTED))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(theme::ROW_HOVER_BG)))
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

#[derive(Clone, Copy)]
enum EditorActionButtonStyle {
    Primary,
    Danger,
    Ghost,
}

fn render_editor_toolbar(
    title: &str,
    subtitle: &str,
    accent: u32,
    save_label: &str,
    on_save: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
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
                        .child(render_editor_action_button(
                            save_label,
                            EditorActionButtonStyle::Primary,
                            on_save,
                        ))
                        .children(on_delete.map(|on_delete| {
                            render_editor_action_button(
                                "Delete",
                                EditorActionButtonStyle::Danger,
                                on_delete,
                            )
                            .into_any_element()
                        }))
                        .child(render_editor_action_button(
                            "Close",
                            EditorActionButtonStyle::Ghost,
                            on_close,
                        )),
                ),
        )
}

fn render_editor_action_button(
    label: &str,
    style: EditorActionButtonStyle,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    let (bg, border, text, hover_bg) = match style {
        EditorActionButtonStyle::Primary => (
            theme::PRIMARY,
            theme::PRIMARY,
            theme::SELECTION_TEXT,
            theme::PRIMARY_HOVER,
        ),
        EditorActionButtonStyle::Danger => (
            theme::EDITOR_CARD_BG,
            0x5a2630,
            theme::DANGER_TEXT,
            0x382029,
        ),
        EditorActionButtonStyle::Ghost => (
            theme::EDITOR_CARD_BG,
            theme::BORDER_SECONDARY,
            theme::TEXT_MUTED,
            theme::ROW_HOVER_BG,
        ),
    };

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

fn render_editor_intro(panel: &EditorPanel) -> impl IntoElement {
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

fn render_project_panel(
    draft: &ProjectDraft,
    model: &EditorPaneModel,
    actions: &EditorActions<'_>,
) -> impl IntoElement {
    let on_toggle_pinned = (actions.on_action)(EditorAction::ToggleProjectPinned);
    let on_toggle_save_logs = (actions.on_action)(EditorAction::ToggleProjectSaveLogs);

    div()
        .flex()
        .flex_col()
        .gap(px(18.0))
        .child(render_editor_section(
            "Overview",
            Some("Name, location, and accent used to identify this project."),
            div()
                .flex()
                .flex_col()
                .gap(px(14.0))
                .child(render_text_field(
                    "Project name",
                    "What the sidebar shows for this workspace.",
                    draft.name.as_str(),
                    EditorField::Project(ProjectField::Name),
                    model,
                    actions,
                ))
                .child(render_text_field(
                    "Root path",
                    "Absolute path to the main repository or workspace root.",
                    draft.root_path.as_str(),
                    EditorField::Project(ProjectField::RootPath),
                    model,
                    actions,
                ))
                .child(render_text_field(
                    "Accent color",
                    "Hex color used for the project marker, for example #6366f1.",
                    draft.color.as_str(),
                    EditorField::Project(ProjectField::Color),
                    model,
                    actions,
                ))
                .into_any_element(),
        ))
        .child(render_editor_section(
            "Notes",
            Some("Useful context about the repo, owners, setup quirks, or onboarding steps."),
            div()
                .flex()
                .flex_col()
                .gap(px(14.0))
                .child(render_multiline_field(
                    "Project notes",
                    "Multi-line notes saved with the project.",
                    draft.notes.as_str(),
                    EditorField::Project(ProjectField::Notes),
                    model,
                    actions,
                ))
                .into_any_element(),
        ))
        .child(render_editor_section(
            "Workspace Defaults",
            Some("How this project should behave in the sidebar and on disk."),
            div()
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(render_toggle_row_with_hint(
                    "Save log files",
                    draft.save_log_files,
                    "Keep command output persisted to log files for this project.",
                    on_toggle_save_logs,
                ))
                .child(render_toggle_row_with_hint(
                    "Pinned",
                    draft.pinned,
                    "Keep this project near the top of the sidebar.",
                    on_toggle_pinned,
                ))
                .into_any_element(),
        ))
}

fn render_folder_panel(
    draft: &FolderDraft,
    model: &EditorPaneModel,
    actions: &EditorActions<'_>,
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

    div()
        .flex()
        .flex_col()
        .gap(px(18.0))
        .child(render_editor_section(
            "Overview",
            Some("Identity, location, and visibility inside this project."),
            div()
                .flex()
                .flex_col()
                .gap(px(14.0))
                .child(render_text_field(
                    "Folder name",
                    "Label shown for this folder in the workspace.",
                    draft.name.as_str(),
                    EditorField::Folder(FolderField::Name),
                    model,
                    actions,
                ))
                .child(render_text_field(
                    "Folder path",
                    "Absolute path to the folder on disk.",
                    draft.folder_path.as_str(),
                    EditorField::Folder(FolderField::FolderPath),
                    model,
                    actions,
                ))
                .child(render_toggle_row_with_hint(
                    "Hidden",
                    draft.hidden,
                    "Hide this folder in the sidebar without removing it from the project.",
                    on_toggle_hidden,
                ))
                .into_any_element(),
        ))
        .child(render_editor_section(
            "Tools",
            Some("Rescan the folder or jump directly into a terminal."),
            div()
                .flex()
                .flex_col()
                .gap(px(14.0))
                .child(render_choice_row(
                    "Folder picker",
                    if draft.folder_path.is_empty() {
                        "Choose directory"
                    } else {
                        draft.folder_path.as_str()
                    },
                    Some("Open the native folder picker and replace the current path."),
                    on_pick_folder,
                ))
                .child(render_choice_row(
                    "Rescan folder",
                    if draft.is_scanning {
                        "Scanning..."
                    } else {
                        "Refresh scripts and env ports"
                    },
                    Some("Read package manifests and .env files again."),
                    on_scan_folder,
                ))
                .child(render_choice_row(
                    "Open in terminal",
                    if draft.folder_path.is_empty() {
                        "Pick a folder first"
                    } else {
                        "Open external terminal"
                    },
                    Some("Launch this folder in your system terminal."),
                    on_open_terminal,
                ))
                .into_any_element(),
        ))
        .child(render_editor_section(
            "Detected Status",
            Some("Read-only information gathered from the local filesystem."),
            div()
                .flex()
                .flex_col()
                .gap(px(14.0))
                .children(draft.git_branch.as_ref().map(|branch| {
                    render_info_row(
                        "Git branch",
                        branch.as_str(),
                        Some("Read directly from .git/HEAD."),
                    )
                    .into_any_element()
                }))
                .children(draft.dependency_status.as_ref().map(|status| {
                    render_info_row(
                        "Dependencies",
                        status.status.as_str(),
                        Some(status.message.as_str()),
                    )
                    .into_any_element()
                }))
                .children(
                    scan_notice
                        .map(|message| render_notice_row(message.as_str()).into_any_element()),
                )
                .into_any_element(),
        ))
        .children(draft.scan_result.as_ref().map(|scan_result| {
            render_editor_section(
                "Scan Results",
                Some("Choose which discovered scripts to merge and which env port should be the default."),
                render_folder_scan_panel(draft, scan_result, actions).into_any_element(),
            )
            .into_any_element()
        }))
        .child(render_editor_section(
            "Environment",
            Some("Choose the env file, the default port variable, and inline env editing."),
            div()
                .flex()
                .flex_col()
                .gap(px(14.0))
                .child(render_text_field(
                    "Env file path",
                    "Relative path to the env file inside this folder, for example .env.local.",
                    draft.env_file_path.as_str(),
                    EditorField::Folder(FolderField::EnvFilePath),
                    model,
                    actions,
                ))
                .child(render_text_field(
                    "Default port variable",
                    "Env var used to derive the default server port for this folder.",
                    draft.port_variable.as_str(),
                    EditorField::Folder(FolderField::PortVariable),
                    model,
                    actions,
                ))
                .child(render_choice_row(
                    "Load env file",
                    if draft.env_file_loaded {
                        "Reload env contents"
                    } else {
                        "Load env contents"
                    },
                    Some("Read the configured env file so it can be edited inline."),
                    on_load_env,
                ))
                .children(
                    (draft.env_file_loaded || !draft.env_file_contents.is_empty()).then(|| {
                        render_multiline_field(
                            "Env file contents",
                            "Comments and blank lines are preserved when you save.",
                            draft.env_file_contents.as_str(),
                            EditorField::Folder(FolderField::EnvContents),
                            model,
                            actions,
                        )
                        .into_any_element()
                    }),
                )
                .into_any_element(),
        ))
}

fn render_command_panel(
    draft: &CommandDraft,
    model: &EditorPaneModel,
    actions: &EditorActions<'_>,
) -> impl IntoElement {
    let on_toggle_restart = (actions.on_action)(EditorAction::ToggleCommandAutoRestart);
    let on_toggle_clear_logs = (actions.on_action)(EditorAction::ToggleCommandClearLogs);

    div()
        .flex()
        .flex_col()
        .gap(px(18.0))
        .child(render_editor_section(
            "Overview",
            Some("What this command is called and what executable it runs."),
            div()
                .flex()
                .flex_col()
                .gap(px(14.0))
                .child(render_text_field(
                    "Command label",
                    "Name shown in the sidebar and command tabs.",
                    draft.label.as_str(),
                    EditorField::Command(CommandField::Label),
                    model,
                    actions,
                ))
                .child(render_text_field(
                    "Executable",
                    "Program or script that should be launched.",
                    draft.command.as_str(),
                    EditorField::Command(CommandField::Command),
                    model,
                    actions,
                ))
                .child(render_text_field(
                    "Arguments",
                    "Space-separated arguments passed to the executable.",
                    draft.args_text.as_str(),
                    EditorField::Command(CommandField::Args),
                    model,
                    actions,
                ))
                .into_any_element(),
        ))
        .child(render_editor_section(
            "Runtime",
            Some("Optional environment overrides and port metadata for this command."),
            div()
                .flex()
                .flex_col()
                .gap(px(14.0))
                .child(render_text_field(
                    "Environment",
                    "Semicolon-separated KEY=VALUE pairs.",
                    draft.env_text.as_str(),
                    EditorField::Command(CommandField::Env),
                    model,
                    actions,
                ))
                .child(render_text_field(
                    "Port",
                    "Numeric port exposed by this command, if known.",
                    draft.port_text.as_str(),
                    EditorField::Command(CommandField::Port),
                    model,
                    actions,
                ))
                .into_any_element(),
        ))
        .child(render_editor_section(
            "Behavior",
            Some("What should happen when this command restarts."),
            div()
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(render_toggle_row_with_hint(
                    "Auto restart",
                    draft.auto_restart,
                    "Restart the command automatically after it exits.",
                    on_toggle_restart,
                ))
                .child(render_toggle_row_with_hint(
                    "Clear logs on restart",
                    draft.clear_logs_on_restart,
                    "Clear previous output before the command starts again.",
                    on_toggle_clear_logs,
                ))
                .into_any_element(),
        ))
}

fn render_ssh_panel(
    draft: &SshDraft,
    model: &EditorPaneModel,
    actions: &EditorActions<'_>,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(render_text_field(
            "Label",
            "Display name for the SSH target",
            draft.label.as_str(),
            EditorField::Ssh(SshField::Label),
            model,
            actions,
        ))
        .child(render_text_field(
            "Host",
            "Hostname or IP address",
            draft.host.as_str(),
            EditorField::Ssh(SshField::Host),
            model,
            actions,
        ))
        .child(render_text_field(
            "Port",
            "Defaults to 22",
            draft.port_text.as_str(),
            EditorField::Ssh(SshField::Port),
            model,
            actions,
        ))
        .child(render_text_field(
            "Username",
            "Remote user name",
            draft.username.as_str(),
            EditorField::Ssh(SshField::Username),
            model,
            actions,
        ))
        .child(render_text_field(
            "Password",
            "Optional saved password",
            draft.password.as_str(),
            EditorField::Ssh(SshField::Password),
            model,
            actions,
        ))
}

fn render_text_field(
    label: &str,
    hint: &str,
    value: &str,
    field: EditorField,
    model: &EditorPaneModel,
    actions: &EditorActions<'_>,
) -> impl IntoElement {
    let focused = model.active_field == Some(field);
    let display_value = if focused {
        display_text_with_cursor(value, model.cursor)
    } else if value.is_empty() {
        "Not set".to_string()
    } else {
        value.to_string()
    };

    let on_focus = (actions.on_action)(EditorAction::FocusField(field));

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
                .on_mouse_down(MouseButton::Left, on_focus),
        )
}

fn render_multiline_field(
    label: &str,
    hint: &str,
    value: &str,
    field: EditorField,
    model: &EditorPaneModel,
    actions: &EditorActions<'_>,
) -> impl IntoElement {
    let focused = model.active_field == Some(field);
    let display_value = if focused {
        display_text_with_cursor(value, model.cursor)
    } else if value.is_empty() {
        "Not set".to_string()
    } else {
        value.to_string()
    };

    let on_focus = (actions.on_action)(EditorAction::FocusField(field));

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
                .h(px(140.0))
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
                .on_mouse_down(MouseButton::Left, on_focus),
        )
}

fn render_choice_row(
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

fn render_toggle_row_with_hint(
    label: &str,
    value: bool,
    hint: &str,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
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
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_SUBTLE))
                                .child(SharedString::from(hint.to_string())),
                        ),
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

fn minimize_to_tray_hint() -> &'static str {
    "Keep DevManager running when the window is closed"
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

fn render_notice_row(message: &str) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(10.0))
        .px(px(12.0))
        .py(px(10.0))
        .rounded_md()
        .bg(rgb(theme::EDITOR_NOTICE_BG))
        .border_1()
        .border_color(rgb(theme::BORDER_ACCENT))
        .child(div().size(px(8.0)).rounded_full().bg(rgb(theme::PRIMARY)))
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_MUTED))
                .child(SharedString::from(message.to_string())),
        )
}

fn render_folder_scan_panel(
    draft: &FolderDraft,
    scan_result: &ScanResult,
    actions: &EditorActions<'_>,
) -> impl IntoElement {
    let script_summary = format!(
        "{} discovered script(s), {} selected",
        scan_result.scripts.len(),
        draft.selected_scanned_scripts.len()
    );

    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(render_info_row(
            "Scan results",
            script_summary.as_str(),
            Some("Selected scripts will be created for new folders and merged into existing folders when they are not already present."),
        ))
        .children((!scan_result.scripts.is_empty()).then(|| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child("Scripts"),
                )
                .children(scan_result.scripts.iter().map(|script| {
                    let on_toggle_script =
                        (actions.on_action)(EditorAction::ToggleFolderScanScript(script.name.clone()));
                    render_selection_row(
                        script.name.clone(),
                        Some(script.command.clone()),
                        draft.selected_scanned_scripts.contains(&script.name),
                        on_toggle_script,
                    )
                    .into_any_element()
                }))
                .into_any_element()
        }))
        .children((!scan_result.ports.is_empty()).then(|| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_MUTED))
                        .child("Port variable"),
                )
                .child(render_selection_row(
                    "None".to_string(),
                    Some("Do not bind a default port variable".to_string()),
                    draft.selected_scanned_port_variable.is_none(),
                    (actions.on_action)(EditorAction::SelectFolderPortVariable(None)),
                ))
                .children(scan_result.ports.iter().map(|port| {
                    let on_select_port =
                        (actions.on_action)(EditorAction::SelectFolderPortVariable(Some(
                            port.variable.clone(),
                        )));
                    render_selection_row(
                        format!("{} = {}", port.variable, port.port),
                        Some(port.source.clone()),
                        draft.selected_scanned_port_variable.as_deref()
                            == Some(port.variable.as_str()),
                        on_select_port,
                    )
                    .into_any_element()
                }))
                .into_any_element()
        }))
}

fn render_selection_row(
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
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(if selected {
                            theme::PRIMARY
                        } else {
                            theme::TEXT_MUTED
                        }))
                        .child(if selected { "Selected" } else { "Available" }),
                ),
        )
        .on_mouse_down(MouseButton::Left, on_click)
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
                Some("This version comes from the signed latest.json manifest."),
            )
        }))
        .children(updater.endpoints.first().map(|endpoint| {
            render_info_row(
                "Manifest endpoint",
                endpoint.as_str(),
                Some("The updater checks this URL for a signed release manifest."),
            )
        }))
        .child(render_choice_row(
            "Check for updates",
            "Check now",
            Some("Queries the configured manifest URL in the background."),
            on_check,
        ))
        .children(on_download.map(|on_download| {
            render_choice_row(
                "Download update",
                "Download now",
                Some("Downloads and verifies the signed installer bundle."),
                on_download,
            )
        }))
        .children(on_install.map(|on_install| {
            render_choice_row(
                "Restart to update",
                "Install and close DevManager",
                Some("Launches the installer and closes the current app to finish the update."),
                on_install,
            )
        }))
        .children(updater.release_notes.as_ref().map(|notes| {
            render_info_row(
                "Release notes",
                notes.as_str(),
                Some("Release notes from the signed manifest / GitHub release."),
            )
        }))
}

fn render_info_row(label: &str, value: &str, hint: Option<&str>) -> impl IntoElement {
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
                .child(render_inline_state_badge("Detected", theme::TEXT_MUTED)),
        )
        .child(
            div()
                .px(px(12.0))
                .py(px(10.0))
                .rounded_md()
                .bg(rgb(theme::EDITOR_FIELD_BG))
                .border_1()
                .border_color(rgb(theme::BORDER_PRIMARY))
                .text_sm()
                .text_color(rgb(theme::TEXT_PRIMARY))
                .child(SharedString::from(value.to_string())),
        )
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

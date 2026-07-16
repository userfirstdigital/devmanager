use super::{
    validate_browser_url, BrowserBounds, BrowserCommand, BrowserDownloadState, BrowserError,
    BrowserHostEvent, BrowserPageLoadState, BrowserResponse, BrowserRevision, BrowserTabSnapshot,
    BrowserViewport, BrowserWorkspaceKey, BrowserWorkspaceSnapshot,
};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use crate::theme;
use gpui::{
    canvas, div, prelude::*, px, rgb, App, Bounds, FocusHandle, IntoElement, KeyDownEvent,
    MouseButton, MouseDownEvent, ParentElement, Pixels, SharedString, Styled, Window,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserPaneSurface {
    Server,
    Claude,
    Codex,
    Ssh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserPaneContext {
    pub browser_enabled: bool,
    pub platform_supported: bool,
    pub active_surface: Option<BrowserPaneSurface>,
    pub editor_open: bool,
    pub modal_open: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrowserSplitLayout {
    pub total_width: f32,
    pub terminal_width: f32,
    pub divider_width: f32,
    pub pane_width: f32,
    pub split_percent: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserViewportPreset {
    Desktop,
    Tablet,
    Mobile,
}

impl BrowserViewportPreset {
    pub fn viewport(self) -> BrowserViewport {
        match self {
            Self::Desktop => BrowserViewport {
                width: 1280,
                height: 720,
                scale_percent: 100,
            },
            Self::Tablet => BrowserViewport {
                width: 768,
                height: 1024,
                scale_percent: 100,
            },
            Self::Mobile => BrowserViewport {
                width: 390,
                height: 844,
                scale_percent: 100,
            },
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Desktop => "Desktop 1280x720",
            Self::Tablet => "Tablet 768x1024",
            Self::Mobile => "Mobile 390x844",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum BrowserPaneAction {
    Open,
    Collapse,
    DividerBegin { pointer_x: f32 },
    DividerUpdate { pointer_x: f32 },
    DividerEnd,
    CreateTab,
    SelectTab(String),
    CloseTab(String),
    Back,
    Forward,
    Reload,
    FocusAddress,
    EditAddress(String),
    SubmitAddress,
    SetViewport(BrowserViewportPreset),
    ToggleAnnotation,
    ToggleRecording,
    OpenDevTools,
    OpenDownloads,
    Stop,
    ResetWorkspace,
    ClearProjectProfile,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BrowserPaneTransient {
    pub address_draft: Option<String>,
    pub address_cursor: usize,
    pub address_focused: bool,
    pub loading: bool,
    pub diagnostic: Option<String>,
    pub action_status: Option<String>,
    pub divider_dragging: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserPaneModel {
    pub workspace_key: BrowserWorkspaceKey,
    pub eligible: bool,
    pub pane_open: bool,
    pub split_percent: u8,
    pub tabs: Vec<BrowserTabSnapshot>,
    pub selected_tab_id: Option<String>,
    pub address_draft: String,
    pub address_cursor: usize,
    pub address_focused: bool,
    pub loading: bool,
    pub diagnostic: Option<String>,
    pub action_status: Option<String>,
    pub divider_dragging: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserHostVisibility {
    Hidden,
    Selected {
        workspace_key: BrowserWorkspaceKey,
        tab_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserHostReconcilePlan {
    pub visibility: BrowserHostVisibility,
    pub ensure_snapshot: Option<BrowserWorkspaceSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserActionPlan {
    pub workspace_key: BrowserWorkspaceKey,
    pub commands: Vec<BrowserCommand>,
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserSnapshotSync {
    pub workspace_key: BrowserWorkspaceKey,
    pub revision: BrowserRevision,
    pub snapshot: BrowserWorkspaceSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserPaneEventPlan {
    SyncSnapshot {
        workspace_key: BrowserWorkspaceKey,
        tab_id: String,
        interrupt_agent: bool,
        loading: Option<bool>,
    },
    OpenLogicalTab {
        workspace_key: BrowserWorkspaceKey,
        url: String,
    },
    DownloadStatus {
        workspace_key: BrowserWorkspaceKey,
        message: String,
    },
    Diagnostic {
        workspace_key: BrowserWorkspaceKey,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserSettingsAction {
    ClearActiveProjectProfile,
    ResetActiveConversation,
    RevealActiveDownloads,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserSettingsPlan {
    pub route_key: BrowserWorkspaceKey,
    pub command: BrowserCommand,
    pub reset_workspaces: Vec<BrowserWorkspaceKey>,
    pub preserve_downloads: bool,
    pub preserve_resources: bool,
}

impl BrowserPaneModel {
    pub fn new(
        workspace_key: BrowserWorkspaceKey,
        context: &BrowserPaneContext,
        snapshot: &BrowserWorkspaceSnapshot,
        transient: BrowserPaneTransient,
    ) -> Self {
        let selected_tab_id = selected_browser_tab_id(snapshot).map(ToOwned::to_owned);
        let selected_url = selected_tab_id
            .as_deref()
            .and_then(|selected| snapshot.tabs.iter().find(|tab| tab.id == selected))
            .map(|tab| tab.url.clone())
            .unwrap_or_default();
        let has_address_draft = transient.address_draft.is_some();
        let address_draft = transient.address_draft.unwrap_or(selected_url);
        let address_cursor = if has_address_draft {
            transient.address_cursor.min(address_draft.chars().count())
        } else {
            address_draft.chars().count()
        };
        Self {
            workspace_key,
            eligible: browser_pane_eligible(context),
            pane_open: snapshot.pane_open,
            split_percent: snapshot.split_percent.clamp(25, 75),
            tabs: snapshot.tabs.clone(),
            selected_tab_id,
            address_draft,
            address_cursor,
            address_focused: transient.address_focused,
            loading: transient.loading,
            diagnostic: transient.diagnostic,
            action_status: transient.action_status,
            divider_dragging: transient.divider_dragging,
        }
    }
}

pub fn selected_browser_tab_id(snapshot: &BrowserWorkspaceSnapshot) -> Option<&str> {
    snapshot
        .selected_tab_id
        .as_deref()
        .filter(|selected| snapshot.tabs.iter().any(|tab| tab.id == *selected))
        .or_else(|| snapshot.tabs.first().map(|tab| tab.id.as_str()))
}

pub fn browser_host_visibility(
    context: &BrowserPaneContext,
    workspace_key: &BrowserWorkspaceKey,
    snapshot: &BrowserWorkspaceSnapshot,
    divider_dragging: bool,
) -> BrowserHostVisibility {
    if !browser_pane_eligible(context) || !snapshot.pane_open || divider_dragging {
        return BrowserHostVisibility::Hidden;
    }
    selected_browser_tab_id(snapshot).map_or(BrowserHostVisibility::Hidden, |tab_id| {
        BrowserHostVisibility::Selected {
            workspace_key: workspace_key.clone(),
            tab_id: tab_id.to_string(),
        }
    })
}

pub fn browser_host_reconcile_plan(
    context: &BrowserPaneContext,
    workspace_key: &BrowserWorkspaceKey,
    persisted_snapshot: &BrowserWorkspaceSnapshot,
    divider_dragging: bool,
    live_host_snapshot: Option<&BrowserWorkspaceSnapshot>,
) -> BrowserHostReconcilePlan {
    let visibility =
        browser_host_visibility(context, workspace_key, persisted_snapshot, divider_dragging);
    let ensure_snapshot = match (&visibility, live_host_snapshot) {
        (BrowserHostVisibility::Selected { .. }, None) => Some(persisted_snapshot.clone()),
        _ => None,
    };
    BrowserHostReconcilePlan {
        visibility,
        ensure_snapshot,
    }
}

pub fn browser_action_plan(
    active_workspace: Option<&BrowserWorkspaceKey>,
    snapshot: Option<&BrowserWorkspaceSnapshot>,
    address_draft: &str,
    action: BrowserPaneAction,
) -> Result<BrowserActionPlan, BrowserError> {
    let workspace_key =
        active_workspace
            .cloned()
            .ok_or_else(|| BrowserError::InvalidWorkspaceKey {
                field: "activeWorkspace".to_string(),
            })?;
    let mut diagnostic = None;
    let commands = match action {
        BrowserPaneAction::Open => {
            let snapshot = snapshot.cloned().unwrap_or_default();
            vec![
                BrowserCommand::Ensure { snapshot },
                BrowserCommand::SetPaneOpen { open: true },
            ]
        }
        BrowserPaneAction::Collapse => vec![BrowserCommand::SetPaneOpen { open: false }],
        BrowserPaneAction::CreateTab => vec![BrowserCommand::CreateTab { url: None }],
        BrowserPaneAction::SelectTab(tab_id) => vec![BrowserCommand::SelectTab { tab_id }],
        BrowserPaneAction::CloseTab(tab_id) => vec![BrowserCommand::CloseTab { tab_id }],
        BrowserPaneAction::Back => vec![BrowserCommand::Back {
            tab_id: selected_tab(snapshot)?.to_string(),
        }],
        BrowserPaneAction::Forward => vec![BrowserCommand::Forward {
            tab_id: selected_tab(snapshot)?.to_string(),
        }],
        BrowserPaneAction::Reload => vec![BrowserCommand::Reload {
            tab_id: selected_tab(snapshot)?.to_string(),
        }],
        BrowserPaneAction::SubmitAddress => vec![BrowserCommand::Navigate {
            tab_id: selected_tab(snapshot)?.to_string(),
            url: normalize_browser_address(address_draft)?,
        }],
        BrowserPaneAction::SetViewport(preset) => vec![BrowserCommand::UpdateViewport {
            tab_id: selected_tab(snapshot)?.to_string(),
            viewport: preset.viewport(),
        }],
        BrowserPaneAction::OpenDevTools => vec![BrowserCommand::OpenDevTools {
            tab_id: selected_tab(snapshot)?.to_string(),
        }],
        BrowserPaneAction::OpenDownloads => vec![BrowserCommand::DownloadDirectory],
        BrowserPaneAction::Stop => vec![BrowserCommand::Stop {
            tab_id: snapshot
                .and_then(selected_browser_tab_id)
                .map(ToOwned::to_owned),
        }],
        BrowserPaneAction::ResetWorkspace => vec![BrowserCommand::ResetWorkspace],
        BrowserPaneAction::ClearProjectProfile => vec![BrowserCommand::ClearProjectProfile],
        BrowserPaneAction::ToggleAnnotation | BrowserPaneAction::ToggleRecording => {
            diagnostic = Some("Not available until browser automation is initialized".to_string());
            Vec::new()
        }
        BrowserPaneAction::DividerBegin { .. }
        | BrowserPaneAction::DividerUpdate { .. }
        | BrowserPaneAction::DividerEnd
        | BrowserPaneAction::FocusAddress
        | BrowserPaneAction::EditAddress(_) => Vec::new(),
    };

    Ok(BrowserActionPlan {
        workspace_key,
        commands,
        diagnostic,
    })
}

pub fn browser_pane_open_fallback(action: &BrowserPaneAction) -> Option<bool> {
    match action {
        BrowserPaneAction::Open => Some(true),
        BrowserPaneAction::Collapse => Some(false),
        _ => None,
    }
}

pub fn browser_response_sync(
    open_workspaces: &[BrowserWorkspaceKey],
    route: &BrowserWorkspaceKey,
    response: &BrowserResponse,
) -> Option<BrowserSnapshotSync> {
    if !open_workspaces.iter().any(|open| open == route) {
        return None;
    }
    match response {
        BrowserResponse::Workspace { mutation } => Some(BrowserSnapshotSync {
            workspace_key: route.clone(),
            revision: mutation.revision,
            snapshot: mutation.snapshot.clone(),
        }),
        BrowserResponse::WorkspaceState { snapshot } => Some(BrowserSnapshotSync {
            workspace_key: route.clone(),
            revision: snapshot.revision,
            snapshot: snapshot.clone(),
        }),
        BrowserResponse::Status { .. }
        | BrowserResponse::Tabs { .. }
        | BrowserResponse::DownloadDirectory { .. }
        | BrowserResponse::Acknowledged => None,
    }
}

pub fn browser_event_plan(
    open_workspaces: &[BrowserWorkspaceKey],
    event: &BrowserHostEvent,
) -> Option<BrowserPaneEventPlan> {
    let workspace_key = match event {
        BrowserHostEvent::UrlChanged { workspace_key, .. }
        | BrowserHostEvent::TitleChanged { workspace_key, .. }
        | BrowserHostEvent::PageLoad { workspace_key, .. }
        | BrowserHostEvent::UserInput { workspace_key, .. }
        | BrowserHostEvent::NewWindow { workspace_key, .. }
        | BrowserHostEvent::Download { workspace_key, .. }
        | BrowserHostEvent::Diagnostic { workspace_key, .. } => workspace_key,
    };
    if !open_workspaces.iter().any(|open| open == workspace_key) {
        return None;
    }

    match event {
        BrowserHostEvent::UrlChanged { tab_id, .. }
        | BrowserHostEvent::TitleChanged { tab_id, .. } => {
            Some(BrowserPaneEventPlan::SyncSnapshot {
                workspace_key: workspace_key.clone(),
                tab_id: tab_id.clone(),
                interrupt_agent: false,
                loading: None,
            })
        }
        BrowserHostEvent::PageLoad { tab_id, state, .. } => {
            Some(BrowserPaneEventPlan::SyncSnapshot {
                workspace_key: workspace_key.clone(),
                tab_id: tab_id.clone(),
                interrupt_agent: false,
                loading: Some(matches!(state, BrowserPageLoadState::Started)),
            })
        }
        BrowserHostEvent::UserInput { tab_id, .. } => Some(BrowserPaneEventPlan::SyncSnapshot {
            workspace_key: workspace_key.clone(),
            tab_id: tab_id.clone(),
            interrupt_agent: true,
            loading: None,
        }),
        BrowserHostEvent::NewWindow { url, .. } => Some(BrowserPaneEventPlan::OpenLogicalTab {
            workspace_key: workspace_key.clone(),
            url: url.clone(),
        }),
        BrowserHostEvent::Download { state, path, .. } => {
            let file = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("download");
            let message = match state {
                BrowserDownloadState::Started => format!("Downloading {file}"),
                BrowserDownloadState::Completed { successful: true } => {
                    format!("Downloaded {file}")
                }
                BrowserDownloadState::Completed { successful: false } => {
                    format!("Download failed: {file}")
                }
            };
            Some(BrowserPaneEventPlan::DownloadStatus {
                workspace_key: workspace_key.clone(),
                message,
            })
        }
        BrowserHostEvent::Diagnostic { message, .. } => Some(BrowserPaneEventPlan::Diagnostic {
            workspace_key: workspace_key.clone(),
            message: message.clone(),
        }),
    }
}

pub fn browser_settings_plan(
    action: BrowserSettingsAction,
    active_workspace: Option<&BrowserWorkspaceKey>,
    open_workspaces: &[BrowserWorkspaceKey],
) -> Result<BrowserSettingsPlan, BrowserError> {
    let route_key = active_workspace
        .cloned()
        .ok_or_else(|| BrowserError::InvalidWorkspaceKey {
            field: "activeWorkspace".to_string(),
        })?;
    let (command, reset_workspaces) = match action {
        BrowserSettingsAction::ClearActiveProjectProfile => (
            BrowserCommand::ClearProjectProfile,
            open_workspaces
                .iter()
                .filter(|key| key.project_id == route_key.project_id)
                .cloned()
                .collect(),
        ),
        BrowserSettingsAction::ResetActiveConversation => {
            (BrowserCommand::ResetWorkspace, vec![route_key.clone()])
        }
        BrowserSettingsAction::RevealActiveDownloads => {
            (BrowserCommand::DownloadDirectory, Vec::new())
        }
    };
    Ok(BrowserSettingsPlan {
        route_key,
        command,
        reset_workspaces,
        preserve_downloads: true,
        preserve_resources: true,
    })
}

fn selected_tab(snapshot: Option<&BrowserWorkspaceSnapshot>) -> Result<&str, BrowserError> {
    snapshot
        .and_then(selected_browser_tab_id)
        .ok_or_else(|| BrowserError::CrashedView {
            message: "browser workspace has no selected tab".to_string(),
        })
}

pub fn browser_pane_eligible(context: &BrowserPaneContext) -> bool {
    context.browser_enabled
        && context.platform_supported
        && !context.editor_open
        && !context.modal_open
        && matches!(
            context.active_surface,
            Some(BrowserPaneSurface::Claude | BrowserPaneSurface::Codex)
        )
}

pub fn calculate_browser_split(
    total_width: f32,
    split_percent: u8,
    terminal_min_width: f32,
    pane_min_width: f32,
    divider_width: f32,
) -> BrowserSplitLayout {
    let total_width = total_width.max(0.0);
    let divider_width = divider_width.max(0.0).min(total_width);
    let available_width = total_width - divider_width;
    let split_percent = split_percent.clamp(25, 75);
    let desired_pane_width = available_width * f32::from(split_percent) / 100.0;
    let terminal_min_width = terminal_min_width.max(0.0);
    let pane_min_width = pane_min_width.max(0.0);
    let pane_width = if available_width >= terminal_min_width + pane_min_width {
        desired_pane_width.clamp(pane_min_width, available_width - terminal_min_width)
    } else {
        desired_pane_width.clamp(0.0, available_width)
    };
    let terminal_width = available_width - pane_width;

    BrowserSplitLayout {
        total_width,
        terminal_width,
        divider_width,
        pane_width,
        split_percent,
    }
}

pub fn browser_content_bounds(
    pane_bounds: BrowserBounds,
    toolbar_height: i32,
) -> Option<BrowserBounds> {
    let toolbar_height = toolbar_height.max(0);
    let height = pane_bounds.height.checked_sub(toolbar_height)?;
    if pane_bounds.width <= 0 || height <= 0 {
        return None;
    }
    Some(BrowserBounds {
        x: pane_bounds.x,
        y: pane_bounds.y.saturating_add(toolbar_height),
        width: pane_bounds.width,
        height,
    })
}

pub fn normalize_browser_address(input: &str) -> Result<String, BrowserError> {
    let address = input.trim();
    let failure = |message: &str| BrowserError::NavigationFailure {
        url: address.to_string(),
        message: message.to_string(),
    };
    if address.is_empty() {
        return Err(failure("address cannot be blank"));
    }
    if address.eq_ignore_ascii_case("about:blank") {
        return Ok("about:blank".to_string());
    }
    if address.contains(char::is_whitespace) || address.contains('\\') {
        return Err(failure("address must contain a host, not free text"));
    }
    if address.contains("://") {
        return validate_browser_url(address);
    }
    if address.contains('@') {
        return Err(failure("address user information is not supported"));
    }

    let authority = address.split(['/', '?', '#']).next().unwrap_or_default();
    let (host, explicit_ipv6) = split_host(authority).ok_or_else(|| failure("invalid host"))?;
    let is_loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<Ipv4Addr>()
            .is_ok_and(|address| address.is_loopback())
        || host
            .parse::<Ipv6Addr>()
            .is_ok_and(|address| address.is_loopback());
    let is_local = host.to_ascii_lowercase().ends_with(".local");
    let host_like = explicit_ipv6
        || host.parse::<Ipv4Addr>().is_ok()
        || host.split('.').all(|label| is_valid_hostname_label(label));
    if !host_like {
        return Err(failure("address must contain a valid host"));
    }

    let scheme = if is_loopback || is_local {
        "http"
    } else {
        "https"
    };
    let normalized_address = if explicit_ipv6 && !authority.starts_with('[') {
        format!("[{authority}]{}", &address[authority.len()..])
    } else {
        address.to_string()
    };
    validate_browser_url(&format!("{scheme}://{normalized_address}"))
}

fn split_host(authority: &str) -> Option<(&str, bool)> {
    if authority.starts_with('[') {
        let close = authority.find(']')?;
        let host = &authority[1..close];
        let suffix = &authority[close + 1..];
        if suffix.is_empty()
            || suffix
                .strip_prefix(':')
                .is_some_and(|port| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()))
        {
            return host.parse::<Ipv6Addr>().ok().map(|_| (host, true));
        }
        return None;
    }

    if authority.parse::<Ipv6Addr>().is_ok() {
        return Some((authority, true));
    }
    let (host, port) = authority.rsplit_once(':').unwrap_or((authority, ""));
    if authority.contains(':') && (port.is_empty() || !port.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    if host.is_empty() || (authority.contains(':') && port.is_empty()) {
        return None;
    }
    Some((host, false))
}

fn is_valid_hostname_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
}

pub struct BrowserPaneActions {
    pub on_action:
        Arc<dyn Fn(BrowserPaneAction) -> Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>>,
    pub on_address_key: Box<dyn Fn(&KeyDownEvent, &mut Window, &mut App)>,
    pub on_page_bounds: Arc<dyn Fn(Bounds<Pixels>, &mut Window, &mut App)>,
}

pub fn render_browser_pane(
    model: BrowserPaneModel,
    address_focus: FocusHandle,
    actions: BrowserPaneActions,
) -> impl IntoElement {
    let action = actions.on_action.clone();
    let selected_viewport = model
        .selected_tab_id
        .as_deref()
        .and_then(|selected| model.tabs.iter().find(|tab| tab.id == selected))
        .map(|tab| tab.viewport.clone());
    let tab_strip = model.tabs.iter().map(|tab| {
        let selected = model.selected_tab_id.as_deref() == Some(tab.id.as_str());
        let select = action(BrowserPaneAction::SelectTab(tab.id.clone()));
        let close = action(BrowserPaneAction::CloseTab(tab.id.clone()));
        div()
            .flex()
            .items_center()
            .min_w(px(0.0))
            .max_w(px(180.0))
            .border_r_1()
            .border_color(rgb(theme::BORDER_PRIMARY))
            .bg(rgb(if selected {
                theme::TAB_ACTIVE_BG
            } else {
                theme::TOPBAR_BG
            }))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .px(px(6.0))
                    .py(px(3.0))
                    .text_xs()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_color(rgb(if selected {
                        theme::TEXT_PRIMARY
                    } else {
                        theme::TEXT_MUTED
                    }))
                    .on_mouse_down(MouseButton::Left, select)
                    .child(SharedString::from(browser_tab_label(tab))),
            )
            .child(
                div()
                    .px(px(4.0))
                    .py(px(3.0))
                    .text_xs()
                    .text_color(rgb(theme::TEXT_DIM))
                    .hover(|style| style.bg(rgb(theme::DANGER_BG_SUBTLE)))
                    .on_mouse_down(MouseButton::Left, close)
                    .child("x"),
            )
            .into_any_element()
    });
    let status = model
        .diagnostic
        .clone()
        .or_else(|| model.action_status.clone())
        .or_else(|| model.loading.then(|| "Loading...".to_string()));
    let address_text = if model.address_draft.is_empty() {
        "Enter an address".to_string()
    } else if model.address_focused {
        let cursor_byte = model
            .address_draft
            .char_indices()
            .nth(model.address_cursor)
            .map(|(index, _)| index)
            .unwrap_or(model.address_draft.len());
        format!(
            "{}|{}",
            &model.address_draft[..cursor_byte],
            &model.address_draft[cursor_byte..]
        )
    } else {
        model.address_draft.clone()
    };
    let page_bounds = actions.on_page_bounds.clone();

    div()
        .h_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .bg(rgb(theme::PANEL_BG))
        .border_l_1()
        .border_color(rgb(theme::BORDER_PRIMARY))
        .child(
            div()
                .h(px(26.0))
                .flex_none()
                .flex()
                .items_center()
                .overflow_hidden()
                .bg(rgb(theme::TOPBAR_BG))
                .border_b_1()
                .border_color(rgb(theme::BORDER_PRIMARY))
                .children(tab_strip)
                .child(browser_button(
                    "+",
                    false,
                    false,
                    action(BrowserPaneAction::CreateTab),
                ))
                .child(browser_button(
                    "collapse",
                    false,
                    false,
                    action(BrowserPaneAction::Collapse),
                )),
        )
        .child(
            div()
                .h(px(30.0))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(3.0))
                .px(px(4.0))
                .bg(rgb(theme::PANEL_HEADER_BG))
                .border_b_1()
                .border_color(rgb(theme::BORDER_PRIMARY))
                .child(browser_button(
                    "back",
                    false,
                    false,
                    action(BrowserPaneAction::Back),
                ))
                .child(browser_button(
                    "forward",
                    false,
                    false,
                    action(BrowserPaneAction::Forward),
                ))
                .child(browser_button(
                    "reload",
                    false,
                    false,
                    action(BrowserPaneAction::Reload),
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(60.0))
                        .h(px(22.0))
                        .flex()
                        .items_center()
                        .px(px(6.0))
                        .border_1()
                        .border_color(rgb(if model.address_focused {
                            theme::PRIMARY
                        } else {
                            theme::BORDER_PRIMARY
                        }))
                        .bg(rgb(theme::APP_BG))
                        .text_xs()
                        .text_color(rgb(if model.address_draft.is_empty() {
                            theme::TEXT_DIM
                        } else {
                            theme::TEXT_PRIMARY
                        }))
                        .track_focus(&address_focus)
                        .on_mouse_down(MouseButton::Left, action(BrowserPaneAction::FocusAddress))
                        .on_key_down(actions.on_address_key)
                        .child(SharedString::from(address_text)),
                )
                .child(browser_button(
                    "go",
                    false,
                    false,
                    action(BrowserPaneAction::SubmitAddress),
                )),
        )
        .child(
            div()
                .h(px(28.0))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(3.0))
                .px(px(4.0))
                .overflow_hidden()
                .bg(rgb(theme::PANEL_HEADER_BG))
                .border_b_1()
                .border_color(rgb(theme::BORDER_PRIMARY))
                .children(
                    [
                        BrowserViewportPreset::Desktop,
                        BrowserViewportPreset::Tablet,
                        BrowserViewportPreset::Mobile,
                    ]
                    .into_iter()
                    .map(|preset| {
                        browser_button(
                            match preset {
                                BrowserViewportPreset::Desktop => "desktop",
                                BrowserViewportPreset::Tablet => "tablet",
                                BrowserViewportPreset::Mobile => "mobile",
                            },
                            selected_viewport.as_ref() == Some(&preset.viewport()),
                            false,
                            action(BrowserPaneAction::SetViewport(preset)),
                        )
                        .into_any_element()
                    }),
                )
                .child(browser_button(
                    "annotate",
                    false,
                    false,
                    action(BrowserPaneAction::ToggleAnnotation),
                ))
                .child(browser_button(
                    "record",
                    false,
                    false,
                    action(BrowserPaneAction::ToggleRecording),
                ))
                .child(browser_button(
                    "devtools",
                    false,
                    false,
                    action(BrowserPaneAction::OpenDevTools),
                ))
                .child(browser_button(
                    "downloads",
                    false,
                    false,
                    action(BrowserPaneAction::OpenDownloads),
                ))
                .child(browser_button(
                    "Stop",
                    false,
                    true,
                    action(BrowserPaneAction::Stop),
                )),
        )
        .child(
            div()
                .h(px(22.0))
                .flex_none()
                .flex()
                .items_center()
                .px(px(6.0))
                .bg(rgb(theme::TOPBAR_BG))
                .border_b_1()
                .border_color(rgb(theme::BORDER_PRIMARY))
                .text_xs()
                .text_color(rgb(if model.diagnostic.is_some() {
                    theme::DANGER_TEXT
                } else if model.loading {
                    theme::WARNING_TEXT
                } else {
                    theme::TEXT_DIM
                }))
                .child(SharedString::from(status.unwrap_or_else(|| {
                    selected_viewport
                        .map(viewport_label)
                        .unwrap_or_else(|| "Browser ready".to_string())
                }))),
        )
        .child(
            div()
                .flex_1()
                .min_h(px(0.0))
                .relative()
                .bg(rgb(theme::TERMINAL_BG))
                .child(
                    canvas(
                        move |bounds, window, cx| {
                            (page_bounds)(bounds, window, cx);
                        },
                        |_, _, _, _| {},
                    )
                    .size_full(),
                ),
        )
}

fn browser_button(
    label: impl Into<SharedString>,
    active: bool,
    danger: bool,
    on_click: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>,
) -> impl IntoElement {
    div()
        .flex_none()
        .px(px(5.0))
        .py(px(3.0))
        .rounded(px(2.0))
        .bg(rgb(if active {
            theme::PRIMARY_MUTED
        } else {
            theme::TOPBAR_BG
        }))
        .hover(|style| style.bg(rgb(theme::BUTTON_HOVER_BG)))
        .text_xs()
        .text_color(rgb(if danger {
            theme::DANGER_TEXT
        } else if active {
            theme::TEXT_PRIMARY
        } else {
            theme::TEXT_MUTED
        }))
        .on_mouse_down(MouseButton::Left, on_click)
        .child(label.into())
}

fn browser_tab_label(tab: &BrowserTabSnapshot) -> String {
    if !tab.title.trim().is_empty() {
        return tab.title.trim().to_string();
    }
    if tab.url.eq_ignore_ascii_case("about:blank") {
        return "New tab".to_string();
    }
    tab.url
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(rest))
        .filter(|host| !host.is_empty())
        .unwrap_or(tab.url.as_str())
        .to_string()
}

fn viewport_label(viewport: BrowserViewport) -> String {
    format!("Viewport {}x{}", viewport.width, viewport.height)
}

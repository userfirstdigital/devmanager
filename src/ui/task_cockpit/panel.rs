//! Shared bounded presentation contracts for Task Cockpit panels.
//!
//! Panels are deliberately projections.  They carry the exact task identity,
//! the task revision observed by the caller, and an existing catalog
//! [`ActionRequest`].  They never create a second command vocabulary or read
//! workspace/configuration state themselves.

use gpui::{div, px, rgb, AnyElement, InteractiveElement, IntoElement, ParentElement, Styled};

use crate::client::action::ActionRequest;
use crate::domain::id::TaskId;
use crate::ui::tokens::ThemeTokens;
use sha2::{Digest, Sha256};

/// Hard cap for all panel rows.  The host already bounds the source payload;
/// keeping the UI cap here prevents a stale or malicious projection from
/// turning one render into an unbounded allocation.
pub const MAX_PANEL_ROWS: usize = 64;
pub const MAX_PANEL_LABEL_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelDisabledReason {
    NoTaskSelected,
    HostProjectionMissing,
    ProjectionLoading,
    SecretPath,
    Directory,
    NotReviewable,
    Unsupported,
    RepositoryUnavailable,
    RepositoryReadOnly,
}

impl PanelDisabledReason {
    pub const fn label(self) -> &'static str {
        match self {
            Self::NoTaskSelected => "Select a task first",
            Self::HostProjectionMissing => "Host projection unavailable",
            Self::ProjectionLoading => "Waiting for host projection",
            Self::SecretPath => "Secret paths are not readable",
            Self::Directory => "Directories are opened from the file list",
            Self::NotReviewable => "This task is not ready for review",
            Self::Unsupported => "This action is not available yet",
            Self::RepositoryUnavailable => "Repository is unavailable",
            Self::RepositoryReadOnly => "Repository is read-only",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PanelIdentity {
    pub task_id: TaskId,
    /// Revision is an optional UI fence.  Host cockpit queries currently
    /// carry the task id only; callers with a durable snapshot should provide
    /// the observed revision so stale row actions can be rejected upstream.
    pub revision: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelAction {
    pub identity: PanelIdentity,
    pub action_id: &'static str,
    pub request: ActionRequest,
    pub disabled_reason: Option<PanelDisabledReason>,
}

impl PanelAction {
    /// Stable GPUI key scoped to the task, revision, catalog action, and row
    /// target.  The target is hashed so secret/path text never appears in the
    /// element identity or diagnostics.
    pub fn element_key(&self, target: &str) -> u64 {
        let mut hasher = Sha256::new();
        hasher.update(self.identity.task_id.as_bytes());
        hasher.update([u8::from(self.identity.revision.is_some())]);
        hasher.update(self.identity.revision.unwrap_or_default().to_le_bytes());
        hasher.update(self.action_id.as_bytes());
        hasher.update(target.as_bytes());
        u64::from_le_bytes(hasher.finalize()[..8].try_into().expect("digest prefix"))
    }
}

impl PanelAction {
    pub fn enabled(identity: PanelIdentity, request: ActionRequest) -> Self {
        let action_id = request.id();
        Self {
            identity,
            action_id,
            request,
            disabled_reason: None,
        }
    }

    pub fn disabled(
        identity: PanelIdentity,
        request: ActionRequest,
        reason: PanelDisabledReason,
    ) -> Self {
        let action_id = request.id();
        Self {
            identity,
            action_id,
            request,
            disabled_reason: Some(reason),
        }
    }

    pub const fn is_enabled(&self) -> bool {
        self.disabled_reason.is_none()
    }
}

pub fn task_identity(task_id: TaskId, revision: Option<u64>) -> PanelIdentity {
    PanelIdentity { task_id, revision }
}

// ---------------------------------------------------------------------------
// The panel body's visual language.
//
// A dock body starts under the panel's tab row and has no chrome of its own:
// the panel frame in `src/ui/panel/render.rs` already paints the title, the
// status and the tabs, so a body that repeats them says the same thing twice.
// The numbers below are the redesign's rules 2/4/5/6/9/10 written once, so a
// panel is restyled by reading them rather than by a painter's memory. They
// are measured against the body of the chosen arrangement in
// `docs/superpowers/specs/2026-09-03-ui-redesign-mockups/02-panel-chrome-2.html`.
// ---------------------------------------------------------------------------

/// Rule 5: list rows are full width with no side margin, so the hover and
/// selection fills reach the panel's edge and the row reads as a band rather
/// than a card. 10 px of horizontal padding matches the `.hdr`/`.tabs` rows
/// above it, so a row's text is on the same left edge as the panel's title.
pub const ROW_PADDING_X: f32 = 10.0;
/// Rule 5: 5 px above and below, which is what makes a two-line row 40 px and
/// a one-line row 22 px at the 11.5/10.5 type scale.
pub const ROW_PADDING_Y: f32 = 5.0;
/// Rule 2: 11.5 px is the body/list size -- the row's own content.
pub const ROW_FONT_SIZE: f32 = 11.5;
/// Rule 2: 10.5 px is the caption size -- a row's metadata line, and the only
/// size at which colour is ever spent on a count.
pub const META_FONT_SIZE: f32 = 10.5;
// Rule 2 also asks for .04-.06em of letter-spacing on a group label. GPUI
// 0.2.2's `Styled` has no letter-spacing or tracking setter -- nothing in this
// application sets one -- so the label is uppercase at the caption size and
// the tracking is the one part of rule 2 that cannot be honoured here. Lane
// 2c's `card_label` in `src/ui/panel/permission.rs` reaches the same place by
// the same route; if a tracking setter ever lands, both change together.
/// Rule 10: a lucide glyph inside a list row is 14 px and `text.muted`. The
/// glyph is never coloured by what it stands for -- a `.rs` file and a `.png`
/// file get the same grey, because the colour budget belongs to status.
pub const ROW_ICON_SIZE: f32 = 14.0;
/// Rule 6: the gap between a row's glyph and its text, and between controls.
pub const ROW_GAP: f32 = 8.0;
/// Rule 6: region padding, used where a body needs a margin of its own rather
/// than a full-width row (an empty state, a group's outer box).
pub const REGION_PADDING: f32 = 10.0;

/// Rule 4: a default button is 11 px on a 1 px `borders.default`, no fill.
const ACTION_FONT_SIZE: f32 = 11.0;
/// Rule 4: padding 2x8.
const ACTION_PADDING_X: f32 = 8.0;
const ACTION_PADDING_Y: f32 = 2.0;
/// Rule 3: radius 6 for buttons.
const ACTION_RADIUS: f32 = 6.0;

/// Render a small action affordance.  The action request stays on the typed
/// projection for the owning shell to dispatch; this renderer intentionally
/// does not invent a click handler or bypass the host action boundary.
///
/// Rule 4's default button: a 1 px `borders.default` outline with no fill, an
/// 11 px `text.primary` label, 2x8 padding and radius 6. Hover fills with
/// `surfaces.hover` rather than changing the label's colour, so a row of these
/// stays quiet until the pointer is on one.
pub fn render_panel_action(action: &PanelAction, target: &str, tokens: ThemeTokens) -> AnyElement {
    let label = action.disabled_reason.map_or_else(
        || action_label(action.action_id),
        PanelDisabledReason::label,
    );
    let mut element = div()
        .id(("task-cockpit-panel-action", action.element_key(target)))
        .flex_none()
        .px(px(ACTION_PADDING_X))
        .py(px(ACTION_PADDING_Y))
        .rounded(px(ACTION_RADIUS))
        .border_1()
        .text_size(px(ACTION_FONT_SIZE))
        .child(label);
    if action.is_enabled() {
        element = element
            .cursor_pointer()
            .border_color(rgb(tokens.borders.default.to_u32()))
            .text_color(rgb(tokens.text.primary.to_u32()))
            .hover(|style| style.bg(rgb(tokens.surfaces.hover.to_u32())));
    } else {
        element = element
            .border_color(rgb(tokens.borders.disabled.to_u32()))
            .text_color(rgb(tokens.text.disabled.to_u32()));
    }
    element.into_any_element()
}

/// Rule 2's group header: 10.5 px uppercase on `text.muted`, letter-spaced,
/// sitting on the row grid rather than in a box of its own.
///
/// This is what a panel body uses where it once painted its own title. The
/// panel already carries its name in the chrome above; a body only ever needs
/// to say which *group* of rows follows.
pub fn panel_group_label(title: &str, tokens: ThemeTokens) -> AnyElement {
    div()
        .w_full()
        .px(px(ROW_PADDING_X))
        .py(px(ROW_PADDING_Y))
        .text_size(px(META_FONT_SIZE))
        .text_color(rgb(tokens.text.muted.to_u32()))
        .child(title.to_uppercase())
        .into_any_element()
}

/// Rule 9's empty state: one 11.5 px `text.muted` sentence. No heading, no
/// illustration, and nothing that looks like an error unless it is one.
pub fn panel_empty_state(sentence: impl Into<String>, tokens: ThemeTokens) -> AnyElement {
    div()
        .w_full()
        .p(px(REGION_PADDING))
        .text_size(px(ROW_FONT_SIZE))
        .text_color(rgb(tokens.text.muted.to_u32()))
        .child(sentence.into())
        .into_any_element()
}

/// Rule 5's list row, as a shell the caller fills and wires.
///
/// The click handler, the element id and the accessibility node stay with the
/// caller -- this only owns the geometry and the two interaction fills, which
/// are the parts every list in the app was getting slightly differently.
pub fn panel_row_shell(tokens: ThemeTokens, selected: bool) -> gpui::Div {
    let row = div()
        .w_full()
        .flex()
        .items_center()
        .gap(px(ROW_GAP))
        .px(px(ROW_PADDING_X))
        .py(px(ROW_PADDING_Y))
        .text_size(px(ROW_FONT_SIZE));
    if selected {
        row.bg(rgb(tokens.surfaces.selection.to_u32()))
            .text_color(rgb(tokens.text.emphasis.to_u32()))
    } else {
        row.text_color(rgb(tokens.text.primary.to_u32()))
            .hover(|style| style.bg(rgb(tokens.surfaces.hover.to_u32())))
    }
}

/// Rule 5's list row, complete: a 14 px grey glyph, an 11.5 px title, an
/// optional 10.5 px `text.muted` metadata line under it, and an optional
/// trailing element the caller owns (a count, a button, a status).
///
/// One painter serves the file tree, the review list, the artifacts list and
/// the services list, because rule 5 gives all four the same row -- and four
/// copies of the same row is how four lists end up with four different ideas
/// of what a selected row looks like.
///
/// The glyph is deliberately not chosen from the row's own type: rule 10 keeps
/// every list glyph 14 px and `text.muted`, so a `.rs` and a `.png` look the
/// same and the colour budget stays with status.
pub fn panel_list_row(
    tokens: ThemeTokens,
    glyph: Option<&'static str>,
    title: impl Into<String>,
    meta: Option<String>,
    trailing: Option<AnyElement>,
    selected: bool,
) -> gpui::Div {
    let title = title.into();
    let lines = div()
        .flex_1()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .child(div().w_full().truncate().child(title))
        .children(meta.map(|meta| {
            div()
                .w_full()
                .truncate()
                .text_size(px(META_FONT_SIZE))
                // A selected row's title goes to `text.emphasis`; its metadata
                // stays muted, so the two lines keep their hierarchy under the
                // selection fill instead of both going white.
                .text_color(rgb(tokens.text.muted.to_u32()))
                .child(meta)
        }));
    panel_row_shell(tokens, selected)
        // A two-line row is aligned to the top of its glyph, not centred on
        // the pair, or the glyph floats between the two lines.
        .items_start()
        .children(glyph.map(|glyph| {
            div().flex_none().child(crate::icons::app_icon(
                glyph,
                ROW_ICON_SIZE,
                tokens.text.muted.to_u32(),
            ))
        }))
        .child(lines)
        .children(trailing)
}

/// The one place rule 1 lets a list row spend colour: a `+n`/`-n` count.
///
/// 10.5 px on `status.success` and `status.destructive`, side by side, and
/// nothing else in the row is tinted. A zero is not painted at all -- a green
/// `+0` is a colour spent on the absence of news.
pub fn panel_change_counts(added: u32, removed: u32, tokens: ThemeTokens) -> AnyElement {
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(ROW_GAP / 2.0))
        .text_size(px(META_FONT_SIZE))
        .children((added > 0).then(|| {
            div()
                .text_color(rgb(tokens.status.success.to_u32()))
                .child(format!("+{added}"))
        }))
        .children((removed > 0).then(|| {
            div()
                .text_color(rgb(tokens.status.destructive.to_u32()))
                .child(format!("-{removed}"))
        }))
        .into_any_element()
}

/// A panel body: the rows, with no title row of its own.
///
/// The `summary` the callers used to print beside a title is now a rule-2
/// group label above the rows, and the panel's name is not repeated at all --
/// the chrome owns it. Controls sit on their own row under the label so a
/// narrow panel wraps the buttons instead of the summary.
pub fn render_panel_frame(
    id: &'static str,
    summary: impl Into<String>,
    actions: impl IntoIterator<Item = PanelAction>,
    body: impl IntoElement,
    tokens: ThemeTokens,
) -> AnyElement {
    let controls: Vec<AnyElement> = actions
        .into_iter()
        .map(|action| render_panel_action(&action, "panel", tokens))
        .collect();
    let summary = summary.into();
    div()
        .id(id)
        .w_full()
        .flex()
        .flex_col()
        .child(panel_group_label(&summary, tokens))
        .children((!controls.is_empty()).then(|| {
            div()
                .flex()
                .flex_wrap()
                .gap(px(ROW_GAP))
                .px(px(ROW_PADDING_X))
                .pb(px(ROW_PADDING_Y))
                .children(controls)
        }))
        .child(body)
        .into_any_element()
}

pub fn action_label(action_id: &str) -> &'static str {
    match action_id {
        crate::client::action::ACTION_WORKSPACE_STATUS => "Refresh workspace",
        crate::client::action::ACTION_GIT_STATUS => "Refresh changes",
        crate::client::action::ACTION_FILES_LIST => "Refresh files",
        crate::client::action::ACTION_FILES_READ => "Read file",
        crate::client::action::ACTION_TASK_SHOW => "Refresh task",
        _ => "Open",
    }
}

/// Accept an action only while its captured task and optional revision remain
/// the selected durable snapshot.  This is the UI capture fence; the host
/// still performs its own task/capability/path admission.
pub fn action_is_current(
    action: &PanelAction,
    selected_task: Option<TaskId>,
    current_revision: Option<u64>,
) -> bool {
    selected_task == Some(action.identity.task_id) && action.identity.revision == current_revision
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::action;
    use crate::domain::cockpit::TaskCockpitQuery;

    #[test]
    fn action_keeps_exact_task_revision_and_catalog_identity() {
        let identity = task_identity(TaskId::new(), Some(17));
        let request = ActionRequest::TaskCockpit {
            task_id: identity.task_id,
            query: TaskCockpitQuery::GitStatus,
        };
        let action = PanelAction::enabled(identity, request.clone());
        assert_eq!(action.identity, identity);
        assert_eq!(action.action_id, action::ACTION_GIT_STATUS);
        assert_eq!(action.request, request);
        assert!(action.is_enabled());
    }

    #[test]
    fn disabled_reason_is_typed_and_truthful() {
        let identity = task_identity(TaskId::new(), None);
        let request = ActionRequest::TaskCockpit {
            task_id: identity.task_id,
            query: TaskCockpitQuery::WorkspaceStatus,
        };
        let action = PanelAction::disabled(
            identity,
            request,
            PanelDisabledReason::HostProjectionMissing,
        );
        assert_eq!(
            action.disabled_reason,
            Some(PanelDisabledReason::HostProjectionMissing)
        );
        assert!(!action.is_enabled());
        assert_eq!(
            action.disabled_reason.map(PanelDisabledReason::label),
            Some("Host projection unavailable")
        );
    }

    #[test]
    fn element_key_is_unique_for_task_revision_action_and_row_target() {
        let task_id = TaskId::new();
        let first = PanelAction::enabled(
            task_identity(task_id, Some(1)),
            ActionRequest::TaskCockpit {
                task_id,
                query: TaskCockpitQuery::FilesRead {
                    relative_path: "src/a.rs".into(),
                    max_bytes: 16,
                },
            },
        );
        let second = PanelAction::enabled(
            task_identity(task_id, Some(1)),
            ActionRequest::TaskCockpit {
                task_id,
                query: TaskCockpitQuery::FilesRead {
                    relative_path: "src/b.rs".into(),
                    max_bytes: 16,
                },
            },
        );
        assert_ne!(
            first.element_key("src/a.rs"),
            second.element_key("src/b.rs")
        );
        assert_ne!(first.element_key("src/a.rs"), first.element_key("src/b.rs"));
    }

    #[test]
    fn current_action_rejects_task_or_revision_drift() {
        let task_id = TaskId::new();
        let action = PanelAction::enabled(
            task_identity(task_id, Some(4)),
            ActionRequest::TaskShow { task_id },
        );
        assert!(action_is_current(&action, Some(task_id), Some(4)));
        assert!(!action_is_current(&action, Some(TaskId::new()), Some(4)));
        assert!(!action_is_current(&action, Some(task_id), Some(5)));
        let unfenced = PanelAction::enabled(
            task_identity(task_id, None),
            ActionRequest::TaskShow { task_id },
        );
        assert!(action_is_current(&unfenced, Some(task_id), None));
        assert!(!action_is_current(&unfenced, Some(task_id), Some(4)));
    }

    /// Rule 2's scale, read off the constants the painters actually use rather
    /// than off a painter's memory of the mockup.
    #[test]
    fn the_body_type_scale_is_the_redesigns_scale() {
        assert_eq!(ROW_FONT_SIZE, 11.5, "rule 2: body/list text is 11.5 px");
        assert_eq!(META_FONT_SIZE, 10.5, "rule 2: captions/meta are 10.5 px");
        assert_eq!(ACTION_FONT_SIZE, 11.0, "rule 4: button labels are 11 px");
        assert_eq!(ROW_ICON_SIZE, 14.0, "rule 10: list glyphs are 14 px");
        assert_eq!(ACTION_RADIUS, 6.0, "rule 3: radius 6 for buttons");
        assert_eq!(ACTION_PADDING_X, 8.0);
        assert_eq!(ACTION_PADDING_Y, 2.0);
        assert_eq!(ROW_PADDING_X, 10.0, "rule 5: rows are padded 5x10");
        assert_eq!(ROW_PADDING_Y, 5.0, "rule 5: rows are padded 5x10");
        assert_eq!(ROW_GAP, 8.0, "rule 6: control gap is 8");
    }

    /// The panel bodies this lane restyled, by path. A file that disappears or
    /// is renamed fails the scan below loudly instead of dropping out of it.
    const RESTYLED_BODIES: [&str; 6] = [
        "src/ui/task_cockpit/panel.rs",
        "src/ui/task_cockpit/browser_panel.rs",
        "src/ui/task_cockpit/changes_panel.rs",
        "src/ui/task_cockpit/config_sidebar.rs",
        "src/ui/task_cockpit/dock.rs",
        "src/ui/native_trusted_hosts_view.rs",
    ];

    /// Every text-size call in a restyled body names a size on rule 2's
    /// scale -- as a literal that is one of the scale's values, or as a
    /// `*_FONT_SIZE` constant, which the test above then pins to a number.
    ///
    /// The needle is assembled from two halves so that this scan cannot match
    /// its own source: written out whole it found three hits inside this very
    /// function, which is a denominator inflated by the scanner counting
    /// itself.
    ///
    /// The regression this exists to catch is the second form going back to
    /// `tokens.density.typography.*`: that scale is the *density* scale
    /// (caption 11/12, body 13/14, title 18/20) and is not rule 2's, so a body
    /// that reads a size from it is off the redesign while looking as
    /// principled as one that is on it. Every one of these files read from it
    /// before this lane.
    ///
    /// The denominator is asserted, not merely computed: this scan is new, so
    /// its first green is the one to distrust, and a scan that has stopped
    /// finding its subject reads exactly like a scan that found nothing wrong.
    /// It has already fired once for that reason.
    #[test]
    fn every_text_size_in_a_restyled_body_names_rule_2s_scale() {
        const ALLOWED_LITERALS: [&str; 5] = ["10.5", "11.0", "11.5", "12.0", "13.0"];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut checked = 0usize;
        let mut offenders: Vec<String> = Vec::new();
        for relative in RESTYLED_BODIES {
            let path = root.join(relative);
            let source = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("cannot read {relative}: {error}"));
            for (line_number, line) in source.lines().enumerate() {
                let needle = concat!("text_size", "(px(");
                let mut rest = line;
                while let Some(start) = rest.find(needle) {
                    rest = &rest[start + needle.len()..];
                    let Some(end) = rest.find(')') else { break };
                    let value = rest[..end].trim();
                    checked += 1;
                    let is_number = !value.is_empty()
                        && value
                            .chars()
                            .all(|character| character.is_ascii_digit() || character == '.');
                    let on_scale = if is_number {
                        ALLOWED_LITERALS.contains(&value)
                    } else {
                        // A named size, pinned by `the_body_type_scale_is_the_
                        // redesigns_scale` or by its own module's equivalent.
                        // The density scale is excluded by name because it is
                        // the one wrong answer that looks right.
                        value.ends_with("FONT_SIZE") && !value.contains("density")
                    };
                    if !on_scale {
                        offenders.push(format!("{relative}:{}: {value}", line_number + 1));
                    }
                }
            }
        }
        assert!(
            checked >= 15,
            "the scan found only {checked} text sizes across {} files; it has \
             stopped seeing its subject",
            RESTYLED_BODIES.len()
        );
        assert!(
            offenders.is_empty(),
            "text sizes off rule 2's scale (of {checked} checked): {offenders:?}"
        );
    }
}

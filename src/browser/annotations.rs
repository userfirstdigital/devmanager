use super::{
    redact_browser_text, validate_browser_url, BrowserAnnotation, BrowserAnnotationKind,
    BrowserBounds, BrowserError, BrowserLocator, BrowserResourceId, BrowserRevision,
    BrowserUserInputKind, BrowserViewport, BrowserWorkspaceKey,
};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io::Cursor;

pub const MAX_ANNOTATION_IPC_BYTES: usize = 32 * 1024;
const MAX_SEMANTIC_TEXT: usize = 1_000;
const MAX_SELECTOR_BYTES: usize = 512;
const MAX_SELECTORS: usize = 8;
const MAX_STYLE_VALUE_BYTES: usize = 256;
const MAX_VIEWPORT_EDGE: u32 = 16_384;
const STYLE_ALLOWLIST: &[&str] = &[
    "display",
    "position",
    "color",
    "backgroundColor",
    "fontFamily",
    "fontSize",
    "fontWeight",
    "border",
    "borderRadius",
    "padding",
    "margin",
    "opacity",
    "visibility",
];

#[derive(Debug, Clone, Serialize, rmcp::schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserAnnotationCandidate {
    pub kind: BrowserAnnotationKind,
    pub url: String,
    pub revision: BrowserRevision,
    pub locator: BrowserLocator,
    pub bounds: BrowserBounds,
    pub viewport: BrowserViewport,
    pub computed_styles: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrowserAnnotationCandidateWire {
    kind: BrowserAnnotationKind,
    url: String,
    revision: BrowserRevision,
    locator: BrowserAnnotationLocatorWire,
    bounds: BrowserAnnotationBoundsWire,
    viewport: BrowserAnnotationViewportWire,
    computed_styles: BTreeMap<String, String>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct BrowserAnnotationLocatorWire {
    accessibility_role: Option<String>,
    accessibility_name: Option<String>,
    test_id: Option<String>,
    css_selectors: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrowserAnnotationBoundsWire {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrowserAnnotationViewportWire {
    width: u32,
    height: u32,
    scale_percent: u16,
}

impl<'de> Deserialize<'de> for BrowserAnnotationCandidate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = BrowserAnnotationCandidateWire::deserialize(deserializer)?;
        Ok(Self {
            kind: wire.kind,
            url: wire.url,
            revision: wire.revision,
            locator: BrowserLocator {
                accessibility_role: wire.locator.accessibility_role,
                accessibility_name: wire.locator.accessibility_name,
                test_id: wire.locator.test_id,
                css_selectors: wire.locator.css_selectors,
            },
            bounds: BrowserBounds {
                x: wire.bounds.x,
                y: wire.bounds.y,
                width: wire.bounds.width,
                height: wire.bounds.height,
            },
            viewport: BrowserViewport {
                width: wire.viewport.width,
                height: wire.viewport.height,
                scale_percent: wire.viewport.scale_percent,
            },
            computed_styles: wire.computed_styles,
        })
    }
}

impl BrowserAnnotationCandidate {
    pub fn validate(&self) -> Result<(), BrowserError> {
        validate_browser_url(&self.url).map_err(|error| BrowserError::InvalidAnnotation {
            field: "url".to_string(),
            message: error.to_string(),
        })?;
        validate_optional_text(
            "locator.accessibilityRole",
            self.locator.accessibility_role.as_deref(),
        )?;
        validate_optional_text(
            "locator.accessibilityName",
            self.locator.accessibility_name.as_deref(),
        )?;
        validate_optional_text("locator.testId", self.locator.test_id.as_deref())?;
        if self.locator.css_selectors.len() > MAX_SELECTORS
            || self.locator.css_selectors.iter().any(|selector| {
                selector.trim().is_empty()
                    || selector.len() > MAX_SELECTOR_BYTES
                    || selector.chars().any(char::is_control)
            })
        {
            return invalid("locator.cssSelectors", "contains invalid selectors");
        }
        if self.viewport.width == 0
            || self.viewport.height == 0
            || self.viewport.width > MAX_VIEWPORT_EDGE
            || self.viewport.height > MAX_VIEWPORT_EDGE
            || !(25..=500).contains(&self.viewport.scale_percent)
        {
            return invalid("viewport", "is outside supported bounds");
        }
        let x = i64::from(self.bounds.x);
        let y = i64::from(self.bounds.y);
        let width = i64::from(self.bounds.width);
        let height = i64::from(self.bounds.height);
        let (Some(right), Some(bottom)) = (x.checked_add(width), y.checked_add(height)) else {
            return invalid("bounds", "overflows the supported coordinate range");
        };
        if x < 0
            || y < 0
            || width <= 0
            || height <= 0
            || x >= i64::from(self.viewport.width)
            || y >= i64::from(self.viewport.height)
            || right > i64::from(self.viewport.width)
            || bottom > i64::from(self.viewport.height)
        {
            return invalid("bounds", "is outside the viewport");
        }
        if self.computed_styles.len() > STYLE_ALLOWLIST.len()
            || self.computed_styles.iter().any(|(key, value)| {
                !STYLE_ALLOWLIST.contains(&key.as_str())
                    || value.len() > MAX_STYLE_VALUE_BYTES
                    || value.chars().any(char::is_control)
            })
        {
            return invalid(
                "computedStyles",
                "contains a non-allowlisted or oversized value",
            );
        }
        if self.kind == BrowserAnnotationKind::Element
            && self.locator.accessibility_role.is_none()
            && self.locator.accessibility_name.is_none()
            && self.locator.test_id.is_none()
            && self.locator.css_selectors.is_empty()
        {
            return invalid("locator", "element annotations require a semantic fallback");
        }
        Ok(())
    }
}

fn validate_optional_text(field: &str, value: Option<&str>) -> Result<(), BrowserError> {
    if value.is_some_and(|value| {
        value.trim().is_empty()
            || value.len() > MAX_SEMANTIC_TEXT
            || value.chars().any(char::is_control)
    }) {
        return invalid(field, "is blank, oversized, or contains control characters");
    }
    Ok(())
}

fn invalid<T>(field: &str, message: &str) -> Result<T, BrowserError> {
    Err(BrowserError::InvalidAnnotation {
        field: field.to_string(),
        message: message.to_string(),
    })
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum BrowserPageIpcMessage {
    UserInput {
        kind: BrowserUserInputKind,
    },
    DomMutation,
    AnnotationCandidate {
        candidate: BrowserAnnotationCandidate,
    },
    AnnotationCanceled,
}

pub fn parse_browser_page_ipc_message(body: &str) -> Result<BrowserPageIpcMessage, BrowserError> {
    if body.len() > MAX_ANNOTATION_IPC_BYTES {
        return invalid("ipcBody", "exceeds 32 KiB");
    }
    serde_json::from_str(body).map_err(|_| BrowserError::InvalidAnnotation {
        field: "ipcBody".to_string(),
        message: "is malformed".to_string(),
    })
}

pub fn parse_browser_annotation_ipc_message(
    body: &str,
) -> Result<BrowserAnnotationCandidate, BrowserError> {
    let BrowserPageIpcMessage::AnnotationCandidate { candidate } =
        parse_browser_page_ipc_message(body)?
    else {
        return invalid("ipcBody", "is not an annotation candidate");
    };
    candidate.validate()?;
    Ok(candidate)
}

pub fn validate_annotation_candidate_context(
    candidate: &BrowserAnnotationCandidate,
    expected_url: &str,
    expected_revision: BrowserRevision,
) -> Result<(), BrowserError> {
    candidate.validate()?;
    if candidate.revision != expected_revision {
        return Err(BrowserError::StaleReference {
            expected: expected_revision,
            actual: candidate.revision,
        });
    }
    if candidate.url != expected_url {
        return invalid("url", "does not match the active browser tab");
    }
    Ok(())
}

pub fn crop_annotation_png(
    png: &[u8],
    bounds: BrowserBounds,
    viewport: &BrowserViewport,
) -> Result<Vec<u8>, BrowserError> {
    if viewport.width == 0 || viewport.height == 0 {
        return invalid("viewport", "cannot be empty");
    }
    let image =
        image::load_from_memory_with_format(png, image::ImageFormat::Png).map_err(|_| {
            BrowserError::InvalidAnnotation {
                field: "screenshot".to_string(),
                message: "is not a valid PNG".to_string(),
            }
        })?;
    let scale_x = f64::from(image.width()) / f64::from(viewport.width);
    let scale_y = f64::from(image.height()) / f64::from(viewport.height);
    let left = (f64::from(bounds.x).max(0.0) * scale_x).floor();
    let top = (f64::from(bounds.y).max(0.0) * scale_y).floor();
    let right = (f64::from(bounds.x.saturating_add(bounds.width)).max(0.0) * scale_x).ceil();
    let bottom = (f64::from(bounds.y.saturating_add(bounds.height)).max(0.0) * scale_y).ceil();
    let left = left.clamp(0.0, f64::from(image.width())) as u32;
    let top = top.clamp(0.0, f64::from(image.height())) as u32;
    let right = right.clamp(0.0, f64::from(image.width())) as u32;
    let bottom = bottom.clamp(0.0, f64::from(image.height())) as u32;
    if right <= left || bottom <= top {
        return invalid("bounds", "does not intersect the screenshot");
    }
    let crop = image.crop_imm(left, top, right - left, bottom - top);
    let mut encoded = Cursor::new(Vec::new());
    crop.write_to(&mut encoded, image::ImageFormat::Png)
        .map_err(|error| BrowserError::CrashedView {
            message: format!("could not encode annotation crop: {error}"),
        })?;
    Ok(encoded.into_inner())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserAnnotationDraft {
    pub id: String,
    pub tab_id: String,
    pub candidate: BrowserAnnotationCandidate,
    pub screenshot_resource: BrowserResourceId,
}

impl BrowserAnnotationDraft {
    pub fn new(
        tab_id: impl Into<String>,
        candidate: BrowserAnnotationCandidate,
        screenshot_resource: BrowserResourceId,
    ) -> Result<Self, BrowserError> {
        let tab_id = tab_id.into();
        if tab_id.trim().is_empty() {
            return invalid("tabId", "cannot be blank");
        }
        candidate.validate()?;
        Ok(Self {
            id: random_id("draft-")?,
            tab_id,
            candidate,
            screenshot_resource,
        })
    }

    pub fn into_annotation(
        self,
        comment: impl Into<String>,
    ) -> Result<BrowserAnnotation, BrowserError> {
        let comment = comment.into().trim().to_string();
        if comment.is_empty() {
            return invalid("comment", "cannot be blank");
        }
        Ok(BrowserAnnotation {
            id: random_id("ann-")?,
            kind: self.candidate.kind,
            tab_id: self.tab_id,
            anchor_revision: self.candidate.revision,
            comment,
            url: redact_browser_text(&self.candidate.url),
            locator: self.candidate.locator,
            bounds: self.candidate.bounds,
            viewport: self.candidate.viewport,
            screenshot_resource: self.screenshot_resource,
            computed_styles: self.candidate.computed_styles,
            resolved: false,
        })
    }
}

fn random_id(prefix: &str) -> Result<String, BrowserError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| BrowserError::CrashedView {
        message: format!("could not generate browser annotation id: {error}"),
    })?;
    let mut id = String::with_capacity(prefix.len() + bytes.len() * 2);
    id.push_str(prefix);
    for byte in bytes {
        let _ = write!(id, "{byte:02x}");
    }
    Ok(id)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BrowserAnnotationRoute {
    pub workspace_key: BrowserWorkspaceKey,
    pub tab_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserAnnotationResourceCleanup {
    pub route: BrowserAnnotationRoute,
    pub resource_id: BrowserResourceId,
}

#[derive(Debug, Default)]
pub struct BrowserAnnotationCleanupLedger {
    pending: HashMap<(BrowserWorkspaceKey, BrowserResourceId), BrowserAnnotationRoute>,
}

impl BrowserAnnotationCleanupLedger {
    pub fn enqueue(
        &mut self,
        route: BrowserAnnotationRoute,
        resource_id: BrowserResourceId,
    ) -> bool {
        let key = (route.workspace_key.clone(), resource_id);
        if self.pending.contains_key(&key) {
            return false;
        }
        self.pending.insert(key, route);
        true
    }

    pub fn pending_for_workspace(
        &self,
        workspace_key: &BrowserWorkspaceKey,
    ) -> Vec<BrowserAnnotationResourceCleanup> {
        self.pending
            .iter()
            .filter(|((owner, _), _)| owner == workspace_key)
            .map(
                |((_, resource_id), route)| BrowserAnnotationResourceCleanup {
                    route: route.clone(),
                    resource_id: resource_id.clone(),
                },
            )
            .collect()
    }

    pub fn pending_for_project(&self, project_id: &str) -> Vec<BrowserAnnotationResourceCleanup> {
        self.pending
            .iter()
            .filter(|((owner, _), _)| owner.project_id == project_id)
            .map(
                |((_, resource_id), route)| BrowserAnnotationResourceCleanup {
                    route: route.clone(),
                    resource_id: resource_id.clone(),
                },
            )
            .collect()
    }

    pub fn retry_workspace<F>(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        mut release: F,
    ) -> Vec<(BrowserAnnotationResourceCleanup, BrowserError)>
    where
        F: FnMut(&BrowserAnnotationResourceCleanup) -> Result<(), BrowserError>,
    {
        let pending = self.pending_for_workspace(workspace_key);
        let mut failures = Vec::new();
        for cleanup in pending {
            match release(&cleanup) {
                Ok(()) | Err(BrowserError::MissingResource { .. }) => {
                    self.pending.remove(&(
                        cleanup.route.workspace_key.clone(),
                        cleanup.resource_id.clone(),
                    ));
                }
                Err(error) => failures.push((cleanup, error)),
            }
        }
        failures
    }
}

impl BrowserAnnotationRoute {
    pub fn new(
        workspace_key: BrowserWorkspaceKey,
        tab_id: impl Into<String>,
    ) -> Result<Self, BrowserError> {
        let tab_id = tab_id.into();
        if tab_id.trim().is_empty() {
            return invalid("tabId", "cannot be blank");
        }
        Ok(Self {
            workspace_key,
            tab_id,
        })
    }
}

#[derive(Debug, Clone)]
struct BrowserAnnotationMode {
    url: String,
    revision: BrowserRevision,
}

#[derive(Debug, Clone)]
struct OwnedDraft {
    route: BrowserAnnotationRoute,
    draft: BrowserAnnotationDraft,
}

#[derive(Debug, Default)]
pub struct BrowserAnnotationLifecycle {
    modes: HashMap<BrowserAnnotationRoute, BrowserAnnotationMode>,
    drafts: HashMap<String, OwnedDraft>,
}

impl BrowserAnnotationLifecycle {
    pub fn activate(
        &mut self,
        route: BrowserAnnotationRoute,
        url: impl Into<String>,
        revision: BrowserRevision,
    ) {
        self.modes.insert(
            route,
            BrowserAnnotationMode {
                url: url.into(),
                revision,
            },
        );
    }

    pub fn is_active(&self, route: &BrowserAnnotationRoute) -> bool {
        self.modes.contains_key(route)
    }

    pub fn deactivate(&mut self, route: &BrowserAnnotationRoute) -> bool {
        self.modes.remove(route).is_some()
    }

    pub fn accept_candidate(
        &mut self,
        route: &BrowserAnnotationRoute,
        candidate: BrowserAnnotationCandidate,
    ) -> Result<BrowserAnnotationCandidate, BrowserError> {
        let mode = self
            .modes
            .get(route)
            .ok_or_else(|| BrowserError::BlockedPermission {
                permission: "inactive annotation mode".to_string(),
            })?;
        validate_annotation_candidate_context(&candidate, &mode.url, mode.revision)?;
        self.modes.remove(route);
        Ok(candidate)
    }

    pub fn store_draft(
        &mut self,
        route: BrowserAnnotationRoute,
        draft: BrowserAnnotationDraft,
    ) -> Result<(), BrowserError> {
        if draft.tab_id != route.tab_id || self.drafts.contains_key(&draft.id) {
            return invalid("draft", "does not match its browser route");
        }
        self.drafts
            .insert(draft.id.clone(), OwnedDraft { route, draft });
        Ok(())
    }

    pub fn take_draft(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        draft_id: &str,
    ) -> Result<BrowserAnnotationDraft, BrowserError> {
        let owned = self
            .drafts
            .get(draft_id)
            .ok_or_else(|| BrowserError::InvalidAnnotation {
                field: "draftId".to_string(),
                message: "does not exist".to_string(),
            })?;
        if &owned.route.workspace_key != workspace_key {
            return Err(BrowserError::BlockedPermission {
                permission: "annotation draft ownership".to_string(),
            });
        }
        Ok(self
            .drafts
            .remove(draft_id)
            .expect("draft was checked above")
            .draft)
    }

    pub fn restore_draft(&mut self, route: BrowserAnnotationRoute, draft: BrowserAnnotationDraft) {
        self.drafts
            .insert(draft.id.clone(), OwnedDraft { route, draft });
    }

    pub fn cancel_route(&mut self, route: &BrowserAnnotationRoute) -> Vec<BrowserAnnotationDraft> {
        self.modes.remove(route);
        let ids: Vec<_> = self
            .drafts
            .iter()
            .filter(|(_, owned)| &owned.route == route)
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| self.drafts.remove(&id).map(|owned| owned.draft))
            .collect()
    }

    pub fn cancel_workspace(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
    ) -> Vec<(BrowserAnnotationRoute, BrowserAnnotationDraft)> {
        self.modes
            .retain(|route, _| &route.workspace_key != workspace_key);
        let ids: Vec<_> = self
            .drafts
            .iter()
            .filter(|(_, owned)| &owned.route.workspace_key == workspace_key)
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| {
                self.drafts
                    .remove(&id)
                    .map(|owned| (owned.route, owned.draft))
            })
            .collect()
    }

    pub fn cancel_project(
        &mut self,
        project_id: &str,
    ) -> Vec<(BrowserAnnotationRoute, BrowserAnnotationDraft)> {
        self.modes
            .retain(|route, _| route.workspace_key.project_id != project_id);
        let ids: Vec<_> = self
            .drafts
            .iter()
            .filter(|(_, owned)| owned.route.workspace_key.project_id == project_id)
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| {
                self.drafts
                    .remove(&id)
                    .map(|owned| (owned.route, owned.draft))
            })
            .collect()
    }
}

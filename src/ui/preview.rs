//! Deterministic, isolated native UI preview contracts.

use gpui::{
    div, px, Action, Context, InteractiveElement, IntoElement, KeyBinding, ParentElement, Render,
    Styled, Window,
};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::assets::AppAssets;
use crate::client::action;
use crate::terminal::terminal_font;
use crate::ui::components::{
    Button, ButtonVariant, IconButton, IconId, StatusLight, TooltipContract,
};
use crate::ui::preview_capture;
use crate::ui::tokens::{theme, Density, Scale, StatusMeaning, ThemeMode};

pub const PREVIEW_SCHEMA: &str = "devmanager.ui.preview/v1";
pub const MAX_FIXTURE_BYTES: u64 = 256 * 1024;
pub const PREVIEW_SENTINEL_RGBA: [u8; 4] = [0x91, 0x2b, 0xd4, 0xff];
const PREVIEW_SENTINEL_RGB: u32 = ((PREVIEW_SENTINEL_RGBA[0] as u32) << 16)
    | ((PREVIEW_SENTINEL_RGBA[1] as u32) << 8)
    | PREVIEW_SENTINEL_RGBA[2] as u32;
const PREVIEW_SENTINEL_SIZE: f32 = 32.0;
const PREVIEW_USAGE: &str =
    "usage: devmanager-next --ui-preview <fixture.json> --output <preview.png>";
static PREVIEW_RUN_NONCE: AtomicU64 = AtomicU64::new(0);

gpui::actions!(devmanager_next, [PreviewDismiss]);

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "host.actions")]
pub struct HostActions;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "host.status")]
pub struct HostStatus;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "task.list")]
pub struct TaskList;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "task.show")]
pub struct TaskShow;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "task.create")]
pub struct TaskCreate;

#[derive(Clone, Debug, Default, PartialEq, Eq, gpui::Action)]
#[action(name = "task.rename")]
pub struct TaskRename;

const TASK_COCKPIT_ACTION_NAMES: [&str; 6] = [
    action::ACTION_HOST_ACTIONS,
    action::ACTION_HOST_STATUS,
    action::ACTION_TASK_LIST,
    action::ACTION_TASK_SHOW,
    action::ACTION_TASK_CREATE,
    action::ACTION_TASK_RENAME,
];

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewFixture {
    pub schema: String,
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub capture: PreviewCaptureFixture,
    pub root: PreviewRootFixture,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewCaptureSetting {
    #[default]
    Excluded,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewCaptureFixture {
    pub cursor: PreviewCaptureSetting,
    pub border: PreviewCaptureSetting,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewRootFixture {
    pub kind: String,
    pub label: String,
    #[serde(default)]
    pub gallery: Option<ComponentGalleryFixture>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GalleryTheme {
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GalleryDensity {
    Compact,
    Comfortable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GalleryState {
    Default,
    Hover,
    Pressed,
    Focused,
    Disabled,
    Loading,
    Destructive,
    Selected,
    Status,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentGallerySamples {
    pub long_text: String,
    pub unicode: String,
    pub missing: String,
    pub error: String,
    pub loading: String,
    pub empty: String,
    pub overflow: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentGalleryFixture {
    pub themes: Vec<GalleryTheme>,
    pub densities: Vec<GalleryDensity>,
    pub scales: Vec<u16>,
    pub states: Vec<GalleryState>,
    pub samples: ComponentGallerySamples,
}

impl ComponentGalleryFixture {
    fn sanitize(&mut self) -> Result<(), String> {
        for (name, value) in [
            ("long_text", &mut self.samples.long_text),
            ("unicode", &mut self.samples.unicode),
            ("missing", &mut self.samples.missing),
            ("error", &mut self.samples.error),
            ("loading", &mut self.samples.loading),
            ("empty", &mut self.samples.empty),
            ("overflow", &mut self.samples.overflow),
        ] {
            *value = crate::ui::components::interaction::redacted_bounded_text(
                "component gallery sample",
                std::mem::take(value),
                4096,
                16384,
            )
            .map_err(|_| format!("component gallery sample {name} is empty or unsafe"))?;
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.themes.len() != 2
            || !self.themes.contains(&GalleryTheme::Dark)
            || !self.themes.contains(&GalleryTheme::Light)
        {
            return Err("component gallery must cover dark and light themes".to_string());
        }
        if self.densities.len() != 2
            || !self.densities.contains(&GalleryDensity::Compact)
            || !self.densities.contains(&GalleryDensity::Comfortable)
        {
            return Err("component gallery must cover compact and comfortable density".to_string());
        }
        if self.scales != [100, 125, 150, 200] {
            return Err(
                "component gallery must cover 100, 125, 150, and 200 percent scales".to_string(),
            );
        }
        let required_states = [
            GalleryState::Default,
            GalleryState::Hover,
            GalleryState::Pressed,
            GalleryState::Focused,
            GalleryState::Disabled,
            GalleryState::Loading,
            GalleryState::Destructive,
            GalleryState::Selected,
            GalleryState::Status,
        ];
        if self.states.len() != required_states.len()
            || required_states
                .iter()
                .any(|state| !self.states.contains(state))
        {
            return Err(
                "component gallery must cover every reusable interaction state".to_string(),
            );
        }
        for (name, value) in [
            ("long_text", &self.samples.long_text),
            ("unicode", &self.samples.unicode),
            ("missing", &self.samples.missing),
            ("error", &self.samples.error),
            ("loading", &self.samples.loading),
            ("empty", &self.samples.empty),
            ("overflow", &self.samples.overflow),
        ] {
            if value.trim().is_empty() {
                return Err(format!("component gallery sample {name} must not be blank"));
            }
            if value.chars().count() > 4096 || value.len() > 16384 {
                return Err(format!("component gallery sample {name} is oversized"));
            }
        }
        if self.samples.long_text.chars().count() <= 256 {
            return Err("component gallery long_text must exercise overflow wrapping".to_string());
        }
        if !self
            .samples
            .unicode
            .chars()
            .any(|character| !character.is_ascii())
        {
            return Err("component gallery unicode sample must contain non-ASCII text".to_string());
        }
        Ok(())
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PreviewPathPolicy {
    fixture_root: PathBuf,
    output_root: PathBuf,
    temp_root: PathBuf,
}

impl PreviewPathPolicy {
    pub fn new(
        fixture_root: impl Into<PathBuf>,
        output_root: impl Into<PathBuf>,
        temp_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            fixture_root: fixture_root.into(),
            output_root: output_root.into(),
            temp_root: temp_root.into(),
        }
    }

    pub fn for_workspace(workspace_root: impl AsRef<Path>) -> Self {
        let workspace_root = workspace_root.as_ref();
        let run_nonce = PREVIEW_RUN_NONCE.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        Self::new(
            workspace_root.join("tests/fixtures/ui"),
            workspace_root.join(".devmanager-next/evidence/phase-05/screenshots"),
            std::env::temp_dir().join(format!(
                "devmanager-next-preview-{}-{timestamp}-{run_nonce}",
                std::process::id()
            )),
        )
    }

    pub fn fixture_root(&self) -> &Path {
        &self.fixture_root
    }

    pub fn output_root(&self) -> &Path {
        &self.output_root
    }

    pub fn temp_root(&self) -> &Path {
        &self.temp_root
    }
}

#[derive(Clone)]
pub struct PreviewRequest {
    fixture_path: PathBuf,
    output_path: PathBuf,
    trusted_output_authority: Arc<preview_capture::CaptureOutputAuthority>,
}

impl PartialEq for PreviewRequest {
    fn eq(&self, other: &Self) -> bool {
        self.fixture_path == other.fixture_path
            && self.output_path == other.output_path
            && self.trusted_output_authority == other.trusted_output_authority
    }
}

impl Eq for PreviewRequest {}

impl PreviewRequest {
    pub fn fixture_path(&self) -> &Path {
        &self.fixture_path
    }

    pub fn output_path(&self) -> &Path {
        &self.output_path
    }

    pub(crate) fn capture_authority(&self) -> &Arc<preview_capture::CaptureOutputAuthority> {
        &self.trusted_output_authority
    }

    pub fn write_bgra_png_atomic(
        &self,
        width: u32,
        height: u32,
        bgra: &[u8],
    ) -> Result<(), preview_capture::PreviewCaptureError> {
        let lease = preview_capture::CaptureGeneration::new().begin();
        preview_capture::encode_bgra_png_atomic_with_authority(
            Arc::clone(&self.trusted_output_authority),
            width,
            height,
            bgra,
            preview_capture::CaptureDeadline::from_now(preview_capture::FIRST_FRAME_DEADLINE),
            &lease,
        )
    }

    pub fn validate(
        fixture_path: impl AsRef<Path>,
        output_path: impl AsRef<Path>,
        policy: &PreviewPathPolicy,
    ) -> Result<Self, PreviewError> {
        let fixture_path = absolute_path(fixture_path.as_ref())?;
        let output_path = absolute_path(output_path.as_ref())?;
        reject_reparse_ancestors(&fixture_path, "fixture")?;
        reject_reparse_ancestors(&output_path, "output")?;
        reject_reparse_ancestors(policy.fixture_root(), "fixture")?;
        reject_reparse_ancestors(policy.output_root(), "output")?;
        reject_reparse_ancestors(policy.temp_root(), "output")?;
        let fixture_check = checked_path(&fixture_path)?;
        let fixture_root = checked_path(policy.fixture_root())?;

        if !is_within(&fixture_check, &fixture_root) {
            return Err(PreviewError::OutsideApprovedRoot {
                path: fixture_path,
                root_kind: "fixture",
            });
        }
        if is_sensitive_path(&fixture_check) {
            return Err(PreviewError::SensitivePath { path: fixture_path });
        }
        if fixture_path.extension().and_then(OsStr::to_str) != Some("json") {
            return Err(PreviewError::InvalidArgument(format!(
                "fixture must use the .json extension: {}",
                safe_preview_path(&fixture_path)
            )));
        }
        match fs::metadata(&fixture_path) {
            Ok(metadata) if metadata.is_file() => {
                if metadata.len() > MAX_FIXTURE_BYTES {
                    return Err(PreviewError::FixtureTooLarge {
                        path: fixture_path,
                        bytes: metadata.len(),
                        max_bytes: MAX_FIXTURE_BYTES,
                    });
                }
            }
            Ok(_) => {
                return Err(PreviewError::FixtureNotRegular { path: fixture_path });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(PreviewError::FixtureMissing { path: fixture_path });
            }
            Err(error) => {
                return Err(PreviewError::FixtureIo {
                    path: fixture_path,
                    message: error.to_string(),
                });
            }
        }

        let output_check = checked_path(&output_path)?;
        let output_root = checked_path(policy.output_root())?;
        let temp_root = checked_path(policy.temp_root())?;
        let output_is_approved =
            is_within(&output_check, &output_root) || is_within(&output_check, &temp_root);
        let trusted_output_root = if is_within(&output_check, &output_root) {
            output_root.clone()
        } else {
            temp_root.clone()
        };
        if is_sensitive_path(&output_check) {
            return Err(PreviewError::SensitivePath { path: output_path });
        }
        if !output_is_approved {
            return Err(PreviewError::OutsideApprovedRoot {
                path: output_path,
                root_kind: "output",
            });
        }
        if output_path.extension().and_then(OsStr::to_str) != Some("png") {
            return Err(PreviewError::InvalidArgument(format!(
                "output must use the .png extension: {}",
                safe_preview_path(&output_path)
            )));
        }
        if output_path.exists() {
            return Err(PreviewError::OutputAlreadyExists { path: output_path });
        }

        let output_parent = output_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| {
                PreviewError::InvalidArgument("output must have a parent directory".into())
            })?;
        fs::create_dir_all(output_parent).map_err(|error| PreviewError::OutputFailed {
            reason: preview_capture::bounded_redacted_diagnostic(&error.to_string()),
        })?;
        reject_reparse_ancestors(output_parent, "output")?;
        let trusted_output_authority =
            preview_capture::CaptureOutputAuthority::new(&output_path, &trusted_output_root)
                .map_err(|error| PreviewError::OutputFailed {
                    reason: preview_capture::bounded_redacted_diagnostic(&error.to_string()),
                })?;
        Ok(Self {
            fixture_path,
            output_path,
            trusted_output_authority: Arc::new(trusted_output_authority),
        })
    }
}

pub fn parse_preview_args<I, S>(
    args: I,
    policy: &PreviewPathPolicy,
) -> Result<PreviewRequest, PreviewError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut args = args.into_iter().map(Into::into);
    let mut fixture = None;
    let mut output = None;
    let mut saw_argument = false;

    while let Some(argument) = args.next() {
        saw_argument = true;
        match argument.to_string_lossy().as_ref() {
            "--ui-preview" => {
                if fixture.is_some() {
                    return Err(PreviewError::Usage(
                        "--ui-preview may be supplied only once".to_string(),
                    ));
                }
                fixture = Some(PathBuf::from(args.next().ok_or_else(|| {
                    PreviewError::Usage("--ui-preview requires a fixture path".to_string())
                })?));
            }
            "--output" => {
                if output.is_some() {
                    return Err(PreviewError::Usage(
                        "--output may be supplied only once".to_string(),
                    ));
                }
                output = Some(PathBuf::from(args.next().ok_or_else(|| {
                    PreviewError::Usage("--output requires a PNG path".to_string())
                })?));
            }
            other => {
                let _ = other;
                return Err(PreviewError::Usage("unknown preview argument".to_string()));
            }
        }
    }

    if !saw_argument {
        return Err(PreviewError::Usage(PREVIEW_USAGE.to_string()));
    }

    let fixture = fixture.ok_or_else(|| PreviewError::Usage(PREVIEW_USAGE.to_string()))?;
    let output = output.ok_or_else(|| PreviewError::Usage(PREVIEW_USAGE.to_string()))?;
    PreviewRequest::validate(fixture, output, policy)
}

#[derive(Clone, PartialEq, Eq)]
pub struct PreviewResources {
    pub asset_paths: Vec<String>,
    pub font_families: Vec<String>,
    pub action_ids: Vec<String>,
}

impl PreviewResources {
    fn new() -> Self {
        let font = terminal_font();
        Self {
            asset_paths: vec!["icons/sparkles.svg".to_string()],
            font_families: vec![font.family.to_string()],
            action_ids: action::catalog()
                .iter()
                .map(|descriptor| descriptor.id.to_string())
                .collect(),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PreviewRootSnapshot {
    pub fixture_id: String,
    pub title: String,
    pub body: String,
    pub component_gallery: Option<ComponentGalleryFixture>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewInitReport {
    pub component_init_count: usize,
    pub assets_registered: bool,
    pub fonts_registered: bool,
    pub actions_registered: bool,
    pub root_constructed: bool,
    pub production_host_started: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewOutputCapability {
    HeadlessProjectionOnly,
    VisibleWindowsNativeCapture,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewOutputMetadata {
    pub schema: String,
    pub fixture_id: String,
    pub output_path: PathBuf,
    pub format: String,
    pub capability: PreviewOutputCapability,
    pub output_written: bool,
    pub host_started: bool,
}

// Debug/Display are intentionally opaque; this type owns validated paths and sample-bearing UI data.
pub struct PreviewApplication {
    request: PreviewRequest,
    root_snapshot: PreviewRootSnapshot,
    resources: PreviewResources,
    capture: PreviewCaptureFixture,
    init_report: RefCell<Option<PreviewInitReport>>,
}

impl PreviewApplication {
    pub fn load(request: PreviewRequest, policy: &PreviewPathPolicy) -> Result<Self, PreviewError> {
        let request = PreviewRequest::validate(
            request.fixture_path.clone(),
            request.output_path.clone(),
            policy,
        )?;
        let bytes = read_fixture_bytes(&request.fixture_path)?;
        let fixture: PreviewFixture =
            serde_json::from_slice(&bytes).map_err(|error| PreviewError::MalformedFixture {
                path: request.fixture_path.clone(),
                message: error.to_string(),
            })?;

        if fixture.schema != PREVIEW_SCHEMA {
            return Err(PreviewError::UnsupportedSchema {
                path: request.fixture_path,
                schema: fixture.schema,
            });
        }
        if fixture.id.trim().is_empty()
            || fixture.id.chars().count() > 128
            || fixture.title.trim().is_empty()
            || fixture.title.chars().count() > 256
            || fixture.capture.cursor != PreviewCaptureSetting::Excluded
            || fixture.capture.border != PreviewCaptureSetting::Excluded
            || fixture.root.label.trim().is_empty()
            || fixture.root.label.chars().count() > 256
        {
            return Err(PreviewError::MalformedFixture {
                path: request.fixture_path,
                message: "fixture fields are empty, oversized, or use an unsupported root".into(),
            });
        }

        let title = crate::ui::components::interaction::redacted_bounded_text(
            "preview title",
            fixture.title,
            crate::ui::components::interaction::MAX_ACCESSIBLE_NAME_SCALARS,
            crate::ui::components::interaction::MAX_ACCESSIBLE_NAME_SCALARS * 4,
        )
        .map_err(|_| PreviewError::MalformedFixture {
            path: request.fixture_path.clone(),
            message: "preview title is empty, oversized, or unsafe".into(),
        })?;
        let root_label = crate::ui::components::interaction::redacted_bounded_text(
            "preview root label",
            fixture.root.label,
            crate::ui::components::interaction::MAX_ACCESSIBLE_NAME_SCALARS,
            crate::ui::components::interaction::MAX_ACCESSIBLE_NAME_SCALARS * 4,
        )
        .map_err(|_| PreviewError::MalformedFixture {
            path: request.fixture_path.clone(),
            message: "preview root label is empty, oversized, or unsafe".into(),
        })?;

        let component_gallery = match (fixture.root.kind.as_str(), fixture.root.gallery) {
            ("minimal", None) => None,
            ("minimal", Some(_)) => {
                return Err(PreviewError::MalformedFixture {
                    path: request.fixture_path,
                    message: "minimal preview roots cannot carry a component gallery".into(),
                });
            }
            ("component_gallery", Some(gallery)) => {
                let mut gallery = gallery;
                gallery
                    .validate()
                    .map_err(|message| PreviewError::MalformedFixture {
                        path: request.fixture_path.clone(),
                        message,
                    })?;
                gallery
                    .sanitize()
                    .map_err(|message| PreviewError::MalformedFixture {
                        path: request.fixture_path.clone(),
                        message,
                    })?;
                Some(gallery)
            }
            ("component_gallery", None) => {
                return Err(PreviewError::MalformedFixture {
                    path: request.fixture_path,
                    message: "component gallery roots must carry gallery data".into(),
                });
            }
            _ => {
                return Err(PreviewError::MalformedFixture {
                    path: request.fixture_path,
                    message: "fixture root kind is unsupported".into(),
                });
            }
        };

        let root_snapshot = PreviewRootSnapshot {
            fixture_id: fixture.id,
            body: format!("{root_label}: {title}"),
            title,
            component_gallery,
        };
        Ok(Self {
            request,
            root_snapshot,
            resources: PreviewResources::new(),
            capture: fixture.capture,
            init_report: RefCell::new(None),
        })
    }

    pub fn root_snapshot(&self) -> &PreviewRootSnapshot {
        &self.root_snapshot
    }

    pub fn resources(&self) -> &PreviewResources {
        &self.resources
    }

    pub fn capture_cursor(&self) -> PreviewCaptureSetting {
        self.capture.cursor
    }

    pub fn capture_border(&self) -> PreviewCaptureSetting {
        self.capture.border
    }

    pub fn component_gallery(&self) -> Option<&ComponentGalleryFixture> {
        self.root_snapshot.component_gallery.as_ref()
    }

    pub fn output_metadata(&self) -> PreviewOutputMetadata {
        PreviewOutputMetadata {
            schema: PREVIEW_SCHEMA.to_string(),
            fixture_id: self.root_snapshot.fixture_id.clone(),
            output_path: self.request.output_path.clone(),
            format: "png".to_string(),
            capability: if cfg!(windows) {
                PreviewOutputCapability::VisibleWindowsNativeCapture
            } else {
                PreviewOutputCapability::HeadlessProjectionOnly
            },
            output_written: false,
            host_started: false,
        }
    }

    pub fn root(&self) -> PreviewRoot {
        PreviewRoot::new(self.root_snapshot.clone())
    }

    pub fn initialize_headless(&self) -> Result<PreviewInitReport, PreviewError> {
        if let Some(report) = self.init_report.borrow().clone() {
            return Ok(report);
        }

        let root_snapshot = self.root_snapshot.clone();
        let report = Rc::new(RefCell::new(None));
        let report_slot = Rc::clone(&report);
        let application = gpui::Application::headless().with_assets(AppAssets::new());
        application.run(move |cx| {
            register_preview_environment(cx);

            let assets_registered = cx
                .asset_source()
                .list("icons")
                .map(|paths| paths.iter().any(|path| path.as_ref() == "sparkles.svg"))
                .unwrap_or(false);
            let fonts_registered = {
                let font = terminal_font();
                let _ = cx.text_system().resolve_font(&font);
                true
            };
            let actions_registered = TASK_COCKPIT_ACTION_NAMES
                .iter()
                .all(|name| cx.all_action_names().contains(name))
                && cx
                    .all_action_names()
                    .contains(&PreviewDismiss::name_for_type());
            let root = PreviewRoot::new(root_snapshot);
            let _root_element = root.element();
            let after = crate::ui::component_init_count();

            *report_slot.borrow_mut() = Some(PreviewInitReport {
                component_init_count: after,
                assets_registered,
                fonts_registered,
                actions_registered,
                root_constructed: true,
                production_host_started: false,
            });
            cx.quit();
        });

        let result = report
            .borrow_mut()
            .take()
            .ok_or_else(|| PreviewError::HeadlessInitializationFailed);
        if let Ok(report) = &result {
            *self.init_report.borrow_mut() = Some(report.clone());
        }
        result
    }

    pub fn render_to_output(&self) -> Result<(), PreviewError> {
        preview_capture::capture_preview(self.root(), &self.request)
            .map(|_| ())
            .map_err(|error| PreviewError::from_capture_error(error, self.request.output_path()))
    }
}

pub(crate) fn register_preview_environment(cx: &mut gpui::App) {
    crate::ui::init(cx);
    cx.bind_keys([
        KeyBinding::new("ctrl-alt-1", HostActions, None),
        KeyBinding::new("ctrl-alt-2", HostStatus, None),
        KeyBinding::new("ctrl-alt-3", TaskList, None),
        KeyBinding::new("ctrl-alt-4", TaskShow, None),
        KeyBinding::new("ctrl-alt-5", TaskCreate, None),
        KeyBinding::new("ctrl-alt-6", TaskRename, None),
        KeyBinding::new("escape", PreviewDismiss, None),
    ]);
}

#[derive(Clone)]
pub struct PreviewRoot {
    snapshot: PreviewRootSnapshot,
}

impl PreviewRoot {
    fn new(snapshot: PreviewRootSnapshot) -> Self {
        Self { snapshot }
    }

    pub fn element(&self) -> impl IntoElement {
        let gallery = self
            .snapshot
            .component_gallery
            .as_ref()
            .map(render_component_gallery);
        let mut root = div()
            .size_full()
            .p(px(16.0))
            .bg(gpui::rgb(crate::ui::tokens::PREVIEW_BACKGROUND.to_u32()))
            .text_color(gpui::rgb(crate::ui::tokens::PREVIEW_FOREGROUND.to_u32()))
            .on_action::<PreviewDismiss>(|_, _, cx: &mut gpui::App| cx.quit())
            .child(
                div()
                    .flex_none()
                    .size(px(PREVIEW_SENTINEL_SIZE))
                    .bg(gpui::rgb(PREVIEW_SENTINEL_RGB)),
            )
            .child(self.snapshot.body.clone());
        if let Some(gallery) = gallery {
            root = root.child(gallery);
        }
        root
    }
}

fn render_component_gallery(gallery: &ComponentGalleryFixture) -> gpui::AnyElement {
    // This projection deliberately calls the production component elements;
    // no gallery-only hand-styled controls are allowed to hide state drift.
    let mut surface =
        div()
            .mt(px(12.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(div().text_sm().child(format!(
                "Component gallery · {} themes · {} densities · {} scales",
                gallery.themes.len(),
                gallery.densities.len(),
                gallery.scales.len()
            )));

    for &gallery_theme in &gallery.themes {
        for &gallery_density in &gallery.densities {
            for &percent in &gallery.scales {
                let Some(scale) = gallery_scale(percent) else {
                    continue;
                };
                let tokens = theme(
                    match gallery_theme {
                        GalleryTheme::Dark => ThemeMode::Dark,
                        GalleryTheme::Light => ThemeMode::Light,
                    },
                    match gallery_density {
                        GalleryDensity::Compact => Density::Compact,
                        GalleryDensity::Comfortable => Density::Comfortable,
                    },
                    scale,
                );
                let mut row = div()
                    .flex()
                    .items_center()
                    .gap(px(tokens.density.spacing.xs))
                    .child(div().text_xs().child(format!(
                        "{} / {} / {}%",
                        gallery_theme_name(gallery_theme),
                        gallery_density_name(gallery_density),
                        percent
                    )));
                for &state in &gallery.states {
                    if state == GalleryState::Status {
                        for meaning in [
                            StatusMeaning::External,
                            StatusMeaning::Attention,
                            StatusMeaning::Success,
                            StatusMeaning::Warning,
                            StatusMeaning::Destructive,
                            StatusMeaning::Inactive,
                        ] {
                            row = row.child(render_gallery_status(tokens, meaning));
                        }
                    } else {
                        row = row
                            .child(render_gallery_button(tokens, state))
                            .child(render_gallery_icon_button(tokens, state));
                    }
                }
                surface = surface.child(row);
            }
        }
    }

    let sample_rows = [
        ("unicode", gallery.samples.unicode.clone()),
        ("long text", gallery.samples.long_text.clone()),
        ("missing", gallery.samples.missing.clone()),
        ("error", gallery.samples.error.clone()),
        ("loading", gallery.samples.loading.clone()),
        ("empty", gallery.samples.empty.clone()),
        ("overflow", gallery.samples.overflow.clone()),
    ];
    for (name, sample) in sample_rows {
        surface = surface.child(
            div()
                .flex()
                .gap(px(4.0))
                .child(div().text_xs().child(format!("{name}:")))
                .child(div().text_xs().child(sample)),
        );
    }
    surface.into_any_element()
}

fn gallery_scale(percent: u16) -> Option<Scale> {
    match percent {
        100 => Some(Scale::Scale100),
        125 => Some(Scale::Scale125),
        150 => Some(Scale::Scale150),
        200 => Some(Scale::Scale200),
        _ => None,
    }
}

fn gallery_theme_name(theme: GalleryTheme) -> &'static str {
    match theme {
        GalleryTheme::Dark => "dark",
        GalleryTheme::Light => "light",
    }
}

fn gallery_density_name(density: GalleryDensity) -> &'static str {
    match density {
        GalleryDensity::Compact => "compact",
        GalleryDensity::Comfortable => "comfortable",
    }
}

fn render_gallery_button(
    tokens: crate::ui::tokens::ThemeTokens,
    state: GalleryState,
) -> gpui::AnyElement {
    let variant = if state == GalleryState::Destructive {
        ButtonVariant::Destructive
    } else {
        ButtonVariant::Primary
    };
    let mut button = Button::new_variant("Inspect task", variant, action::ActionRequest::TaskList)
        .expect("validated gallery button");
    match state {
        GalleryState::Hover => {
            let _ = button.set_hovered(true);
        }
        GalleryState::Pressed | GalleryState::Selected => button.set_pressed_for_preview(),
        GalleryState::Focused => {
            let _ = button.focus();
        }
        GalleryState::Disabled => {
            let _ = button.disable("Disabled for gallery coverage");
        }
        GalleryState::Loading => {
            let _ = button.set_loading(true);
        }
        GalleryState::Destructive => button.set_destructive_for_preview(),
        GalleryState::Default | GalleryState::Status => {}
    }
    debug_assert!(!button.accessibility().name().is_empty());
    button.element(tokens).into_any_element()
}

fn render_gallery_icon_button(
    tokens: crate::ui::tokens::ThemeTokens,
    state: GalleryState,
) -> gpui::AnyElement {
    let mut icon_button = IconButton::new(
        IconId::Sparkles,
        "Open provider",
        TooltipContract::new("Open provider", 500).expect("validated gallery tooltip"),
        action::ActionRequest::TaskList,
    )
    .expect("validated gallery icon button");
    match state {
        GalleryState::Hover => {
            let _ = icon_button.set_hovered_for_preview(true);
        }
        GalleryState::Pressed | GalleryState::Selected => icon_button.set_pressed_for_preview(),
        GalleryState::Focused => {
            let _ = icon_button.focus();
        }
        GalleryState::Disabled => {
            let _ = icon_button.disable("Disabled for gallery coverage");
        }
        GalleryState::Loading => {
            let _ = icon_button.set_loading(true);
        }
        GalleryState::Default | GalleryState::Destructive | GalleryState::Status => {}
    }
    debug_assert!(!icon_button.accessibility().name().is_empty());
    debug_assert!(!icon_button.tooltip().label.is_empty());
    icon_button.element(tokens).into_any_element()
}

fn render_gallery_status(
    tokens: crate::ui::tokens::ThemeTokens,
    meaning: StatusMeaning,
) -> gpui::AnyElement {
    let status = StatusLight::new(meaning, "Worker status", "Semantic worker status")
        .expect("validated gallery status");
    debug_assert!(!status.accessibility().name().is_empty());
    debug_assert!(!status.description().is_empty());
    status.element(tokens).into_any_element()
}

macro_rules! opaque_preview_format {
    ($type:ty, $label:literal) => {
        impl std::fmt::Debug for $type {
            fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
                formatter.write_str($label)
            }
        }

        impl Display for $type {
            fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
                formatter.write_str($label)
            }
        }
    };
}

opaque_preview_format!(PreviewFixture, "PreviewFixture(<opaque>)");
opaque_preview_format!(PreviewRootFixture, "PreviewRootFixture(<opaque>)");
opaque_preview_format!(ComponentGallerySamples, "ComponentGallerySamples(<opaque>)");
opaque_preview_format!(ComponentGalleryFixture, "ComponentGalleryFixture(<opaque>)");
opaque_preview_format!(PreviewPathPolicy, "PreviewPathPolicy(<opaque>)");
opaque_preview_format!(PreviewRequest, "PreviewRequest(<opaque>)");
opaque_preview_format!(PreviewResources, "PreviewResources(<opaque>)");
opaque_preview_format!(PreviewRootSnapshot, "PreviewRootSnapshot(<opaque>)");
opaque_preview_format!(PreviewOutputMetadata, "PreviewOutputMetadata(<opaque>)");
opaque_preview_format!(PreviewApplication, "PreviewApplication(<opaque>)");
opaque_preview_format!(PreviewRoot, "PreviewRoot(<opaque>)");

impl Render for PreviewRoot {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.element()
    }
}

pub fn run_cli<I, S>(args: I, policy: &PreviewPathPolicy) -> Result<(), PreviewError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let request = parse_preview_args(args, policy)?;
    let preview = PreviewApplication::load(request, policy)?;
    preview.render_to_output()
}

fn absolute_path(path: &Path) -> Result<PathBuf, PreviewError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|current| current.join(path))
        .map_err(|error| PreviewError::InvalidArgument(error.to_string()))
}

fn read_fixture_bytes(path: &Path) -> Result<Vec<u8>, PreviewError> {
    reject_reparse_ancestors(path, "fixture")?;
    let metadata = fs::metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            PreviewError::FixtureMissing {
                path: path.to_path_buf(),
            }
        } else {
            PreviewError::FixtureIo {
                path: path.to_path_buf(),
                message: error.to_string(),
            }
        }
    })?;
    if !metadata.is_file() {
        return Err(PreviewError::FixtureNotRegular {
            path: path.to_path_buf(),
        });
    }
    if metadata.len() > MAX_FIXTURE_BYTES {
        return Err(PreviewError::FixtureTooLarge {
            path: path.to_path_buf(),
            bytes: metadata.len(),
            max_bytes: MAX_FIXTURE_BYTES,
        });
    }

    let bytes = fs::read(path).map_err(|error| PreviewError::FixtureIo {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    if bytes.len() as u64 > MAX_FIXTURE_BYTES {
        return Err(PreviewError::FixtureTooLarge {
            path: path.to_path_buf(),
            bytes: bytes.len() as u64,
            max_bytes: MAX_FIXTURE_BYTES,
        });
    }
    Ok(bytes)
}

fn reject_reparse_ancestors(path: &Path, root_kind: &'static str) -> Result<(), PreviewError> {
    let absolute = absolute_path(path)?;
    let mut current = absolute.clone();
    loop {
        if current.exists() {
            let metadata =
                fs::symlink_metadata(&current).map_err(|error| PreviewError::FixtureIo {
                    path: absolute.clone(),
                    message: error.to_string(),
                })?;
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes()
                    & windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT.0
                    != 0
                {
                    return Err(PreviewError::UnsafePath {
                        path: absolute,
                        root_kind,
                    });
                }
            }
            #[cfg(not(windows))]
            if metadata.file_type().is_symlink() {
                return Err(PreviewError::UnsafePath {
                    path: absolute,
                    root_kind,
                });
            }
        }
        let Some(parent) = current.parent() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    Ok(())
}

fn checked_path(path: &Path) -> Result<PathBuf, PreviewError> {
    let absolute = absolute_path(path)?;
    let mut suffix = Vec::new();
    let mut existing = absolute.clone();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            return Ok(absolute);
        };
        suffix.push(name.to_os_string());
        let Some(parent) = existing.parent() else {
            return Ok(absolute);
        };
        existing = parent.to_path_buf();
    }

    let mut checked = fs::canonicalize(existing).map_err(|error| PreviewError::FixtureIo {
        path: absolute.clone(),
        message: error.to_string(),
    })?;
    for component in suffix.iter().rev() {
        checked.push(component);
    }
    Ok(checked)
}

fn is_within(path: &Path, root: &Path) -> bool {
    if path.starts_with(root) {
        return true;
    }
    #[cfg(windows)]
    {
        let path = path.to_string_lossy().to_ascii_lowercase();
        let root = root.to_string_lossy().to_ascii_lowercase();
        let root = root.trim_end_matches(['\\', '/']);
        return path == root
            || path
                .strip_prefix(root)
                .is_some_and(|suffix| suffix.starts_with(['\\', '/']));
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn is_sensitive_path(path: &Path) -> bool {
    let sensitive_components = [
        "appdata",
        "programdata",
        "program files",
        "program files (x86)",
        "install",
        "installed",
        "profile",
        "profiles",
        "com.userfirst.devmanager",
    ];
    let sensitive_names = [
        "config.json",
        "remote.json",
        "session.json",
        "devmanager.exe",
        "devmanager-host.exe",
    ];
    let is_windows_temp_path = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().to_ascii_lowercase()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .windows(3)
        .any(|window| window == ["appdata", "local", "temp"]);
    path.components().any(|component| {
        let Component::Normal(value) = component else {
            return false;
        };
        let value = value.to_string_lossy().to_ascii_lowercase();
        (sensitive_components.contains(&value.as_str())
            && !(value == "appdata" && is_windows_temp_path))
            || sensitive_names.contains(&value.as_str())
    })
}

fn safe_preview_path(path: &Path) -> String {
    let _ = path;
    "<approved preview path>".to_string()
}

fn bounded_preview_error(message: String) -> String {
    preview_capture::bounded_redacted_diagnostic(&message)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureUnavailableKind {
    UnsupportedPlatform,
    InvalidHwnd,
    ForeignHwnd,
    InvalidWindowState { reason: &'static str },
    DeadlineExceeded,
    CaptureClosed,
}

#[derive(Clone, PartialEq, Eq)]
pub enum PreviewError {
    Usage(String),
    InvalidArgument(String),
    OutsideApprovedRoot {
        path: PathBuf,
        root_kind: &'static str,
    },
    SensitivePath {
        path: PathBuf,
    },
    UnsafePath {
        path: PathBuf,
        root_kind: &'static str,
    },
    FixtureMissing {
        path: PathBuf,
    },
    FixtureNotRegular {
        path: PathBuf,
    },
    FixtureTooLarge {
        path: PathBuf,
        bytes: u64,
        max_bytes: u64,
    },
    FixtureIo {
        path: PathBuf,
        message: String,
    },
    MalformedFixture {
        path: PathBuf,
        message: String,
    },
    UnsupportedSchema {
        path: PathBuf,
        schema: String,
    },
    OutputAlreadyExists {
        path: PathBuf,
    },
    HeadlessInitializationFailed,
    VisibleWindowsCaptureUnavailable {
        kind: CaptureUnavailableKind,
        reason: String,
    },
    PngFailed {
        reason: String,
    },
    OutputFailed {
        reason: String,
    },
    ForegroundChanged {
        before: isize,
        after: isize,
    },
    ApplicationFailed {
        reason: String,
    },
    WindowsGraphicsCaptureFailed {
        reason: String,
    },
    CaptureCleanupFailed {
        primary: Box<Self>,
        operation: &'static str,
        reason: String,
    },
}

impl PreviewError {
    pub fn from_capture_error(
        error: preview_capture::PreviewCaptureError,
        output_path: &Path,
    ) -> Self {
        Self::from_capture_error_at_depth(&error, output_path, 0)
    }

    fn from_capture_error_at_depth(
        error: &preview_capture::PreviewCaptureError,
        output_path: &Path,
        depth: usize,
    ) -> Self {
        let reason = error.to_string();
        match error {
            preview_capture::PreviewCaptureError::UnsupportedPlatform => {
                Self::VisibleWindowsCaptureUnavailable {
                    kind: CaptureUnavailableKind::UnsupportedPlatform,
                    reason,
                }
            }
            preview_capture::PreviewCaptureError::InvalidHwnd => {
                Self::VisibleWindowsCaptureUnavailable {
                    kind: CaptureUnavailableKind::InvalidHwnd,
                    reason,
                }
            }
            preview_capture::PreviewCaptureError::ForeignHwnd => {
                Self::VisibleWindowsCaptureUnavailable {
                    kind: CaptureUnavailableKind::ForeignHwnd,
                    reason,
                }
            }
            preview_capture::PreviewCaptureError::InvalidWindowState {
                reason: window_reason,
            } => Self::VisibleWindowsCaptureUnavailable {
                kind: CaptureUnavailableKind::InvalidWindowState {
                    reason: *window_reason,
                },
                reason,
            },
            preview_capture::PreviewCaptureError::DeadlineExceeded => {
                Self::VisibleWindowsCaptureUnavailable {
                    kind: CaptureUnavailableKind::DeadlineExceeded,
                    reason,
                }
            }
            preview_capture::PreviewCaptureError::CaptureCancelled => {
                Self::VisibleWindowsCaptureUnavailable {
                    kind: CaptureUnavailableKind::CaptureClosed,
                    reason,
                }
            }
            preview_capture::PreviewCaptureError::CaptureClosed => {
                Self::VisibleWindowsCaptureUnavailable {
                    kind: CaptureUnavailableKind::CaptureClosed,
                    reason,
                }
            }
            preview_capture::PreviewCaptureError::CaptureFailed(message) => {
                Self::WindowsGraphicsCaptureFailed {
                    reason: preview_capture::bounded_redacted_diagnostic(message),
                }
            }
            preview_capture::PreviewCaptureError::ApplicationFailed(message) => {
                Self::ApplicationFailed {
                    reason: preview_capture::bounded_redacted_diagnostic(message),
                }
            }
            preview_capture::PreviewCaptureError::PngFailed(message) => Self::PngFailed {
                reason: preview_capture::bounded_redacted_diagnostic(message),
            },
            preview_capture::PreviewCaptureError::OutputAlreadyExists => {
                Self::OutputAlreadyExists {
                    path: output_path.to_path_buf(),
                }
            }
            preview_capture::PreviewCaptureError::OutputFailed(message) => Self::OutputFailed {
                reason: preview_capture::bounded_redacted_diagnostic(message),
            },
            preview_capture::PreviewCaptureError::ForegroundChanged { before, after } => {
                Self::ForegroundChanged {
                    before: *before,
                    after: *after,
                }
            }
            preview_capture::PreviewCaptureError::CleanupFailed(context) => {
                let primary = if depth < preview_capture::MAX_CLEANUP_DIAGNOSTIC_DEPTH {
                    Self::from_capture_error_at_depth(context.primary(), output_path, depth + 1)
                } else {
                    Self::WindowsGraphicsCaptureFailed { reason }
                };
                Self::CaptureCleanupFailed {
                    primary: Box::new(primary),
                    operation: context.operation(),
                    reason: preview_capture::bounded_redacted_diagnostic(
                        &context.secondary().to_string(),
                    ),
                }
            }
        }
    }
}

impl Display for PreviewError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage(message) => f.write_str(&bounded_preview_error(format!(
                "{message}\n{PREVIEW_USAGE}"
            ))),
            Self::InvalidArgument(message) => f.write_str(&bounded_preview_error(message.clone())),
            Self::OutsideApprovedRoot { path, root_kind } => {
                f.write_str(&bounded_preview_error(format!(
                    "{root_kind} path is outside approved roots: {}",
                    safe_preview_path(path)
                )))
            }
            Self::SensitivePath { path } => f.write_str(&bounded_preview_error(format!(
                "sensitive production path refused: {}",
                safe_preview_path(path)
            ))),
            Self::UnsafePath { path, root_kind } => f.write_str(&bounded_preview_error(format!(
                "{root_kind} path contains a reparse or symbolic-link ancestor: {}",
                safe_preview_path(path)
            ))),
            Self::FixtureMissing { path } => f.write_str(&bounded_preview_error(format!(
                "fixture does not exist: {}",
                safe_preview_path(path)
            ))),
            Self::FixtureNotRegular { path } => f.write_str(&bounded_preview_error(format!(
                "fixture is not a regular file: {}",
                safe_preview_path(path)
            ))),
            Self::FixtureTooLarge {
                path,
                bytes,
                max_bytes,
            } => f.write_str(&bounded_preview_error(format!(
                "fixture is too large ({} bytes; max {}): {}",
                bytes,
                max_bytes,
                safe_preview_path(path)
            ))),
            Self::FixtureIo { path, message } => {
                let _ = (path, message);
                f.write_str("fixture I/O failed")
            }
            Self::MalformedFixture { path, message } => {
                let _ = (path, message);
                f.write_str("malformed preview fixture")
            }
            Self::UnsupportedSchema { path, schema } => {
                let _ = (path, schema);
                f.write_str("unsupported fixture schema")
            }
            Self::OutputAlreadyExists { path } => f.write_str(&bounded_preview_error(format!(
                "refusing to overwrite existing output: {}",
                safe_preview_path(path)
            ))),
            Self::HeadlessInitializationFailed => f.write_str(&bounded_preview_error(
                "headless preview initialization did not complete".into(),
            )),
            Self::VisibleWindowsCaptureUnavailable { kind, reason } => {
                f.write_str(&bounded_preview_error(format!(
                    "visible Windows preview capture unavailable ({kind:?}): {reason}"
                )))
            }
            Self::PngFailed { reason } => f.write_str(&bounded_preview_error(format!(
                "PNG encoding failed: {reason}"
            ))),
            Self::OutputFailed { reason } => f.write_str(&bounded_preview_error(format!(
                "PNG output failed: {reason}"
            ))),
            Self::ForegroundChanged { before, after } => {
                let _ = (before, after);
                f.write_str(&bounded_preview_error(
                    "foreground window changed during capture".into(),
                ))
            }
            Self::ApplicationFailed { reason } => f.write_str(&bounded_preview_error(format!(
                "GPUI preview application failed: {reason}"
            ))),
            Self::WindowsGraphicsCaptureFailed { reason } => f.write_str(&bounded_preview_error(
                format!("Windows Graphics Capture failed: {reason}"),
            )),
            Self::CaptureCleanupFailed {
                primary,
                operation,
                reason,
            } => f.write_str(&bounded_preview_error(format!(
                "{primary}; cleanup {operation} failed: {}",
                reason
            ))),
        }
    }
}

impl std::fmt::Debug for PreviewError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreviewError")
            .field("message", &self.to_string())
            .finish()
    }
}

impl Error for PreviewError {}

//! Deterministic, isolated native UI preview contracts.

use gpui::{
    div, px, Action, AppContext, Context, InteractiveElement, IntoElement, KeyBinding,
    ParentElement, Render, Styled, Window,
};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use crate::assets::AppAssets;
use crate::client::action;
use crate::terminal::terminal_font;
use crate::ui::actions::{register_task_cockpit_bindings, TASK_COCKPIT_ACTION_NAMES};
use crate::ui::native_shell::{
    isolated_dev_profile, NativeHostBootstrap, NativeHostRuntimeAttachment, NativeShell,
    ProcessNativeHostBootstrap,
};
use crate::ui::preview_capture;
use crate::ui::tokens::{RuntimePreferencesSnapshot, PREVIEW_SENTINEL};

pub const PREVIEW_SCHEMA: &str = "devmanager.ui.preview/v1";
pub const MAX_FIXTURE_BYTES: u64 = 256 * 1024;
pub const PREVIEW_SENTINEL_RGBA: [u8; 4] = [0x91, 0x2b, 0xd4, 0xff];
const PREVIEW_SENTINEL_SIZE: f32 = 32.0;
const PREVIEW_USAGE: &str =
    "usage: devmanager-next --ui-preview <fixture.json> --output <preview.png>";

gpui::actions!(devmanager_next, [PreviewDismiss]);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentGalleryFixture {
    pub themes: Vec<GalleryTheme>,
    pub densities: Vec<GalleryDensity>,
    pub scales: Vec<u16>,
    pub states: Vec<GalleryState>,
    pub samples: ComponentGallerySamples,
}

impl ComponentGalleryFixture {
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
        ];
        if self.states.len() != required_states.len()
            || required_states.iter().any(|state| !self.states.contains(state))
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
        if !self.samples.unicode.chars().any(|character| !character.is_ascii()) {
            return Err("component gallery unicode sample must contain non-ASCII text".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
        Self::new(
            workspace_root.join("tests/fixtures/ui"),
            workspace_root.join(".devmanager-next/evidence/phase-05/screenshots"),
            std::env::temp_dir().join("devmanager-next-preview"),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewRequest {
    fixture_path: PathBuf,
    output_path: PathBuf,
}

impl PreviewRequest {
    pub fn fixture_path(&self) -> &Path {
        &self.fixture_path
    }

    pub fn output_path(&self) -> &Path {
        &self.output_path
    }

    pub fn write_bgra_png_atomic(
        &self,
        width: u32,
        height: u32,
        bgra: &[u8],
    ) -> Result<(), preview_capture::PreviewCaptureError> {
        preview_capture::encode_bgra_png_atomic(&self.output_path, width, height, bgra)
    }

    pub fn validate(
        fixture_path: impl AsRef<Path>,
        output_path: impl AsRef<Path>,
        policy: &PreviewPathPolicy,
    ) -> Result<Self, PreviewError> {
        let fixture_path = absolute_path(fixture_path.as_ref())?;
        let output_path = absolute_path(output_path.as_ref())?;
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
                fixture_path.display()
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
                output_path.display()
            )));
        }
        if output_path.exists() {
            return Err(PreviewError::OutputAlreadyExists { path: output_path });
        }

        Ok(Self {
            fixture_path,
            output_path,
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
                return Err(PreviewError::Usage(format!("unknown argument: {other}")));
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

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewRootSnapshot {
    pub fixture_id: String,
    pub root_kind: String,
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
    pub native_shell_instantiated: bool,
    pub production_host_started: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewOutputCapability {
    HeadlessProjectionOnly,
    VisibleWindowsNativeCapture,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug)]
pub struct PreviewApplication {
    request: PreviewRequest,
    workspace_root: PathBuf,
    root_snapshot: PreviewRootSnapshot,
    resources: PreviewResources,
    capture: PreviewCaptureFixture,
    init_report: RefCell<Option<PreviewInitReport>>,
    host_started: RefCell<bool>,
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
            || !matches!(
                fixture.root.kind.as_str(),
                "minimal" | "task-cockpit" | "component_gallery"
            )
            || fixture.root.label.trim().is_empty()
            || fixture.root.label.chars().count() > 256
        {
            return Err(PreviewError::MalformedFixture {
                path: request.fixture_path,
                message: "fixture fields are empty, oversized, or use an unsupported root".into(),
            });
        }

        let component_gallery = match (fixture.root.kind.as_str(), fixture.root.gallery) {
            ("component_gallery", Some(gallery)) => {
                gallery
                    .validate()
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
            ("minimal", Some(_)) | ("task-cockpit", Some(_)) => {
                return Err(PreviewError::MalformedFixture {
                    path: request.fixture_path,
                    message: "non-gallery preview roots cannot carry a component gallery".into(),
                });
            }
            (_, None) => None,
        };
        let is_task_cockpit = fixture.root.kind == "task-cockpit";
        let body = if is_task_cockpit {
            format!(
                "Task Cockpit\nHeader unavailable\nTask Inbox\nContext Dock\nHost unavailable\n{}",
                fixture.title
            )
        } else {
            format!("{}: {}", fixture.root.label, fixture.title)
        };
        let root_snapshot = PreviewRootSnapshot {
            fixture_id: fixture.id,
            root_kind: fixture.root.kind,
            body,
            title: fixture.title,
            component_gallery,
        };
        Ok(Self {
            request,
            workspace_root: preview_workspace_root(policy.fixture_root()),
            root_snapshot,
            resources: PreviewResources::new(),
            capture: fixture.capture,
            init_report: RefCell::new(None),
            host_started: RefCell::new(false),
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
            host_started: *self.host_started.borrow(),
        }
    }

    pub fn root(&self) -> PreviewRoot {
        PreviewRoot::new(self.root_snapshot.clone(), self.workspace_root.clone())
    }

    pub fn initialize_headless(&self) -> Result<PreviewInitReport, PreviewError> {
        if let Some(report) = self.init_report.borrow().clone() {
            return Ok(report);
        }

        let root_snapshot = self.root_snapshot.clone();
        let workspace_root = self.workspace_root.clone();
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
            let root = match PreviewRoot::new(root_snapshot, workspace_root)
                .instantiate_native_shell(cx)
            {
                Ok(root) => root,
                Err(_error) => {
                    cx.quit();
                    return;
                }
            };
            let native_shell_instantiated = root.has_native_shell();
            let _root_element = root.element();
            let after = crate::ui::component_init_count();

            *report_slot.borrow_mut() = Some(PreviewInitReport {
                component_init_count: after,
                assets_registered,
                fonts_registered,
                actions_registered,
                root_constructed: true,
                native_shell_instantiated,
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
            .map(|_| {
                *self.host_started.borrow_mut() = self.root_snapshot.root_kind == "task-cockpit";
            })
            .map_err(|error| PreviewError::from_capture_error(error, self.request.output_path()))
    }
}

pub(crate) fn register_preview_environment(cx: &mut gpui::App) {
    crate::ui::init(cx);
    register_task_cockpit_bindings(cx);
    cx.bind_keys([KeyBinding::new("escape", PreviewDismiss, None)]);
}

fn preview_workspace_root(fixture_root: &Path) -> PathBuf {
    let fixture_root = fixture_root.to_path_buf();
    let Some(fixtures_root) = fixture_root.parent() else {
        return fixture_root;
    };
    let Some(candidate) = fixtures_root.parent() else {
        return fixture_root;
    };
    if candidate.file_name().and_then(OsStr::to_str) == Some("tests") {
        candidate.parent().unwrap_or(candidate).to_path_buf()
    } else {
        candidate.to_path_buf()
    }
}

#[derive(Debug, Clone)]
pub struct PreviewRoot {
    snapshot: PreviewRootSnapshot,
    workspace_root: PathBuf,
    native_shell: Option<gpui::Entity<NativeShell>>,
}

impl PreviewRoot {
    fn new(snapshot: PreviewRootSnapshot, workspace_root: PathBuf) -> Self {
        Self {
            snapshot,
            workspace_root,
            native_shell: None,
        }
    }

    pub(crate) fn instantiate_native_shell(
        mut self,
        cx: &mut gpui::App,
    ) -> Result<Self, PreviewError> {
        if self.snapshot.root_kind != "task-cockpit" {
            return Ok(self);
        }
        let profile = isolated_dev_profile(&self.workspace_root).map_err(|error| {
            PreviewError::ApplicationFailed {
                reason: format!("preview native shell profile: {error}"),
            }
        })?;
        let shell = cx.new(|cx| NativeShell::new_for_headless(profile, cx));
        self.native_shell = Some(shell);
        Ok(self)
    }

    /// Visible capture owns the one real isolated host/runtime. The host
    /// attachment is moved into the shell exactly once and is dropped with
    /// the GPUI entity; no disconnected fake transport is used for capture.
    pub(crate) fn instantiate_native_shell_for_capture(
        mut self,
        cx: &mut gpui::App,
        deadline: std::time::Instant,
    ) -> Result<Self, PreviewError> {
        if self.snapshot.root_kind != "task-cockpit" {
            return Ok(self);
        }
        let profile = isolated_dev_profile(&self.workspace_root).map_err(|error| {
            PreviewError::ApplicationFailed {
                reason: format!("preview native shell profile: {error}"),
            }
        })?;
        let mut bootstrap = ProcessNativeHostBootstrap;
        let attachment = bootstrap.start_until(&profile, deadline).map_err(|error| {
            PreviewError::ApplicationFailed {
                reason: error.to_string(),
            }
        })?;
        let shell = match attachment {
            NativeHostRuntimeAttachment::Client(runtime) => {
                cx.new(|cx| NativeShell::new_with_host_runtime(profile, Some(runtime), cx))
            }
            NativeHostRuntimeAttachment::Injected(runtime) => cx.new(|cx| {
                NativeShell::new_with_host_runtime_port(
                    profile,
                    runtime,
                    RuntimePreferencesSnapshot::default(),
                    cx,
                )
            }),
        };
        self.native_shell = Some(shell);
        Ok(self)
    }

    pub(crate) fn install_window_observers(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(shell) = self.native_shell.clone() {
            let _ = shell.update(cx, |shell, cx| {
                shell.install_window_observers(window, cx);
            });
        }
    }

    fn has_native_shell(&self) -> bool {
        self.native_shell.is_some()
    }

    pub fn element(&self) -> impl IntoElement {
        let tokens = RuntimePreferencesSnapshot::default().tokens();
        let shell = if let Some(native_shell) = self.native_shell.clone() {
            div()
                .id("preview-task-cockpit")
                .size_full()
                .child(native_shell)
                .into_any_element()
        } else if self.snapshot.root_kind == "task-cockpit" {
            div()
                .id("preview-task-cockpit")
                .flex_col()
                .gap(px(12.0))
                .child(div().id("preview-shell-title").child("Task Cockpit"))
                .child(div().id("preview-header").child("Header unavailable"))
                .child(div().id("preview-inbox").child("Task Inbox"))
                .child(div().id("preview-dock").child("Context Dock"))
                .child(div().id("preview-host-state").child("Host unavailable"))
                .into_any_element()
        } else {
            div().child(self.snapshot.body.clone()).into_any_element()
        };
        let preview = div()
            .size_full()
            .p(px(16.0))
            .bg(tokens.surfaces.canvas.to_gpui())
            .text_color(tokens.text.primary.to_gpui())
            .on_action::<PreviewDismiss>(|_, _, cx: &mut gpui::App| cx.quit())
            .child(shell);
        if self.native_shell.is_none() {
            preview.child(
                div()
                    .flex_none()
                    .size(px(PREVIEW_SENTINEL_SIZE))
                    .bg(PREVIEW_SENTINEL.to_gpui()),
            )
        } else {
            preview
        }
    }
}

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

pub(crate) fn is_within(path: &Path, root: &Path) -> bool {
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

pub(crate) fn is_sensitive_path(path: &Path) -> bool {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureUnavailableKind {
    UnsupportedPlatform,
    InvalidHwnd,
    ForeignHwnd,
    InvalidWindowState { reason: &'static str },
    DeadlineExceeded,
    CaptureClosed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
            preview_capture::PreviewCaptureError::CaptureClosed => {
                Self::VisibleWindowsCaptureUnavailable {
                    kind: CaptureUnavailableKind::CaptureClosed,
                    reason,
                }
            }
            preview_capture::PreviewCaptureError::CaptureFailed(message) => {
                Self::WindowsGraphicsCaptureFailed {
                    reason: message.to_owned(),
                }
            }
            preview_capture::PreviewCaptureError::ApplicationFailed(message) => {
                Self::ApplicationFailed {
                    reason: message.to_owned(),
                }
            }
            preview_capture::PreviewCaptureError::PngFailed(message) => Self::PngFailed {
                reason: message.to_owned(),
            },
            preview_capture::PreviewCaptureError::OutputAlreadyExists => {
                Self::OutputAlreadyExists {
                    path: output_path.to_path_buf(),
                }
            }
            preview_capture::PreviewCaptureError::OutputFailed(message) => Self::OutputFailed {
                reason: message.to_owned(),
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
                    reason: context.secondary().to_string(),
                }
            }
        }
    }
}

impl Display for PreviewError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage(message) => write!(f, "{message}\n{PREVIEW_USAGE}"),
            Self::InvalidArgument(message) => f.write_str(message),
            Self::OutsideApprovedRoot { path, root_kind } => {
                write!(
                    f,
                    "{root_kind} path is outside approved roots: {}",
                    path.display()
                )
            }
            Self::SensitivePath { path } => {
                write!(f, "sensitive production path refused: {}", path.display())
            }
            Self::FixtureMissing { path } => {
                write!(f, "fixture does not exist: {}", path.display())
            }
            Self::FixtureNotRegular { path } => {
                write!(f, "fixture is not a regular file: {}", path.display())
            }
            Self::FixtureTooLarge {
                path,
                bytes,
                max_bytes,
            } => write!(
                f,
                "fixture is too large ({} bytes; max {}): {}",
                bytes,
                max_bytes,
                path.display()
            ),
            Self::FixtureIo { path, message } => {
                write!(f, "fixture I/O failed for {}: {message}", path.display())
            }
            Self::MalformedFixture { path, message } => {
                write!(f, "malformed fixture {}: {message}", path.display())
            }
            Self::UnsupportedSchema { path, schema } => write!(
                f,
                "unsupported fixture schema {schema} in {}",
                path.display()
            ),
            Self::OutputAlreadyExists { path } => write!(
                f,
                "refusing to overwrite existing output: {}",
                path.display()
            ),
            Self::HeadlessInitializationFailed => {
                f.write_str("headless preview initialization did not complete")
            }
            Self::VisibleWindowsCaptureUnavailable { kind, reason } => {
                write!(
                    f,
                    "visible Windows preview capture unavailable ({kind:?}): {reason}"
                )
            }
            Self::PngFailed { reason } => write!(f, "PNG encoding failed: {reason}"),
            Self::OutputFailed { reason } => write!(f, "PNG output failed: {reason}"),
            Self::ForegroundChanged { before, after } => write!(
                f,
                "foreground HWND changed during capture (before {before:#x}, after {after:#x})"
            ),
            Self::ApplicationFailed { reason } => {
                write!(f, "GPUI preview application failed: {reason}")
            }
            Self::WindowsGraphicsCaptureFailed { reason } => {
                write!(f, "Windows Graphics Capture failed: {reason}")
            }
            Self::CaptureCleanupFailed {
                primary,
                operation,
                reason,
            } => {
                write!(f, "{primary}; cleanup {operation} failed: {reason}")
            }
        }
    }
}

impl Error for PreviewError {}

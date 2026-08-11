//! Deterministic, isolated native UI preview contracts.

use gpui::{
    div, px, Action, Context, InteractiveElement, IntoElement, KeyBinding, ParentElement, Render,
    Styled, Window,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt::{Display, Formatter};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
const PREVIEW_PADDING_PX: f32 = 16.0;
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

/// One deterministic gallery capture page. A single page contains one
/// theme/density/scale tuple and all reusable interaction states, so the
/// 640x360 native surface never has to render sixteen wide rows at once.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct GalleryPage {
    pub theme: GalleryTheme,
    pub density: GalleryDensity,
    pub scale: u16,
    pub state_page: u8,
    pub status_page: u8,
    pub sample_page: u8,
    pub section: GalleryPageSection,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GalleryPageSection {
    States,
    Status,
    Samples,
}

impl Default for GalleryPage {
    fn default() -> Self {
        Self {
            theme: GalleryTheme::Dark,
            density: GalleryDensity::Compact,
            scale: 100,
            state_page: 0,
            status_page: 0,
            sample_page: 0,
            section: GalleryPageSection::States,
        }
    }
}

pub const GALLERY_PAGE_COLUMNS: u16 = 1;
pub const GALLERY_PAGE_ROWS: u16 = 3;
const GALLERY_PAGE_GAP_PX: f32 = 8.0;
const GALLERY_CONTENT_WIDTH_PX: f32 = 608.0;
const GALLERY_SAMPLE_VALUE_WIDTH_PX: f32 = 400.0;

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
    gallery_page: Option<GalleryPage>,
    window_hold_ms: u32,
    fixture_authority: Arc<FixtureFileAuthority>,
    trusted_output_authority: Arc<preview_capture::CaptureOutputAuthority>,
}

impl PartialEq for PreviewRequest {
    fn eq(&self, other: &Self) -> bool {
        self.fixture_path == other.fixture_path
            && self.output_path == other.output_path
            && self.gallery_page == other.gallery_page
            && self.window_hold_ms == other.window_hold_ms
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

    pub fn gallery_page(&self) -> Option<GalleryPage> {
        self.gallery_page
    }

    pub(crate) fn window_hold_ms(&self) -> u32 {
        self.window_hold_ms
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
        let fixture_authority = open_fixture_authority(&fixture_path, &fixture_root)?;

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
            gallery_page: None,
            window_hold_ms: 0,
            fixture_authority,
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
    let mut gallery_theme = None;
    let mut gallery_density = None;
    let mut gallery_scale = None;
    let mut gallery_state_page = None;
    let mut gallery_status_page = None;
    let mut gallery_sample_page = None;
    let mut gallery_section = None;
    let mut window_hold_ms = None;
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
            "--theme" => {
                if gallery_theme.is_some() {
                    return Err(PreviewError::Usage(
                        "--theme may be supplied only once".to_string(),
                    ));
                }
                let value = args.next().ok_or_else(|| {
                    PreviewError::Usage("--theme requires dark or light".to_string())
                })?;
                gallery_theme = Some(match value.to_string_lossy().as_ref() {
                    "dark" => GalleryTheme::Dark,
                    "light" => GalleryTheme::Light,
                    _ => {
                        return Err(PreviewError::Usage(
                            "--theme requires dark or light".to_string(),
                        ));
                    }
                });
            }
            "--density" => {
                if gallery_density.is_some() {
                    return Err(PreviewError::Usage(
                        "--density may be supplied only once".to_string(),
                    ));
                }
                let value = args.next().ok_or_else(|| {
                    PreviewError::Usage("--density requires compact or comfortable".to_string())
                })?;
                gallery_density = Some(match value.to_string_lossy().as_ref() {
                    "compact" => GalleryDensity::Compact,
                    "comfortable" => GalleryDensity::Comfortable,
                    _ => {
                        return Err(PreviewError::Usage(
                            "--density requires compact or comfortable".to_string(),
                        ));
                    }
                });
            }
            "--scale" => {
                if gallery_scale.is_some() {
                    return Err(PreviewError::Usage(
                        "--scale may be supplied only once".to_string(),
                    ));
                }
                let value = args.next().ok_or_else(|| {
                    PreviewError::Usage("--scale requires 100, 125, 150, or 200".to_string())
                })?;
                gallery_scale = Some(value.to_string_lossy().parse::<u16>().map_err(|_| {
                    PreviewError::Usage("--scale requires 100, 125, 150, or 200".to_string())
                })?);
            }
            "--state-page" => {
                if gallery_state_page.is_some() {
                    return Err(PreviewError::Usage(
                        "--state-page may be supplied only once".to_string(),
                    ));
                }
                let value = args.next().ok_or_else(|| {
                    PreviewError::Usage("--state-page requires 0, 1, or 2".to_string())
                })?;
                let value = value.to_string_lossy().parse::<u8>().map_err(|_| {
                    PreviewError::Usage("--state-page requires 0, 1, or 2".to_string())
                })?;
                if value >= 3 {
                    return Err(PreviewError::Usage(
                        "--state-page requires 0, 1, or 2".to_string(),
                    ));
                }
                gallery_state_page = Some(value);
            }
            "--status-page" => {
                if gallery_status_page.is_some() {
                    return Err(PreviewError::Usage(
                        "--status-page may be supplied only once".to_string(),
                    ));
                }
                let value = args.next().ok_or_else(|| {
                    PreviewError::Usage("--status-page requires 0 or 1".to_string())
                })?;
                let value = value.to_string_lossy().parse::<u8>().map_err(|_| {
                    PreviewError::Usage("--status-page requires 0 or 1".to_string())
                })?;
                if value >= 2 {
                    return Err(PreviewError::Usage(
                        "--status-page requires 0 or 1".to_string(),
                    ));
                }
                gallery_status_page = Some(value);
            }
            "--sample-page" => {
                if gallery_sample_page.is_some() {
                    return Err(PreviewError::Usage(
                        "--sample-page may be supplied only once".to_string(),
                    ));
                }
                let value = args.next().ok_or_else(|| {
                    PreviewError::Usage("--sample-page requires 0 or 1".to_string())
                })?;
                let value = value.to_string_lossy().parse::<u8>().map_err(|_| {
                    PreviewError::Usage("--sample-page requires 0 or 1".to_string())
                })?;
                if value >= 2 {
                    return Err(PreviewError::Usage(
                        "--sample-page requires 0 or 1".to_string(),
                    ));
                }
                gallery_sample_page = Some(value);
            }
            "--section" => {
                if gallery_section.is_some() {
                    return Err(PreviewError::Usage(
                        "--section may be supplied only once".to_string(),
                    ));
                }
                let value = args.next().ok_or_else(|| {
                    PreviewError::Usage("--section requires states or samples".to_string())
                })?;
                gallery_section = Some(match value.to_string_lossy().as_ref() {
                    "states" => GalleryPageSection::States,
                    "status" => GalleryPageSection::Status,
                    "samples" => GalleryPageSection::Samples,
                    _ => {
                        return Err(PreviewError::Usage(
                            "--section requires states or samples".to_string(),
                        ));
                    }
                });
            }
            "--hold-ms" => {
                if window_hold_ms.is_some() {
                    return Err(PreviewError::Usage(
                        "--hold-ms may be supplied only once".to_string(),
                    ));
                }
                let value = args.next().ok_or_else(|| {
                    PreviewError::Usage("--hold-ms requires a bounded duration".to_string())
                })?;
                let value = value.to_string_lossy().parse::<u32>().map_err(|_| {
                    PreviewError::Usage("--hold-ms requires a bounded duration".to_string())
                })?;
                if value > 2_000 {
                    return Err(PreviewError::Usage(
                        "--hold-ms must be at most 2000 milliseconds".to_string(),
                    ));
                }
                window_hold_ms = Some(value);
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
    let mut request = PreviewRequest::validate(fixture, output, policy)?;
    match (gallery_theme, gallery_density, gallery_scale) {
        (None, None, None) => {}
        (Some(theme), Some(density), Some(scale)) if [100, 125, 150, 200].contains(&scale) => {
            request.gallery_page = Some(GalleryPage {
                theme,
                density,
                scale,
                state_page: gallery_state_page.unwrap_or_default(),
                status_page: gallery_status_page.unwrap_or_default(),
                sample_page: gallery_sample_page.unwrap_or_default(),
                section: gallery_section.unwrap_or(GalleryPageSection::States),
            });
        }
        _ => {
            return Err(PreviewError::Usage(
                "--theme, --density, and --scale must be supplied together with a supported scale"
                    .to_string(),
            ));
        }
    }
    request.window_hold_ms = window_hold_ms.unwrap_or_default();
    Ok(request)
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
    pub gallery_page: Option<GalleryPage>,
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
        let gallery_page = request.gallery_page;
        let window_hold_ms = request.window_hold_ms;
        let fixture_authority = Arc::clone(&request.fixture_authority);
        let request = PreviewRequest::validate(
            request.fixture_path.clone(),
            request.output_path.clone(),
            policy,
        )
        .map(|mut request| {
            request.gallery_page = gallery_page;
            request.window_hold_ms = window_hold_ms;
            request.fixture_authority = Arc::clone(&fixture_authority);
            request
        })?;
        let bytes = read_fixture_bytes(&request.fixture_authority)?;
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
            gallery_page,
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
        self.element_for_scale(1.0)
    }

    fn element_for_scale(&self, scale_factor: f32) -> impl IntoElement {
        let scale_factor = if scale_factor.is_finite() && scale_factor > 0.0 {
            scale_factor
        } else {
            1.0
        };
        let physical_px = |value: f32| px(value / scale_factor);
        let gallery_page = self.snapshot.gallery_page.unwrap_or_default();
        let gallery_tokens = gallery_theme_tokens(gallery_page);
        let gallery = self
            .snapshot
            .component_gallery
            .as_ref()
            .map(|gallery| render_component_gallery(gallery, gallery_page, scale_factor));
        let mut root = div()
            .size_full()
            .p(physical_px(PREVIEW_PADDING_PX))
            .bg(gpui::rgb(gallery_tokens.surfaces.canvas.to_u32()))
            .text_color(gpui::rgb(gallery_tokens.text.primary.to_u32()))
            .on_action::<PreviewDismiss>(|_, _, cx: &mut gpui::App| cx.quit())
            .child(
                div()
                    .flex_none()
                    .size(physical_px(PREVIEW_SENTINEL_SIZE))
                    .bg(gpui::rgb(PREVIEW_SENTINEL_RGB)),
            )
            .child(
                div()
                    .w(physical_px(GALLERY_CONTENT_WIDTH_PX))
                    .line_clamp(2)
                    .child(self.snapshot.title.clone()),
            );
        if let Some(gallery) = gallery {
            root = root.child(gallery);
        }
        root
    }
}

fn render_component_gallery(
    gallery: &ComponentGalleryFixture,
    page: GalleryPage,
    scale_factor: f32,
) -> gpui::AnyElement {
    // This projection deliberately calls the production component elements;
    // no gallery-only hand-styled controls are allowed to hide state drift.
    gallery_layout_assertion(gallery);
    if gallery_scale(page.scale).is_none() {
        return div().child("Unsupported gallery scale").into_any_element();
    }
    let tokens = gallery_theme_tokens(page);

    let page_start = usize::from(page.state_page) * usize::from(GALLERY_PAGE_ROWS);
    let mut state_grid = div().flex().flex_col().gap_y_2().w_full();
    for &state in gallery
        .states
        .iter()
        .filter(|&&state| state != GalleryState::Status)
        .skip(page_start)
        .take(usize::from(GALLERY_PAGE_ROWS))
    {
        let mut cell = div()
            .flex()
            .items_center()
            .gap(px(tokens.density.spacing.xs))
            .w_full()
            .child(
                div()
                    .w(px(88.0))
                    .flex_none()
                    .text_xs()
                    .child(gallery_state_name(state)),
            );
        cell = cell.child(
            div()
                .flex()
                .flex_wrap()
                .gap_x_1()
                .gap_y_1()
                .child(render_gallery_button(tokens, state))
                .child(render_gallery_icon_button(tokens, state)),
        );
        state_grid = state_grid.child(cell);
    }

    let mut samples = div().flex().flex_col().gap_y_1().w_full();
    let sample_start = usize::from(page.sample_page) * 4;
    for (name, sample) in [
        ("unicode", gallery.samples.unicode.clone()),
        ("long text", gallery.samples.long_text.clone()),
        ("missing", gallery.samples.missing.clone()),
        ("error", gallery.samples.error.clone()),
        ("loading", gallery.samples.loading.clone()),
        ("empty", gallery.samples.empty.clone()),
        ("overflow", gallery.samples.overflow.clone()),
    ]
    .into_iter()
    .skip(sample_start)
    .take(4)
    {
        let mut sample_value = div()
            .flex_shrink_0()
            .w(px(GALLERY_SAMPLE_VALUE_WIDTH_PX / scale_factor))
            .text_xs();
        if name == "long text" || name == "unicode" {
            sample_value = sample_value
                .max_h(px(32.0))
                .overflow_hidden()
                .whitespace_normal()
                .line_clamp(2)
                .child(wrapped_gallery_sample(&sample));
        } else {
            sample_value = sample_value
                .truncate()
                .child(bounded_gallery_sample(&sample));
        }
        samples = samples.child(
            div()
                .flex()
                .w_full()
                .gap(px(4.0))
                .child(div().text_xs().child(format!("{name}:")))
                .child(sample_value),
        );
    }

    let page_content = match page.section {
        GalleryPageSection::States => div()
            .child(div().text_xs().child(format!(
                "States page {}/{}",
                u16::from(page.state_page) + 1,
                3
            )))
            .child(state_grid),
        GalleryPageSection::Status => {
            let meanings = [
                StatusMeaning::External,
                StatusMeaning::Attention,
                StatusMeaning::Success,
                StatusMeaning::Warning,
                StatusMeaning::Destructive,
                StatusMeaning::Inactive,
            ];
            let status_start = usize::from(page.status_page) * 3;
            let mut status_grid = div().flex().flex_col().gap_y_2().w_full();
            for meaning in meanings.iter().skip(status_start).take(3).copied() {
                status_grid = status_grid.child(render_gallery_status(tokens, meaning));
            }
            div()
                .child(div().text_xs().child(format!(
                    "Status page {}/{}",
                    u16::from(page.status_page) + 1,
                    2
                )))
                .child(status_grid)
        }
        GalleryPageSection::Samples => div()
            .child(div().text_xs().child(format!(
                "Sanitized fixture samples page {}/{}",
                u16::from(page.sample_page) + 1,
                2
            )))
            .child(samples),
    };

    div()
        .mt(px(12.0))
        .w(px(GALLERY_CONTENT_WIDTH_PX / scale_factor))
        .flex()
        .flex_col()
        .gap(px(8.0))
        .child(div().text_sm().child(format!(
            "Component gallery · {} / {} / {}%",
            gallery_theme_name(page.theme),
            gallery_density_name(page.density),
            page.scale
        )))
        .child(page_content)
        .into_any_element()
}

fn gallery_theme_tokens(page: GalleryPage) -> crate::ui::tokens::ThemeTokens {
    theme(
        match page.theme {
            GalleryTheme::Dark => ThemeMode::Dark,
            GalleryTheme::Light => ThemeMode::Light,
        },
        match page.density {
            GalleryDensity::Compact => Density::Compact,
            GalleryDensity::Comfortable => Density::Comfortable,
        },
        gallery_scale(page.scale).unwrap_or(Scale::Scale100),
    )
}

fn bounded_gallery_sample(value: &str) -> String {
    const MAX_VISIBLE_SCALARS: usize = 36;
    let mut bounded: String = value.chars().take(MAX_VISIBLE_SCALARS).collect();
    if value.chars().count() > MAX_VISIBLE_SCALARS {
        bounded.push('…');
    }
    bounded
}

fn wrapped_gallery_sample(value: &str) -> String {
    const MAX_LINE_SCALARS: usize = 20;
    let mut wrapped = String::with_capacity(value.len());
    let mut line_scalars = 0;
    for word in value.split_whitespace() {
        let word_scalars = word.chars().count();
        if line_scalars > 0 && line_scalars + 1 + word_scalars > MAX_LINE_SCALARS {
            wrapped.push('\n');
            line_scalars = 0;
        } else if line_scalars > 0 {
            wrapped.push(' ');
            line_scalars += 1;
        }
        wrapped.push_str(word);
        line_scalars += word_scalars;
    }
    wrapped
}

fn gallery_layout_assertion(gallery: &ComponentGalleryFixture) {
    // The page contract is intentionally explicit: all nine states occupy a
    // bounded pages, with each cell wrapping its production controls.
    assert!(
        gallery.states.len() <= 9,
        "gallery page layout assertion: too many states for the bounded grid"
    );
    assert!(
        (GALLERY_CONTENT_WIDTH_PX - (f32::from(GALLERY_PAGE_COLUMNS) - 1.0) * GALLERY_PAGE_GAP_PX)
            / f32::from(GALLERY_PAGE_COLUMNS)
            >= 180.0,
        "gallery page layout assertion: columns exceed the 640px content width"
    );
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

fn gallery_state_name(state: GalleryState) -> &'static str {
    match state {
        GalleryState::Default => "default",
        GalleryState::Hover => "hover",
        GalleryState::Pressed => "pressed",
        GalleryState::Focused => "focused",
        GalleryState::Disabled => "disabled",
        GalleryState::Loading => "loading",
        GalleryState::Destructive => "destructive",
        GalleryState::Selected => "selected",
        GalleryState::Status => "status",
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
opaque_preview_format!(FixtureFileAuthority, "FixtureFileAuthority(<opaque>)");
opaque_preview_format!(
    FixtureDirectoryAuthority,
    "FixtureDirectoryAuthority(<opaque>)"
);
opaque_preview_format!(GalleryPage, "GalleryPage(<opaque>)");
opaque_preview_format!(PreviewPathPolicy, "PreviewPathPolicy(<opaque>)");
opaque_preview_format!(PreviewRequest, "PreviewRequest(<opaque>)");
opaque_preview_format!(PreviewResources, "PreviewResources(<opaque>)");
opaque_preview_format!(PreviewRootSnapshot, "PreviewRootSnapshot(<opaque>)");
opaque_preview_format!(PreviewOutputMetadata, "PreviewOutputMetadata(<opaque>)");
opaque_preview_format!(PreviewApplication, "PreviewApplication(<opaque>)");
opaque_preview_format!(PreviewRoot, "PreviewRoot(<opaque>)");

impl Render for PreviewRoot {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.element_for_scale(window.scale_factor())
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

struct FixtureFileAuthority {
    path: PathBuf,
    file: Mutex<fs::File>,
    identity: FixtureFileIdentity,
    size: u64,
    fixture_ancestor_chain: Vec<FixtureDirectoryAuthority>,
}

struct FixtureDirectoryAuthority {
    path: PathBuf,
    file: fs::File,
    identity: FixtureFileIdentity,
}

fn open_fixture_authority(
    path: &Path,
    fixture_root: &Path,
) -> Result<Arc<FixtureFileAuthority>, PreviewError> {
    let fixture_ancestor_chain = open_fixture_ancestor_chain(path, fixture_root)?;
    let file = open_fixture_relative(path).map_err(|error| {
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
    let metadata = file.metadata().map_err(|error| PreviewError::FixtureIo {
        path: path.to_path_buf(),
        message: error.to_string(),
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
    let identity = fixture_file_identity(&file).map_err(|error| PreviewError::FixtureIo {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    let authority = Arc::new(FixtureFileAuthority {
        path: path.to_path_buf(),
        file: Mutex::new(file),
        identity,
        size: metadata.len(),
        fixture_ancestor_chain,
    });
    authority.verify_fixture_containment()?;
    Ok(authority)
}

impl FixtureFileAuthority {
    fn verify_fixture_containment(&self) -> Result<(), PreviewError> {
        for ancestor in &self.fixture_ancestor_chain {
            let reopened = open_fixture_directory_handle(&ancestor.path).map_err(|error| {
                PreviewError::FixtureIo {
                    path: self.path.clone(),
                    message: format!("fixture ancestor changed during capture: {error}"),
                }
            })?;
            let reopened_identity =
                fixture_file_identity(&reopened).map_err(|error| PreviewError::FixtureIo {
                    path: self.path.clone(),
                    message: error.to_string(),
                })?;
            let retained_identity =
                fixture_file_identity(&ancestor.file).map_err(|error| PreviewError::FixtureIo {
                    path: self.path.clone(),
                    message: error.to_string(),
                })?;
            if reopened_identity != ancestor.identity || retained_identity != ancestor.identity {
                return Err(PreviewError::FixtureIo {
                    path: self.path.clone(),
                    message: "fixture root or ancestor identity changed during capture".into(),
                });
            }
        }
        Ok(())
    }
}

fn read_fixture_bytes(authority: &FixtureFileAuthority) -> Result<Vec<u8>, PreviewError> {
    authority.verify_fixture_containment()?;
    let path = &authority.path;
    let mut fixture_handle = authority
        .file
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    fixture_handle
        .seek(SeekFrom::Start(0))
        .map_err(|error| PreviewError::FixtureIo {
            path: path.clone(),
            message: error.to_string(),
        })?;
    let metadata_before = fixture_handle
        .metadata()
        .map_err(|error| PreviewError::FixtureIo {
            path: path.clone(),
            message: error.to_string(),
        })?;
    let identity_before =
        fixture_file_identity(&fixture_handle).map_err(|error| PreviewError::FixtureIo {
            path: path.clone(),
            message: error.to_string(),
        })?;
    if !metadata_before.is_file() {
        return Err(PreviewError::FixtureNotRegular { path: path.clone() });
    }
    if metadata_before.len() > MAX_FIXTURE_BYTES {
        return Err(PreviewError::FixtureTooLarge {
            path: path.clone(),
            bytes: metadata_before.len(),
            max_bytes: MAX_FIXTURE_BYTES,
        });
    }
    if identity_before != authority.identity || metadata_before.len() != authority.size {
        return Err(PreviewError::FixtureIo {
            path: path.clone(),
            message: "fixture authority changed before it was read".into(),
        });
    }

    let mut bytes = Vec::with_capacity(metadata_before.len() as usize);
    fixture_handle
        .by_ref()
        .take(MAX_FIXTURE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| PreviewError::FixtureIo {
            path: path.clone(),
            message: error.to_string(),
        })?;
    let hash_before = Sha256::digest(&bytes);
    fixture_handle
        .seek(SeekFrom::Start(0))
        .and_then(|_| {
            let mut verification = Vec::with_capacity(bytes.len());
            fixture_handle.read_to_end(&mut verification)?;
            Ok(verification)
        })
        .map(|verification| {
            let hash_after = Sha256::digest(&verification);
            (verification, hash_after)
        })
        .map_err(|error| PreviewError::FixtureIo {
            path: path.clone(),
            message: error.to_string(),
        })
        .and_then(|(verification, hash_after)| {
            let metadata_after =
                fixture_handle
                    .metadata()
                    .map_err(|error| PreviewError::FixtureIo {
                        path: path.clone(),
                        message: error.to_string(),
                    })?;
            let identity_after = fixture_file_identity(&fixture_handle).map_err(|error| {
                PreviewError::FixtureIo {
                    path: path.clone(),
                    message: error.to_string(),
                }
            })?;
            if bytes.len() as u64 > MAX_FIXTURE_BYTES {
                return Err(PreviewError::FixtureTooLarge {
                    path: path.clone(),
                    bytes: bytes.len() as u64,
                    max_bytes: MAX_FIXTURE_BYTES,
                });
            }
            authority.verify_fixture_containment()?;
            if identity_before != authority.identity
                || identity_before != identity_after
                || metadata_before.len() != authority.size
                || metadata_before.len() != metadata_after.len()
                || bytes != verification
                || hash_before != hash_after
            {
                return Err(PreviewError::FixtureIo {
                    path: path.clone(),
                    message: "fixture changed while it was read through its authority handle"
                        .into(),
                });
            }
            Ok(bytes)
        })
}

/// Open the final fixture through the already retained ancestor authority.
/// The no-follow flags make the final boundary fail closed; the caller
/// immediately revalidates every retained ancestor before reading this handle.
fn open_fixture_relative(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT.0);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    options.open(path)
}

fn open_fixture_ancestor_chain(
    path: &Path,
    fixture_root: &Path,
) -> Result<Vec<FixtureDirectoryAuthority>, PreviewError> {
    let absolute_fixture = checked_path(path)?;
    let absolute_root = checked_path(fixture_root)?;
    if !is_within(&absolute_fixture, &absolute_root) {
        return Err(PreviewError::OutsideApprovedRoot {
            path: absolute_fixture,
            root_kind: "fixture",
        });
    }
    let mut paths = Vec::new();
    let mut current = absolute_fixture
        .parent()
        .ok_or_else(|| PreviewError::FixtureIo {
            path: absolute_fixture.clone(),
            message: "fixture has no parent directory".into(),
        })?;
    loop {
        paths.push(current.to_path_buf());
        if is_same_path(current, &absolute_root) {
            break;
        }
        current = current.parent().ok_or_else(|| PreviewError::UnsafePath {
            path: absolute_fixture.clone(),
            root_kind: "fixture",
        })?;
        if !is_within(current, &absolute_root) {
            return Err(PreviewError::UnsafePath {
                path: absolute_fixture.clone(),
                root_kind: "fixture",
            });
        }
    }
    paths.reverse();
    paths
        .into_iter()
        .map(|path| {
            let file = open_fixture_directory_handle(&path).map_err(|error| {
                let _ = error;
                PreviewError::UnsafePath {
                    path: absolute_fixture.clone(),
                    root_kind: "fixture",
                }
            })?;
            let identity =
                fixture_file_identity(&file).map_err(|error| PreviewError::FixtureIo {
                    path: absolute_fixture.clone(),
                    message: error.to_string(),
                })?;
            Ok(FixtureDirectoryAuthority {
                path,
                file,
                identity,
            })
        })
        .collect()
}

fn open_fixture_directory_handle(path: &Path) -> std::io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(
            windows::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS.0
                | windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT.0,
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if !metadata.is_dir()
            || metadata.file_attributes()
                & windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT.0
                != 0
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                "fixture ancestor is not a regular directory",
            ));
        }
    }
    #[cfg(not(windows))]
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            "fixture ancestor is not a regular directory",
        ));
    }
    Ok(file)
}

fn is_same_path(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        return left
            .to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy());
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FixtureFileIdentity {
    #[cfg(windows)]
    volume_serial: u32,
    #[cfg(windows)]
    file_index: u64,
    #[cfg(all(not(windows), unix))]
    dev: u64,
    #[cfg(all(not(windows), unix))]
    inode: u64,
    #[cfg(all(not(windows), not(unix)))]
    modified_nanos: u128,
    #[cfg(all(not(windows), not(unix)))]
    length: u64,
}

fn fixture_file_identity(file: &fs::File) -> std::io::Result<FixtureFileIdentity> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut information) }
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        return Ok(FixtureFileIdentity {
            volume_serial: information.dwVolumeSerialNumber,
            file_index: (u64::from(information.nFileIndexHigh) << 32)
                | u64::from(information.nFileIndexLow),
        });
    }
    #[cfg(all(not(windows), unix))]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        return Ok(FixtureFileIdentity {
            dev: metadata.dev(),
            inode: metadata.ino(),
        });
    }
    #[cfg(all(not(windows), not(unix)))]
    {
        let metadata = file.metadata()?;
        let modified_nanos = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        return Ok(FixtureFileIdentity {
            modified_nanos,
            length: metadata.len(),
        });
    }
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

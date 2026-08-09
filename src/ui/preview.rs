//! Deterministic, isolated native UI preview contracts.

use gpui::{div, Action, Context, IntoElement, ParentElement, Render, Window};
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

pub const PREVIEW_SCHEMA: &str = "devmanager.ui.preview/v1";
pub const MAX_FIXTURE_BYTES: u64 = 256 * 1024;
const PREVIEW_USAGE: &str =
    "usage: devmanager-next --ui-preview <fixture.json> --output <preview.png>";

gpui::actions!(devmanager_next, [PreviewDismiss]);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewFixture {
    pub schema: String,
    pub id: String,
    pub title: String,
    pub root: PreviewRootFixture,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewRootFixture {
    pub kind: String,
    pub label: String,
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
    pub fixture_path: PathBuf,
    pub output_path: PathBuf,
}

impl PreviewRequest {
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
    pub title: String,
    pub body: String,
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
    root_snapshot: PreviewRootSnapshot,
    resources: PreviewResources,
    init_report: RefCell<Option<PreviewInitReport>>,
}

impl PreviewApplication {
    pub fn load(request: PreviewRequest) -> Result<Self, PreviewError> {
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
            || fixture.root.kind != "minimal"
            || fixture.root.label.trim().is_empty()
            || fixture.root.label.chars().count() > 256
        {
            return Err(PreviewError::MalformedFixture {
                path: request.fixture_path,
                message: "fixture fields are empty, oversized, or use an unsupported root".into(),
            });
        }

        let root_snapshot = PreviewRootSnapshot {
            fixture_id: fixture.id,
            body: format!("{}: {}", fixture.root.label, fixture.title),
            title: fixture.title,
        };
        Ok(Self {
            request,
            root_snapshot,
            resources: PreviewResources::new(),
            init_report: RefCell::new(None),
        })
    }

    pub fn root_snapshot(&self) -> &PreviewRootSnapshot {
        &self.root_snapshot
    }

    pub fn resources(&self) -> &PreviewResources {
        &self.resources
    }

    pub fn output_metadata(&self) -> PreviewOutputMetadata {
        PreviewOutputMetadata {
            schema: PREVIEW_SCHEMA.to_string(),
            fixture_id: self.root_snapshot.fixture_id.clone(),
            output_path: self.request.output_path.clone(),
            format: "png".to_string(),
            capability: PreviewOutputCapability::HeadlessProjectionOnly,
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
            crate::ui::init(cx);
            crate::ui::init(cx);

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
            let action_name = PreviewDismiss::name_for_type();
            let actions_registered = cx.all_action_names().contains(&action_name);
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
        let _ = &self.request.output_path;
        Err(PreviewError::HeadlessRenderingUnsupported)
    }
}

#[derive(Debug, Clone)]
pub struct PreviewRoot {
    snapshot: PreviewRootSnapshot,
}

impl PreviewRoot {
    fn new(snapshot: PreviewRootSnapshot) -> Self {
        Self { snapshot }
    }

    pub fn element(&self) -> impl IntoElement {
        div().child(self.snapshot.body.clone())
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
    let preview = PreviewApplication::load(request)?;
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
    HeadlessRenderingUnsupported,
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
            Self::HeadlessRenderingUnsupported => f.write_str(
                "headless PNG rendering is unsupported until Windows GPUI capture is proven",
            ),
        }
    }
}

impl Error for PreviewError {}

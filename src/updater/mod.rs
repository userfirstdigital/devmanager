use cargo_packager_updater::{
    self, semver::Version, url::Url, Config as PackagerUpdaterConfig, Update as PackagerUpdate,
    WindowsConfig as PackagerWindowsConfig, WindowsUpdateInstallMode,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::SystemTime;

const UPDATE_ENDPOINTS_VAR: &str = "DEVMANAGER_UPDATE_ENDPOINTS";
const UPDATE_PUBKEY_VAR: &str = "DEVMANAGER_UPDATE_PUBKEY";
const UPDATE_WINDOWS_INSTALL_MODE_VAR: &str = "DEVMANAGER_UPDATE_WINDOWS_INSTALL_MODE";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdaterStage {
    Disabled,
    Idle,
    Checking,
    UpToDate,
    UpdateAvailable,
    Downloading,
    ReadyToInstall,
    Installing,
    Error,
}

#[derive(Debug, Clone)]
pub struct UpdaterSnapshot {
    pub configured: bool,
    pub current_version: String,
    pub endpoints: Vec<String>,
    pub stage: UpdaterStage,
    pub target_version: Option<String>,
    pub detail: String,
    pub release_notes: Option<String>,
    pub last_checked_at: Option<SystemTime>,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
}

impl UpdaterSnapshot {
    pub fn is_busy(&self) -> bool {
        matches!(
            self.stage,
            UpdaterStage::Checking | UpdaterStage::Downloading | UpdaterStage::Installing
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdaterWindowsInstallMode {
    BasicUi,
    Quiet,
    Passive,
}

impl UpdaterWindowsInstallMode {
    fn into_packager(self) -> WindowsUpdateInstallMode {
        match self {
            Self::BasicUi => WindowsUpdateInstallMode::BasicUi,
            Self::Quiet => WindowsUpdateInstallMode::Quiet,
            Self::Passive => WindowsUpdateInstallMode::Passive,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedUpdaterConfig {
    pub endpoints: Vec<String>,
    pub pubkey: String,
    pub windows_install_mode: UpdaterWindowsInstallMode,
}

impl ResolvedUpdaterConfig {
    fn into_packager_config(self) -> Result<PackagerUpdaterConfig, String> {
        let endpoints = self
            .endpoints
            .iter()
            .map(|endpoint| {
                Url::parse(endpoint).map_err(|error| {
                    format!("Failed to parse updater endpoint `{endpoint}`: {error}")
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(PackagerUpdaterConfig {
            endpoints,
            pubkey: self.pubkey,
            windows: Some(PackagerWindowsConfig {
                installer_args: None,
                install_mode: Some(self.windows_install_mode.into_packager()),
            }),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReleaseManifest {
    pub version: String,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub pub_date: Option<String>,
    #[serde(default)]
    pub platforms: HashMap<String, ReleaseManifestPlatform>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReleaseManifestPlatform {
    pub format: String,
    pub signature: String,
    pub url: String,
}

#[derive(Clone)]
pub struct UpdaterService {
    inner: Arc<UpdaterInner>,
}

struct UpdaterInner {
    current_version: Version,
    config: Option<PackagerUpdaterConfig>,
    state: RwLock<UpdaterState>,
}

struct UpdaterState {
    snapshot: UpdaterSnapshot,
    pending_update: Option<PackagerUpdate>,
    downloaded_bytes: Option<Vec<u8>>,
}

impl UpdaterService {
    pub fn new() -> Self {
        let current_version =
            Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| Version::new(0, 0, 0));
        let resolved = resolve_embedded_config();
        let (config, snapshot) = match resolved {
            Ok(Some(config)) => {
                let snapshot = UpdaterSnapshot {
                    configured: true,
                    current_version: current_version.to_string(),
                    endpoints: config.endpoints.clone(),
                    stage: UpdaterStage::Idle,
                    target_version: None,
                    detail: format!(
                        "Ready to check {} for updates.",
                        summarize_endpoint(config.endpoints.first())
                    ),
                    release_notes: None,
                    last_checked_at: None,
                    downloaded_bytes: 0,
                    total_bytes: None,
                };
                match config.into_packager_config() {
                    Ok(config) => (Some(config), snapshot),
                    Err(error) => (
                        None,
                        UpdaterSnapshot {
                            configured: false,
                            current_version: current_version.to_string(),
                            endpoints: Vec::new(),
                            stage: UpdaterStage::Disabled,
                            target_version: None,
                            detail: format!("Updater is disabled: {error}"),
                            release_notes: None,
                            last_checked_at: None,
                            downloaded_bytes: 0,
                            total_bytes: None,
                        },
                    ),
                }
            }
            Ok(None) => (
                None,
                UpdaterSnapshot {
                    configured: false,
                    current_version: current_version.to_string(),
                    endpoints: Vec::new(),
                    stage: UpdaterStage::Disabled,
                    target_version: None,
                    detail: format!(
                        "Updater is disabled. Build with {UPDATE_ENDPOINTS_VAR} and \
{UPDATE_PUBKEY_VAR} to enable GitHub-hosted updates."
                    ),
                    release_notes: None,
                    last_checked_at: None,
                    downloaded_bytes: 0,
                    total_bytes: None,
                },
            ),
            Err(error) => (
                None,
                UpdaterSnapshot {
                    configured: false,
                    current_version: current_version.to_string(),
                    endpoints: Vec::new(),
                    stage: UpdaterStage::Disabled,
                    target_version: None,
                    detail: format!("Updater is disabled: {error}"),
                    release_notes: None,
                    last_checked_at: None,
                    downloaded_bytes: 0,
                    total_bytes: None,
                },
            ),
        };

        Self {
            inner: Arc::new(UpdaterInner {
                current_version,
                config,
                state: RwLock::new(UpdaterState {
                    snapshot,
                    pending_update: None,
                    downloaded_bytes: None,
                }),
            }),
        }
    }

    pub fn snapshot(&self) -> UpdaterSnapshot {
        self.inner
            .state
            .read()
            .map(|state| state.snapshot.clone())
            .unwrap_or_else(|_| UpdaterSnapshot {
                configured: false,
                current_version: self.inner.current_version.to_string(),
                endpoints: Vec::new(),
                stage: UpdaterStage::Error,
                target_version: None,
                detail: "Updater state is unavailable.".to_string(),
                release_notes: None,
                last_checked_at: None,
                downloaded_bytes: 0,
                total_bytes: None,
            })
    }

    pub fn is_configured(&self) -> bool {
        self.snapshot().configured
    }

    pub fn check_for_updates(&self) -> Result<(), String> {
        let config = self
            .inner
            .config
            .clone()
            .ok_or_else(|| self.snapshot().detail)?;

        self.inner.set_checking()?;
        let inner = self.inner.clone();
        let current_version = self.inner.current_version.clone();
        thread::spawn(move || {
            match cargo_packager_updater::check_update(current_version, config) {
                Ok(Some(update)) => inner.set_update_available(update),
                Ok(None) => inner.set_up_to_date(),
                Err(error) => inner.set_error(format!("Update check failed: {error}")),
            }
        });
        Ok(())
    }

    pub fn download_update(&self) -> Result<(), String> {
        let update = self.inner.prepare_download()?;
        let version = update.version.clone();
        let inner = self.inner.clone();
        thread::spawn(move || {
            let progress_inner = inner.clone();
            match update.download_extended(
                move |chunk_size, total| {
                    progress_inner.record_download_progress(chunk_size as u64, total);
                },
                || {},
            ) {
                Ok(bytes) => inner.set_ready_to_install(update, bytes),
                Err(error) => inner.set_error(format!("Download failed for {version}: {error}")),
            }
        });
        Ok(())
    }

    pub fn install_update(&self) -> Result<String, String> {
        let (update, bytes, version) = self.inner.prepare_install()?;
        match update.install(bytes) {
            Ok(()) => Ok(version),
            Err(error) => {
                self.inner
                    .set_error(format!("Failed to hand off installer: {error}"));
                Err(error.to_string())
            }
        }
    }
}

impl Default for UpdaterService {
    fn default() -> Self {
        Self::new()
    }
}

impl UpdaterInner {
    fn set_checking(&self) -> Result<(), String> {
        let mut state = self
            .state
            .write()
            .map_err(|_| "Updater state is unavailable.".to_string())?;
        if state.snapshot.is_busy() {
            return Err("Updater is busy. Wait for the current action to finish.".to_string());
        }
        if !state.snapshot.configured {
            return Err(state.snapshot.detail.clone());
        }
        state.pending_update = None;
        state.downloaded_bytes = None;
        state.snapshot.stage = UpdaterStage::Checking;
        state.snapshot.target_version = None;
        state.snapshot.release_notes = None;
        state.snapshot.detail = format!(
            "Checking {} for a newer release...",
            summarize_endpoint(state.snapshot.endpoints.first())
        );
        state.snapshot.downloaded_bytes = 0;
        state.snapshot.total_bytes = None;
        Ok(())
    }

    fn set_up_to_date(&self) {
        if let Ok(mut state) = self.state.write() {
            state.pending_update = None;
            state.downloaded_bytes = None;
            state.snapshot.stage = UpdaterStage::UpToDate;
            state.snapshot.target_version = None;
            state.snapshot.release_notes = None;
            state.snapshot.last_checked_at = Some(SystemTime::now());
            state.snapshot.downloaded_bytes = 0;
            state.snapshot.total_bytes = None;
            state.snapshot.detail = format!(
                "DevManager {} is up to date.",
                state.snapshot.current_version
            );
        }
    }

    fn set_update_available(&self, update: PackagerUpdate) {
        if let Ok(mut state) = self.state.write() {
            state.downloaded_bytes = None;
            state.pending_update = Some(update.clone());
            state.snapshot.stage = UpdaterStage::UpdateAvailable;
            state.snapshot.target_version = Some(update.version.clone());
            state.snapshot.release_notes = update.body.clone();
            state.snapshot.last_checked_at = Some(SystemTime::now());
            state.snapshot.downloaded_bytes = 0;
            state.snapshot.total_bytes = None;
            state.snapshot.detail = format!(
                "Version {} is available. Download it when you are ready.",
                update.version
            );
        }
    }

    fn prepare_download(&self) -> Result<PackagerUpdate, String> {
        let mut state = self
            .state
            .write()
            .map_err(|_| "Updater state is unavailable.".to_string())?;
        if state.snapshot.is_busy() {
            return Err("Updater is busy. Wait for the current action to finish.".to_string());
        }
        let update = state
            .pending_update
            .clone()
            .ok_or_else(|| "Check for an available update first.".to_string())?;
        state.downloaded_bytes = None;
        state.snapshot.stage = UpdaterStage::Downloading;
        state.snapshot.target_version = Some(update.version.clone());
        state.snapshot.downloaded_bytes = 0;
        state.snapshot.total_bytes = None;
        state.snapshot.detail = format!("Downloading version {}...", update.version);
        Ok(update)
    }

    fn record_download_progress(&self, chunk_size: u64, total: Option<u64>) {
        if let Ok(mut state) = self.state.write() {
            state.snapshot.stage = UpdaterStage::Downloading;
            state.snapshot.downloaded_bytes =
                state.snapshot.downloaded_bytes.saturating_add(chunk_size);
            state.snapshot.total_bytes = total;
            state.snapshot.detail = if let Some(total) = total {
                format!(
                    "Downloaded {} of {}.",
                    human_bytes(state.snapshot.downloaded_bytes),
                    human_bytes(total)
                )
            } else {
                format!(
                    "Downloaded {}...",
                    human_bytes(state.snapshot.downloaded_bytes)
                )
            };
        }
    }

    fn set_ready_to_install(&self, update: PackagerUpdate, bytes: Vec<u8>) {
        let size = bytes.len() as u64;
        if let Ok(mut state) = self.state.write() {
            state.pending_update = Some(update.clone());
            state.downloaded_bytes = Some(bytes);
            state.snapshot.stage = UpdaterStage::ReadyToInstall;
            state.snapshot.target_version = Some(update.version.clone());
            state.snapshot.last_checked_at = Some(SystemTime::now());
            state.snapshot.downloaded_bytes = size;
            state.snapshot.total_bytes = Some(size);
            state.snapshot.detail = format!(
                "Version {} is downloaded. Restart DevManager to install it.",
                update.version
            );
        }
    }

    fn prepare_install(&self) -> Result<(PackagerUpdate, Vec<u8>, String), String> {
        let mut state = self
            .state
            .write()
            .map_err(|_| "Updater state is unavailable.".to_string())?;
        if state.snapshot.is_busy() {
            return Err("Updater is busy. Wait for the current action to finish.".to_string());
        }
        let update = state
            .pending_update
            .clone()
            .ok_or_else(|| "No downloaded update is ready to install.".to_string())?;
        let bytes = state
            .downloaded_bytes
            .take()
            .ok_or_else(|| "Download the update before installing it.".to_string())?;
        let version = update.version.clone();
        state.snapshot.stage = UpdaterStage::Installing;
        state.snapshot.detail = format!("Launching installer for version {version}...");
        Ok((update, bytes, version))
    }

    fn set_error(&self, message: String) {
        if let Ok(mut state) = self.state.write() {
            state.downloaded_bytes = None;
            state.snapshot.stage = UpdaterStage::Error;
            state.snapshot.last_checked_at = Some(SystemTime::now());
            state.snapshot.detail = message;
            state.snapshot.total_bytes = None;
        }
    }
}

pub fn resolve_embedded_config() -> Result<Option<ResolvedUpdaterConfig>, String> {
    resolve_updater_config(
        read_runtime_or_embedded(UPDATE_ENDPOINTS_VAR),
        read_runtime_or_embedded(UPDATE_PUBKEY_VAR),
        read_runtime_or_embedded(UPDATE_WINDOWS_INSTALL_MODE_VAR),
    )
}

pub fn resolve_updater_config(
    endpoints_value: Option<String>,
    pubkey_value: Option<String>,
    install_mode_value: Option<String>,
) -> Result<Option<ResolvedUpdaterConfig>, String> {
    let endpoints = split_config_list(endpoints_value);
    let pubkey = pubkey_value.unwrap_or_default().trim().to_string();

    if endpoints.is_empty() && pubkey.is_empty() {
        return Ok(None);
    }

    if endpoints.is_empty() || pubkey.is_empty() {
        return Err(format!(
            "{UPDATE_ENDPOINTS_VAR} and {UPDATE_PUBKEY_VAR} must both be set to enable updates."
        ));
    }

    let windows_install_mode = parse_windows_install_mode(install_mode_value.as_deref())?;

    Ok(Some(ResolvedUpdaterConfig {
        endpoints,
        pubkey,
        windows_install_mode,
    }))
}

pub fn parse_release_manifest(json: &str) -> Result<ReleaseManifest, serde_json::Error> {
    serde_json::from_str(json)
}

pub fn is_remote_version_newer(
    current_version: &str,
    remote_version: &str,
) -> Result<bool, String> {
    let current = parse_version(current_version)?;
    let remote = parse_version(remote_version)?;
    Ok(remote > current)
}

fn parse_windows_install_mode(value: Option<&str>) -> Result<UpdaterWindowsInstallMode, String> {
    match value.unwrap_or("passive").trim() {
        "" | "passive" | "Passive" => Ok(UpdaterWindowsInstallMode::Passive),
        "basic-ui" | "basic_ui" | "basicUi" | "BasicUi" => Ok(UpdaterWindowsInstallMode::BasicUi),
        "quiet" | "Quiet" => Ok(UpdaterWindowsInstallMode::Quiet),
        other => Err(format!(
            "Unsupported Windows install mode `{other}`. Use `passive`, `quiet`, or `basicUi`."
        )),
    }
}

fn split_config_list(value: Option<String>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split(|character| matches!(character, ',' | ';' | '\n' | '\r'))
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn parse_version(value: &str) -> Result<Version, String> {
    Version::parse(value.trim().trim_start_matches('v'))
        .map_err(|error| format!("Invalid version `{value}`: {error}"))
}

fn read_runtime_or_embedded(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            let embedded = match name {
                UPDATE_ENDPOINTS_VAR => option_env!("DEVMANAGER_UPDATE_ENDPOINTS"),
                UPDATE_PUBKEY_VAR => option_env!("DEVMANAGER_UPDATE_PUBKEY"),
                UPDATE_WINDOWS_INSTALL_MODE_VAR => {
                    option_env!("DEVMANAGER_UPDATE_WINDOWS_INSTALL_MODE")
                }
                _ => None,
            }?;
            let trimmed = embedded.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
}

fn summarize_endpoint(endpoint: Option<&String>) -> String {
    endpoint
        .map(|value| value.as_str())
        .unwrap_or("the configured update endpoint")
        .to_string()
}

fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    let value = bytes as f64;
    if value >= GB {
        format!("{:.1} GB", value / GB)
    } else if value >= MB {
        format!("{:.1} MB", value / MB)
    } else if value >= KB {
        format!("{:.1} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

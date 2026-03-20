use cargo_packager_updater::{
    self,
    semver::{BuildMetadata, Prerelease, Version},
    url::Url,
    Config as PackagerUpdaterConfig, Update as PackagerUpdate,
    WindowsConfig as PackagerWindowsConfig, WindowsUpdateInstallMode,
};
use serde::Deserialize;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering as AtomicOrdering},
    Arc, RwLock,
};
use std::thread;
use std::time::{Duration, SystemTime};

const UPDATE_ENDPOINTS_VAR: &str = "DEVMANAGER_UPDATE_ENDPOINTS";
const UPDATE_PUBKEY_VAR: &str = "DEVMANAGER_UPDATE_PUBKEY";
const UPDATE_WINDOWS_INSTALL_MODE_VAR: &str = "DEVMANAGER_UPDATE_WINDOWS_INSTALL_MODE";
const BACKGROUND_UPDATE_INTERVAL: Duration = Duration::from_secs(30 * 60);

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

#[derive(Debug, Clone, PartialEq, Eq)]
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
    background_checks_started: AtomicBool,
    state: RwLock<UpdaterState>,
}

struct DownloadedUpdate {
    update: PackagerUpdate,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckPlan {
    Fresh,
    PreserveReady,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoDownloadAction {
    Start,
    KeepReady,
}

struct UpdaterState {
    snapshot: UpdaterSnapshot,
    pending_update: Option<PackagerUpdate>,
    ready_update: Option<DownloadedUpdate>,
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
                background_checks_started: AtomicBool::new(false),
                state: RwLock::new(UpdaterState {
                    snapshot,
                    pending_update: None,
                    ready_update: None,
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

    pub fn start_background_checks(&self) {
        if !self.is_configured() {
            return;
        }
        if self
            .inner
            .background_checks_started
            .swap(true, AtomicOrdering::SeqCst)
        {
            return;
        }

        let updater = self.clone();
        thread::spawn(move || {
            let _ = updater.check_for_updates();
            loop {
                thread::sleep(BACKGROUND_UPDATE_INTERVAL);
                let _ = updater.check_for_updates();
            }
        });
    }

    pub fn check_for_updates(&self) -> Result<(), String> {
        let config = self
            .inner
            .config
            .clone()
            .ok_or_else(|| self.snapshot().detail)?;

        let check_plan = self.inner.prepare_check()?;
        let inner = self.inner.clone();
        let current_version = self.inner.current_version.clone();
        thread::spawn(move || {
            match cargo_packager_updater::check_update(current_version, config) {
                Ok(Some(update)) => match inner.prepare_auto_download(&update) {
                    Ok(AutoDownloadAction::Start) => {
                        Self::spawn_download_thread(inner, update);
                    }
                    Ok(AutoDownloadAction::KeepReady) => {
                        inner.finish_check_without_update(check_plan);
                    }
                    Err(error) => inner.finish_check_error(
                        check_plan,
                        format!(
                            "Version {} is available, but the background download could not start: {error}",
                            update.version
                        ),
                    ),
                },
                Ok(None) => inner.finish_check_without_update(check_plan),
                Err(error) => inner.finish_check_error(
                    check_plan,
                    format!("Update check failed: {error}"),
                ),
            }
        });
        Ok(())
    }

    pub fn download_update(&self) -> Result<(), String> {
        let update = self.inner.prepare_download()?;
        Self::spawn_download_thread(self.inner.clone(), update);
        Ok(())
    }

    pub fn install_update(&self) -> Result<String, String> {
        let ready_update = self.inner.prepare_install()?;
        let version = ready_update.update.version.clone();
        match ready_update.update.install(ready_update.bytes) {
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

impl UpdaterService {
    fn spawn_download_thread(inner: Arc<UpdaterInner>, update: PackagerUpdate) {
        let version = update.version.clone();
        thread::spawn(move || {
            let progress_inner = inner.clone();
            match update.download_extended(
                move |chunk_size, total| {
                    progress_inner.record_download_progress(chunk_size as u64, total);
                },
                || {},
            ) {
                Ok(bytes) => inner.set_ready_to_install(update, bytes),
                Err(error) => inner.restore_ready_after_failed_download(format!(
                    "Download failed for {version}: {error}"
                )),
            }
        });
    }
}

impl UpdaterInner {
    fn prepare_check(&self) -> Result<CheckPlan, String> {
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
        if state.ready_update.is_some() {
            return Ok(CheckPlan::PreserveReady);
        }
        state.pending_update = None;
        state.ready_update = None;
        clear_update_metadata(&mut state.snapshot);
        state.snapshot.stage = UpdaterStage::Checking;
        state.snapshot.detail = format!(
            "Checking {} for a newer release...",
            summarize_endpoint(state.snapshot.endpoints.first())
        );
        Ok(CheckPlan::Fresh)
    }

    fn finish_check_without_update(&self, plan: CheckPlan) {
        match plan {
            CheckPlan::Fresh => self.set_up_to_date(),
            CheckPlan::PreserveReady => self.restore_ready_snapshot(None),
        }
    }

    fn finish_check_error(&self, plan: CheckPlan, message: String) {
        match plan {
            CheckPlan::Fresh => self.set_error(message),
            CheckPlan::PreserveReady => self.restore_ready_snapshot(None),
        }
    }

    fn set_up_to_date(&self) {
        if let Ok(mut state) = self.state.write() {
            state.pending_update = None;
            state.ready_update = None;
            state.snapshot.stage = UpdaterStage::UpToDate;
            state.snapshot.last_checked_at = Some(SystemTime::now());
            clear_update_metadata(&mut state.snapshot);
            state.snapshot.detail = format!(
                "DevManager {} is up to date.",
                state.snapshot.current_version
            );
        }
    }

    fn prepare_auto_download(&self, update: &PackagerUpdate) -> Result<AutoDownloadAction, String> {
        let mut state = self
            .state
            .write()
            .map_err(|_| "Updater state is unavailable.".to_string())?;
        if state.snapshot.is_busy() {
            return Err("Updater is busy. Wait for the current action to finish.".to_string());
        }
        if let Some(ready_update) = state.ready_update.as_ref() {
            match compare_versions(&update.version, &ready_update.update.version)? {
                Ordering::Greater => {}
                Ordering::Equal | Ordering::Less => return Ok(AutoDownloadAction::KeepReady),
            }
        }
        state.pending_update = Some(update.clone());
        state.snapshot.stage = UpdaterStage::Downloading;
        state.snapshot.target_version = Some(update.version.clone());
        state.snapshot.release_notes = update.body.clone();
        state.snapshot.downloaded_bytes = 0;
        state.snapshot.total_bytes = None;
        state.snapshot.detail = format!(
            "Version {} is available. Downloading it in the background...",
            update.version
        );
        Ok(AutoDownloadAction::Start)
    }

    fn prepare_download(&self) -> Result<PackagerUpdate, String> {
        let mut state = self
            .state
            .write()
            .map_err(|_| "Updater state is unavailable.".to_string())?;
        if state.snapshot.is_busy() {
            return Err("Updater is busy. Wait for the current action to finish.".to_string());
        }
        let update = state.pending_update.clone().ok_or_else(|| {
            state
                .ready_update
                .as_ref()
                .map(|ready_update| {
                    format!(
                        "Version {} is already downloaded. Restart DevManager to install it.",
                        ready_update.update.version
                    )
                })
                .unwrap_or_else(|| "Check for an available update first.".to_string())
        })?;
        state.snapshot.stage = UpdaterStage::Downloading;
        state.snapshot.target_version = Some(update.version.clone());
        state.snapshot.release_notes = update.body.clone();
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
        if let Ok(mut state) = self.state.write() {
            state.pending_update = None;
            state.ready_update = Some(DownloadedUpdate { update, bytes });
            restore_ready_snapshot_locked(&mut state, None);
        }
    }

    fn restore_ready_after_failed_download(&self, message: String) {
        if let Ok(mut state) = self.state.write() {
            state.pending_update = None;
            if state.ready_update.is_some() {
                restore_ready_snapshot_locked(&mut state, None);
            } else {
                state.snapshot.stage = UpdaterStage::Error;
                state.snapshot.last_checked_at = Some(SystemTime::now());
                clear_update_metadata(&mut state.snapshot);
                state.snapshot.detail = message;
            }
        }
    }

    fn restore_ready_snapshot(&self, detail_override: Option<String>) {
        if let Ok(mut state) = self.state.write() {
            if state.ready_update.is_some() {
                state.pending_update = None;
                restore_ready_snapshot_locked(&mut state, detail_override);
            }
        }
    }

    fn prepare_install(&self) -> Result<DownloadedUpdate, String> {
        let mut state = self
            .state
            .write()
            .map_err(|_| "Updater state is unavailable.".to_string())?;
        if state.snapshot.is_busy() {
            return Err("Updater is busy. Wait for the current action to finish.".to_string());
        }
        let ready_update = state
            .ready_update
            .take()
            .ok_or_else(|| "No downloaded update is ready to install.".to_string())?;
        let version = ready_update.update.version.clone();
        let size = ready_update.bytes.len() as u64;
        state.pending_update = None;
        state.snapshot.stage = UpdaterStage::Installing;
        state.snapshot.target_version = Some(version.clone());
        state.snapshot.release_notes = ready_update.update.body.clone();
        state.snapshot.downloaded_bytes = size;
        state.snapshot.total_bytes = Some(size);
        state.snapshot.detail = format!("Launching installer for version {version}...");
        Ok(ready_update)
    }

    fn set_error(&self, message: String) {
        if let Ok(mut state) = self.state.write() {
            state.pending_update = None;
            state.ready_update = None;
            state.snapshot.stage = UpdaterStage::Error;
            state.snapshot.last_checked_at = Some(SystemTime::now());
            clear_update_metadata(&mut state.snapshot);
            state.snapshot.detail = message;
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

pub fn next_patch_release_version(
    latest_release: Option<&str>,
    cargo_version: &str,
) -> Result<String, String> {
    let mut version = parse_version(latest_release.unwrap_or(cargo_version))?;
    version.patch = version.patch.saturating_add(1);
    version.pre = Prerelease::EMPTY;
    version.build = BuildMetadata::EMPTY;
    Ok(version.to_string())
}

pub fn github_release_manifest_endpoint(repository: &str) -> String {
    let repository = repository.trim().trim_matches('/');
    format!("https://github.com/{repository}/releases/latest/download/latest.json")
}

fn clear_update_metadata(snapshot: &mut UpdaterSnapshot) {
    snapshot.target_version = None;
    snapshot.release_notes = None;
    snapshot.downloaded_bytes = 0;
    snapshot.total_bytes = None;
}

fn restore_ready_snapshot_locked(state: &mut UpdaterState, detail_override: Option<String>) {
    let Some(ready_update) = state.ready_update.as_ref() else {
        clear_update_metadata(&mut state.snapshot);
        return;
    };
    let size = ready_update.bytes.len() as u64;
    state.snapshot.stage = UpdaterStage::ReadyToInstall;
    state.snapshot.target_version = Some(ready_update.update.version.clone());
    state.snapshot.release_notes = ready_update.update.body.clone();
    state.snapshot.last_checked_at = Some(SystemTime::now());
    state.snapshot.downloaded_bytes = size;
    state.snapshot.total_bytes = Some(size);
    state.snapshot.detail = detail_override.unwrap_or_else(|| {
        format!(
            "Version {} is downloaded. Restart DevManager to install it.",
            ready_update.update.version
        )
    });
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

fn compare_versions(left: &str, right: &str) -> Result<Ordering, String> {
    let left = parse_version(left)?;
    let right = parse_version(right)?;
    Ok(left.cmp(&right))
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

#[cfg(test)]
mod tests {
    use super::*;
    use cargo_packager_updater::UpdateFormat;
    use std::path::PathBuf;

    fn test_update(version: &str, body: Option<&str>) -> PackagerUpdate {
        PackagerUpdate {
            config: PackagerUpdaterConfig {
                endpoints: vec![Url::parse("https://example.com/latest.json").unwrap()],
                pubkey: "public-key".to_string(),
                windows: None,
            },
            body: body.map(ToOwned::to_owned),
            current_version: "0.2.0".to_string(),
            version: version.to_string(),
            date: None,
            target: "windows-x86_64".to_string(),
            extract_path: PathBuf::from("."),
            download_url: Url::parse("https://example.com/devmanager.exe").unwrap(),
            signature: "signature".to_string(),
            timeout: None,
            headers: Default::default(),
            format: UpdateFormat::Nsis,
        }
    }

    fn test_inner() -> UpdaterInner {
        UpdaterInner {
            current_version: Version::new(0, 2, 0),
            config: None,
            background_checks_started: AtomicBool::new(false),
            state: RwLock::new(UpdaterState {
                snapshot: UpdaterSnapshot {
                    configured: true,
                    current_version: "0.2.0".to_string(),
                    endpoints: vec!["https://example.com/latest.json".to_string()],
                    stage: UpdaterStage::Idle,
                    target_version: None,
                    detail: "Ready to check https://example.com/latest.json for updates."
                        .to_string(),
                    release_notes: None,
                    last_checked_at: None,
                    downloaded_bytes: 0,
                    total_bytes: None,
                },
                pending_update: None,
                ready_update: None,
            }),
        }
    }

    #[test]
    fn newer_release_supersedes_downloaded_ready_update() {
        let inner = test_inner();
        let ready_update = test_update("0.2.1", Some("old release"));
        let newer_update = test_update("0.2.2", Some("new release"));

        inner.set_ready_to_install(ready_update.clone(), vec![1, 2, 3]);
        assert_eq!(inner.prepare_check().unwrap(), CheckPlan::PreserveReady);
        assert_eq!(
            inner.prepare_auto_download(&newer_update).unwrap(),
            AutoDownloadAction::Start
        );

        let state = inner.state.read().unwrap();
        assert_eq!(state.snapshot.stage, UpdaterStage::Downloading);
        assert_eq!(state.snapshot.target_version.as_deref(), Some("0.2.2"));
        assert_eq!(
            state
                .ready_update
                .as_ref()
                .map(|update| update.update.version.as_str()),
            Some("0.2.1")
        );
    }

    #[test]
    fn failed_replacement_download_keeps_existing_ready_update() {
        let inner = test_inner();
        let ready_update = test_update("0.2.1", Some("old release"));

        inner.set_ready_to_install(ready_update, vec![1, 2, 3]);
        inner.restore_ready_after_failed_download("Download failed".to_string());

        let state = inner.state.read().unwrap();
        assert_eq!(state.snapshot.stage, UpdaterStage::ReadyToInstall);
        assert_eq!(state.snapshot.target_version.as_deref(), Some("0.2.1"));
        assert_eq!(state.snapshot.release_notes.as_deref(), Some("old release"));
        assert_eq!(state.snapshot.downloaded_bytes, 3);
    }

    #[test]
    fn set_error_clears_stale_update_metadata() {
        let inner = test_inner();
        let update = test_update("0.2.1", Some("release notes"));

        inner.prepare_auto_download(&update).unwrap();
        inner.set_error("boom".to_string());

        let state = inner.state.read().unwrap();
        assert_eq!(state.snapshot.stage, UpdaterStage::Error);
        assert!(state.pending_update.is_none());
        assert!(state.ready_update.is_none());
        assert!(state.snapshot.target_version.is_none());
        assert!(state.snapshot.release_notes.is_none());
        assert_eq!(state.snapshot.downloaded_bytes, 0);
        assert!(state.snapshot.total_bytes.is_none());
    }
}

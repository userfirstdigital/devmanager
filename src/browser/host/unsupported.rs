#[cfg(not(target_os = "windows"))]
use super::super::{
    BrowserBounds, BrowserCommand, BrowserCommandRequest, BrowserHostEvent, BrowserResponse,
};
use super::super::{BrowserError, BrowserHostStatus};
#[cfg(not(target_os = "windows"))]
use super::{BrowserHostState, BrowserWorkspaceSnapshot};
#[cfg(not(target_os = "windows"))]
use std::{marker::PhantomData, path::Path, rc::Rc};

pub fn unsupported_host_status(platform: impl Into<String>) -> BrowserHostStatus {
    let platform = platform.into();
    BrowserHostStatus {
        available: false,
        diagnostic: Some(format!(
            "embedded browser support is unavailable on {platform}"
        )),
        platform,
        version: None,
    }
}

pub fn unsupported_platform_error(platform: impl Into<String>) -> BrowserError {
    BrowserError::UnavailablePlatform {
        platform: platform.into(),
    }
}

#[cfg(not(target_os = "windows"))]
pub struct BrowserWebViewHost {
    status: BrowserHostStatus,
    #[allow(dead_code)]
    state: BrowserHostState,
    _main_thread_only: PhantomData<Rc<()>>,
}

#[cfg(not(target_os = "windows"))]
impl BrowserWebViewHost {
    pub fn new(app_config_dir: impl AsRef<Path>) -> Self {
        Self {
            status: unsupported_host_status(std::env::consts::OS),
            state: BrowserHostState::new(app_config_dir),
            _main_thread_only: PhantomData,
        }
    }

    pub fn status(&self) -> BrowserHostStatus {
        self.status.clone()
    }

    pub fn handle_command(
        &mut self,
        _window: &gpui::Window,
        _workspace_key: &super::super::BrowserWorkspaceKey,
        command: BrowserCommand,
    ) -> Result<BrowserResponse, BrowserError> {
        if command == BrowserCommand::Status {
            Ok(BrowserResponse::Status {
                status: self.status(),
            })
        } else {
            Err(unsupported_platform_error(std::env::consts::OS))
        }
    }

    pub fn handle_request(&mut self, window: &gpui::Window, request: BrowserCommandRequest) {
        let result =
            self.handle_command(window, request.workspace_key(), request.command().clone());
        request.respond(result);
    }

    pub fn pump_async_completions(&mut self, _window: &gpui::Window) {}

    pub fn resolve_approval(
        &mut self,
        _window: &gpui::Window,
        _workspace_key: &super::super::BrowserWorkspaceKey,
        _tab_id: &str,
        _operation_id: &str,
        _approved: bool,
    ) -> Result<(), BrowserError> {
        Err(unsupported_platform_error(std::env::consts::OS))
    }

    pub fn set_active_workspace(
        &mut self,
        _workspace_key: Option<super::super::BrowserWorkspaceKey>,
    ) -> Result<(), BrowserError> {
        Err(unsupported_platform_error(std::env::consts::OS))
    }

    pub fn set_bounds(&mut self, _bounds: BrowserBounds) -> Result<(), BrowserError> {
        Err(unsupported_platform_error(std::env::consts::OS))
    }

    pub fn drain_events(&mut self) -> Vec<BrowserHostEvent> {
        Vec::new()
    }

    pub fn workspace_snapshot(
        &self,
        workspace_key: &super::super::BrowserWorkspaceKey,
    ) -> Option<&BrowserWorkspaceSnapshot> {
        self.state.workspace(workspace_key)
    }
}

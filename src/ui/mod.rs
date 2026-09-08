//! Native GPUI UI foundations.
//!
//! This module is the only boundary allowed to initialize `gpui-component`.

use std::sync::atomic::{AtomicUsize, Ordering};

use gpui::{App, Global};

pub mod actions;
pub mod agent_connection;
pub mod board;
pub mod browser_dock_lifecycle;
pub mod browser_gateway_identity;
pub mod components;
pub mod conversation;
pub mod frame_trace;
pub mod header_actions;
#[path = "composer/mod.rs"]
pub mod native_composer;
pub mod native_fleet;
pub mod native_host_state;
pub mod native_shell;
#[cfg(test)]
mod native_ux_behavior_tests;
pub mod overlay_chrome;
pub mod panel;
pub mod preview;
pub mod preview_capture;
pub mod project_actions;
pub mod project_scope;
pub mod provider_catalog;
pub mod provider_catalog_contract;
pub mod provider_catalog_seeds;
pub mod provider_metadata;
pub mod provider_settings;
pub mod renderers;
pub mod scrollbar;
pub mod shell;
pub mod startup_status;
pub mod startup_trace;
pub mod task_cockpit;
pub mod task_search;
pub mod task_workspace;
pub mod terminal_adapter;
pub mod theme_system;
pub mod tokens;
pub mod workspace_layout;
mod window_identity;

pub use native_shell::{NativeClientDetach, NativeHostFullQuit};
pub use preview::PreviewInitReport;

static COMPONENT_INIT_COUNT: AtomicUsize = AtomicUsize::new(0);

struct ComponentInitialized;

impl Global for ComponentInitialized {}

/// Initialize the shared native component layer exactly once for this GPUI app.
///
/// All future UI features must call this wrapper instead of calling the
/// third-party initializer directly.
pub fn init(cx: &mut App) {
    if cx.try_global::<ComponentInitialized>().is_none() {
        gpui_component::init(cx);
        window_identity::init(cx);
        cx.set_global(ComponentInitialized);
        COMPONENT_INIT_COUNT.fetch_add(1, Ordering::SeqCst);
    }
}

/// Return the number of successful component initializations in this process.
///
/// This is intentionally a small diagnostic surface used by the preview smoke
/// test and can also be used by diagnostics without inspecting third-party
/// global state.
pub fn component_init_count() -> usize {
    COMPONENT_INIT_COUNT.load(Ordering::SeqCst)
}

pub mod prompts;
pub mod quality;
pub use prompts::{
    chain_editor, editor, fixtures, history, library, mutation, picker, put_in_composer_action,
    version_diff, PromptLibraryAction, PromptLibraryKey, PromptLibraryLoadState,
    PromptLibrarySession, MAX_VIRTUALIZED_LINKS, MAX_VIRTUALIZED_PROMPTS,
};
pub use shell::{
    AccessibleName, LibrarySection, PromptLibraryChrome, PromptLibraryUiError,
    PromptLibraryViewport, SyncOrgHooks,
};
pub use task_cockpit::composer;
pub use task_cockpit::{
    project_services_panel, ServiceActionAffordance, ServicePanelAction, ServicePanelRow,
    ServicePanelTone, ServicesPanelProjection,
};

/// Finish a headless acceptance run after the platform event loop starts.
///
/// Linux's calloop resets its stop signal on entry, so quitting synchronously
/// inside `Application::run`'s launch callback loses the request. Dispatching it
/// also lets the callback release its window/entity borrows before teardown.
#[doc(hidden)]
pub fn finish_headless_test(cx: &mut App) {
    cx.spawn(async |cx| {
        cx.update(|cx| cx.quit())
            .expect("headless application is alive");
    })
    .detach();
}

/// Initialize GTK on the process main thread before host/provider threads start.
/// The backend override exists only during GTK initialization; launched tools
/// retain the user's original Wayland/X11 environment.
#[cfg(target_os = "linux")]
pub fn prepare_native_platform() {
    // WebKitGTK's DMA-BUF renderer can produce a black child surface with the
    // proprietary NVIDIA driver. Keep acceleration on other drivers and honor
    // an explicit user override. This must run before WebKit/provider threads.
    // https://v2.tauri.app/develop/debug/linux-graphics/
    if needs_webkit_dmabuf_fallback(
        std::path::Path::new("/sys/module/nvidia").is_dir(),
        std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").as_deref(),
    ) {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
    if !std::env::var_os("DISPLAY").is_some_and(|display| !display.is_empty())
        || gtk::is_initialized()
    {
        return;
    }
    struct RestoreBackend(Option<std::ffi::OsString>);
    impl Drop for RestoreBackend {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var("GDK_BACKEND", value),
                None => std::env::remove_var("GDK_BACKEND"),
            }
        }
    }
    let _restore = RestoreBackend(std::env::var_os("GDK_BACKEND"));
    std::env::set_var("GDK_BACKEND", "x11");
    gtk::gdk::set_allowed_backends("x11");
    let _ = gtk::init();
}

#[cfg(target_os = "linux")]
fn needs_webkit_dmabuf_fallback(
    nvidia_loaded: bool,
    override_value: Option<&std::ffi::OsStr>,
) -> bool {
    nvidia_loaded && override_value.is_none()
}

#[cfg(all(test, target_os = "linux"))]
mod linux_graphics_tests {
    use super::needs_webkit_dmabuf_fallback;
    use std::ffi::OsStr;

    #[test]
    fn webkit_dmabuf_fallback_is_nvidia_only_and_preserves_explicit_overrides() {
        assert!(needs_webkit_dmabuf_fallback(true, None));
        assert!(!needs_webkit_dmabuf_fallback(false, None));
        for value in ["0", "1", ""] {
            assert!(!needs_webkit_dmabuf_fallback(true, Some(OsStr::new(value))));
        }
    }
}

/// The native browser is an X11 child, so use XWayland when it is available.
/// Headless and non-Linux launches keep GPUI's ordinary platform selection.
pub(crate) fn desktop_application() -> gpui::Application {
    #[cfg(target_os = "linux")]
    if std::env::var_os("ZED_HEADLESS").is_none()
        && std::env::var_os("DISPLAY").is_some_and(|display| !display.is_empty())
    {
        return gpui::Application::new_x11().expect("initialize native X11 application");
    }
    gpui::Application::new()
}

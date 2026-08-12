//! Native GPUI UI foundations.
//!
//! This module is the only boundary allowed to initialize `gpui-component`.

use std::sync::atomic::{AtomicUsize, Ordering};

use gpui::{App, Global};

pub mod actions;
pub mod components;
pub mod native_shell;
pub mod preview;
pub mod preview_capture;
pub mod renderers;
pub mod shell;
pub mod task_cockpit;
pub mod terminal_adapter;
pub mod tokens;

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
    MAX_VIRTUALIZED_LINKS, MAX_VIRTUALIZED_PROMPTS, PromptLibraryAction, PromptLibraryKey,
    PromptLibraryLoadState, PromptLibrarySession, chain_editor, editor, fixtures, history, library,
    picker, put_in_composer_action, version_diff,
};
pub use shell::{
    AccessibleName, LibrarySection, PromptLibraryChrome, PromptLibraryUiError,
    PromptLibraryViewport, SyncOrgHooks,
};
pub use task_cockpit::composer;

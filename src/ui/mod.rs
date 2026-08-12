//! Native Prompt Library view-model projections and client-local actions.
//!
//! This is the compile-ready UI contract for Tasks 7.5/7.6/7.8. Host FTS search
//! and history stay fail-closed until Task 7.3; GPUI rail/widget wiring is a
//! later shell step and is not performed here.

pub mod prompts;
pub mod shell;
pub mod task_cockpit;

pub use prompts::{
    chain_editor, editor, fixtures, history, library, picker, put_in_composer_action, version_diff,
    PromptLibraryAction, PromptLibraryKey, PromptLibraryLoadState, PromptLibrarySession,
    MAX_VIRTUALIZED_LINKS, MAX_VIRTUALIZED_PROMPTS,
};
pub use shell::{
    AccessibleName, LibrarySection, PromptLibraryChrome, PromptLibraryUiError,
    PromptLibraryViewport, SyncOrgHooks,
};
pub use task_cockpit::composer;

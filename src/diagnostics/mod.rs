//! Developer environment diagnostics: probes, catalog, repairs, and profile edits.

pub mod catalog;
pub mod model;
pub mod preview;
pub mod probe;
pub mod profile;
pub mod repair;
pub mod resolve;
pub mod runner;
pub mod windows;

pub use model::*;
pub use preview::{format_pending_repairs_preview, format_repair_operation_lines};
pub use probe::DiagnosticProbe;
pub use profile::{
    apply_profile_edit, classify_cc_ast, inspect_marked_block, managed_block_preview,
    parse_cc_ast_probe_output, preview_profile_edit, rollback_profile_edit, CcAstProbeResult,
    CcClassification, MarkedBlockState, ProfileApplyResult, ProfileEditPreview, ProfileError,
    BEGIN_MARKER, END_MARKER,
};
pub use repair::{
    diagnostics_settings_delta_from_plans, execute_repair_batch, validate_plan,
    winget_install_args, DiagnosticsRepairBatchResult, DiagnosticsSettingsDelta, RepairExecutor,
    SettingsRepairSink, WINGET_RESTART_NOTICE,
};
pub use resolve::{collapse_same_directory_installs, resolve_all, resolve_all_on_path};
pub use runner::{
    display_command, elide_home_paths, sanitize_captured, CommandFailure, CommandOutput,
    CommandRunner, CommandSpec, TokioCommandRunner,
};

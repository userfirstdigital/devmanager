//! Thin Command Center view over ClientModel and the shared ActionCatalog.
//!
//! This module does not probe Git, ports, processes, or the filesystem. It
//! does not accept a caller path or `ServiceEvidence`. Missing Git/worktree/
//! service/accounting host pages stay typed Unavailable/HOLD. GPUI focus and
//! token chrome are not owned here (`src/ui/command_center` is absent).
//!
//! ```compile_fail
//! use devmanager::client::command_center::CommandCenterFacts;
//! ```
//! ```compile_fail
//! use devmanager::client::command_center::{PortAuthority, MetricObservation, ProcessAuthorityKind};
//! ```
//! ```compile_fail
//! use devmanager::client::command_center::{PermittedServiceAction, HostActionAdmission, LivePortCapability};
//! ```
//! ```compile_fail
//! use devmanager::client::command_center::CommandCenterInput;
//! use devmanager::services::health::ServiceEvidence;
//! fn takes_evidence(evidence: ServiceEvidence) {
//!     let _ = CommandCenterInput {
//!         model: None,
//!         actions: None,
//!         service_evidence: evidence,
//!     };
//! }
//! ```
//! ```compile_fail
//! use devmanager::client::command_center::{CommandCenterInput, ServiceRow};
//! use devmanager::services::model::ServiceCatalog;
//! fn config_is_not_a_fact_slot(catalog: &ServiceCatalog) {
//!     let _ = CommandCenterInput {
//!         model: None,
//!         catalog: Some(catalog),
//!         actions: None,
//!     };
//!     let _ = ServiceRow::healthy(());
//! }
//! ```
//! ```compile_fail
//! use devmanager::client::command_center::ProjectionSection;
//! let _ = ProjectionSection::<()>::Ready(());
//! ```

use std::fmt;

use crate::{
    client::{action::ActionDescriptor, model::ClientModel},
    process::registry::MAX_PROCESS_DISPLAY_LABEL_BYTES,
    protocol::ClientRequest,
    services::model::{ServiceId, MAX_SERVICE_COUNT},
};

/// Deterministic process-row cap for one Command Center projection.
pub const MAX_COMMAND_CENTER_PROCESS_ROWS: usize = MAX_SERVICE_COUNT;
/// Deterministic service-row cap.
pub const MAX_COMMAND_CENTER_SERVICE_ROWS: usize = MAX_SERVICE_COUNT;
/// Display-label byte cap; matches the process-registry label bound.
pub const MAX_COMMAND_CENTER_LABEL_BYTES: usize = MAX_PROCESS_DISPLAY_LABEL_BYTES;

/// How many iterator items a bounded collector inspected and classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BoundInspection {
    pub inspected: usize,
    pub validated: usize,
    pub duplicates: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandCenterBoundError {
    TooMany { limit: usize, inspected: usize },
    Conflicting,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ProcessFactError {
    EmptyLabel,
    LabelTooLong { actual: usize, max: usize },
    PathOrCommandLineLabel,
    UntrustedLabel,
    InvalidUnicode,
}

impl ProcessFactError {
    fn code(self) -> &'static str {
        match self {
            Self::EmptyLabel => "cc.label.empty",
            Self::LabelTooLong { .. } => "cc.label.too_long",
            Self::PathOrCommandLineLabel => "cc.label.path",
            Self::UntrustedLabel => "cc.label.untrusted",
            Self::InvalidUnicode => "cc.label.unicode",
        }
    }
}

impl fmt::Display for ProcessFactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl fmt::Debug for ProcessFactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// Canonical label derived from trusted configured identity, never from a
/// caller-supplied process command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalProcessLabel(ServiceId);

impl CanonicalProcessLabel {
    pub fn from_service_id(service_id: ServiceId) -> Self {
        Self(service_id)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn service_id(&self) -> &ServiceId {
        &self.0
    }

    pub fn try_from_untrusted_bytes(bytes: &[u8]) -> Result<Self, ProcessFactError> {
        let value = std::str::from_utf8(bytes).map_err(|_| ProcessFactError::InvalidUnicode)?;
        if value.is_empty() || value.trim().is_empty() {
            return Err(ProcessFactError::EmptyLabel);
        }
        if value.len() > MAX_COMMAND_CENTER_LABEL_BYTES {
            return Err(ProcessFactError::LabelTooLong {
                actual: value.len(),
                max: MAX_COMMAND_CENTER_LABEL_BYTES,
            });
        }
        if has_forbidden_unicode(value) {
            return Err(ProcessFactError::UntrustedLabel);
        }
        if looks_like_secret_or_assignment(value) {
            return Err(ProcessFactError::UntrustedLabel);
        }
        if looks_like_path_or_command_line(value) {
            return Err(ProcessFactError::PathOrCommandLineLabel);
        }
        let service_id = ServiceId::new(value).map_err(|_| ProcessFactError::UntrustedLabel)?;
        Ok(Self(service_id))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldDependency {
    GitWorkspaceAuthority,
    ServiceHealth,
    ProcessAccounting,
    PortInventory,
    ActionCatalog,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnavailableReason {
    HostFactMissing,
    BoundExceeded,
    Hold { dependency: HoldDependency },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProjectionSectionInner<T> {
    Ready(T),
    Unavailable { reason: UnavailableReason },
}

/// Sealed section. Callers cannot construct `Ready`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionSection<T>(ProjectionSectionInner<T>);

impl<T> ProjectionSection<T> {
    fn host_ready(value: T) -> Self {
        Self(ProjectionSectionInner::Ready(value))
    }

    fn unavailable(reason: UnavailableReason) -> Self {
        Self(ProjectionSectionInner::Unavailable { reason })
    }

    pub fn ready(&self) -> Option<&T> {
        match &self.0 {
            ProjectionSectionInner::Ready(value) => Some(value),
            ProjectionSectionInner::Unavailable { .. } => None,
        }
    }

    pub fn unavailable_reason(&self) -> Option<UnavailableReason> {
        match self.0 {
            ProjectionSectionInner::Ready(_) => None,
            ProjectionSectionInner::Unavailable { reason } => Some(reason),
        }
    }
}

/// Borrowed ClientModel and ActionCatalog only. `ServiceCatalog` is not an input:
/// config definitions are not HealthTracker facts and cannot open a Ready section.
pub struct CommandCenterInput<'a> {
    pub model: Option<&'a ClientModel>,
    pub actions: Option<&'a [ActionDescriptor]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    id: crate::domain::id::TaskId,
    title: String,
}

impl TaskRow {
    pub fn id(&self) -> crate::domain::id::TaskId {
        self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceRow {
    service_id: ServiceId,
}

impl ServiceRow {
    pub fn service_id(&self) -> &ServiceId {
        &self.service_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortRow {
    service_id: ServiceId,
}

impl PortRow {
    pub fn service_id(&self) -> &ServiceId {
        &self.service_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescribedAction {
    id: &'static str,
}

impl DescribedAction {
    pub fn id(&self) -> &'static str {
        self.id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandCenterView {
    tasks: ProjectionSection<Vec<TaskRow>>,
    services: ProjectionSection<Vec<ServiceRow>>,
    ports: ProjectionSection<Vec<PortRow>>,
    processes: ProjectionSection<()>,
    git: ProjectionSection<()>,
    worktree: ProjectionSection<()>,
    actions: ProjectionSection<Vec<DescribedAction>>,
}

impl CommandCenterView {
    pub fn tasks(&self) -> &ProjectionSection<Vec<TaskRow>> {
        &self.tasks
    }

    pub fn services(&self) -> &ProjectionSection<Vec<ServiceRow>> {
        &self.services
    }

    pub fn ports(&self) -> &ProjectionSection<Vec<PortRow>> {
        &self.ports
    }

    pub fn processes(&self) -> &ProjectionSection<()> {
        &self.processes
    }

    pub fn git(&self) -> &ProjectionSection<()> {
        &self.git
    }

    pub fn worktree(&self) -> &ProjectionSection<()> {
        &self.worktree
    }

    pub fn actions(&self) -> &ProjectionSection<Vec<DescribedAction>> {
        &self.actions
    }
}

/// Project one immutable Command Center view. Pure: no OS/Git/port probes.
pub fn project_command_center(input: &CommandCenterInput<'_>) -> CommandCenterView {
    CommandCenterView {
        tasks: project_tasks(input.model),
        services: service_section_without_health_tracker(),
        ports: project_ports(),
        processes: ProjectionSection::unavailable(UnavailableReason::Hold {
            dependency: HoldDependency::ProcessAccounting,
        }),
        git: ProjectionSection::unavailable(UnavailableReason::Hold {
            dependency: HoldDependency::GitWorkspaceAuthority,
        }),
        worktree: ProjectionSection::unavailable(UnavailableReason::Hold {
            dependency: HoldDependency::GitWorkspaceAuthority,
        }),
        actions: action_section_without_host_request(input.actions),
    }
}

/// Canonical action output is `ClientRequest` (Phase 2.4). This view does not
/// mint envelopes; catalogued service ActionIds stay Hold until a host factory
/// exists. Unknown ids remain HostFactMissing.
pub fn request_action(
    actions: Option<&[ActionDescriptor]>,
    action_id: &str,
) -> Result<ClientRequest, UnavailableReason> {
    let Some(actions) = actions else {
        return Err(UnavailableReason::HostFactMissing);
    };
    if !actions.iter().any(|descriptor| descriptor.id == action_id) {
        return Err(UnavailableReason::HostFactMissing);
    }
    Err(UnavailableReason::Hold {
        dependency: HoldDependency::ActionCatalog,
    })
}

/// Bounded unique collect. Identical items dedupe; conflicting keys reject.
pub fn collect_unique<T, K>(
    iter: impl IntoIterator<Item = T>,
    cap: usize,
    key: impl Fn(&T) -> K,
) -> Result<(Vec<T>, BoundInspection), CommandCenterBoundError>
where
    T: PartialEq,
    K: Clone + Ord,
{
    let mut rows = Vec::new();
    let mut by_key = std::collections::BTreeMap::new();
    let mut inspection = BoundInspection::default();
    let mut iter = iter.into_iter();
    loop {
        if inspection.inspected == cap {
            if iter.next().is_some() {
                inspection.inspected = inspection.inspected.saturating_add(1);
                return Err(CommandCenterBoundError::TooMany {
                    limit: cap,
                    inspected: inspection.inspected,
                });
            }
            break;
        }
        match iter.next() {
            Some(item) => {
                inspection.inspected = inspection.inspected.saturating_add(1);
                let item_key = key(&item);
                if let Some(existing_index) = by_key.get(&item_key).copied() {
                    if rows[existing_index] == item {
                        inspection.validated = inspection.validated.saturating_add(1);
                        inspection.duplicates = inspection.duplicates.saturating_add(1);
                        continue;
                    }
                    return Err(CommandCenterBoundError::Conflicting);
                }
                inspection.validated = inspection.validated.saturating_add(1);
                by_key.insert(item_key, rows.len());
                rows.push(item);
            }
            None => break,
        }
    }
    Ok((rows, inspection))
}

fn project_tasks(model: Option<&ClientModel>) -> ProjectionSection<Vec<TaskRow>> {
    let Some(model) = model else {
        return ProjectionSection::unavailable(UnavailableReason::HostFactMissing);
    };
    let mut rows = Vec::new();
    for (id, snapshot) in model.tasks() {
        if rows.len() == MAX_COMMAND_CENTER_SERVICE_ROWS {
            return ProjectionSection::unavailable(UnavailableReason::BoundExceeded);
        }
        // TaskId + title only. Do not copy WorkspaceRef paths; git/worktree stay HOLD.
        rows.push(TaskRow {
            id: *id,
            title: snapshot.task.title.clone(),
        });
    }
    ProjectionSection::host_ready(rows)
}

/// Entire service section is Hold until a host HealthTracker page exists.
/// `ServiceCatalog::definitions` must not be mapped into `Ready`.
fn service_section_without_health_tracker() -> ProjectionSection<Vec<ServiceRow>> {
    ProjectionSection::unavailable(UnavailableReason::Hold {
        dependency: HoldDependency::ServiceHealth,
    })
}

fn project_ports() -> ProjectionSection<Vec<PortRow>> {
    ProjectionSection::unavailable(UnavailableReason::Hold {
        dependency: HoldDependency::PortInventory,
    })
}

/// Catalog descriptors are not an action history. No Ready list, and no
/// `ClientRequest`, until a host-issued factory observes the ActionId.
fn action_section_without_host_request(
    actions: Option<&[ActionDescriptor]>,
) -> ProjectionSection<Vec<DescribedAction>> {
    let _ = actions;
    ProjectionSection::unavailable(UnavailableReason::Hold {
        dependency: HoldDependency::ActionCatalog,
    })
}

fn has_forbidden_unicode(value: &str) -> bool {
    value.chars().any(has_forbidden_scalar)
}

fn has_forbidden_scalar(character: char) -> bool {
    let code = character as u32;
    character.is_control()
        || character == '\u{007f}'
        || (0x202A..=0x202E).contains(&code)
        || (0x2066..=0x2069).contains(&code)
        || matches!(character, '\u{200e}' | '\u{200f}' | '\u{061c}')
        || (0xFDD0..=0xFDEF).contains(&code)
        || matches!(character, '\u{fffe}' | '\u{ffff}')
}

fn looks_like_secret_or_assignment(value: &str) -> bool {
    if value.contains('=') {
        return true;
    }
    let lower = value.to_ascii_lowercase();
    lower.contains("token")
        || lower.contains("bearer")
        || lower.contains("private-key")
        || lower.contains("private_key")
        || lower.contains("private key")
        || lower.contains("-----begin")
        || lower.contains("api_key")
        || lower.contains("secret")
}

fn looks_like_path_or_command_line(value: &str) -> bool {
    value.contains('/')
        || value.contains('\\')
        || value.contains('\0')
        || value.chars().any(char::is_whitespace)
        || value.contains("--")
        || is_windows_drive_prefix(value)
}

fn is_windows_drive_prefix(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

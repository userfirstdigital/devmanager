//! Portable ownership and validation boundary for a future host-owned browser surface.
//!
//! This module deliberately does not create a WebView2 controller, call Win32, or
//! attach a GPUI window. It records the facts that a future host bridge must
//! prove before it may do those things. In particular, the client receives an
//! opaque window token, never a raw pointer or an unvalidated platform handle.
//!
//! Authority is the exact task, agent session, and runtime generation captured at
//! registration. HWND parentage is captured once on the host UI/COM thread and
//! reused; attach, input, bounds, focus, and task-follow never probe processes or
//! windows. Automation stays outside this catalog and must be authorized separately.

use crate::domain::id::{
    AgentSessionId, BrowserContextId, BrowserTabId, ClientId, RequestId, ResourceId, TaskId,
};
use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

pub const BROWSER_SURFACE_FIXTURE_VISIBLE_TOKEN: &str = "dm-surface-visible-v1";
pub const BROWSER_SURFACE_FIXTURE_CLICK_TOKEN: &str = "dm-surface-trusted-click-v1";
pub const BROWSER_SURFACE_FIXTURE_RETAINED_STATE: &str = "dm-surface-retained-state-v1";
pub const MAX_SURFACE_TEXT_INPUT_BYTES: usize = 4096;
pub const MAX_SURFACE_TARGET_TOKEN_BYTES: usize = 256;
pub const MAX_SURFACE_EVENTS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessIdentityError {
    ZeroPid,
    ZeroCreationTime,
    EmptyExecutable,
}

impl fmt::Display for ProcessIdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroPid => write!(f, "process PID must be non-zero"),
            Self::ZeroCreationTime => write!(f, "process creation time must be non-zero"),
            Self::EmptyExecutable => write!(f, "process executable must be non-empty"),
        }
    }
}

impl std::error::Error for ProcessIdentityError {}

/// A portable process identity. PID alone is not sufficient because it can be
/// reused; the creation time and executable path are part of the match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessIdentity {
    pid: u32,
    creation_time_100ns: u64,
    executable: PathBuf,
}

/// Alias used by host-facing callers that want the ownership role explicit.
pub type HostProcessIdentity = ProcessIdentity;

impl ProcessIdentity {
    pub fn new(
        pid: u32,
        creation_time_100ns: u64,
        executable: impl Into<PathBuf>,
    ) -> Result<Self, ProcessIdentityError> {
        if pid == 0 {
            return Err(ProcessIdentityError::ZeroPid);
        }
        if creation_time_100ns == 0 {
            return Err(ProcessIdentityError::ZeroCreationTime);
        }
        let executable = executable.into();
        if executable.as_os_str().is_empty() {
            return Err(ProcessIdentityError::EmptyExecutable);
        }
        Ok(Self {
            pid,
            creation_time_100ns,
            executable,
        })
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn creation_time_100ns(&self) -> u64 {
        self.creation_time_100ns
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn matches(&self, other: &Self) -> bool {
        self == other
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceWindowHandleError {
    Zero,
    InvalidWireValue,
}

impl fmt::Display for SurfaceWindowHandleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero => write!(f, "surface window handle must be non-zero"),
            Self::InvalidWireValue => write!(f, "invalid opaque surface window handle"),
        }
    }
}

impl std::error::Error for SurfaceWindowHandleError {}

/// An HWND-shaped value that is safe to carry over a protocol boundary.
///
/// The wire representation is a validated string (`hwnd:<non-zero-u64>`),
/// not a pointer, `usize`, or platform-specific raw handle type. Only the
/// future host bridge should turn the validated value back into an HWND.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SurfaceWindowHandle(String);

impl SurfaceWindowHandle {
    pub fn from_raw(raw: u64) -> Result<Self, SurfaceWindowHandleError> {
        if raw == 0 {
            return Err(SurfaceWindowHandleError::Zero);
        }
        Ok(Self(format!("hwnd:{raw}")))
    }

    pub fn from_wire(wire: impl Into<String>) -> Result<Self, SurfaceWindowHandleError> {
        let wire = wire.into();
        let Some(raw) = wire.strip_prefix("hwnd:") else {
            return Err(SurfaceWindowHandleError::InvalidWireValue);
        };
        if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(SurfaceWindowHandleError::InvalidWireValue);
        }
        let value = raw
            .parse::<u64>()
            .map_err(|_| SurfaceWindowHandleError::InvalidWireValue)?;
        Self::from_raw(value)
    }

    /// Returns the validated integer only at the host/platform boundary.
    pub fn raw_value(&self) -> u64 {
        self.0
            .strip_prefix("hwnd:")
            .and_then(|raw| raw.parse().ok())
            .expect("SurfaceWindowHandle invariant violated")
    }

    pub fn wire_value(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SurfaceWindowHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostHwndOwnershipError {
    ChildEqualsParking,
    NotParentedToParking,
    WrongThreadAffinity,
}

impl fmt::Display for HostHwndOwnershipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChildEqualsParking => {
                write!(
                    f,
                    "WebView2 child HWND must not equal the host parking HWND"
                )
            }
            Self::NotParentedToParking => {
                write!(
                    f,
                    "WebView2 child HWND must be parented to the host parking HWND"
                )
            }
            Self::WrongThreadAffinity => {
                write!(
                    f,
                    "WebView2 HWND ownership must be captured on the host UI/COM thread"
                )
            }
        }
    }
}

impl std::error::Error for HostHwndOwnershipError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SurfaceThreadAffinity {
    HostUiComThread,
}

/// Host-captured WebView2 HWND ownership. Parking HWND stays host-registry
/// state; only the child token is later copied onto a client descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostHwndOwnership {
    child_hwnd: SurfaceWindowHandle,
    parking_hwnd: SurfaceWindowHandle,
    child_parented_to_parking: bool,
    thread_affinity: SurfaceThreadAffinity,
}

impl HostHwndOwnership {
    pub fn new(
        child_hwnd: SurfaceWindowHandle,
        parking_hwnd: SurfaceWindowHandle,
        child_parented_to_parking: bool,
        on_host_ui_com_thread: bool,
    ) -> Result<Self, HostHwndOwnershipError> {
        if child_hwnd == parking_hwnd {
            return Err(HostHwndOwnershipError::ChildEqualsParking);
        }
        if !child_parented_to_parking {
            return Err(HostHwndOwnershipError::NotParentedToParking);
        }
        if !on_host_ui_com_thread {
            return Err(HostHwndOwnershipError::WrongThreadAffinity);
        }
        Ok(Self {
            child_hwnd,
            parking_hwnd,
            child_parented_to_parking: true,
            thread_affinity: SurfaceThreadAffinity::HostUiComThread,
        })
    }

    pub fn child_hwnd(&self) -> &SurfaceWindowHandle {
        &self.child_hwnd
    }

    pub fn parking_hwnd(&self) -> &SurfaceWindowHandle {
        &self.parking_hwnd
    }

    pub fn child_parented_to_parking(&self) -> bool {
        self.child_parented_to_parking
    }

    pub fn thread_affinity(&self) -> SurfaceThreadAffinity {
        self.thread_affinity
    }

    fn validate(&self) -> Result<(), SurfaceError> {
        if self.child_hwnd == self.parking_hwnd
            || !self.child_parented_to_parking
            || self.thread_affinity != SurfaceThreadAffinity::HostUiComThread
        {
            return Err(SurfaceError::InvalidHwndOwnership);
        }
        Ok(())
    }
}

impl Serialize for SurfaceWindowHandle {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SurfaceWindowHandle {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = String::deserialize(deserializer)?;
        Self::from_wire(wire).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceNonceError {
    Zero,
}

impl fmt::Display for SurfaceNonceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero => write!(f, "surface nonce must not be all zeroes"),
        }
    }
}

impl std::error::Error for SurfaceNonceError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SurfaceNonce([u8; 16]);

impl SurfaceNonce {
    pub fn new(bytes: [u8; 16]) -> Result<Self, SurfaceNonceError> {
        if bytes.iter().all(|byte| *byte == 0) {
            return Err(SurfaceNonceError::Zero);
        }
        Ok(Self(bytes))
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceEpochError {
    Zero,
    Exhausted,
}

impl fmt::Display for SurfaceEpochError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero => write!(f, "surface epoch must be non-zero"),
            Self::Exhausted => write!(f, "surface epoch space is exhausted"),
        }
    }
}

impl std::error::Error for SurfaceEpochError {}

macro_rules! define_surface_counter {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(u64);

        impl $name {
            pub fn new(value: u64) -> Result<Self, SurfaceEpochError> {
                if value == 0 {
                    return Err(SurfaceEpochError::Zero);
                }
                Ok(Self(value))
            }

            pub const fn initial() -> Self {
                Self(1)
            }

            pub fn value(self) -> u64 {
                self.0
            }

            #[allow(dead_code)]
            fn next(self) -> Result<Self, SurfaceEpochError> {
                self.0
                    .checked_add(1)
                    .map(Self)
                    .ok_or(SurfaceEpochError::Exhausted)
            }
        }
    };
}

define_surface_counter!(RuntimeGeneration);
define_surface_counter!(BoundsEpoch);
define_surface_counter!(FocusEpoch);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhysicalBoundsError {
    ZeroWidth,
    ZeroHeight,
}

impl fmt::Display for PhysicalBoundsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroWidth => write!(f, "surface physical width must be non-zero"),
            Self::ZeroHeight => write!(f, "surface physical height must be non-zero"),
        }
    }
}

impl std::error::Error for PhysicalBoundsError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhysicalBounds {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl PhysicalBounds {
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Result<Self, PhysicalBoundsError> {
        if width == 0 {
            return Err(PhysicalBoundsError::ZeroWidth);
        }
        if height == 0 {
            return Err(PhysicalBoundsError::ZeroHeight);
        }
        Ok(Self {
            x,
            y,
            width,
            height,
        })
    }

    fn contains_content_point(&self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && (x as u32) < self.width && (y as u32) < self.height
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DpiScaleError {
    Zero,
    TooLarge,
}

impl fmt::Display for DpiScaleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero => write!(f, "surface DPI scale must be non-zero"),
            Self::TooLarge => write!(f, "surface DPI scale is outside the portable range"),
        }
    }
}

impl std::error::Error for DpiScaleError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DpiScale(u16);

impl DpiScale {
    pub fn new(scale_percent: u16) -> Result<Self, DpiScaleError> {
        if scale_percent == 0 {
            return Err(DpiScaleError::Zero);
        }
        if scale_percent > 1000 {
            return Err(DpiScaleError::TooLarge);
        }
        Ok(Self(scale_percent))
    }

    pub fn scale_percent(self) -> u16 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserSurfaceIdentity {
    pub task_id: TaskId,
    pub session_id: AgentSessionId,
    pub context_id: BrowserContextId,
    pub resource_id: ResourceId,
}

impl BrowserSurfaceIdentity {
    pub const fn new(
        task_id: TaskId,
        session_id: AgentSessionId,
        context_id: BrowserContextId,
        resource_id: ResourceId,
    ) -> Self {
        Self {
            task_id,
            session_id,
            context_id,
            resource_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SurfaceAuthority {
    pub task_id: TaskId,
    pub session_id: AgentSessionId,
    pub runtime_generation: RuntimeGeneration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SurfaceOwner {
    Host,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SurfacePermission {
    Observe,
    Attach,
    Resize,
    Focus,
    TrustedClick,
    TextInput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfacePermissions(BTreeSet<SurfacePermission>);

impl SurfacePermissions {
    fn host_default() -> Self {
        Self(BTreeSet::from([
            SurfacePermission::Observe,
            SurfacePermission::Attach,
            SurfacePermission::Resize,
            SurfacePermission::Focus,
            SurfacePermission::TrustedClick,
            SurfacePermission::TextInput,
        ]))
    }

    pub fn contains(&self, permission: SurfacePermission) -> bool {
        self.0.contains(&permission)
    }

    pub fn iter(&self) -> impl Iterator<Item = &SurfacePermission> {
        self.0.iter()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceAuthorization {
    owner: SurfaceOwner,
    permissions: SurfacePermissions,
}

impl SurfaceAuthorization {
    fn host_default() -> Self {
        Self {
            owner: SurfaceOwner::Host,
            permissions: SurfacePermissions::host_default(),
        }
    }

    pub fn automation_granted(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserSurfaceDescriptor {
    pub identity: BrowserSurfaceIdentity,
    pub host_process: ProcessIdentity,
    pub child_hwnd: SurfaceWindowHandle,
    pub nonce: SurfaceNonce,
    pub runtime_generation: RuntimeGeneration,
    pub bounds_epoch: BoundsEpoch,
    pub focus_epoch: FocusEpoch,
    pub physical_bounds: PhysicalBounds,
    pub dpi: DpiScale,
    authorization: SurfaceAuthorization,
}

impl BrowserSurfaceDescriptor {
    pub fn owner(&self) -> SurfaceOwner {
        self.authorization.owner
    }

    pub fn allows(&self, permission: SurfacePermission) -> bool {
        self.authorization.permissions.contains(permission)
    }

    pub fn allows_automation(&self) -> bool {
        self.authorization.automation_granted()
    }

    pub fn permissions(&self) -> impl Iterator<Item = &SurfacePermission> {
        self.authorization.permissions.iter()
    }

    pub fn authority(&self) -> SurfaceAuthority {
        SurfaceAuthority {
            task_id: self.identity.task_id,
            session_id: self.identity.session_id,
            runtime_generation: self.runtime_generation,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SurfaceParkReason {
    Initial,
    Explicit,
    TaskSwitch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SurfaceDetachReason {
    ClientRequested,
    ClientCrashed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SurfaceTeardownReason {
    ContextClosed,
    HostShutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SurfaceLifecycle {
    Parked { reason: SurfaceParkReason },
    Attached { client_id: ClientId },
    Detached { reason: SurfaceDetachReason },
    Terminal { reason: SurfaceTeardownReason },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurfaceDescriptorField {
    ResourceId,
    Identity,
    HostProcess,
    ChildWindow,
    Nonce,
    Authorization,
    RuntimeGeneration,
    BoundsEpoch,
    FocusEpoch,
    Geometry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurfaceError {
    DuplicateResource {
        resource_id: ResourceId,
    },
    MissingSurface {
        resource_id: ResourceId,
    },
    ForeignDescriptor {
        field: SurfaceDescriptorField,
    },
    StaleDescriptor {
        field: SurfaceDescriptorField,
    },
    InvalidLifecycle {
        operation: &'static str,
        lifecycle: SurfaceLifecycle,
    },
    ActiveSurfaceConflict {
        resource_id: ResourceId,
    },
    ClientMismatch {
        expected: ClientId,
        actual: ClientId,
    },
    ClientProcessMismatch {
        client_id: ClientId,
    },
    ClientNotAttached {
        client_id: ClientId,
    },
    PermissionDenied {
        permission: SurfacePermission,
    },
    InputRequiresFocus,
    InputOutsideBounds,
    InputTooLarge {
        bytes: usize,
        max: usize,
    },
    InvalidTeardownProof {
        field: &'static str,
    },
    InvalidHwndOwnership,
    DuplicateHwnd {
        hwnd: SurfaceWindowHandle,
    },
    TaskSurfaceConflict {
        task_id: TaskId,
    },
    DuplicateRequest {
        request_id: RequestId,
    },
    AutomationSeparatelyAuthorized,
    EpochExhausted,
}

impl fmt::Display for SurfaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateResource { resource_id } => {
                write!(
                    f,
                    "browser surface resource {resource_id} is already registered"
                )
            }
            Self::MissingSurface { resource_id } => {
                write!(
                    f,
                    "browser surface resource {resource_id} is not registered"
                )
            }
            Self::ForeignDescriptor { field } => {
                write!(f, "foreign surface descriptor field {field:?}")
            }
            Self::S…5887 tokens truncated…  incoming.focused = false;
        }
        self.active_resource_id = Some(incoming_id);
        let incoming_receipt = self.receipt(incoming_id);
        self.record_event(
            RequestId::new(),
            SurfaceEventKind::TaskFollowed,
            &incoming_receipt.descriptor,
        )?;
        Ok(SurfaceTaskSwitchReceipt {
            outgoing: self.receipt(outgoing_id),
            incoming: incoming_receipt,
            pointer_consumed: true,
        })
    }

    pub fn receive_bounds(
        &mut self,
        update: SurfaceBoundsUpdate,
    ) -> Result<SurfaceReceipt, SurfaceError> {
        let resource_id = self.validate_descriptor(&update.descriptor)?;
        self.validate_attached_client(resource_id, update.client_id)?;
        let record = self.surfaces.get(&resource_id).expect("surface exists");
        if !record.descriptor.allows(SurfacePermission::Resize) {
            return Err(SurfaceError::PermissionDenied {
                permission: SurfacePermission::Resize,
            });
        }
        let _ = update.client_sequence;
        let next_bounds = record
            .descriptor
            .bounds_epoch
            .next()
            .map_err(|_| SurfaceError::EpochExhausted)?;
        let record = self.surfaces.get_mut(&resource_id).expect("surface exists");
        record.descriptor.physical_bounds = update.physical_bounds;
        record.descriptor.dpi = update.dpi;
        record.descriptor.bounds_epoch = next_bounds;
        let receipt = self.receipt(resource_id);
        self.record_event(
            RequestId::new(),
            SurfaceEventKind::BoundsUpdated,
            &receipt.descriptor,
        )?;
        Ok(receipt)
    }

    pub fn receive_focus(
        &mut self,
        update: SurfaceFocusUpdate,
    ) -> Result<SurfaceReceipt, SurfaceError> {
        let resource_id = self.validate_descriptor(&update.descriptor)?;
        self.validate_attached_client(resource_id, update.client_id)?;
        let record = self.surfaces.get(&resource_id).expect("surface exists");
        if !record.descriptor.allows(SurfacePermission::Focus) {
            return Err(SurfaceError::PermissionDenied {
                permission: SurfacePermission::Focus,
            });
        }
        let _ = update.client_sequence;
        let next_focus = record
            .descriptor
            .focus_epoch
            .next()
            .map_err(|_| SurfaceError::EpochExhausted)?;
        let record = self.surfaces.get_mut(&resource_id).expect("surface exists");
        record.descriptor.focus_epoch = next_focus;
        record.focused = update.focused;
        let receipt = self.receipt(resource_id);
        self.record_event(
            RequestId::new(),
            SurfaceEventKind::FocusUpdated,
            &receipt.descriptor,
        )?;
        Ok(receipt)
    }

    pub fn receive_input(
        &mut self,
        request: SurfaceInputRequest,
    ) -> Result<SurfaceInputReceipt, SurfaceError> {
        let resource_id = self.validate_descriptor(&request.descriptor)?;
        self.validate_attached_client(resource_id, request.client_id)?;
        let record = self.surfaces.get(&resource_id).expect("surface exists");
        if !record.focused {
            return Err(SurfaceError::InputRequiresFocus);
        }
        let permission = match &request.action {
            SurfaceInputAction::TrustedClick { .. } => SurfacePermission::TrustedClick,
            SurfaceInputAction::TextInput { .. } => SurfacePermission::TextInput,
        };
        if !record.descriptor.allows(permission) {
            return Err(SurfaceError::PermissionDenied { permission });
        }
        match &request.action {
            SurfaceInputAction::TrustedClick { x, y, target_token } => {
                if target_token.len() > MAX_SURFACE_TARGET_TOKEN_BYTES {
                    return Err(SurfaceError::InputTooLarge {
                        bytes: target_token.len(),
                        max: MAX_SURFACE_TARGET_TOKEN_BYTES,
                    });
                }
                if !record
                    .descriptor
                    .physical_bounds
                    .contains_content_point(*x, *y)
                {
                    return Err(SurfaceError::InputOutsideBounds);
                }
            }
            SurfaceInputAction::TextInput { text } => {
                if text.len() > MAX_SURFACE_TEXT_INPUT_BYTES {
                    return Err(SurfaceError::InputTooLarge {
                        bytes: text.len(),
                        max: MAX_SURFACE_TEXT_INPUT_BYTES,
                    });
                }
            }
        }
        let receipt = SurfaceInputReceipt {
            descriptor: record.descriptor.clone(),
            action: request.action,
        };
        self.record_event(
            RequestId::new(),
            SurfaceEventKind::InputAccepted,
            &receipt.descriptor,
        )?;
        Ok(receipt)
    }

    pub fn close_context(
        &mut self,
        proof: HostTeardownProof,
    ) -> Result<SurfaceReceipt, SurfaceError> {
        let resource_id = self.validate_descriptor(&proof.descriptor)?;
        if self.host_process != proof.host_process {
            return Err(SurfaceError::ForeignDescriptor {
                field: SurfaceDescriptorField::HostProcess,
            });
        }
        let lifecycle = self
            .surfaces
            .get(&resource_id)
            .expect("surface exists")
            .lifecycle;
        if matches!(lifecycle, SurfaceLifecycle::Terminal { .. }) {
            return Err(SurfaceError::InvalidLifecycle {
                operation: "close context",
                lifecycle,
            });
        }
        if !matches!(lifecycle, SurfaceLifecycle::Parked { .. }) {
            return Err(SurfaceError::InvalidLifecycle {
                operation: "close context",
                lifecycle,
            });
        }
        for (condition, field) in [
            (proof.surface_parked, "surface parked"),
            (proof.controller_closed, "controller closed"),
            (proof.environment_closed, "environment closed"),
            (proof.context_closed, "context closed"),
        ] {
            if !condition {
                return Err(SurfaceError::InvalidTeardownProof { field });
            }
        }
        if proof.helper_processes_remaining != 0 {
            return Err(SurfaceError::InvalidTeardownProof {
                field: "zero helper processes",
            });
        }
        if self.active_resource_id == Some(resource_id) {
            self.active_resource_id = None;
        }
        let task_id = proof.descriptor.identity.task_id;
        self.advance_attachment_epochs(resource_id)?;
        let record = self.surfaces.get_mut(&resource_id).expect("surface exists");
        record.lifecycle = SurfaceLifecycle::Terminal {
            reason: proof.reason,
        };
        record.client = None;
        record.focused = false;
        if self.live_task_surfaces.get(&task_id) == Some(&resource_id) {
            self.live_task_surfaces.remove(&task_id);
        }
        let receipt = self.receipt(resource_id);
        self.record_event(
            RequestId::new(),
            SurfaceEventKind::Closed,
            &receipt.descriptor,
        )?;
        Ok(receipt)
    }

    fn bind_client(
        &mut self,
        request: SurfaceAttachRequest,
        detached_only: bool,
        operation: &'static str,
    ) -> Result<SurfaceReceipt, SurfaceError> {
        let resource_id = self.validate_descriptor(&request.descriptor)?;
        if let Some(active_resource_id) = self.active_resource_id {
            if active_resource_id != resource_id {
                return Err(SurfaceError::ActiveSurfaceConflict { resource_id });
            }
        }
        let lifecycle = {
            let record = self.surfaces.get(&resource_id).expect("surface exists");
            record.hwnd_ownership.validate()?;
            if !record.descriptor.allows(SurfacePermission::Attach) {
                return Err(SurfaceError::PermissionDenied {
                    permission: SurfacePermission::Attach,
                });
            }
            record.lifecycle
        };
        let allowed = if detached_only {
            matches!(lifecycle, SurfaceLifecycle::Detached { .. })
        } else {
            matches!(lifecycle, SurfaceLifecycle::Parked { .. })
        };
        if !allowed {
            return Err(SurfaceError::InvalidLifecycle {
                operation,
                lifecycle,
            });
        }
        self.advance_attachment_epochs(resource_id)?;
        let record = self.surfaces.get_mut(&resource_id).expect("surface exists");
        record.lifecycle = SurfaceLifecycle::Attached {
            client_id: request.client.id,
        };
        record.client = Some(request.client);
        record.focused = false;
        self.active_resource_id = Some(resource_id);
        let receipt = self.receipt(resource_id);
        self.record_event(
            RequestId::new(),
            SurfaceEventKind::Attached,
            &receipt.descriptor,
        )?;
        Ok(receipt)
    }

    fn validate_descriptor(
        &self,
        descriptor: &BrowserSurfaceDescriptor,
    ) -> Result<ResourceId, SurfaceError> {
        let resource_id = descriptor.identity.resource_id;
        let Some(record) = self.surfaces.get(&resource_id) else {
            return Err(SurfaceError::ForeignDescriptor {
                field: SurfaceDescriptorField::ResourceId,
            });
        };
        let expected = &record.descriptor;
        if descriptor.identity != expected.identity {
            return Err(SurfaceError::ForeignDescriptor {
                field: SurfaceDescriptorField::Identity,
            });
        }
        if descriptor.host_process != expected.host_process {
            return Err(SurfaceError::ForeignDescriptor {
                field: SurfaceDescriptorField::HostProcess,
            });
        }
        if descriptor.child_hwnd != expected.child_hwnd {
            return Err(SurfaceError::ForeignDescriptor {
                field: SurfaceDescriptorField::ChildWindow,
            });
        }
        if descriptor.nonce != expected.nonce {
            return Err(SurfaceError::ForeignDescriptor {
                field: SurfaceDescriptorField::Nonce,
            });
        }
        if descriptor.authorization != expected.authorization {
            return Err(SurfaceError::ForeignDescriptor {
                field: SurfaceDescriptorField::Authorization,
            });
        }
        if descriptor.runtime_generation != expected.runtime_generation {
            return Err(SurfaceError::StaleDescriptor {
                field: SurfaceDescriptorField::RuntimeGeneration,
            });
        }
        if descriptor.bounds_epoch != expected.bounds_epoch {
            return Err(SurfaceError::StaleDescriptor {
                field: SurfaceDescriptorField::BoundsEpoch,
            });
        }
        if descriptor.focus_epoch != expected.focus_epoch {
            return Err(SurfaceError::StaleDescriptor {
                field: SurfaceDescriptorField::FocusEpoch,
            });
        }
        if descriptor.physical_bounds != expected.physical_bounds || descriptor.dpi != expected.dpi
        {
            return Err(SurfaceError::StaleDescriptor {
                field: SurfaceDescriptorField::Geometry,
            });
        }
        Ok(resource_id)
    }

    fn validate_attached_client(
        &self,
        resource_id: ResourceId,
        client_id: ClientId,
    ) -> Result<(), SurfaceError> {
        let record = self.surfaces.get(&resource_id).expect("surface exists");
        let Some(client) = record.client.as_ref() else {
            return Err(SurfaceError::ClientNotAttached { client_id });
        };
        if client.id != client_id {
            return Err(SurfaceError::ClientMismatch {
                expected: client.id,
                actual: client_id,
            });
        }
        if !matches!(record.lifecycle, SurfaceLifecycle::Attached { .. }) {
            return Err(SurfaceError::InvalidLifecycle {
                operation: "use surface",
                lifecycle: record.lifecycle,
            });
        }
        Ok(())
    }

    fn advance_attachment_epochs(&mut self, resource_id: ResourceId) -> Result<(), SurfaceError> {
        let record = self.surfaces.get(&resource_id).expect("surface exists");
        let next_bounds = record
            .descriptor
            .bounds_epoch
            .next()
            .map_err(|_| SurfaceError::EpochExhausted)?;
        let next_focus = record
            .descriptor
            .focus_epoch
            .next()
            .map_err(|_| SurfaceError::EpochExhausted)?;
        let record = self.surfaces.get_mut(&resource_id).expect("surface exists");
        record.descriptor.bounds_epoch = next_bounds;
        record.descriptor.focus_epoch = next_focus;
        Ok(())
    }

    fn receipt(&self, resource_id: ResourceId) -> SurfaceReceipt {
        let record = self.surfaces.get(&resource_id).expect("surface exists");
        SurfaceReceipt {
            descriptor: record.descriptor.clone(),
            lifecycle: record.lifecycle,
        }
    }

    fn reject_duplicate_hwnd(&self, ownership: &HostHwndOwnership) -> Result<(), SurfaceError> {
        for record in self.surfaces.values() {
            if matches!(record.lifecycle, SurfaceLifecycle::Terminal { .. }) {
                continue;
            }
            if record.hwnd_ownership.child_hwnd() == ownership.child_hwnd()
                || record.hwnd_ownership.parking_hwnd() == ownership.parking_hwnd()
                || record.hwnd_ownership.child_hwnd() == ownership.parking_hwnd()
                || record.hwnd_ownership.parking_hwnd() == ownership.child_hwnd()
            {
                return Err(SurfaceError::DuplicateHwnd {
                    hwnd: ownership.child_hwnd().clone(),
                });
            }
        }
        Ok(())
    }

    fn record_event(
        &mut self,
        request_id: RequestId,
        kind: SurfaceEventKind,
        descriptor: &BrowserSurfaceDescriptor,
    ) -> Result<(), SurfaceError> {
        let event = SurfaceEvent {
            request_id,
            kind,
            authority: descriptor.authority(),
            resource_id: descriptor.identity.resource_id,
            descriptor: descriptor.clone(),
        };
        if self.events.len() >= MAX_SURFACE_EVENTS {
            self.events.remove(0);
        }
        self.events.push(RecordedSurfaceEvent { request_id, event });
        Ok(())
    }

    fn replace_last_request_id(
        &mut self,
        request_id: RequestId,
    ) -> Result<SurfaceEvent, SurfaceError> {
        let last = self
            .events
            .last_mut()
            .expect("successful surface action records an event");
        last.request_id = request_id;
        last.event.request_id = request_id;
        Ok(last.event.clone())
    }

    fn snapshot_for(
        &self,
        resource_id: ResourceId,
        record: &SurfaceRecord,
    ) -> BrowserSurfaceSnapshot {
        let _parking_hwnd_is_host_only = record.hwnd_ownership.parking_hwnd();
        BrowserSurfaceSnapshot {
            descriptor: record.descriptor.clone(),
            lifecycle: record.lifecycle,
            active: self.active_resource_id == Some(resource_id),
            context_retained: !matches!(record.lifecycle, SurfaceLifecycle::Terminal { .. }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextInputError {
    TooLarge { bytes: usize, max: usize },
}

impl fmt::Display for TextInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { bytes, max } => {
                write!(f, "fixture text is {bytes} bytes; maximum is {max}")
            }
        }
    }
}

impl std::error::Error for TextInputError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserSurfaceFixtureError {
    UnexpectedClickToken,
}

impl fmt::Display for BrowserSurfaceFixtureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedClickToken => write!(f, "fixture click token was not trusted"),
        }
    }
}

impl std::error::Error for BrowserSurfaceFixtureError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserSurfaceFixtureSnapshot {
    pub visible_token: String,
    pub trusted_click_token: Option<String>,
    pub text_value: String,
    pub resize_token: String,
    pub retained_state: String,
}

/// Bounded, deterministic contract used by ordinary tests before a real
/// WebView2 fixture is available. It models the observable tokens and state
/// that the later host/GPUI proof must preserve across park/reattach.
#[derive(Debug, Clone)]
pub struct BrowserSurfaceFixture {
    trusted_click_token: Option<String>,
    text_value: String,
    resize_token: String,
}

impl BrowserSurfaceFixture {
    pub fn new() -> Self {
        Self {
            trusted_click_token: None,
            text_value: String::new(),
            resize_token: "dm-surface-resize-initial".to_string(),
        }
    }

    pub fn snapshot(&self) -> BrowserSurfaceFixtureSnapshot {
        BrowserSurfaceFixtureSnapshot {
            visible_token: BROWSER_SURFACE_FIXTURE_VISIBLE_TOKEN.to_string(),
            trusted_click_token: self.trusted_click_token.clone(),
            text_value: self.text_value.clone(),
            resize_token: self.resize_token.clone(),
            retained_state: BROWSER_SURFACE_FIXTURE_RETAINED_STATE.to_string(),
        }
    }

    pub fn trusted_click(&mut self, token: &str) -> Result<(), BrowserSurfaceFixtureError> {
        if token != BROWSER_SURFACE_FIXTURE_CLICK_TOKEN {
            return Err(BrowserSurfaceFixtureError::UnexpectedClickToken);
        }
        self.trusted_click_token = Some(token.to_string());
        Ok(())
    }

    pub fn text_input(&mut self, text: impl AsRef<str>) -> Result<(), TextInputError> {
        let text = text.as_ref();
        let bytes = text.len();
        if bytes > MAX_SURFACE_TEXT_INPUT_BYTES {
            return Err(TextInputError::TooLarge {
                bytes,
                max: MAX_SURFACE_TEXT_INPUT_BYTES,
            });
        }
        self.text_value = text.to_string();
        Ok(())
    }

    pub fn resize(&mut self, bounds: PhysicalBounds, dpi: DpiScale) {
        self.resize_token = format!(
            "dm-surface-resize-{}x{}@{}",
            bounds.width,
            bounds.height,
            dpi.scale_percent()
        );
    }

    pub fn apply_input(
        &mut self,
        action: &SurfaceInputAction,
    ) -> Result<(), BrowserSurfaceFixtureError> {
        match action {
            SurfaceInputAction::TrustedClick { target_token, .. } => {
                self.trusted_click(target_token)
            }
            SurfaceInputAction::TextInput { text } => self
                .text_input(text)
                .map_err(|_| BrowserSurfaceFixtureError::UnexpectedClickToken),
        }
    }
}

use crate::protocol::{
    BrowserAttachRequest, BrowserPhysicalBounds,
    BrowserSurfaceDescriptor as BrowserProtocolSurfaceDescriptor, BrowserSurfaceLifecycle,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserDockError {
    StaleGeneration,
    StaleBoundsEpoch,
    StaleFocusEpoch,
    StaleNonce,
    CrossTask,
    HiddenForLayout,
    PointerConsumed,
    NotAttached,
    AlreadyAttached,
    InvalidRequest,
}

impl std::fmt::Display for BrowserDockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleGeneration => write!(f, "browser dock generation is stale"),
            Self::StaleBoundsEpoch => write!(f, "browser dock bounds epoch is stale"),
            Self::StaleFocusEpoch => write!(f, "browser dock focus epoch is stale"),
            Self::StaleNonce => write!(f, "browser dock surface nonce is stale"),
            Self::CrossTask => write!(f, "browser dock surface belongs to another Task"),
            Self::HiddenForLayout => write!(f, "browser dock surface is hidden for layout"),
            Self::PointerConsumed => write!(f, "shell gesture consumed page pointer"),
            Self::NotAttached => write!(f, "browser dock surface is not attached"),
            Self::AlreadyAttached => write!(f, "browser dock surface is already attached"),
            Self::InvalidRequest => write!(f, "browser dock request is invalid"),
        }
    }
}

impl std::error::Error for BrowserDockError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserDockChrome {
    Tabs,
    Address,
    Status,
    Approvals,
    Artifacts,
    Diagnostics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserDockFocusTarget {
    Terminal,
    BrowserChrome,
    BrowserPage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserDockGesture {
    TaskSwitch,
    DockResize,
    TabSelect,
    AddressSubmit,
    PopupSelect,
    PageClick,
    PageKey,
    PageDrag,
    FileChoice,
    PermissionAnswer,
    TerminalFocus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserPointerDisposition {
    ConsumeShellGesture,
    ForwardToPage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserDockSurface {
    descriptor: BrowserProtocolSurfaceDescriptor,
    client_id: Option<ClientId>,
    lifecycle: BrowserSurfaceLifecycle,
    shell_focus_epoch: u64,
    pointer_armed: bool,
    hidden_for_layout: bool,
    selected_tab: Option<BrowserTabId>,
    focus_target: BrowserDockFocusTarget,
}

impl BrowserDockSurface {
    pub fn from_descriptor(
        descriptor: BrowserProtocolSurfaceDescriptor,
    ) -> Result<Self, BrowserDockError> {
        descriptor
            .validate()
            .map_err(|_| BrowserDockError::InvalidRequest)?;
        Ok(Self {
            descriptor,
            client_id: None,
            lifecycle: BrowserSurfaceLifecycle::Parked,
            shell_focus_epoch: 1,
            pointer_armed: false,
            hidden_for_layout: false,
            selected_tab: None,
            focus_target: BrowserDockFocusTarget::BrowserChrome,
        })
    }

    pub fn uses_web_chrome() -> bool {
        false
    }

    pub fn required_chrome() -> &'static [BrowserDockChrome] {
        &[
            BrowserDockChrome::Tabs,
            BrowserDockChrome::Address,
            BrowserDockChrome::Status,
            BrowserDockChrome::Approvals,
            BrowserDockChrome::Artifacts,
            BrowserDockChrome::Diagnostics,
        ]
    }

    pub fn task_id(&self) -> TaskId {
        self.descriptor.identity.task_id
    }

    pub fn generation(&self) -> u64 {
        self.descriptor.runtime_generation.value()
    }

    pub fn bounds_epoch(&self) -> u64 {
        self.descriptor.bounds_epoch.value()
    }

    pub fn focus_epoch(&self) -> u64 {
        self.descriptor.focus_epoch.value()
    }

    pub fn shell_focus_epoch(&self) -> u64 {
        self.shell_focus_epoch
    }

    pub fn lifecycle(&self) -> &BrowserSurfaceLifecycle {
        &self.lifecycle
    }

    pub fn hidden_for_layout(&self) -> bool {
        self.hidden_for_layout
    }

    pub fn pointer_armed(&self) -> bool {
        self.pointer_armed
    }

    pub fn focus_target(&self) -> BrowserDockFocusTarget {
        self.focus_target
    }

    pub fn selected_tab(&self) -> Option<BrowserTabId> {
        self.selected_tab
    }

    pub fn attach(&mut self, request: BrowserAttachRequest) -> Result<(), BrowserDockError> {
        request
            .descriptor
            .validate()
            .map_err(|_| BrowserDockError::InvalidRequest)?;
        self.require_same_surface(&request.descriptor)?;
        if matches!(self.lifecycle, BrowserSurfaceLifecycle::Attached { .. }) {
            return Err(BrowserDockError::AlreadyAttached);
        }
        self.descriptor = request.descriptor;
        self.client_id = Some(request.client_id);
        self.lifecycle = BrowserSurfaceLifecycle::Attached {
            client_id: request.client_id,
        };
        self.hidden_for_layout = false;
        self.pointer_armed = false;
        self.focus_target = BrowserDockFocusTarget::BrowserChrome;
        Ok(())
    }

    pub fn detach(&mut self, crashed: bool) -> Result<(), BrowserDockError> {
        if matches!(self.lifecycle, BrowserSurfaceLifecycle::Parked) {
            return Err(BrowserDockError::NotAttached);
        }
        self.lifecycle = BrowserSurfaceLifecycle::Detached {
            client_id: self.client_id,
            crashed,
        };
        self.pointer_armed = false;
        self.hidden_for_layout = true;
        self.focus_target = BrowserDockFocusTarget::BrowserChrome;
        Ok(())
    }

    pub fn hide_for_layout(&mut self) -> Result<(), BrowserDockError> {
        self.hidden_for_layout = true;
        self.pointer_armed = false;
        Ok(())
    }

    pub fn apply_bounds(
        &mut self,
        generation: u64,
        bounds: BrowserPhysicalBounds,
        bounds_epoch: u64,
    ) -> Result<u64, BrowserDockError> {
        self.require_generation(generation)?;
        if !self.hidden_for_layout {
            return Err(BrowserDockError::HiddenForLayout);
        }
        if bounds_epoch != self.descriptor.bounds_epoch.value() {
            return Err(BrowserDockError::StaleBoundsEpoch);
        }
        let next = self
            .descriptor
            .bounds_epoch
            .next()
            .map_err(|_| BrowserDockError::InvalidRequest)?;
        self.descriptor.physical_bounds = bounds;
        self.descriptor.bounds_epoch = next;
        self.hidden_for_layout = false;
        self.pointer_armed = false;
        Ok(next.value())
    }

    pub fn switch_task(
        &mut self,
        incoming: BrowserAttachRequest,
    ) -> Result<(), BrowserDockError> {
        incoming
            .descriptor
            .validate()
            .map_err(|_| BrowserDockError::InvalidRequest)?;
        self.hide_for_layout()?;
        self.advance_shell_and_surface_focus()?;
        self.lifecycle = BrowserSurfaceLifecycle::Parked;
        self.client_id = None;
        self.descriptor = incoming.descriptor.clone();
        self.attach(incoming)?;
        Ok(())
    }

    pub fn select_tab(&mut self, tab_id: BrowserTabId) -> Result<(), BrowserDockError> {
        self.selected_tab = Some(tab_id);
        self.advance_shell_and_surface_focus()?;
        self.pointer_armed = false;
        self.focus_target = BrowserDockFocusTarget::BrowserChrome;
        Ok(())
    }

    pub fn focus_terminal(&mut self) -> Result<(), BrowserDockError> {
        self.hide_for_layout()?;
        self.advance_shell_and_surface_focus()?;
        self.focus_target = BrowserDockFocusTarget::Terminal;
        Ok(())
    }

    pub fn classify_gesture(&self, gesture: BrowserDockGesture) -> BrowserPointerDisposition {
        match gesture {
            BrowserDockGesture::PageClick
            | BrowserDockGesture::PageKey
            | BrowserDockGesture::PageDrag
                if self.pointer_armed
                    && !self.hidden_for_layout
                    && matches!(self.lifecycle, BrowserSurfaceLifecycle::Attached { .. })
                    && self.focus_target == BrowserDockFocusTarget::BrowserPage =>
            {
                BrowserPointerDisposition::ForwardToPage
            }
            _ => BrowserPointerDisposition::ConsumeShellGesture,
        }
    }

    pub fn arm_page_input_after_gesture(&mut self) -> Result<(), BrowserDockError> {
        if !matches!(self.lifecycle, BrowserSurfaceLifecycle::Attached { .. }) {
            return Err(BrowserDockError::NotAttached);
        }
        if self.hidden_for_layout {
            return Err(BrowserDockError::HiddenForLayout);
        }
        self.pointer_armed = true;
        self.focus_target = BrowserDockFocusTarget::BrowserPage;
        Ok(())
    }

    pub fn admit_page_input(
        &self,
        generation: u64,
        bounds_epoch: u64,
        focus_epoch: u64,
        gesture: BrowserDockGesture,
    ) -> Result<(), BrowserDockError> {
        self.require_generation(generation)?;
        if bounds_epoch != self.descriptor.bounds_epoch.value() {
            return Err(BrowserDockError::StaleBoundsEpoch);
        }
        if focus_epoch != self.descriptor.focus_epoch.value() {
            return Err(BrowserDockError::StaleFocusEpoch);
        }
        match self.classify_gesture(gesture) {
            BrowserPointerDisposition::ForwardToPage => Ok(()),
            BrowserPointerDisposition::ConsumeShellGesture => Err(BrowserDockError::PointerConsumed),
        }
    }

    fn require_generation(&self, generation: u64) -> Result<(), BrowserDockError> {
        if generation == 0 || generation != self.descriptor.runtime_generation.value() {
            return Err(BrowserDockError::StaleGeneration);
        }
        Ok(())
    }

    fn require_same_surface(
        &self,
        descriptor: &BrowserProtocolSurfaceDescriptor,
    ) -> Result<(), BrowserDockError> {
        if descriptor.identity.task_id != self.descriptor.identity.task_id {
            return Err(BrowserDockError::CrossTask);
        }
        if descriptor.runtime_generation != self.descriptor.runtime_generation {
            return Err(BrowserDockError::StaleGeneration);
        }
        if descriptor.nonce.as_bytes() != self.descriptor.nonce.as_bytes() {
            return Err(BrowserDockError::StaleNonce);
        }
        Ok(())
    }

    fn advance_shell_and_surface_focus(&mut self) -> Result<(), BrowserDockError> {
        self.shell_focus_epoch = self
            .shell_focus_epoch
            .checked_add(1)
            .ok_or(BrowserDockError::InvalidRequest)?;
        self.descriptor.focus_epoch = self
            .descriptor
            .focus_epoch
            .next()
            .map_err(|_| BrowserDockError::InvalidRequest)?;
        self.pointer_armed = false;
        Ok(())
    }
}


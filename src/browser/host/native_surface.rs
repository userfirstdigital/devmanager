//! Host-owned native surface backend for the single BrowserWebViewHost owner.
//!
//! Production mutations must succeed only after live Win32/Wry observation.
//! Synthetic HWND maps exist only under `cfg(test)` and cannot mint proof.

use super::{
    browser_native_surface_backend_seal, BrowserNativeSurfaceBackend, BrowserNativeViewRegistration,
};
use crate::protocol::{BrowserPhysicalBounds, BrowserSurfaceDescriptor, BrowserWindowHandle};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// Opaque proof that a host-owned surface was observed live. A copied
/// descriptor is not authority. Only the Windows host observation path may
/// mark [`HostSurfaceObservation::LiveWindows`].
///
/// ```compile_fail
/// use devmanager::browser::BrowserHostOwnedSurfaceProof;
/// let _ = BrowserHostOwnedSurfaceProof { descriptor: unreachable!() };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserHostOwnedSurfaceProof {
    descriptor: BrowserSurfaceDescriptor,
    observation: HostSurfaceObservation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostSurfaceObservation {
    Unverified,
    LiveWindows,
}

impl BrowserHostOwnedSurfaceProof {
    pub(crate) fn from_unverified_descriptor(descriptor: BrowserSurfaceDescriptor) -> Self {
        Self {
            descriptor,
            observation: HostSurfaceObservation::Unverified,
        }
    }

    pub(super) fn from_windows_child_observation(descriptor: BrowserSurfaceDescriptor) -> Self {
        Self {
            descriptor,
            observation: HostSurfaceObservation::LiveWindows,
        }
    }

    pub fn descriptor(&self) -> &BrowserSurfaceDescriptor {
        &self.descriptor
    }

    pub fn is_live_windows_observation(&self) -> bool {
        matches!(self.observation, HostSurfaceObservation::LiveWindows)
    }

    pub fn is_live_verified(&self) -> bool {
        self.is_live_windows_observation()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOwnedSurfaceBindError {
    UiThread,
    MissingAllocation,
    ChildEqualsParking,
    DuplicateChild,
    ForeignChild,
    StaleParking,
    ControllerClosed,
    ResiduePresent,
    LiveWindowRequired,
    Win32Mutation,
    ChildHwndUnobservable,
    DestinationIsParking,
    DestinationEqualsChild,
    DestinationParentMismatch,
}

impl std::fmt::Display for HostOwnedSurfaceBindError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::UiThread => "host-owned surface backend requires the UI thread",
            Self::MissingAllocation => "native surface allocation was not recorded by the host",
            Self::ChildEqualsParking => "child HWND must differ from parking HWND",
            Self::DuplicateChild => "child HWND is already owned by the host surface backend",
            Self::ForeignChild => "child HWND is not owned by the host surface backend",
            Self::StaleParking => "parking HWND does not match the host-owned allocation",
            Self::ControllerClosed => "WebView controller ownership is already closed",
            Self::ResiduePresent => "native surface residue remains after teardown observation",
            Self::LiveWindowRequired => "production surface ops require a live Win32 HWND",
            Self::Win32Mutation => "Win32 surface mutation failed",
            Self::ChildHwndUnobservable => {
                "Wry does not expose a distinct child HWND from controller ParentWindow"
            }
            Self::DestinationIsParking => "attach destination must not be the parking HWND",
            Self::DestinationEqualsChild => "attach destination must differ from the child HWND",
            Self::DestinationParentMismatch => {
                "live GetParent does not match the attach destination HWND"
            }
        })
    }
}

impl std::error::Error for HostOwnedSurfaceBindError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserTaskSurfaceBindBlocker {
    TaskIdentityUnavailableAtBuildCompletion,
    ChildHwndUnobservable,
}

impl std::fmt::Display for BrowserTaskSurfaceBindBlocker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::TaskIdentityUnavailableAtBuildCompletion => {
                "completed Wry view has no TaskId/BrowserContextId/ResourceId to bind"
            }
            Self::ChildHwndUnobservable => {
                "Wry WebViewExtWindows exposes controller/environment/reparent, not a child HWND"
            }
        })
    }
}

pub fn require_completed_wry_task_identity(
    identity: Option<crate::protocol::BrowserSurfaceIdentity>,
) -> Result<crate::protocol::BrowserSurfaceIdentity, BrowserTaskSurfaceBindBlocker> {
    identity.ok_or(BrowserTaskSurfaceBindBlocker::TaskIdentityUnavailableAtBuildCompletion)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyMcpTaskSurfaceBlocker {
    WorkspaceCommandLacksTaskId,
    CrossTaskOrMissingSurface,
}

impl std::fmt::Display for LegacyMcpTaskSurfaceBlocker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::WorkspaceCommandLacksTaskId => {
                "legacy MCP/chrome workspace commands carry no TaskId"
            }
            Self::CrossTaskOrMissingSurface => {
                "legacy MCP command is not the exact live task-bound surface"
            }
        })
    }
}

/// MCP dispatch cannot mint TaskId from workspace_key. Callers pass the
/// optional identity they actually own; `None` is a typed blocker.
pub fn legacy_mcp_command_task_identity(
    task_id: Option<crate::domain::id::TaskId>,
) -> Result<crate::domain::id::TaskId, LegacyMcpTaskSurfaceBlocker> {
    task_id.ok_or(LegacyMcpTaskSurfaceBlocker::WorkspaceCommandLacksTaskId)
}

#[derive(Debug, Clone)]
struct LiveSurfaceRecord {
    parking: BrowserWindowHandle,
    parent: BrowserWindowHandle,
    bounds: BrowserPhysicalBounds,
    focused: bool,
    attached: bool,
    controller_open: bool,
    environment_open: bool,
    helper_residue: u32,
}

/// Sealed production backend owned by BrowserWebViewHost.
pub struct HostOwnedNativeSurfaceBackend {
    _main_thread_only: std::marker::PhantomData<Rc<()>>,
    on_ui_thread: bool,
    #[cfg(test)]
    synthetic: bool,
    live: HashMap<u64, LiveSurfaceRecord>,
    zero_residue_observed: HashSet<u64>,
}

impl Default for HostOwnedNativeSurfaceBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl HostOwnedNativeSurfaceBackend {
    pub(crate) fn new() -> Self {
        Self {
            _main_thread_only: std::marker::PhantomData,
            on_ui_thread: true,
            #[cfg(test)]
            synthetic: false,
            live: HashMap::new(),
            zero_residue_observed: HashSet::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_synthetic_for_test() -> Self {
        Self {
            _main_thread_only: std::marker::PhantomData,
            on_ui_thread: true,
            synthetic: true,
            live: HashMap::new(),
            zero_residue_observed: HashSet::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_ui_thread_for_test(&mut self, on_ui_thread: bool) {
        self.on_ui_thread = on_ui_thread;
    }

    fn allows_synthetic(&self) -> bool {
        #[cfg(test)]
        {
            self.synthetic
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    pub(crate) fn admit_host_allocation(
        &mut self,
        child: &BrowserWindowHandle,
        parking: &BrowserWindowHandle,
        bounds: BrowserPhysicalBounds,
    ) -> Result<(), HostOwnedSurfaceBindError> {
        self.require_ui_thread()?;
        if child == parking {
            return Err(HostOwnedSurfaceBindError::ChildEqualsParking);
        }
        if !self.allows_synthetic() {
            Self::win32_require_live(child)
                .map_err(|_| HostOwnedSurfaceBindError::LiveWindowRequired)?;
            Self::win32_require_live(parking)
                .map_err(|_| HostOwnedSurfaceBindError::LiveWindowRequired)?;
        }
        let key = child.raw_value();
        if self.live.contains_key(&key) {
            return Err(HostOwnedSurfaceBindError::DuplicateChild);
        }
        self.live.insert(
            key,
            LiveSurfaceRecord {
                parking: parking.clone(),
                parent: parking.clone(),
                bounds,
                focused: false,
                attached: false,
                controller_open: true,
                environment_open: true,
                helper_residue: 0,
            },
        );
        Ok(())
    }

    pub(crate) fn release_host_allocation(
        &mut self,
        child: &BrowserWindowHandle,
    ) -> Result<(), HostOwnedSurfaceBindError> {
        self.require_ui_thread()?;
        self.live.remove(&child.raw_value());
        Ok(())
    }

    pub(crate) fn mark_controller_closed(
        &mut self,
        child: &BrowserWindowHandle,
    ) -> Result<(), HostOwnedSurfaceBindError> {
        self.require_ui_thread()?;
        let record = self
            .live
            .get_mut(&child.raw_value())
            .ok_or(HostOwnedSurfaceBindError::ForeignChild)?;
        record.controller_open = false;
        record.environment_open = false;
        record.helper_residue = 0;
        record.attached = false;
        record.focused = false;
        record.parent = record.parking.clone();
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn inject_helper_residue_for_test(
        &mut self,
        child: &BrowserWindowHandle,
        residue: u32,
    ) {
        if let Some(record) = self.live.get_mut(&child.raw_value()) {
            record.helper_residue = residue;
        }
    }

    fn require_ui_thread(&self) -> Result<(), HostOwnedSurfaceBindError> {
        if self.on_ui_thread {
            Ok(())
        } else {
            Err(HostOwnedSurfaceBindError::UiThread)
        }
    }

    fn record_mut(
        &mut self,
        child: &BrowserWindowHandle,
    ) -> Result<&mut LiveSurfaceRecord, String> {
        self.live
            .get_mut(&child.raw_value())
            .ok_or_else(|| HostOwnedSurfaceBindError::ForeignChild.to_string())
    }

    fn record(&self, child: &BrowserWindowHandle) -> Result<&LiveSurfaceRecord, String> {
        self.live
            .get(&child.raw_value())
            .ok_or_else(|| HostOwnedSurfaceBindError::ForeignChild.to_string())
    }
}

impl HostOwnedNativeSurfaceBackend {
    fn run_or_synthetic(&self, op: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
        if self.allows_synthetic() {
            return Ok(());
        }
        op()
    }

    #[cfg(target_os = "windows")]
    fn win32_require_live(handle: &BrowserWindowHandle) -> Result<(), String> {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::IsWindow;
        let hwnd = HWND(handle.raw_value() as usize as *mut _);
        unsafe {
            if hwnd.0.is_null() || !IsWindow(Some(hwnd)).as_bool() {
                return Err(HostOwnedSurfaceBindError::LiveWindowRequired.to_string());
            }
        }
        Ok(())
    }

    #[cfg(not(target_os = "windows"))]
    fn win32_require_live(_handle: &BrowserWindowHandle) -> Result<(), String> {
        Err(HostOwnedSurfaceBindError::LiveWindowRequired.to_string())
    }

    #[cfg(target_os = "windows")]
    fn win32_is_window(handle: &BrowserWindowHandle) -> Result<bool, String> {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::IsWindow;
        let hwnd = HWND(handle.raw_value() as usize as *mut _);
        if hwnd.0.is_null() {
            return Ok(false);
        }
        Ok(unsafe { IsWindow(Some(hwnd)).as_bool() })
    }

    #[cfg(not(target_os = "windows"))]
    fn win32_is_window(_handle: &BrowserWindowHandle) -> Result<bool, String> {
        Err(HostOwnedSurfaceBindError::LiveWindowRequired.to_string())
    }

    #[cfg(target_os = "windows")]
    fn win32_reparent(
        child: &BrowserWindowHandle,
        parent: &BrowserWindowHandle,
    ) -> Result<(), String> {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::SetParent;
        Self::win32_require_live(child)?;
        Self::win32_require_live(parent)?;
        let child_hwnd = HWND(child.raw_value() as usize as *mut _);
        let parent_hwnd = HWND(parent.raw_value() as usize as *mut _);
        unsafe {
            SetParent(child_hwnd, Some(parent_hwnd)).map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    #[cfg(not(target_os = "windows"))]
    fn win32_reparent(
        _child: &BrowserWindowHandle,
        _parent: &BrowserWindowHandle,
    ) -> Result<(), String> {
        Err(HostOwnedSurfaceBindError::Win32Mutation.to_string())
    }

    #[cfg(target_os = "windows")]
    fn win32_set_bounds(
        child: &BrowserWindowHandle,
        bounds: BrowserPhysicalBounds,
    ) -> Result<(), String> {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER};
        Self::win32_require_live(child)?;
        let child_hwnd = HWND(child.raw_value() as usize as *mut _);
        unsafe {
            SetWindowPos(
                child_hwnd,
                None,
                bounds.x(),
                bounds.y(),
                bounds.width() as i32,
                bounds.height() as i32,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
            .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    #[cfg(not(target_os = "windows"))]
    fn win32_set_bounds(
        _child: &BrowserWindowHandle,
        _bounds: BrowserPhysicalBounds,
    ) -> Result<(), String> {
        Err(HostOwnedSurfaceBindError::Win32Mutation.to_string())
    }

    #[cfg(target_os = "windows")]
    fn win32_set_focus(child: &BrowserWindowHandle, focused: bool) -> Result<(), String> {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::SetFocus;
        Self::win32_require_live(child)?;
        if !focused {
            return Ok(());
        }
        let child_hwnd = HWND(child.raw_value() as usize as *mut _);
        unsafe {
            SetFocus(Some(child_hwnd)).map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    #[cfg(not(target_os = "windows"))]
    fn win32_set_focus(_child: &BrowserWindowHandle, _focused: bool) -> Result<(), String> {
        Err(HostOwnedSurfaceBindError::Win32Mutation.to_string())
    }

    #[cfg(target_os = "windows")]
    fn win32_parent_matches(
        child: &BrowserWindowHandle,
        expected_parent: &BrowserWindowHandle,
    ) -> Result<bool, String> {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::GetParent;
        Self::win32_require_live(child)?;
        let child_hwnd = HWND(child.raw_value() as usize as *mut _);
        let parent = unsafe { GetParent(child_hwnd).map_err(|error| error.to_string())? };
        Ok(parent.0 as usize as u64 == expected_parent.raw_value())
    }

    #[cfg(not(target_os = "windows"))]
    fn win32_parent_matches(
        _child: &BrowserWindowHandle,
        _expected_parent: &BrowserWindowHandle,
    ) -> Result<bool, String> {
        Err(HostOwnedSurfaceBindError::Win32Mutation.to_string())
    }
}

impl browser_native_surface_backend_seal::Sealed for HostOwnedNativeSurfaceBackend {}

impl BrowserNativeSurfaceBackend for HostOwnedNativeSurfaceBackend {
    fn preflight_native_view_allocation(
        &mut self,
        registration: &BrowserNativeViewRegistration,
    ) -> Result<(), String> {
        self.require_ui_thread()
            .map_err(|error| error.to_string())?;
        let child = registration.child_window();
        let parking = registration.parking_window_handle();
        let record = self.record(child)?;
        if &record.parking != parking {
            return Err(HostOwnedSurfaceBindError::StaleParking.to_string());
        }
        if !record.controller_open || !record.environment_open {
            return Err(HostOwnedSurfaceBindError::ControllerClosed.to_string());
        }
        if !self.allows_synthetic() {
            Self::win32_require_live(child)?;
            Self::win32_require_live(parking)?;
        }
        Ok(())
    }

    fn rollback_native_view_allocation(
        &mut self,
        registration: &BrowserNativeViewRegistration,
    ) -> Result<(), String> {
        self.require_ui_thread()
            .map_err(|error| error.to_string())?;
        self.live.remove(&registration.child_window().raw_value());
        Ok(())
    }

    fn preflight_native_view_operation(
        &mut self,
        descriptor: &BrowserSurfaceDescriptor,
        parking: &BrowserWindowHandle,
    ) -> Result<(), String> {
        self.require_ui_thread()
            .map_err(|error| error.to_string())?;
        let record = self.record(&descriptor.child_hwnd)?;
        if &record.parking != parking {
            return Err(HostOwnedSurfaceBindError::StaleParking.to_string());
        }
        if !record.controller_open {
            return Err(HostOwnedSurfaceBindError::ControllerClosed.to_string());
        }
        if !self.allows_synthetic() {
            Self::win32_require_live(&descriptor.child_hwnd)?;
            Self::win32_require_live(parking)?;
        }
        Ok(())
    }

    fn assert_ui_thread(&self) -> Result<(), String> {
        self.require_ui_thread().map_err(|error| error.to_string())
    }

    fn park_surface(
        &mut self,
        child: &BrowserWindowHandle,
        parking: &BrowserWindowHandle,
    ) -> Result<(), String> {
        self.require_ui_thread()
            .map_err(|error| error.to_string())?;
        {
            let record = self.record(child)?;
            if &record.parking != parking {
                return Err(HostOwnedSurfaceBindError::StaleParking.to_string());
            }
        }
        self.run_or_synthetic(|| Self::win32_reparent(child, parking))?;
        let record = self.record_mut(child)?;
        record.parent = parking.clone();
        record.attached = false;
        record.focused = false;
        Ok(())
    }

    fn attach_surface(
        &mut self,
        child: &BrowserWindowHandle,
        destination: &BrowserWindowHandle,
        bounds: BrowserPhysicalBounds,
    ) -> Result<(), String> {
        self.require_ui_thread()
            .map_err(|error| error.to_string())?;
        {
            let record = self.record(child)?;
            if destination == &record.parking {
                return Err(HostOwnedSurfaceBindError::DestinationIsParking.to_string());
            }
        }
        if child == destination {
            return Err(HostOwnedSurfaceBindError::DestinationEqualsChild.to_string());
        }
        self.run_or_synthetic(|| {
            Self::win32_reparent(child, destination)?;
            Self::win32_set_bounds(child, bounds)?;
            let matches = Self::win32_parent_matches(child, destination)?;
            if !matches {
                return Err(HostOwnedSurfaceBindError::DestinationParentMismatch.to_string());
            }
            Ok(())
        })?;
        let record = self.record_mut(child)?;
        record.parent = destination.clone();
        record.bounds = bounds;
        record.attached = true;
        Ok(())
    }

    fn set_surface_bounds(
        &mut self,
        child: &BrowserWindowHandle,
        bounds: BrowserPhysicalBounds,
    ) -> Result<(), String> {
        self.require_ui_thread()
            .map_err(|error| error.to_string())?;
        self.run_or_synthetic(|| Self::win32_set_bounds(child, bounds))?;
        let record = self.record_mut(child)?;
        record.bounds = bounds;
        Ok(())
    }

    fn set_surface_focus(
        &mut self,
        child: &BrowserWindowHandle,
        focused: bool,
    ) -> Result<(), String> {
        self.require_ui_thread()
            .map_err(|error| error.to_string())?;
        self.run_or_synthetic(|| Self::win32_set_focus(child, focused))?;
        let record = self.record_mut(child)?;
        record.focused = focused;
        Ok(())
    }

    fn verify_surface_state(
        &mut self,
        descriptor: &BrowserSurfaceDescriptor,
        parking: &BrowserWindowHandle,
        attached_parent: Option<&BrowserWindowHandle>,
        attached: bool,
        bounds: BrowserPhysicalBounds,
        focused: bool,
    ) -> Result<(), String> {
        self.require_ui_thread()
            .map_err(|error| error.to_string())?;
        let record = self.record(&descriptor.child_hwnd)?;
        if &record.parking != parking {
            return Err(HostOwnedSurfaceBindError::StaleParking.to_string());
        }
        if record.attached != attached {
            return Err("attached postcondition mismatch".to_string());
        }
        if record.bounds != bounds {
            return Err("bounds postcondition mismatch".to_string());
        }
        if record.focused != focused {
            return Err("focus postcondition mismatch".to_string());
        }
        if attached {
            let destination = attached_parent
                .ok_or_else(|| HostOwnedSurfaceBindError::DestinationParentMismatch.to_string())?;
            if destination == parking {
                return Err(HostOwnedSurfaceBindError::DestinationIsParking.to_string());
            }
            if &record.parent != destination {
                return Err(HostOwnedSurfaceBindError::DestinationParentMismatch.to_string());
            }
        } else if attached_parent.is_some() {
            return Err("parked surface must not retain an attach destination".to_string());
        } else if record.parent != *parking {
            return Err("parked surface must remain parented to parking HWND".to_string());
        }
        if !self.allows_synthetic() {
            let expected = if attached {
                attached_parent.ok_or_else(|| {
                    HostOwnedSurfaceBindError::DestinationParentMismatch.to_string()
                })?
            } else {
                parking
            };
            let matches = Self::win32_parent_matches(&descriptor.child_hwnd, expected)?;
            if !matches {
                return Err(HostOwnedSurfaceBindError::DestinationParentMismatch.to_string());
            }
        }
        Ok(())
    }

    fn observe_surface_crash(
        &mut self,
        descriptor: &BrowserSurfaceDescriptor,
        parking: &BrowserWindowHandle,
    ) -> Result<(), String> {
        self.preflight_native_view_operation(descriptor, parking)?;
        let record = self.record_mut(&descriptor.child_hwnd)?;
        if !record.controller_open {
            return Ok(());
        }
        Err("controller remains open; crash was not observed".to_string())
    }

    fn observe_teardown_zero_residue(
        &mut self,
        descriptor: &BrowserSurfaceDescriptor,
        parking: &BrowserWindowHandle,
    ) -> Result<(), String> {
        self.require_ui_thread()
            .map_err(|error| error.to_string())?;
        let child_key = descriptor.child_hwnd.raw_value();
        if self.live.get(&child_key).is_none() {
            if !self.allows_synthetic() && self.zero_residue_observed.contains(&child_key) {
                return self.observe_production_hwnd_drain(&descriptor.child_hwnd, parking);
            }
            return Err(HostOwnedSurfaceBindError::ForeignChild.to_string());
        }
        let record = self.record(&descriptor.child_hwnd)?;
        if &record.parking != parking {
            return Err(HostOwnedSurfaceBindError::StaleParking.to_string());
        }
        if record.controller_open || record.environment_open || record.helper_residue != 0 {
            return Err(HostOwnedSurfaceBindError::ResiduePresent.to_string());
        }
        if record.attached {
            return Err("surface must be parked before zero-residue observation".to_string());
        }
        if !self.allows_synthetic() {
            self.observe_production_hwnd_drain(&descriptor.child_hwnd, parking)?;
            self.live.remove(&child_key);
            self.zero_residue_observed.insert(child_key);
            return Ok(());
        }
        Ok(())
    }

    fn observe_production_hwnd_drain(
        &self,
        child: &BrowserWindowHandle,
        parking: &BrowserWindowHandle,
    ) -> Result<(), String> {
        let child_live = Self::win32_is_window(child)?;
        let parking_live = Self::win32_is_window(parking)?;
        if child_live {
            return Err(HostOwnedSurfaceBindError::ResiduePresent.to_string());
        }
        if !parking_live {
            return Err(HostOwnedSurfaceBindError::LiveWindowRequired.to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod production_win32_failure_tests {
    use super::*;
    use crate::protocol::BrowserDpi;

    fn fake_registration() -> BrowserNativeViewRegistration {
        BrowserNativeViewRegistration::from_host_record(
            crate::protocol::BrowserSurfaceIdentity {
                task_id: crate::domain::id::TaskId::new(),
                context_id: crate::domain::id::BrowserContextId::new(),
                resource_id: crate::domain::id::ResourceId::new(),
            },
            BrowserWindowHandle::from_raw(0x9101).expect("child"),
            BrowserWindowHandle::from_raw(0x9201).expect("parking"),
            crate::protocol::BrowserHostProcessIdentity::new(1, 1, "C:\\DevManager\\host.exe")
                .expect("proc"),
            BrowserPhysicalBounds::new(0, 0, 10, 10).expect("bounds"),
            BrowserDpi::new(96, 96).expect("dpi"),
        )
        .expect("registration")
    }

    #[test]
    fn synthetic_attach_tracks_destination_and_rejects_parking() {
        let mut backend = HostOwnedNativeSurfaceBackend::new_synthetic_for_test();
        let registration = fake_registration();
        backend
            .admit_host_allocation(
                registration.child_window(),
                registration.parking_window_handle(),
                registration.physical_bounds(),
            )
            .expect("admit");
        assert_eq!(
            backend.attach_surface(
                registration.child_window(),
                registration.parking_window_handle(),
                registration.physical_bounds(),
            ),
            Err(HostOwnedSurfaceBindError::DestinationIsParking.to_string())
        );
        let destination = BrowserWindowHandle::from_raw(0xA021).expect("destination");
        backend
            .attach_surface(
                registration.child_window(),
                &destination,
                registration.physical_bounds(),
            )
            .expect("attach destination");
        let descriptor = crate::protocol::BrowserSurfaceDescriptor {
            identity: registration.identity(),
            child_hwnd: registration.child_window().clone(),
            host_process: registration.host_process().clone(),
            host_fence: crate::protocol::BrowserHostFence::new(1, 1).expect("fence"),
            runtime_generation: crate::protocol::BrowserRuntimeGeneration::new(1).expect("gen"),
            nonce: crate::protocol::BrowserSurfaceNonce::new([4; 16]).expect("nonce"),
            bounds_epoch: crate::protocol::BrowserBoundsEpoch::initial(),
            focus_epoch: crate::protocol::BrowserFocusEpoch::initial(),
            physical_bounds: registration.physical_bounds(),
            dpi: crate::protocol::BrowserDpi::new(96, 96).expect("dpi"),
        };
        backend
            .verify_surface_state(
                &descriptor,
                registration.parking_window_handle(),
                Some(&destination),
                true,
                registration.physical_bounds(),
                false,
            )
            .expect("destination parent");
        backend
            .park_surface(
                registration.child_window(),
                registration.parking_window_handle(),
            )
            .expect("park");
        backend
            .verify_surface_state(
                &descriptor,
                registration.parking_window_handle(),
                None,
                false,
                registration.physical_bounds(),
                false,
            )
            .expect("parked parent");
    }

    #[test]
    fn production_backend_rejects_synthetic_hwnd_admission() {
        let mut backend = HostOwnedNativeSurfaceBackend::new();
        let registration = fake_registration();
        assert_eq!(
            backend.admit_host_allocation(
                registration.child_window(),
                registration.parking_window_handle(),
                registration.physical_bounds(),
            ),
            Err(HostOwnedSurfaceBindError::LiveWindowRequired)
        );
    }

    #[test]
    fn production_backend_propagates_win32_park_failure_without_mutation() {
        let mut backend = HostOwnedNativeSurfaceBackend::new();
        let child = BrowserWindowHandle::from_raw(0x9301).expect("child");
        let parking = BrowserWindowHandle::from_raw(0x9401).expect("parking");
        assert!(backend.park_surface(&child, &parking).is_err());
        assert!(backend.record(&child).is_err());
    }

    #[test]
    fn production_verify_propagates_parent_query_failure() {
        let mut backend = HostOwnedNativeSurfaceBackend::new();
        let registration = fake_registration();
        let descriptor = crate::protocol::BrowserSurfaceDescriptor {
            identity: registration.identity(),
            child_hwnd: registration.child_window().clone(),
            host_process: registration.host_process().clone(),
            host_fence: crate::protocol::BrowserHostFence::new(1, 1).expect("fence"),
            runtime_generation: crate::protocol::BrowserRuntimeGeneration::new(1).expect("gen"),
            nonce: crate::protocol::BrowserSurfaceNonce::new([1; 16]).expect("nonce"),
            bounds_epoch: crate::protocol::BrowserBoundsEpoch::initial(),
            focus_epoch: crate::protocol::BrowserFocusEpoch::initial(),
            physical_bounds: registration.physical_bounds(),
            dpi: crate::protocol::BrowserDpi::new(96, 96).expect("dpi"),
        };
        assert!(backend
            .verify_surface_state(
                &descriptor,
                registration.parking_window_handle(),
                None,
                false,
                registration.physical_bounds(),
                false,
            )
            .is_err());
    }

    #[test]
    fn completed_wry_view_without_task_identity_is_blocked() {
        assert_eq!(
            require_completed_wry_task_identity(None),
            Err(BrowserTaskSurfaceBindBlocker::TaskIdentityUnavailableAtBuildCompletion)
        );
    }

    #[test]
    fn legacy_mcp_without_task_id_is_blocked() {
        assert_eq!(
            legacy_mcp_command_task_identity(None),
            Err(LegacyMcpTaskSurfaceBlocker::WorkspaceCommandLacksTaskId)
        );
    }

    #[test]
    fn completed_wry_view_with_identity_is_not_auto_bound() {
        let identity = crate::protocol::BrowserSurfaceIdentity {
            task_id: crate::domain::id::TaskId::new(),
            context_id: crate::domain::id::BrowserContextId::new(),
            resource_id: crate::domain::id::ResourceId::new(),
        };
        assert_eq!(
            require_completed_wry_task_identity(Some(identity)),
            Ok(identity)
        );
        assert_eq!(
            BrowserTaskSurfaceBindBlocker::ChildHwndUnobservable.to_string(),
            "Wry WebViewExtWindows exposes controller/environment/reparent, not a child HWND"
        );
    }

    #[test]
    fn production_teardown_observation_cannot_use_iswindow_false_as_proof() {
        let mut backend = HostOwnedNativeSurfaceBackend::new();
        let registration = fake_registration();
        let descriptor = crate::protocol::BrowserSurfaceDescriptor {
            identity: registration.identity(),
            child_hwnd: registration.child_window().clone(),
            host_process: registration.host_process().clone(),
            host_fence: crate::protocol::BrowserHostFence::new(1, 1).expect("fence"),
            runtime_generation: crate::protocol::BrowserRuntimeGeneration::new(1).expect("gen"),
            nonce: crate::protocol::BrowserSurfaceNonce::new([1; 16]).expect("nonce"),
            bounds_epoch: crate::protocol::BrowserBoundsEpoch::initial(),
            focus_epoch: crate::protocol::BrowserFocusEpoch::initial(),
            physical_bounds: registration.physical_bounds(),
            dpi: crate::protocol::BrowserDpi::new(96, 96).expect("dpi"),
        };
        assert!(backend
            .observe_teardown_zero_residue(&descriptor, registration.parking_window_handle())
            .is_err());
        assert!(backend
            .set_surface_bounds(&descriptor.child_hwnd, registration.physical_bounds())
            .is_err());
        assert!(backend
            .set_surface_focus(&descriptor.child_hwnd, true)
            .is_err());
    }

    #[test]
    fn synthetic_residue_blocks_zero_residue_observation() {
        let mut backend = HostOwnedNativeSurfaceBackend::new_synthetic_for_test();
        let registration = fake_registration();
        backend
            .admit_host_allocation(
                registration.child_window(),
                registration.parking_window_handle(),
                registration.physical_bounds(),
            )
            .expect("admit");
        backend
            .mark_controller_closed(registration.child_window())
            .expect("closed");
        backend.inject_helper_residue_for_test(registration.child_window(), 1);
        let descriptor = crate::protocol::BrowserSurfaceDescriptor {
            identity: registration.identity(),
            child_hwnd: registration.child_window().clone(),
            host_process: registration.host_process().clone(),
            host_fence: crate::protocol::BrowserHostFence::new(1, 1).expect("fence"),
            runtime_generation: crate::protocol::BrowserRuntimeGeneration::new(1).expect("gen"),
            nonce: crate::protocol::BrowserSurfaceNonce::new([2; 16]).expect("nonce"),
            bounds_epoch: crate::protocol::BrowserBoundsEpoch::initial(),
            focus_epoch: crate::protocol::BrowserFocusEpoch::initial(),
            physical_bounds: registration.physical_bounds(),
            dpi: crate::protocol::BrowserDpi::new(96, 96).expect("dpi"),
        };
        assert_eq!(
            backend
                .observe_teardown_zero_residue(&descriptor, registration.parking_window_handle()),
            Err(HostOwnedSurfaceBindError::ResiduePresent.to_string())
        );
        assert!(
            backend
                .observe_teardown_zero_residue(&descriptor, registration.parking_window_handle())
                .is_err(),
            "synthetic residue must remain fail-closed"
        );
        backend.inject_helper_residue_for_test(registration.child_window(), 0);
        assert_eq!(
            backend
                .observe_teardown_zero_residue(&descriptor, registration.parking_window_handle()),
            Ok(())
        );
        assert!(
            backend.record(registration.child_window()).is_ok(),
            "synthetic observation keeps the test map until explicit release"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn production_zero_residue_requires_dead_child_and_live_parking() {
        use windows::core::w;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, IsWindow, WS_CHILD, WS_DISABLED, WS_EX_NOACTIVATE,
            WS_EX_TOOLWINDOW, WS_POPUP,
        };
        let bounds = crate::protocol::BrowserPhysicalBounds::new(0, 0, 8, 8).expect("bounds");
        unsafe {
            let parking = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!(""),
                WS_POPUP | WS_DISABLED,
                -32_000,
                -32_000,
                8,
                8,
                None,
                None,
                None,
                None,
            )
            .expect("create parking HWND");
            let child = CreateWindowExW(
                WS_EX_NOACTIVATE,
                w!("STATIC"),
                w!(""),
                WS_CHILD | WS_DISABLED,
                0,
                0,
                8,
                8,
                Some(parking),
                None,
                None,
                None,
            )
            .expect("create child HWND");
            assert!(IsWindow(Some(parking)).as_bool());
            assert!(IsWindow(Some(child)).as_bool());
            let child_handle =
                BrowserWindowHandle::from_raw(child.0 as usize as u64).expect("child handle");
            let parking_handle =
                BrowserWindowHandle::from_raw(parking.0 as usize as u64).expect("parking handle");
            let mut backend = HostOwnedNativeSurfaceBackend::new();
            backend
                .admit_host_allocation(&child_handle, &parking_handle, bounds)
                .expect("admit live HWNDs");
            let registration = BrowserNativeViewRegistration::from_host_record(
                crate::protocol::BrowserSurfaceIdentity {
                    task_id: crate::domain::id::TaskId::new(),
                    context_id: crate::domain::id::BrowserContextId::new(),
                    resource_id: crate::domain::id::ResourceId::new(),
                },
                child_handle.clone(),
                parking_handle.clone(),
                crate::protocol::BrowserHostProcessIdentity::new(1, 1, "C:\\DevManager\\host.exe")
                    .expect("proc"),
                bounds,
                crate::protocol::BrowserDpi::new(96, 96).expect("dpi"),
            )
            .expect("registration");
            let descriptor = crate::protocol::BrowserSurfaceDescriptor {
                identity: registration.identity(),
                child_hwnd: child_handle.clone(),
                host_process: registration.host_process().clone(),
                host_fence: crate::protocol::BrowserHostFence::new(1, 1).expect("fence"),
                runtime_generation: crate::protocol::BrowserRuntimeGeneration::new(1).expect("gen"),
                nonce: crate::protocol::BrowserSurfaceNonce::new([3; 16]).expect("nonce"),
                bounds_epoch: crate::protocol::BrowserBoundsEpoch::initial(),
                focus_epoch: crate::protocol::BrowserFocusEpoch::initial(),
                physical_bounds: bounds,
                dpi: crate::protocol::BrowserDpi::new(96, 96).expect("dpi"),
            };
            backend
                .mark_controller_closed(&child_handle)
                .expect("closed");
            assert_eq!(
                backend.observe_teardown_zero_residue(&descriptor, &parking_handle),
                Err(HostOwnedSurfaceBindError::ResiduePresent.to_string()),
                "live child HWND is residue"
            );
            let _ = DestroyWindow(child);
            backend
                .observe_teardown_zero_residue(&descriptor, &parking_handle)
                .expect("dead child + live parking");
            assert!(
                backend.record(&child_handle).is_err(),
                "production observation must drop the live record"
            );
            backend
                .observe_teardown_zero_residue(&descriptor, &parking_handle)
                .expect("Ready re-observe stays fail-closed and idempotent");
            let _ = DestroyWindow(parking);
        }
    }
}

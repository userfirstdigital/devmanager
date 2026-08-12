use super::{
    redacted_browser_annotation, BrowserAnnotation, BrowserAnnotationDetails,
    BrowserAnnotationOperation, BrowserAnnotationSummary, BrowserAttachmentProjection,
    BrowserAttachmentRevision, BrowserError, BrowserResourceHandle, BrowserResourceId,
    BrowserResourceKind, BrowserResourceStore, BrowserRevision, BrowserStorageLayout,
    BrowserTabSnapshot, BrowserViewport, BrowserWorkspaceKey, BrowserWorkspaceSnapshot,
};
use crate::domain::id::ResourceId;
use crate::protocol::{
    browser_logical_to_physical, BrowserAttachRequest, BrowserAttachmentLease, BrowserBoundsEpoch,
    BrowserClientRequest, BrowserDpi, BrowserDtoError, BrowserFocusEpoch, BrowserGeometryInput,
    BrowserHostFence, BrowserHostProcessIdentity, BrowserHostRequest, BrowserHostRequestLease,
    BrowserNativeViewReconciliation, BrowserPhysicalBounds, BrowserRuntimeGeneration,
    BrowserSurfaceDescriptor, BrowserSurfaceIdentity, BrowserSurfaceLifecycle, BrowserSurfaceNonce,
    BrowserWindowHandle, MAX_BROWSER_CLIENT_SEQUENCE,
};
mod initialization;
mod native_surface;
mod unsupported;
#[cfg(target_os = "windows")]
mod windows;

pub use native_surface::{
    legacy_mcp_command_task_identity, require_completed_wry_task_identity,
    BrowserHostOwnedSurfaceProof, BrowserTaskSurfaceBindBlocker, HostOwnedNativeSurfaceBackend,
    HostOwnedSurfaceBindError, LegacyMcpTaskSurfaceBlocker,
};

use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(not(target_os = "windows"))]
pub use unsupported::BrowserWebViewHost;
pub use unsupported::{
    unsupported_command_response, unsupported_host_status, unsupported_platform_error,
};
#[cfg(test)]
pub(crate) use unsupported::{
    unsupported_request_response, unsupported_validated_command_response,
};
#[cfg(target_os = "windows")]
pub use windows::BrowserWebViewHost;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BrowserAppExitDisposition {
    ExitNow,
    Deferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowserNativeWindowPhase {
    Open,
    Closing,
    Draining,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BrowserNativeWindowLifetimeError {
    AdmissionClosed,
    WindowLeaseConflict,
    GenerationExhausted,
    LeaseCountExhausted,
}

impl std::fmt::Display for BrowserNativeWindowLifetimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::AdmissionClosed => "browser window admission is closed",
            Self::WindowLeaseConflict => "browser window identity is still leased",
            Self::GenerationExhausted => "browser window lifetime generation is exhausted",
            Self::LeaseCountExhausted => "browser window lease count is exhausted",
        })
    }
}

struct BrowserNativeWindowLifetimeState {
    phase: Cell<BrowserNativeWindowPhase>,
    window_identity: Cell<Option<isize>>,
    parking_hwnd: Cell<Option<u64>>,
    generation: Cell<u64>,
    lease_count: Cell<usize>,
}

#[derive(Clone)]
pub(crate) struct BrowserNativeWindowLifetime {
    state: Rc<BrowserNativeWindowLifetimeState>,
}

impl Default for BrowserNativeWindowLifetime {
    fn default() -> Self {
        Self {
            state: Rc::new(BrowserNativeWindowLifetimeState {
                phase: Cell::new(BrowserNativeWindowPhase::Open),
                window_identity: Cell::new(None),
                parking_hwnd: Cell::new(None),
                generation: Cell::new(0),
                lease_count: Cell::new(0),
            }),
        }
    }
}

impl BrowserNativeWindowLifetime {
    pub(crate) fn guard_window_close(&self, handler: impl FnOnce() -> bool) -> bool {
        if self.state.phase.get() == BrowserNativeWindowPhase::Closing {
            return false;
        }
        let had_window_lease = self.state.lease_count.get() != 0;
        let handler_result = handler();
        if had_window_lease || self.window_close_must_be_deferred() {
            false
        } else {
            handler_result
        }
    }

    pub(crate) fn bind_window(
        &self,
        window_identity: isize,
    ) -> Result<u64, BrowserNativeWindowLifetimeError> {
        if self.state.phase.get() != BrowserNativeWindowPhase::Open {
            return Err(BrowserNativeWindowLifetimeError::AdmissionClosed);
        }
        if self.state.generation.get() == u64::MAX {
            return Err(BrowserNativeWindowLifetimeError::GenerationExhausted);
        }
        match self.state.window_identity.get() {
            Some(current) if current == window_identity => Ok(self.state.generation.get()),
            Some(_) if self.state.lease_count.get() != 0 => {
                Err(BrowserNativeWindowLifetimeError::WindowLeaseConflict)
            }
            _ => {
                let generation = self
                    .state
                    .generation
                    .get()
                    .checked_add(1)
                    .ok_or(BrowserNativeWindowLifetimeError::GenerationExhausted)?;
                self.state.window_identity.set(Some(window_identity));
                self.state.generation.set(generation);
                Ok(generation)
            }
        }
    }

    pub(crate) fn acquire(
        &self,
        window_identity: isize,
        generation: u64,
    ) -> Result<BrowserNativeWindowBuildLease, BrowserNativeWindowLifetimeError> {
        if self.state.phase.get() != BrowserNativeWindowPhase::Open
            || self.state.window_identity.get() != Some(window_identity)
            || self.state.generation.get() != generation
        {
            return Err(BrowserNativeWindowLifetimeError::AdmissionClosed);
        }
        if generation == u64::MAX {
            return Err(BrowserNativeWindowLifetimeError::GenerationExhausted);
        }
        let lease_count = self
            .state
            .lease_count
            .get()
            .checked_add(1)
            .ok_or(BrowserNativeWindowLifetimeError::LeaseCountExhausted)?;
        self.state.lease_count.set(lease_count);
        Ok(BrowserNativeWindowBuildLease {
            state: Rc::clone(&self.state),
            window_identity,
            generation,
        })
    }

    pub(crate) fn begin_teardown(
        &self,
    ) -> Result<BrowserAppExitDisposition, BrowserNativeWindowLifetimeError> {
        if self.state.phase.get() != BrowserNativeWindowPhase::Closing {
            let generation = self
                .state
                .generation
                .get()
                .checked_add(1)
                .ok_or(BrowserNativeWindowLifetimeError::GenerationExhausted)?;
            self.state.phase.set(BrowserNativeWindowPhase::Closing);
            self.state.generation.set(generation);
        }
        Ok(self.exit_disposition())
    }

    pub(crate) fn retain_teardown_cleanup(
        &self,
    ) -> Result<BrowserNativeWindowTeardownLease, BrowserNativeWindowLifetimeError> {
        if self.state.phase.get() != BrowserNativeWindowPhase::Closing {
            return Err(BrowserNativeWindowLifetimeError::AdmissionClosed);
        }
        let lease_count = self
            .state
            .lease_count
            .get()
            .checked_add(1)
            .ok_or(BrowserNativeWindowLifetimeError::LeaseCountExhausted)?;
        self.state.lease_count.set(lease_count);
        Ok(BrowserNativeWindowTeardownLease {
            state: Rc::clone(&self.state),
        })
    }

    pub(crate) fn resume_after_canceled_teardown(&self) -> bool {
        if self.state.phase.get() != BrowserNativeWindowPhase::Closing {
            return false;
        }
        self.state.phase.set(if self.state.lease_count.get() == 0 {
            BrowserNativeWindowPhase::Open
        } else {
            BrowserNativeWindowPhase::Draining
        });
        true
    }

    pub(crate) fn exit_disposition(&self) -> BrowserAppExitDisposition {
        if self.state.lease_count.get() == 0 {
            BrowserAppExitDisposition::ExitNow
        } else {
            BrowserAppExitDisposition::Deferred
        }
    }

    pub(crate) fn teardown_ready(&self) -> bool {
        self.state.phase.get() == BrowserNativeWindowPhase::Closing
            && self.state.lease_count.get() == 0
    }

    pub(crate) fn parking_window_handle(&self) -> Option<BrowserWindowHandle> {
        let raw = self.state.parking_hwnd.get()?;
        BrowserWindowHandle::from_raw(raw).ok()
    }

    pub(crate) fn install_parking_hwnd(
        &self,
        parking_hwnd: u64,
        gpui_window_identity: isize,
    ) -> Result<BrowserWindowHandle, BrowserNativeWindowLifetimeError> {
        if parking_hwnd == 0 || parking_hwnd == gpui_window_identity as u64 {
            return Err(BrowserNativeWindowLifetimeError::WindowLeaseConflict);
        }
        match self.state.parking_hwnd.get() {
            Some(existing) if existing == parking_hwnd => BrowserWindowHandle::from_raw(existing)
                .map_err(|_| BrowserNativeWindowLifetimeError::WindowLeaseConflict),
            Some(_) => Err(BrowserNativeWindowLifetimeError::WindowLeaseConflict),
            None => {
                self.state.parking_hwnd.set(Some(parking_hwnd));
                BrowserWindowHandle::from_raw(parking_hwnd)
                    .map_err(|_| BrowserNativeWindowLifetimeError::WindowLeaseConflict)
            }
        }
    }

    pub(crate) fn take_parking_hwnd_for_destroy(&self) -> Option<u64> {
        if !matches!(
            self.state.phase.get(),
            BrowserNativeWindowPhase::Closing | BrowserNativeWindowPhase::Draining
        ) {
            return None;
        }
        if self.state.lease_count.get() != 0 {
            return None;
        }
        self.state.parking_hwnd.take()
    }

    pub(crate) fn window_close_must_be_deferred(&self) -> bool {
        self.state.phase.get() != BrowserNativeWindowPhase::Open
            || self.state.lease_count.get() != 0
    }

    pub(crate) fn assert_drained_after_window_close(&self) {
        debug_assert_eq!(
            self.state.lease_count.get(),
            0,
            "GPUI window closed while native browser builds retained its HWND"
        );
    }
}

pub(crate) struct BrowserNativeWindowBuildLease {
    state: Rc<BrowserNativeWindowLifetimeState>,
    window_identity: isize,
    generation: u64,
}

impl BrowserNativeWindowBuildLease {
    pub(crate) fn build_is_allowed(&self) -> bool {
        self.state.phase.get() == BrowserNativeWindowPhase::Open
            && self.state.window_identity.get() == Some(self.window_identity)
            && self.state.generation.get() == self.generation
    }
}

impl Drop for BrowserNativeWindowBuildLease {
    fn drop(&mut self) {
        release_native_window_lease(&self.state);
    }
}

pub(crate) struct BrowserNativeWindowTeardownLease {
    state: Rc<BrowserNativeWindowLifetimeState>,
}

impl Drop for BrowserNativeWindowTeardownLease {
    fn drop(&mut self) {
        release_native_window_lease(&self.state);
    }
}

fn release_native_window_lease(state: &BrowserNativeWindowLifetimeState) {
    let leases = state.lease_count.get();
    debug_assert!(leases > 0, "native browser window lease underflow");
    let Some(remaining) = leases.checked_sub(1) else {
        return;
    };
    state.lease_count.set(remaining);
    if remaining == 0 && state.phase.get() == BrowserNativeWindowPhase::Draining {
        state.phase.set(BrowserNativeWindowPhase::Open);
    }
}

#[cfg(test)]
mod native_window_lifetime_tests {
    use super::{BrowserAppExitDisposition, BrowserNativeWindowLifetime};
    use std::cell::Cell;

    trait ExhaustionOutcome {
        fn is_exhausted(&self) -> bool;
    }

    impl ExhaustionOutcome for Option<u64> {
        fn is_exhausted(&self) -> bool {
            false
        }
    }

    impl<T, E> ExhaustionOutcome for Result<T, E> {
        fn is_exhausted(&self) -> bool {
            self.is_err()
        }
    }

    #[test]
    fn parking_hwnd_survives_until_closing_drain() {
        let lifetime = BrowserNativeWindowLifetime::default();
        let generation = lifetime.bind_window(101).unwrap();
        let lease = lifetime.acquire(101, generation).unwrap();
        lifetime
            .install_parking_hwnd(202, 101)
            .expect("distinct parking");
        drop(lease);
        assert!(
            lifetime.take_parking_hwnd_for_destroy().is_none(),
            "open-phase host operation must retain the parking HWND even with zero leases"
        );
        let lease = lifetime.acquire(101, generation).unwrap();
        assert_eq!(
            lifetime.begin_teardown().unwrap(),
            BrowserAppExitDisposition::Deferred
        );
        assert!(
            lifetime.take_parking_hwnd_for_destroy().is_none(),
            "parking HWND stays while build leases remain"
        );
        drop(lease);
        assert!(lifetime.teardown_ready());
        assert_eq!(lifetime.take_parking_hwnd_for_destroy(), Some(202));
        assert!(lifetime.parking_window_handle().is_none());
    }

    #[test]
    fn parking_hwnd_is_not_the_gpui_window_alias() {
        let lifetime = BrowserNativeWindowLifetime::default();
        let generation = lifetime.bind_window(101).unwrap();
        assert!(
            lifetime.parking_window_handle().is_none(),
            "GPUI parent identity must not be treated as the parking HWND"
        );
        assert!(lifetime.install_parking_hwnd(101, 101).is_err());
        let parking = lifetime
            .install_parking_hwnd(202, 101)
            .expect("distinct parking");
        assert_eq!(parking.raw_value(), 202);
        assert_ne!(parking.raw_value(), 101);
        drop(generation);
    }

    #[test]
    fn active_and_queued_window_leases_defer_exit_until_the_last_completion_releases() {
        let lifetime = BrowserNativeWindowLifetime::default();
        let generation = lifetime.bind_window(101).unwrap();
        let active = lifetime.acquire(101, generation).unwrap();
        let queued = lifetime.acquire(101, generation).unwrap();

        assert_eq!(
            lifetime.begin_teardown().unwrap(),
            BrowserAppExitDisposition::Deferred
        );
        assert!(lifetime.window_close_must_be_deferred());
        assert!(!active.build_is_allowed());
        assert!(!queued.build_is_allowed());
        assert!(!lifetime.teardown_ready());

        drop(queued);
        assert!(!lifetime.teardown_ready());
        drop(active);
        assert!(lifetime.teardown_ready());
        assert_eq!(
            lifetime.exit_disposition(),
            BrowserAppExitDisposition::ExitNow
        );
    }

    #[test]
    fn canceled_shutdown_waits_for_old_generation_to_drain_before_reopening_admission() {
        let lifetime = BrowserNativeWindowLifetime::default();
        let generation = lifetime.bind_window(202).unwrap();
        let canceled = lifetime.acquire(202, generation).unwrap();
        assert_eq!(
            lifetime.begin_teardown().unwrap(),
            BrowserAppExitDisposition::Deferred
        );
        assert!(lifetime.acquire(202, generation).is_err());

        assert!(lifetime.resume_after_canceled_teardown());
        assert!(lifetime.bind_window(202).is_err());
        assert!(lifetime.window_close_must_be_deferred());
        assert!(!canceled.build_is_allowed());

        drop(canceled);
        let resumed_generation = lifetime.bind_window(202).unwrap();
        assert_ne!(resumed_generation, generation);
        let replacement = lifetime.acquire(202, resumed_generation).unwrap();
        assert!(replacement.build_is_allowed());
        assert!(lifetime.window_close_must_be_deferred());

        drop(replacement);
        assert!(!lifetime.window_close_must_be_deferred());
    }

    #[test]
    fn changing_the_actual_window_identity_requires_all_old_leases_to_drain() {
        let lifetime = BrowserNativeWindowLifetime::default();
        let generation = lifetime.bind_window(303).unwrap();
        let lease = lifetime.acquire(303, generation).unwrap();
        assert!(lifetime.bind_window(404).is_err());
        drop(lease);
        assert!(lifetime.bind_window(404).is_ok());
    }

    #[test]
    fn canceled_shutdown_before_first_browser_build_reopens_window_admission() {
        let lifetime = BrowserNativeWindowLifetime::default();
        assert_eq!(
            lifetime.begin_teardown().unwrap(),
            BrowserAppExitDisposition::ExitNow
        );
        assert!(lifetime.window_close_must_be_deferred());
        assert!(lifetime.resume_after_canceled_teardown());
        assert!(!lifetime.window_close_must_be_deferred());
        assert!(lifetime.bind_window(505).is_ok());
    }

    #[test]
    fn lifetime_generation_exhaustion_is_typed_and_does_not_silently_close_admission() {
        let lifetime = BrowserNativeWindowLifetime::default();
        lifetime.state.window_identity.set(Some(505));
        lifetime.state.generation.set(u64::MAX);
        let outcome = lifetime.bind_window(505);
        assert!(
            outcome.is_exhausted(),
            "window lifetime generation exhaustion must be a typed failure"
        );
        assert!(
            lifetime.acquire(505, u64::MAX).is_err(),
            "the terminal generation must never be admitted for a native build"
        );

        let teardown = lifetime.begin_teardown();
        assert!(
            teardown.is_exhausted(),
            "window teardown generation exhaustion must be a typed failure"
        );
        assert_eq!(
            lifetime.state.phase.get(),
            BrowserNativeWindowPhase::Open,
            "failed teardown advancement must not silently close admission"
        );
    }

    #[test]
    fn open_leased_close_enters_teardown_once_and_repeated_close_is_idempotent() {
        let lifetime = BrowserNativeWindowLifetime::default();
        let generation = lifetime.bind_window(606).unwrap();
        let lease = lifetime.acquire(606, generation).unwrap();
        let handler_calls = Cell::new(0);

        let first_result = lifetime.guard_window_close(|| {
            handler_calls.set(handler_calls.get() + 1);
            assert_eq!(
                lifetime.begin_teardown().unwrap(),
                BrowserAppExitDisposition::Deferred
            );
            true
        });
        assert!(!first_result, "an accepted HWND close must remain deferred");
        assert_eq!(handler_calls.get(), 1);

        let repeated_result = lifetime.guard_window_close(|| {
            handler_calls.set(handler_calls.get() + 1);
            true
        });
        assert!(!repeated_result);
        assert_eq!(
            handler_calls.get(),
            1,
            "Closing must reject repeat close requests without duplicating lifecycle work"
        );
        assert!(!lifetime.teardown_ready());

        drop(lease);
        assert!(lifetime.teardown_ready());
    }

    #[test]
    fn deferred_webview_destruction_retains_the_window_until_cleanup_finishes() {
        let lifetime = BrowserNativeWindowLifetime::default();

        assert_eq!(
            lifetime.begin_teardown().unwrap(),
            BrowserAppExitDisposition::ExitNow
        );
        let cleanup = lifetime
            .retain_teardown_cleanup()
            .expect("teardown cleanup lease");

        assert_eq!(
            lifetime.exit_disposition(),
            BrowserAppExitDisposition::Deferred
        );
        assert!(!lifetime.teardown_ready());

        drop(cleanup);
        assert!(lifetime.teardown_ready());
        assert_eq!(
            lifetime.exit_disposition(),
            BrowserAppExitDisposition::ExitNow
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserWorkspaceMutation {
    pub revision: BrowserRevision,
    pub snapshot: BrowserWorkspaceSnapshot,
}

impl BrowserWorkspaceMutation {
    fn new(snapshot: BrowserWorkspaceSnapshot) -> Self {
        Self {
            revision: snapshot.revision,
            snapshot,
        }
    }
}

pub fn acknowledge_attachment_projection_and_reconcile_pins(
    state: &mut BrowserHostState,
    resources: &BrowserResourceStore,
    projection: &BrowserAttachmentProjection,
    mut additional_pinned_resource_ids: BTreeSet<BrowserResourceId>,
) -> Result<BrowserWorkspaceSnapshot, BrowserError> {
    let mutation = state.acknowledge_attachment_projection(
        &projection.workspace_key,
        projection.revision,
        &projection.pending_annotation_ids,
        &projection.tombstone_annotation_ids,
    )?;
    additional_pinned_resource_ids.extend(mutation.snapshot.pinned_annotation_resource_ids());
    resources
        .reconcile_annotation_pins(&projection.workspace_key, &additional_pinned_resource_ids)?;
    Ok(mutation.snapshot)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserAnnotationMutationResult {
    pub operation: BrowserAnnotationOperation,
    pub annotation_id: String,
    pub screenshot: BrowserResourceHandle,
    pub mutation: BrowserWorkspaceMutation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserViewCreationPlan {
    pub workspace_key: BrowserWorkspaceKey,
    pub tab_id: String,
    pub url: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserMemoryTarget {
    Normal,
    Low,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserViewVisibilityPlan {
    pub workspace_key: BrowserWorkspaceKey,
    pub tab_id: String,
    pub visible: bool,
    pub memory_target: BrowserMemoryTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BrowserProjectContextKey {
    pub project_id: String,
    pub profile_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserProfileClearPlan {
    pub profile_dir: PathBuf,
}

impl BrowserProfileClearPlan {
    pub fn paths(&self) -> [&Path; 1] {
        [self.profile_dir.as_path()]
    }
}

/// The host-owned parking-window boundary.  This is only an opaque handle
/// today; creating and destroying the real native window belongs to a later
/// Windows/WebView2 phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BrowserParkingWindow {
    handle: BrowserWindowHandle,
}

impl BrowserParkingWindow {
    pub(crate) fn new(handle: BrowserWindowHandle) -> Self {
        Self { handle }
    }

    pub(crate) fn handle(&self) -> &BrowserWindowHandle {
        &self.handle
    }
}

/// Host-created native-view data.  This is an internal subordinate record of
/// BrowserWebViewHost's existing WebView record; it is not a client input.
///
/// ```compile_fail
/// use devmanager::browser::BrowserNativeViewRegistration;
///
/// let _constructor = BrowserNativeViewRegistration::new;
/// ```
#[derive(Debug, Clone)]
pub struct BrowserNativeViewRegistration {
    identity: BrowserSurfaceIdentity,
    child_window: BrowserWindowHandle,
    parking_window: BrowserParkingWindow,
    host_process: BrowserHostProcessIdentity,
    physical_bounds: BrowserPhysicalBounds,
    dpi: BrowserDpi,
}

impl BrowserNativeViewRegistration {
    pub(crate) fn from_host_record(
        identity: BrowserSurfaceIdentity,
        child_window: BrowserWindowHandle,
        parking_window: BrowserWindowHandle,
        host_process: BrowserHostProcessIdentity,
        physical_bounds: BrowserPhysicalBounds,
        dpi: BrowserDpi,
    ) -> Result<Self, BrowserNativeViewError> {
        if child_window == parking_window {
            return Err(BrowserNativeViewError::Descriptor(
                BrowserDtoError::Invalid("child and parking windows must differ"),
            ));
        }
        host_process.validate()?;
        let physical_bounds = BrowserPhysicalBounds::new(
            physical_bounds.x(),
            physical_bounds.y(),
            physical_bounds.width(),
            physical_bounds.height(),
        )?;
        let dpi = BrowserDpi::new(dpi.horizontal, dpi.vertical)?;
        Ok(Self {
            identity,
            child_window,
            parking_window: BrowserParkingWindow::new(parking_window),
            host_process,
            physical_bounds,
            dpi,
        })
    }

    pub fn identity(&self) -> BrowserSurfaceIdentity {
        self.identity
    }

    pub(crate) fn child_window(&self) -> &BrowserWindowHandle {
        &self.child_window
    }

    pub(crate) fn parking_window_handle(&self) -> &BrowserWindowHandle {
        self.parking_window.handle()
    }

    pub(crate) fn physical_bounds(&self) -> BrowserPhysicalBounds {
        self.physical_bounds
    }

    pub(crate) fn host_process(&self) -> &BrowserHostProcessIdentity {
        &self.host_process
    }
}

mod browser_native_surface_backend_seal {
    pub trait Sealed {}
}

/// Opaque UI-thread contract for native surface operations. Only host-owned
/// code can provide the UI-thread implementation that is allowed to issue
/// authority.
///
/// ```compile_fail
/// struct ExternalBackend;
/// impl devmanager::browser::BrowserNativeSurfaceBackend for ExternalBackend {}
/// ```
pub trait BrowserNativeSurfaceBackend: browser_native_surface_backend_seal::Sealed {
    /// Prove that the backend allocated both live windows for this exact
    /// task/resource before the host issues a descriptor or mutates its
    /// registry.
    fn preflight_native_view_allocation(
        &mut self,
        registration: &BrowserNativeViewRegistration,
    ) -> Result<(), String>;

    /// Release every native allocation made by the matching allocation
    /// preflight.  Registration owns this rollback through a transaction
    /// guard, so a backend must make this operation idempotent.
    fn rollback_native_view_allocation(
        &mut self,
        registration: &BrowserNativeViewRegistration,
    ) -> Result<(), String>;

    /// Revalidate the live HWND owner/job and the complete host-issued
    /// task/resource/generation/connection fence immediately before every
    /// native operation.
    fn preflight_native_view_operation(
        &mut self,
        descriptor: &BrowserSurfaceDescriptor,
        parking: &BrowserWindowHandle,
    ) -> Result<(), String>;

    fn assert_ui_thread(&self) -> Result<(), String>;

    fn park_surface(
        &mut self,
        child: &BrowserWindowHandle,
        parking: &BrowserWindowHandle,
    ) -> Result<(), String>;

    fn attach_surface(
        &mut self,
        child: &BrowserWindowHandle,
        destination: &BrowserWindowHandle,
        bounds: BrowserPhysicalBounds,
    ) -> Result<(), String>;

    fn set_surface_bounds(
        &mut self,
        child: &BrowserWindowHandle,
        bounds: BrowserPhysicalBounds,
    ) -> Result<(), String>;

    fn set_surface_focus(
        &mut self,
        child: &BrowserWindowHandle,
        focused: bool,
    ) -> Result<(), String>;

    /// Verify the postcondition by querying the live native surface.  A
    /// backend must not report success from its mutation method until this
    /// confirms the actual parent/bounds/focus state.
    fn verify_surface_state(
        &mut self,
        descriptor: &BrowserSurfaceDescriptor,
        parking: &BrowserWindowHandle,
        attached_parent: Option<&BrowserWindowHandle>,
        attached: bool,
        bounds: BrowserPhysicalBounds,
        focused: bool,
    ) -> Result<(), String>;

    /// Observe a renderer/controller crash through the host-owned backend.
    /// A caller cannot turn a capability into a crash fact without this live
    /// observation succeeding.
    fn observe_surface_crash(
        &mut self,
        descriptor: &BrowserSurfaceDescriptor,
        parking: &BrowserWindowHandle,
    ) -> Result<(), String>;

    /// Observe that controller/environment/listener ownership and helper
    /// residue are gone for this exact parked surface. Success is required
    /// before teardown may return Ready; backends must not invent zero residue.
    fn observe_teardown_zero_residue(
        &mut self,
        descriptor: &BrowserSurfaceDescriptor,
        parking: &BrowserWindowHandle,
    ) -> Result<(), String>;
}

const MAX_NATIVE_ALLOCATION_ORPHANS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BrowserNativeAllocationOrphan {
    identity: BrowserSurfaceIdentity,
}

#[derive(Debug, Default)]
struct BrowserNativeAllocationOrphanStore {
    records: VecDeque<BrowserNativeAllocationOrphan>,
}

impl BrowserNativeAllocationOrphanStore {
    fn record(&mut self, identity: BrowserSurfaceIdentity) {
        if self
            .records
            .iter()
            .any(|record| record.identity == identity)
        {
            return;
        }
        if self.records.len() >= MAX_NATIVE_ALLOCATION_ORPHANS {
            self.records.pop_front();
        }
        self.records
            .push_back(BrowserNativeAllocationOrphan { identity });
    }

    fn len(&self) -> usize {
        self.records.len()
    }
}

struct BrowserNativeViewAllocationGuard<'a, B: BrowserNativeSurfaceBackend> {
    backend: &'a mut B,
    registration: &'a BrowserNativeViewRegistration,
    orphan_store: Arc<Mutex<BrowserNativeAllocationOrphanStore>>,
    committed: bool,
    rolled_back: bool,
    orphan_recorded: bool,
}

impl<'a, B: BrowserNativeSurfaceBackend> BrowserNativeViewAllocationGuard<'a, B> {
    fn new(
        backend: &'a mut B,
        registration: &'a BrowserNativeViewRegistration,
        orphan_store: Arc<Mutex<BrowserNativeAllocationOrphanStore>>,
    ) -> Self {
        Self {
            backend,
            registration,
            orphan_store,
            committed: false,
            rolled_back: false,
            orphan_recorded: false,
        }
    }

    fn backend_mut(&mut self) -> &mut B {
        self.backend
    }

    fn rollback(&mut self) -> Result<(), String> {
        if self.committed || self.rolled_back {
            return Ok(());
        }
        match self
            .backend
            .rollback_native_view_allocation(self.registration)
        {
            Ok(()) => {
                self.rolled_back = true;
                Ok(())
            }
            Err(error) => {
                self.record_orphan();
                Err(error)
            }
        }
    }

    fn record_orphan(&mut self) {
        if self.orphan_recorded {
            return;
        }
        self.orphan_recorded = true;
        self.orphan_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record(self.registration.identity);
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

impl<B: BrowserNativeSurfaceBackend> Drop for BrowserNativeViewAllocationGuard<'_, B> {
    fn drop(&mut self) {
        if !self.committed && !self.rolled_back {
            match self
                .backend
                .rollback_native_view_allocation(self.registration)
            {
                Ok(()) => self.rolled_back = true,
                Err(_) => self.record_orphan(),
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserTeardownStatus {
    /// Live backend observation proved zero residue; close may complete.
    Ready,
    /// Teardown work is still in flight and must not be claimed complete.
    Pending,
    Blocked(BrowserTeardownBlocker),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserTeardownBlocker {
    SurfaceMustBeParked,
    NativeSurfaceReconciliationRequired,
    RealRuntimeObservationUnavailable,
}

/// Compatibility marker for older callers. Teardown proof is no longer
/// accepted from this trait; the host/backend observation seam owns it.
pub trait BrowserTeardownObserver {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserNativeViewError {
    Descriptor(BrowserDtoError),
    Entropy,
    DuplicateView,
    MissingView,
    ForeignDescriptor(&'static str),
    StaleDescriptor(&'static str),
    InvalidLifecycle(&'static str),
    ClientMismatch,
    AttachmentLeaseMismatch,
    ActiveViewConflict,
    InvalidInput(&'static str),
    UiThread,
    Backend,
    ReconciliationRequired,
    ControllerObservationMismatch(&'static str),
    TeardownPending,
    TeardownBlocked(BrowserTeardownBlocker),
    HostRequestLeaseMismatch,
    LiveWryObservationUnavailable,
    TaskIdentityUnavailable,
    LegacyMcpTaskIdentityUnavailable,
}

impl From<BrowserDtoError> for BrowserNativeViewError {
    fn from(error: BrowserDtoError) -> Self {
        Self::Descriptor(error)
    }
}

impl std::fmt::Display for BrowserNativeViewError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Descriptor(error) => write!(formatter, "browser descriptor: {error}"),
            Self::Entropy => formatter.write_str("could not issue browser capability"),
            Self::DuplicateView => formatter.write_str("browser native view is already registered"),
            Self::MissingView => formatter.write_str("browser native view is not registered"),
            Self::ForeignDescriptor(field) => {
                write!(
                    formatter,
                    "foreign browser surface descriptor field: {field}"
                )
            }
            Self::StaleDescriptor(field) => {
                write!(formatter, "stale browser surface descriptor field: {field}")
            }
            Self::InvalidLifecycle(field) => {
                write!(formatter, "invalid browser native view lifecycle: {field}")
            }
            Self::ClientMismatch => formatter.write_str("browser surface client does not match"),
            Self::AttachmentLeaseMismatch => {
                formatter.write_str("browser surface attachment lease does not match")
            }
            Self::ActiveViewConflict => {
                formatter.write_str("another browser native view is already attached")
            }
            Self::InvalidInput(field) => {
                write!(formatter, "invalid browser surface input: {field}")
            }
            Self::UiThread => formatter.write_str("browser surface UI-thread violation"),
            Self::Backend => formatter.write_str("browser surface backend failed"),
            Self::ReconciliationRequired => {
                formatter.write_str("browser native surface requires reconciliation")
            }
            Self::ControllerObservationMismatch(field) => {
                write!(
                    formatter,
                    "browser controller observation mismatch: {field}"
                )
            }
            Self::TeardownPending => formatter.write_str("browser teardown is still pending"),
            Self::TeardownBlocked(blocker) => {
                write!(formatter, "browser teardown blocked: {blocker:?}")
            }
            Self::HostRequestLeaseMismatch => {
                formatter.write_str("browser host request lease does not match")
            }
            Self::LiveWryObservationUnavailable => {
                formatter.write_str("live Wry/WebView2 controller observation is unavailable")
            }
            Self::TaskIdentityUnavailable => {
                formatter.write_str("completed native view has no exact task/context identity")
            }
            Self::LegacyMcpTaskIdentityUnavailable => {
                formatter.write_str("legacy MCP/chrome command has no exact TaskId")
            }
        }
    }
}

impl std::error::Error for BrowserNativeViewError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserNativeViewReceipt {
    pub descriptor: BrowserSurfaceDescriptor,
    pub lifecycle: BrowserSurfaceLifecycle,
    pub attachment_lease: Option<BrowserAttachmentLease>,
    pub attached_parent: Option<BrowserWindowHandle>,
    pub focused: bool,
    pub reconciliation: BrowserNativeViewReconciliation,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct BrowserControllerCapability {
    identity: BrowserSurfaceIdentity,
    child_window: BrowserWindowHandle,
    host_process: BrowserHostProcessIdentity,
    host_fence: BrowserHostFence,
    runtime_generation: BrowserRuntimeGeneration,
    nonce: BrowserSurfaceNonce,
}

impl std::fmt::Debug for BrowserControllerCapability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("BrowserControllerCapability(<redacted>)")
    }
}

impl BrowserControllerCapability {
    fn from_descriptor(descriptor: &BrowserSurfaceDescriptor) -> Self {
        Self {
            identity: descriptor.identity,
            child_window: descriptor.child_hwnd.clone(),
            host_process: descriptor.host_process.clone(),
            host_fence: descriptor.host_fence,
            runtime_generation: descriptor.runtime_generation,
            nonce: descriptor.nonce,
        }
    }
}

#[derive(Debug, Clone)]
struct BrowserNativeView {
    descriptor: BrowserSurfaceDescriptor,
    parking_window: BrowserParkingWindow,
    lifecycle: BrowserSurfaceLifecycle,
    attachment_lease: Option<BrowserAttachmentLease>,
    attached_parent: Option<BrowserWindowHandle>,
    host_request_lease: BrowserHostRequestLease,
    focused: bool,
    last_client_sequence: Option<u64>,
    reconciliation: BrowserNativeViewReconciliation,
    controller_capability: BrowserControllerCapability,
}

struct BrowserNativeViewRegistrationPlan {
    registration: BrowserNativeViewRegistration,
    descriptor: BrowserSurfaceDescriptor,
    host_request_lease: BrowserHostRequestLease,
}

static NEXT_NATIVE_HOST_EPOCH: AtomicU64 = AtomicU64::new(0);

fn next_native_host_epoch() -> Result<u64, BrowserNativeViewError> {
    NEXT_NATIVE_HOST_EPOCH
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .map_err(|_| {
            BrowserNativeViewError::Descriptor(BrowserDtoError::Overflow(
                "global browser host epoch",
            ))
        })
        .and_then(|previous| {
            previous
                .checked_add(1)
                .ok_or(BrowserNativeViewError::Descriptor(
                    BrowserDtoError::Overflow("global browser host epoch"),
                ))
        })
}

fn issue_opaque_bytes<const N: usize>() -> Result<[u8; N], BrowserNativeViewError> {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes).map_err(|_| BrowserNativeViewError::Entropy)?;
    if bytes.iter().all(|byte| *byte == 0) {
        return Err(BrowserNativeViewError::Entropy);
    }
    Ok(bytes)
}

pub struct BrowserHostState {
    app_config_dir: PathBuf,
    workspaces: HashMap<BrowserWorkspaceKey, BrowserWorkspaceSnapshot>,
    active_workspace: Option<BrowserWorkspaceKey>,
    native_views: HashMap<ResourceId, BrowserNativeView>,
    native_host_process: Option<BrowserHostProcessIdentity>,
    native_host_fence: BrowserHostFence,
    next_native_connection_epoch: u64,
    next_native_request_epoch: u64,
    next_native_runtime_generation: u64,
    active_native_view: Option<ResourceId>,
    native_authority_available: bool,
    native_allocation_orphans: Arc<Mutex<BrowserNativeAllocationOrphanStore>>,
}

impl BrowserHostState {
    pub fn new(app_config_dir: impl AsRef<Path>) -> Result<Self, BrowserNativeViewError> {
        let boot_epoch = next_native_host_epoch()?;
        let connection_epoch = next_native_host_epoch()?;
        Ok(Self {
            app_config_dir: app_config_dir.as_ref().to_path_buf(),
            workspaces: HashMap::new(),
            active_workspace: None,
            native_views: HashMap::new(),
            native_host_process: None,
            native_host_fence: BrowserHostFence::new(boot_epoch, connection_epoch)?,
            next_native_connection_epoch: 0,
            next_native_request_epoch: 0,
            next_native_runtime_generation: 0,
            active_native_view: None,
            native_authority_available: true,
            native_allocation_orphans: Arc::new(Mutex::new(
                BrowserNativeAllocationOrphanStore::default(),
            )),
        })
    }

    pub(crate) fn unavailable(app_config_dir: impl AsRef<Path>) -> Self {
        Self {
            app_config_dir: app_config_dir.as_ref().to_path_buf(),
            workspaces: HashMap::new(),
            active_workspace: None,
            native_views: HashMap::new(),
            native_host_process: None,
            native_host_fence: BrowserHostFence {
                boot_epoch: 1,
                connection_epoch: 1,
            },
            next_native_connection_epoch: 0,
            next_native_request_epoch: 0,
            next_native_runtime_generation: 0,
            active_native_view: None,
            native_authority_available: false,
            native_allocation_orphans: Arc::new(Mutex::new(
                BrowserNativeAllocationOrphanStore::default(),
            )),
        }
    }

    fn native_allocation_orphan_count(&self) -> usize {
        self.native_allocation_orphans
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    #[cfg(test)]
    fn native_allocation_orphan_count_for_test(&self) -> usize {
        self.native_allocation_orphan_count()
    }

    fn prepare_native_view_registration(
        &self,
        registration: BrowserNativeViewRegistration,
    ) -> Result<BrowserNativeViewRegistrationPlan, BrowserNativeViewError> {
        let resource_id = registration.identity.resource_id;
        if self.native_views.contains_key(&resource_id) {
            return Err(BrowserNativeViewError::DuplicateView);
        }
        if let Some(expected) = &self.native_host_process {
            if expected != &registration.host_process {
                return Err(BrowserNativeViewError::ForeignDescriptor("host process"));
            }
        }

        let runtime_generation = self.next_native_runtime_generation.checked_add(1).ok_or(
            BrowserNativeViewError::Descriptor(BrowserDtoError::Overflow("runtime generation")),
        )?;
        let runtime_generation = BrowserRuntimeGeneration::new(runtime_generation)?;
        let nonce = BrowserSurfaceNonce::new(issue_opaque_bytes()?)?;
        let host_request_lease =
            self.next_host_request_lease(self.native_host_fence.connection_epoch)?;
        let descriptor = BrowserSurfaceDescriptor {
            identity: registration.identity,
            child_hwnd: registration.child_window.clone(),
            host_process: registration.host_process.clone(),
            host_fence: self.native_host_fence,
            runtime_generation,
            nonce,
            bounds_epoch: BrowserBoundsEpoch::initial(),
            focus_epoch: BrowserFocusEpoch::initial(),
            physical_bounds: registration.physical_bounds,
            dpi: registration.dpi,
        };
        descriptor.validate()?;
        Ok(BrowserNativeViewRegistrationPlan {
            registration,
            descriptor,
            host_request_lease,
        })
    }

    fn commit_native_view_registration(
        &mut self,
        plan: BrowserNativeViewRegistrationPlan,
    ) -> BrowserNativeViewReceipt {
        let resource_id = plan.descriptor.identity.resource_id;
        let runtime_generation = plan.descriptor.runtime_generation.value();
        let host_request_epoch = plan.host_request_lease.request_epoch();
        let controller_capability = BrowserControllerCapability::from_descriptor(&plan.descriptor);
        let view = BrowserNativeView {
            descriptor: plan.descriptor,
            parking_window: plan.registration.parking_window,
            lifecycle: BrowserSurfaceLifecycle::Parked,
            attachment_lease: None,
            attached_parent: None,
            host_request_lease: plan.host_request_lease,
            focused: false,
            last_client_sequence: None,
            reconciliation: BrowserNativeViewReconciliation::Healthy,
            controller_capability,
        };
        self.next_native_runtime_generation = runtime_generation;
        self.native_host_process = Some(plan.registration.host_process);
        self.next_native_request_epoch = host_request_epoch;
        let receipt = BrowserNativeViewReceipt {
            descriptor: view.descriptor.clone(),
            lifecycle: view.lifecycle.clone(),
            attachment_lease: view.attachment_lease.clone(),
            attached_parent: view.attached_parent.clone(),
            focused: view.focused,
            reconciliation: view.reconciliation,
        };
        self.native_views.insert(resource_id, view);
        receipt
    }

    fn next_host_request_lease(
        &self,
        connection_epoch: u64,
    ) -> Result<BrowserHostRequestLease, BrowserNativeViewError> {
        let request_epoch = self.next_native_request_epoch.checked_add(1).ok_or(
            BrowserNativeViewError::Descriptor(BrowserDtoError::Overflow(
                "browser host request epoch",
            )),
        )?;
        BrowserHostRequestLease::from_parts(connection_epoch, request_epoch, issue_opaque_bytes()?)
            .map_err(BrowserNativeViewError::Descriptor)
    }

    pub(crate) fn host_request(
        &self,
        identity: &BrowserSurfaceIdentity,
    ) -> Result<BrowserHostRequest, BrowserNativeViewError> {
        let view = self
            .native_views
            .get(&identity.resource_id)
            .filter(|view| view.descriptor.identity == *identity)
            .ok_or(BrowserNativeViewError::MissingView)?;
        Ok(BrowserHostRequest::new(
            view.descriptor.clone(),
            view.host_request_lease.clone(),
        ))
    }

    pub(crate) fn register_native_view_with_backend<B: BrowserNativeSurfaceBackend>(
        &mut self,
        registration: BrowserNativeViewRegistration,
        backend: &mut B,
    ) -> Result<BrowserNativeViewReceipt, BrowserNativeViewError> {
        if !self.native_authority_available {
            return Err(BrowserNativeViewError::ReconciliationRequired);
        }
        if self.native_allocation_orphan_count() != 0 {
            return Err(BrowserNativeViewError::ReconciliationRequired);
        }
        backend
            .assert_ui_thread()
            .map_err(|_| BrowserNativeViewError::UiThread)?;
        let plan = self.prepare_native_view_registration(registration)?;
        let mut allocation = BrowserNativeViewAllocationGuard::new(
            backend,
            &plan.registration,
            Arc::clone(&self.native_allocation_orphans),
        );
        if allocation
            .backend_mut()
            .preflight_native_view_allocation(&plan.registration)
            .is_err()
        {
            let rollback_failed = allocation.rollback().is_err();
            if rollback_failed {
                self.native_authority_available = false;
            }
            return Err(if rollback_failed {
                BrowserNativeViewError::ReconciliationRequired
            } else {
                BrowserNativeViewError::Backend
            });
        }
        for _ in 0..2 {
            if allocation
                .backend_mut()
                .preflight_native_view_operation(
                    &plan.descriptor,
                    plan.registration.parking_window.handle(),
                )
                .is_err()
            {
                let rollback_failed = allocation.rollback().is_err();
                if rollback_failed {
                    self.native_authority_available = false;
                }
                return Err(if rollback_failed {
                    BrowserNativeViewError::ReconciliationRequired
                } else {
                    BrowserNativeViewError::Backend
                });
            }
        }
        // The second check is the final admission barrier.  Host state is not
        // committed until identity, owner, and process verification have
        // succeeded again after allocation and the first preflight.
        allocation.commit();
        drop(allocation);
        Ok(self.commit_native_view_registration(plan))
    }

    pub fn native_view(
        &self,
        identity: &BrowserSurfaceIdentity,
    ) -> Option<BrowserNativeViewReceipt> {
        self.native_views
            .get(&identity.resource_id)
            .filter(|view| view.descriptor.identity == *identity)
            .map(|view| BrowserNativeViewReceipt {
                descriptor: view.descriptor.clone(),
                lifecycle: view.lifecycle.clone(),
                attachment_lease: view.attachment_lease.clone(),
                attached_parent: view.attached_parent.clone(),
                focused: view.focused,
                reconciliation: view.reconciliation,
            })
    }

    pub fn attach_native_view_with_backend<B: BrowserNativeSurfaceBackend>(
        &mut self,
        request: BrowserAttachRequest,
        destination: BrowserWindowHandle,
        backend: &mut B,
    ) -> Result<BrowserNativeViewReceipt, BrowserNativeViewError> {
        backend
            .assert_ui_thread()
            .map_err(|_| BrowserNativeViewError::UiThread)?;
        let (resource_id, prepared) =
            self.prepare_attach_native_view(&request, &destination, false)?;
        let before = self.native_view_record(resource_id)?;
        let child = prepared.descriptor.child_hwnd.clone();
        let parking = prepared.parking_window.handle().clone();
        let bounds = prepared.descriptor.physical_bounds;
        let action_view = prepared.clone();
        let rollback_child = child.clone();
        let rollback_parking = parking.clone();
        self.execute_backend_mutation(
            resource_id,
            before,
            prepared,
            backend,
            move |backend| {
                Self::backend_attach(
                    backend,
                    &action_view,
                    &child,
                    &parking,
                    &destination,
                    bounds,
                )
            },
            move |backend, before| {
                Self::backend_park(backend, before, &rollback_child, &rollback_parking)
            },
        )?;
        self.native_view_receipt(resource_id)
    }

    pub fn reattach_native_view_with_backend<B: BrowserNativeSurfaceBackend>(
        &mut self,
        request: BrowserAttachRequest,
        destination: BrowserWindowHandle,
        backend: &mut B,
    ) -> Result<BrowserNativeViewReceipt, BrowserNativeViewError> {
        backend
            .assert_ui_thread()
            .map_err(|_| BrowserNativeViewError::UiThread)?;
        let (resource_id, prepared) =
            self.prepare_attach_native_view(&request, &destination, true)?;
        let before = self.native_view_record(resource_id)?;
        let child = prepared.descriptor.child_hwnd.clone();
        let parking = prepared.parking_window.handle().clone();
        let bounds = prepared.descriptor.physical_bounds;
        let action_view = prepared.clone();
        let rollback_child = child.clone();
        let rollback_parking = parking.clone();
        self.execute_backend_mutation(
            resource_id,
            before,
            prepared,
            backend,
            move |backend| {
                Self::backend_attach(
                    backend,
                    &action_view,
                    &child,
                    &parking,
                    &destination,
                    bounds,
                )
            },
            move |backend, before| {
                Self::backend_park(backend, before, &rollback_child, &rollback_parking)
            },
        )?;
        self.native_view_receipt(resource_id)
    }

    pub(crate) fn park_native_view_with_backend<B: BrowserNativeSurfaceBackend>(
        &mut self,
        request: BrowserHostRequest,
        backend: &mut B,
    ) -> Result<BrowserNativeViewReceipt, BrowserNativeViewError> {
        backend
            .assert_ui_thread()
            .map_err(|_| BrowserNativeViewError::UiThread)?;
        let (resource_id, prepared) = self.prepare_park_native_view(&request)?;
        let before = self.native_view_record(resource_id)?;
        let child = prepared.descriptor.child_hwnd.clone();
        let parking = prepared.parking_window.handle().clone();
        let action_view = before.clone();
        let rollback_child = child.clone();
        self.execute_backend_mutation(
            resource_id,
            before,
            prepared,
            backend,
            move |backend| Self::backend_park(backend, &action_view, &child, &parking),
            move |backend, before| {
                Self::backend_attach_and_restore_focus(backend, before, &rollback_child)
            },
        )?;
        self.native_view_receipt(resource_id)
    }

    pub fn update_native_view_focus_with_backend<B: BrowserNativeSurfaceBackend>(
        &mut self,
        request: BrowserClientRequest,
        focused: bool,
        backend: &mut B,
    ) -> Result<BrowserNativeViewReceipt, BrowserNativeViewError> {
        backend
            .assert_ui_thread()
            .map_err(|_| BrowserNativeViewError::UiThread)?;
        let (resource_id, prepared) = self.prepare_focus_native_view(&request, focused)?;
        let before = self.native_view_record(resource_id)?;
        let child = prepared.descriptor.child_hwnd.clone();
        let action_view = before.clone();
        let rollback_child = child.clone();
        self.execute_backend_mutation(
            resource_id,
            before,
            prepared,
            backend,
            move |backend| Self::backend_set_focus(backend, &action_view, &child, focused),
            move |backend, before| {
                Self::backend_set_focus(backend, before, &rollback_child, before.focused)
            },
        )?;
        self.native_view_receipt(resource_id)
    }

    pub fn update_native_view_geometry_with_backend<B: BrowserNativeSurfaceBackend>(
        &mut self,
        request: BrowserClientRequest,
        geometry: BrowserGeometryInput,
        backend: &mut B,
    ) -> Result<BrowserNativeViewReceipt, BrowserNativeViewError> {
        backend
            .assert_ui_thread()
            .map_err(|_| BrowserNativeViewError::UiThread)?;
        let (resource_id, prepared) = self.prepare_geometry_native_view(&request, geometry)?;
        let before = self.native_view_record(resource_id)?;
        let child = prepared.descriptor.child_hwnd.clone();
        let physical_bounds = prepared.descriptor.physical_bounds;
        let action_view = before.clone();
        let rollback_child = child.clone();
        self.execute_backend_mutation(
            resource_id,
            before,
            prepared,
            backend,
            move |backend| Self::backend_set_bounds(backend, &action_view, &child, physical_bounds),
            move |backend, before| {
                Self::backend_set_bounds(
                    backend,
                    before,
                    &rollback_child,
                    before.descriptor.physical_bounds,
                )
            },
        )?;
        self.native_view_receipt(resource_id)
    }

    /// Return the opaque controller binding that the actual host/controller
    /// may carry into a correlated crash observation. It is never included in
    /// a public receipt.
    pub(crate) fn controller_capability(
        &self,
        identity: &BrowserSurfaceIdentity,
    ) -> Result<BrowserControllerCapability, BrowserNativeViewError> {
        let view = self
            .native_views
            .get(&identity.resource_id)
            .filter(|view| view.descriptor.identity == *identity)
            .ok_or(BrowserNativeViewError::MissingView)?;
        Ok(view.controller_capability.clone())
    }

    /// Record a crash only from the correlated controller binding. A public
    /// host request or caller-supplied crash bit cannot manufacture this fact.
    pub(crate) fn observe_native_view_crash_with_backend<B: BrowserNativeSurfaceBackend>(
        &mut self,
        capability: BrowserControllerCapability,
        backend: &mut B,
    ) -> Result<BrowserNativeViewReceipt, BrowserNativeViewError> {
        let resource_id = capability.identity.resource_id;
        if backend.assert_ui_thread().is_err() {
            return self
                .mark_crash_observation_failure(resource_id, BrowserNativeViewError::UiThread);
        }
        let before = match self.native_view_record(resource_id) {
            Ok(before) => before,
            Err(error) => return self.mark_crash_observation_failure(resource_id, error),
        };
        if before.reconciliation == BrowserNativeViewReconciliation::Unknown {
            return Err(BrowserNativeViewError::ReconciliationRequired);
        }
        let view = &before;
        if view.controller_capability != capability {
            self.mark_reconciliation_unknown(resource_id, &before);
            return Err(BrowserNativeViewError::ControllerObservationMismatch(
                "controller identity or generation",
            ));
        }
        if view.descriptor.identity != capability.identity
            || view.descriptor.child_hwnd != capability.child_window
            || view.descriptor.host_process != capability.host_process
            || view.descriptor.host_fence != capability.host_fence
            || view.descriptor.runtime_generation != capability.runtime_generation
            || view.descriptor.nonce != capability.nonce
        {
            self.mark_reconciliation_unknown(resource_id, &before);
            return Err(BrowserNativeViewError::ControllerObservationMismatch(
                "current surface descriptor",
            ));
        }
        let client_id = match view.lifecycle.clone() {
            BrowserSurfaceLifecycle::Attached { client_id } => client_id,
            _ => {
                self.mark_reconciliation_unknown(resource_id, &before);
                return Err(BrowserNativeViewError::InvalidLifecycle(
                    "crash observation requires an attached view",
                ));
            }
        };
        let mut prepared = before.clone();
        if let Err(error) = Self::bump_native_epochs_for_view(&mut prepared) {
            return self.mark_crash_observation_failure(resource_id, error);
        }
        prepared.lifecycle = BrowserSurfaceLifecycle::Detached {
            client_id: Some(client_id),
            crashed: true,
        };
        prepared.attachment_lease = None;
        prepared.attached_parent = None;
        prepared.focused = false;
        prepared.host_request_lease =
            match self.next_host_request_lease(prepared.descriptor.host_fence.connection_epoch) {
                Ok(lease) => lease,
                Err(error) => return self.mark_crash_observation_failure(resource_id, error),
            };
        if let Err(error) =
            Self::backend_preflight(backend, &before, before.parking_window.handle())
        {
            return self.mark_crash_observation_failure(resource_id, error);
        }
        if backend
            .observe_surface_crash(&before.descriptor, before.parking_window.handle())
            .is_err()
        {
            self.mark_reconciliation_unknown(resource_id, &before);
            return Err(BrowserNativeViewError::ReconciliationRequired);
        }
        match self.commit_prepared_native_view(resource_id, &before, prepared) {
            Ok(()) => {}
            Err(BrowserNativeViewError::StaleDescriptor(_)) => {
                self.mark_reconciliation_unknown(resource_id, &before);
                return Err(BrowserNativeViewError::ReconciliationRequired);
            }
            Err(error) => return self.mark_crash_observation_failure(resource_id, error),
        }
        match self.native_view_receipt(resource_id) {
            Ok(receipt) => Ok(receipt),
            Err(error) => self.mark_crash_observation_failure(resource_id, error),
        }
    }

    pub fn detach_native_view_with_backend<B: BrowserNativeSurfaceBackend>(
        &mut self,
        request: BrowserClientRequest,
        backend: &mut B,
    ) -> Result<BrowserNativeViewReceipt, BrowserNativeViewError> {
        backend
            .assert_ui_thread()
            .map_err(|_| BrowserNativeViewError::UiThread)?;
        let resource_id = self.validate_client_request(&request)?;
        let before = self.native_view_record(resource_id)?;
        let mut prepared = before.clone();
        Self::bump_native_epochs_for_view(&mut prepared)?;
        prepared.lifecycle = BrowserSurfaceLifecycle::Detached {
            client_id: Some(request.client_id),
            crashed: false,
        };
        prepared.attachment_lease = None;
        prepared.attached_parent = None;
        prepared.focused = false;
        prepared.last_client_sequence = Some(request.client_sequence);
        prepared.host_request_lease =
            self.next_host_request_lease(prepared.descriptor.host_fence.connection_epoch)?;
        let child = prepared.descriptor.child_hwnd.clone();
        let parking = prepared.parking_window.handle().clone();
        let action_view = before.clone();
        let rollback_child = child.clone();
        self.execute_backend_mutation(
            resource_id,
            before,
            prepared,
            backend,
            move |backend| Self::backend_park(backend, &action_view, &child, &parking),
            move |backend, before| {
                Self::backend_attach_and_restore_focus(backend, before, &rollback_child)
            },
        )?;
        self.native_view_receipt(resource_id)
    }

    pub(crate) fn native_teardown_status(
        &self,
        request: &BrowserHostRequest,
    ) -> Result<BrowserTeardownStatus, BrowserNativeViewError> {
        match self.native_teardown_preconditions(request)? {
            Err(blocker) => Ok(BrowserTeardownStatus::Blocked(blocker)),
            Ok(_resource_id) => {
                // Without a host-owned backend observation, never synthesize zero residue.
                Ok(BrowserTeardownStatus::Blocked(
                    BrowserTeardownBlocker::RealRuntimeObservationUnavailable,
                ))
            }
        }
    }

    pub(crate) fn native_teardown_status_with_backend<B: BrowserNativeSurfaceBackend>(
        &self,
        request: &BrowserHostRequest,
        backend: &mut B,
    ) -> Result<BrowserTeardownStatus, BrowserNativeViewError> {
        let resource_id = match self.native_teardown_preconditions(request)? {
            Err(blocker) => return Ok(BrowserTeardownStatus::Blocked(blocker)),
            Ok(resource_id) => resource_id,
        };
        let view = self
            .native_views
            .get(&resource_id)
            .ok_or(BrowserNativeViewError::MissingView)?;
        backend
            .assert_ui_thread()
            .map_err(|_| BrowserNativeViewError::UiThread)?;
        // Teardown observation intentionally does not reuse operation preflight:
        // zero-residue proof requires the controller/environment to be closed.
        match backend.observe_teardown_zero_residue(&view.descriptor, view.parking_window.handle())
        {
            Ok(()) => Ok(BrowserTeardownStatus::Ready),
            Err(_) => Ok(BrowserTeardownStatus::Blocked(
                BrowserTeardownBlocker::RealRuntimeObservationUnavailable,
            )),
        }
    }

    fn native_teardown_preconditions(
        &self,
        request: &BrowserHostRequest,
    ) -> Result<Result<ResourceId, BrowserTeardownBlocker>, BrowserNativeViewError> {
        let resource_id = self.validate_descriptor_inner(&request.descriptor, false)?;
        let view = self
            .native_views
            .get(&resource_id)
            .ok_or(BrowserNativeViewError::MissingView)?;
        if request.request_lease.connection_epoch() != view.descriptor.host_fence.connection_epoch {
            return Err(BrowserNativeViewError::HostRequestLeaseMismatch);
        }
        if view.host_request_lease != request.request_lease {
            return Err(BrowserNativeViewError::HostRequestLeaseMismatch);
        }
        if view.reconciliation == BrowserNativeViewReconciliation::Unknown {
            return Ok(Err(
                BrowserTeardownBlocker::NativeSurfaceReconciliationRequired,
            ));
        }
        if self.native_allocation_orphan_count() != 0 {
            return Ok(Err(
                BrowserTeardownBlocker::NativeSurfaceReconciliationRequired,
            ));
        }
        if !matches!(view.lifecycle, BrowserSurfaceLifecycle::Parked) {
            return Ok(Err(BrowserTeardownBlocker::SurfaceMustBeParked));
        }
        Ok(Ok(resource_id))
    }

    pub(crate) fn close_native_context(
        &mut self,
        request: BrowserHostRequest,
    ) -> Result<BrowserNativeViewReceipt, BrowserNativeViewError> {
        match self.native_teardown_status(&request)? {
            BrowserTeardownStatus::Ready => Err(BrowserNativeViewError::TeardownBlocked(
                BrowserTeardownBlocker::RealRuntimeObservationUnavailable,
            )),
            BrowserTeardownStatus::Pending => Err(BrowserNativeViewError::TeardownPending),
            BrowserTeardownStatus::Blocked(blocker) => {
                Err(BrowserNativeViewError::TeardownBlocked(blocker))
            }
        }
    }

    pub(crate) fn close_native_context_with_backend<B: BrowserNativeSurfaceBackend>(
        &mut self,
        request: BrowserHostRequest,
        backend: &mut B,
    ) -> Result<BrowserNativeViewReceipt, BrowserNativeViewError> {
        match self.native_teardown_status_with_backend(&request, backend)? {
            BrowserTeardownStatus::Ready => self.commit_native_context_closed(request),
            BrowserTeardownStatus::Pending => Err(BrowserNativeViewError::TeardownPending),
            BrowserTeardownStatus::Blocked(blocker) => {
                Err(BrowserNativeViewError::TeardownBlocked(blocker))
            }
        }
    }

    fn commit_native_context_closed(
        &mut self,
        request: BrowserHostRequest,
    ) -> Result<BrowserNativeViewReceipt, BrowserNativeViewError> {
        let resource_id = self.validate_host_request(&request)?;
        let before = self.native_view_record(resource_id)?;
        if !matches!(before.lifecycle, BrowserSurfaceLifecycle::Parked) {
            return Err(BrowserNativeViewError::TeardownBlocked(
                BrowserTeardownBlocker::SurfaceMustBeParked,
            ));
        }
        let mut prepared = before.clone();
        Self::bump_native_epochs_for_view(&mut prepared)?;
        prepared.lifecycle = BrowserSurfaceLifecycle::Closed;
        prepared.attachment_lease = None;
        prepared.attached_parent = None;
        prepared.focused = false;
        prepared.host_request_lease =
            self.next_host_request_lease(prepared.descriptor.host_fence.connection_epoch)?;
        self.commit_prepared_native_view(resource_id, &before, prepared)?;
        self.native_view_receipt(resource_id)
    }

    /// A copied descriptor is not live Wry proof. BrowserHostState cannot
    /// observe the WebView2 controller/environment; only BrowserWebViewHost can.
    pub fn host_owned_surface_proof(
        &self,
        identity: &BrowserSurfaceIdentity,
    ) -> Result<BrowserHostOwnedSurfaceProof, BrowserNativeViewError> {
        let _ = self.registered_surface_descriptor(identity)?;
        Err(BrowserNativeViewError::LiveWryObservationUnavailable)
    }

    fn registered_surface_descriptor(
        &self,
        identity: &BrowserSurfaceIdentity,
    ) -> Result<&BrowserSurfaceDescriptor, BrowserNativeViewError> {
        if !self.native_authority_available {
            return Err(BrowserNativeViewError::ReconciliationRequired);
        }
        if self.native_allocation_orphan_count() != 0 {
            return Err(BrowserNativeViewError::ReconciliationRequired);
        }
        let view = self
            .native_views
            .get(&identity.resource_id)
            .filter(|view| view.descriptor.identity == *identity)
            .ok_or(BrowserNativeViewError::MissingView)?;
        if view.reconciliation != BrowserNativeViewReconciliation::Healthy {
            return Err(BrowserNativeViewError::ReconciliationRequired);
        }
        if matches!(view.lifecycle, BrowserSurfaceLifecycle::Closed) {
            return Err(BrowserNativeViewError::InvalidLifecycle("view is closed"));
        }
        Ok(&view.descriptor)
    }

    pub(crate) fn unverified_surface_descriptor(
        &self,
        identity: &BrowserSurfaceIdentity,
    ) -> Result<BrowserSurfaceDescriptor, BrowserNativeViewError> {
        self.registered_surface_descriptor(identity).cloned()
    }

    pub(crate) fn has_live_native_surface(&self) -> bool {
        self.native_views.values().any(|view| {
            view.reconciliation == BrowserNativeViewReconciliation::Healthy
                && !matches!(view.lifecycle, BrowserSurfaceLifecycle::Closed)
        })
    }

    /// Resolve the live task-bound surface identity legacy MCP/chrome commands
    /// must use. Missing surfaces stay `None` so legacy automation can continue
    /// before a native binding exists; a registered surface cannot be bypassed.
    pub fn normalize_legacy_mcp_task_surface(
        &self,
        task_id: crate::domain::id::TaskId,
    ) -> Option<BrowserSurfaceIdentity> {
        self.native_views.values().find_map(|view| {
            if view.descriptor.identity.task_id == task_id
                && !matches!(view.lifecycle, BrowserSurfaceLifecycle::Closed)
                && view.reconciliation == BrowserNativeViewReconciliation::Healthy
            {
                Some(view.descriptor.identity)
            } else {
                None
            }
        })
    }

    pub fn require_legacy_mcp_normalized_surface(
        &self,
        task_id: Option<crate::domain::id::TaskId>,
    ) -> Result<(), LegacyMcpTaskSurfaceBlocker> {
        self.require_legacy_mcp_exact_binding(task_id.map(|task_id| (task_id, None, None)))
    }

    pub fn require_legacy_mcp_exact_binding(
        &self,
        binding: Option<(
            crate::domain::id::TaskId,
            Option<crate::domain::id::BrowserContextId>,
            Option<crate::domain::id::ResourceId>,
        )>,
    ) -> Result<(), LegacyMcpTaskSurfaceBlocker> {
        if !self.has_live_native_surface() {
            return Ok(());
        }
        let (task_id, context_id, resource_id) =
            binding.ok_or(LegacyMcpTaskSurfaceBlocker::WorkspaceCommandLacksTaskId)?;
        let task_id = legacy_mcp_command_task_identity(Some(task_id))?;
        let identity = self
            .normalize_legacy_mcp_task_surface(task_id)
            .ok_or(LegacyMcpTaskSurfaceBlocker::CrossTaskOrMissingSurface)?;
        let Some(context_id) = context_id else {
            return Err(LegacyMcpTaskSurfaceBlocker::CrossTaskOrMissingSurface);
        };
        let Some(resource_id) = resource_id else {
            return Err(LegacyMcpTaskSurfaceBlocker::CrossTaskOrMissingSurface);
        };
        if context_id != identity.context_id || resource_id != identity.resource_id {
            return Err(LegacyMcpTaskSurfaceBlocker::CrossTaskOrMissingSurface);
        }
        Ok(())
    }

    fn prepare_attach_native_view(
        &self,
        request: &BrowserAttachRequest,
        destination: &BrowserWindowHandle,
        reattach: bool,
    ) -> Result<(ResourceId, BrowserNativeView), BrowserNativeViewError> {
        let resource_id = self.preflight_attachment(request, reattach)?;
        let mut view = self
            .native_views
            .get(&resource_id)
            .ok_or(BrowserNativeViewError::MissingView)?
            .clone();
        if destination == view.parking_window.handle() {
            return Err(BrowserNativeViewError::InvalidInput(
                "attach destination must not be parking",
            ));
        }
        if destination == &view.descriptor.child_hwnd {
            return Err(BrowserNativeViewError::InvalidInput(
                "attach destination must not be child",
            ));
        }
        let attachment_lease = BrowserAttachmentLease::from_bytes(issue_opaque_bytes()?)?;
        let connection_fence = self.next_native_connection_fence()?;
        Self::bump_native_epochs_for_view(&mut view)?;
        view.lifecycle = BrowserSurfaceLifecycle::Attached {
            client_id: request.client_id.clone(),
        };
        view.descriptor.host_fence = connection_fence;
        view.attachment_lease = Some(attachment_lease);
        view.attached_parent = Some(destination.clone());
        view.focused = false;
        view.last_client_sequence = None;
        view.controller_capability = BrowserControllerCapability::from_descriptor(&view.descriptor);
        view.host_request_lease =
            self.next_host_request_lease(view.descriptor.host_fence.connection_epoch)?;
        Ok((resource_id, view))
    }

    fn prepare_park_native_view(
        &self,
        request: &BrowserHostRequest,
    ) -> Result<(ResourceId, BrowserNativeView), BrowserNativeViewError> {
        let resource_id = self.validate_host_request(request)?;
        let mut view = self
            .native_views
            .get(&resource_id)
            .ok_or(BrowserNativeViewError::MissingView)?
            .clone();
        if matches!(view.lifecycle, BrowserSurfaceLifecycle::Closed) {
            return Err(BrowserNativeViewError::InvalidLifecycle("view is closed"));
        }
        Self::bump_native_epochs_for_view(&mut view)?;
        view.lifecycle = BrowserSurfaceLifecycle::Parked;
        view.attachment_lease = None;
        view.attached_parent = None;
        view.focused = false;
        view.host_request_lease =
            self.next_host_request_lease(view.descriptor.host_fence.connection_epoch)?;
        Ok((resource_id, view))
    }

    fn prepare_focus_native_view(
        &self,
        request: &BrowserClientRequest,
        focused: bool,
    ) -> Result<(ResourceId, BrowserNativeView), BrowserNativeViewError> {
        let resource_id = self.validate_client_request(request)?;
        let mut view = self
            .native_views
            .get(&resource_id)
            .ok_or(BrowserNativeViewError::MissingView)?
            .clone();
        Self::bump_native_epochs_for_view(&mut view)?;
        view.focused = focused;
        view.last_client_sequence = Some(request.client_sequence);
        Ok((resource_id, view))
    }

    fn prepare_geometry_native_view(
        &self,
        request: &BrowserClientRequest,
        geometry: BrowserGeometryInput,
    ) -> Result<(ResourceId, BrowserNativeView), BrowserNativeViewError> {
        let resource_id = self.validate_client_request(request)?;
        let physical_bounds = browser_logical_to_physical(
            geometry.bounds,
            geometry.dpi,
            geometry.origin,
            geometry.space,
        )?;
        let mut view = self
            .native_views
            .get(&resource_id)
            .ok_or(BrowserNativeViewError::MissingView)?
            .clone();
        Self::bump_native_epochs_for_view(&mut view)?;
        view.descriptor.physical_bounds = physical_bounds;
        view.descriptor.dpi = geometry.dpi;
        view.last_client_sequence = Some(request.client_sequence);
        Ok((resource_id, view))
    }

    fn commit_prepared_native_view(
        &mut self,
        resource_id: ResourceId,
        before: &BrowserNativeView,
        view: BrowserNativeView,
    ) -> Result<(), BrowserNativeViewError> {
        let current = self
            .native_views
            .get(&resource_id)
            .ok_or(BrowserNativeViewError::MissingView)?;
        if !Self::native_view_matches(current, before) {
            return Err(BrowserNativeViewError::StaleDescriptor(
                "native view operation",
            ));
        }
        let attached = matches!(view.lifecycle, BrowserSurfaceLifecycle::Attached { .. });
        if view.descriptor.host_fence.boot_epoch == self.native_host_fence.boot_epoch
            && view.descriptor.host_fence.connection_epoch > self.native_host_fence.connection_epoch
        {
            self.native_host_fence = view.descriptor.host_fence;
        }
        self.next_native_connection_epoch = self
            .next_native_connection_epoch
            .max(view.descriptor.host_fence.connection_epoch);
        self.next_native_request_epoch = self
            .next_native_request_epoch
            .max(view.host_request_lease.request_epoch());
        self.native_views.insert(resource_id, view);
        if attached {
            self.active_native_view = Some(resource_id);
        } else if self.active_native_view == Some(resource_id) {
            self.active_native_view = None;
        }
        Ok(())
    }

    fn native_view_matches(current: &BrowserNativeView, expected: &BrowserNativeView) -> bool {
        current.descriptor == expected.descriptor
            && current.parking_window == expected.parking_window
            && current.lifecycle == expected.lifecycle
            && current.attachment_lease == expected.attachment_lease
            && current.attached_parent == expected.attached_parent
            && current.host_request_lease == expected.host_request_lease
            && current.focused == expected.focused
            && current.last_client_sequence == expected.last_client_sequence
            && current.reconciliation == expected.reconciliation
            && current.controller_capability == expected.controller_capability
    }

    fn mark_reconciliation_unknown(&mut self, resource_id: ResourceId, before: &BrowserNativeView) {
        let Some(current) = self.native_views.get_mut(&resource_id) else {
            return;
        };
        if Self::native_view_matches(current, before) {
            let mut unknown = before.clone();
            unknown.reconciliation = BrowserNativeViewReconciliation::Unknown;
            *current = unknown;
        } else {
            current.reconciliation = BrowserNativeViewReconciliation::Unknown;
        }
    }

    fn mark_crash_observation_failure(
        &mut self,
        resource_id: ResourceId,
        error: BrowserNativeViewError,
    ) -> Result<BrowserNativeViewReceipt, BrowserNativeViewError> {
        if let Some(view) = self.native_views.get_mut(&resource_id) {
            view.reconciliation = BrowserNativeViewReconciliation::Unknown;
        }
        Err(error)
    }

    fn native_view_record(
        &self,
        resource_id: ResourceId,
    ) -> Result<BrowserNativeView, BrowserNativeViewError> {
        self.native_views
            .get(&resource_id)
            .cloned()
            .ok_or(BrowserNativeViewError::MissingView)
    }

    fn backend_preflight<B: BrowserNativeSurfaceBackend>(
        backend: &mut B,
        view: &BrowserNativeView,
        parking: &BrowserWindowHandle,
    ) -> Result<(), BrowserNativeViewError> {
        backend
            .preflight_native_view_operation(&view.descriptor, parking)
            .map_err(|_| BrowserNativeViewError::Backend)
    }

    fn backend_verify<B: BrowserNativeSurfaceBackend>(
        backend: &mut B,
        view: &BrowserNativeView,
    ) -> Result<(), BrowserNativeViewError> {
        let parking = view.parking_window.handle();
        Self::backend_preflight(backend, view, parking)?;
        backend
            .verify_surface_state(
                &view.descriptor,
                parking,
                view.attached_parent.as_ref(),
                matches!(view.lifecycle, BrowserSurfaceLifecycle::Attached { .. }),
                view.descriptor.physical_bounds,
                view.focused,
            )
            .map_err(|_| BrowserNativeViewError::Backend)
    }

    fn backend_park<B: BrowserNativeSurfaceBackend>(
        backend: &mut B,
        view: &BrowserNativeView,
        child: &BrowserWindowHandle,
        parking: &BrowserWindowHandle,
    ) -> Result<(), BrowserNativeViewError> {
        Self::backend_preflight(backend, view, parking)?;
        backend
            .park_surface(child, parking)
            .map_err(|_| BrowserNativeViewError::Backend)
    }

    fn backend_attach<B: BrowserNativeSurfaceBackend>(
        backend: &mut B,
        view: &BrowserNativeView,
        child: &BrowserWindowHandle,
        parking: &BrowserWindowHandle,
        destination: &BrowserWindowHandle,
        bounds: BrowserPhysicalBounds,
    ) -> Result<(), BrowserNativeViewError> {
        Self::backend_preflight(backend, view, parking)?;
        backend
            .attach_surface(child, destination, bounds)
            .map_err(|_| BrowserNativeViewError::Backend)
    }

    fn backend_attach_and_restore_focus<B: BrowserNativeSurfaceBackend>(
        backend: &mut B,
        view: &BrowserNativeView,
        child: &BrowserWindowHandle,
    ) -> Result<(), BrowserNativeViewError> {
        let parking = view.parking_window.handle();
        let destination = view
            .attached_parent
            .as_ref()
            .ok_or(BrowserNativeViewError::InvalidInput("attached destination"))?;
        Self::backend_attach(
            backend,
            view,
            child,
            parking,
            destination,
            view.descriptor.physical_bounds,
        )?;
        Self::backend_preflight(backend, view, parking)?;
        backend
            .set_surface_focus(child, view.focused)
            .map_err(|_| BrowserNativeViewError::Backend)
    }

    fn backend_set_focus<B: BrowserNativeSurfaceBackend>(
        backend: &mut B,
        view: &BrowserNativeView,
        child: &BrowserWindowHandle,
        focused: bool,
    ) -> Result<(), BrowserNativeViewError> {
        let parking = view.parking_window.handle();
        Self::backend_preflight(backend, view, parking)?;
        backend
            .set_surface_focus(child, focused)
            .map_err(|_| BrowserNativeViewError::Backend)
    }

    fn backend_set_bounds<B: BrowserNativeSurfaceBackend>(
        backend: &mut B,
        view: &BrowserNativeView,
        child: &BrowserWindowHandle,
        bounds: BrowserPhysicalBounds,
    ) -> Result<(), BrowserNativeViewError> {
        let parking = view.parking_window.handle();
        Self::backend_preflight(backend, view, parking)?;
        backend
            .set_surface_bounds(child, bounds)
            .map_err(|_| BrowserNativeViewError::Backend)
    }

    fn execute_backend_mutation<B, A, R>(
        &mut self,
        resource_id: ResourceId,
        before: BrowserNativeView,
        prepared: BrowserNativeView,
        backend: &mut B,
        action: A,
        rollback: R,
    ) -> Result<(), BrowserNativeViewError>
    where
        B: BrowserNativeSurfaceBackend,
        A: FnOnce(&mut B) -> Result<(), BrowserNativeViewError>,
        R: FnOnce(&mut B, &BrowserNativeView) -> Result<(), BrowserNativeViewError>,
    {
        Self::backend_preflight(backend, &before, before.parking_window.handle())?;
        let consumed_host_request_lease = prepared.host_request_lease.clone();

        let action_result = action(backend);
        let failure = match action_result {
            Ok(()) => match Self::backend_verify(backend, &prepared) {
                Ok(()) => match self.commit_prepared_native_view(resource_id, &before, prepared) {
                    Ok(()) => return Ok(()),
                    Err(BrowserNativeViewError::StaleDescriptor(_)) => {
                        if let Some(current) = self.native_views.get_mut(&resource_id) {
                            current.reconciliation = BrowserNativeViewReconciliation::Unknown;
                        }
                        return Err(BrowserNativeViewError::ReconciliationRequired);
                    }
                    Err(error) => error,
                },
                Err(error) => error,
            },
            Err(error) => error,
        };

        let rollback_result = rollback(backend, &before);
        let verification_result = Self::backend_verify(backend, &before);
        if rollback_result.is_ok() && verification_result.is_ok() {
            let mut consumed = before.clone();
            consumed.host_request_lease = consumed_host_request_lease;
            match self.commit_prepared_native_view(resource_id, &before, consumed) {
                Ok(()) => return Err(failure),
                Err(BrowserNativeViewError::StaleDescriptor(_)) => {
                    self.mark_reconciliation_unknown(resource_id, &before);
                    return Err(BrowserNativeViewError::ReconciliationRequired);
                }
                Err(error) => return Err(error),
            }
        }

        self.mark_reconciliation_unknown(resource_id, &before);
        Err(BrowserNativeViewError::ReconciliationRequired)
    }

    fn preflight_attachment(
        &self,
        request: &BrowserAttachRequest,
        reattach: bool,
    ) -> Result<ResourceId, BrowserNativeViewError> {
        let resource_id = self.validate_descriptor(&request.descriptor)?;
        {
            let view = self
                .native_views
                .get(&resource_id)
                .ok_or(BrowserNativeViewError::MissingView)?;
            let allowed = if reattach {
                matches!(
                    view.lifecycle,
                    BrowserSurfaceLifecycle::Parked | BrowserSurfaceLifecycle::Detached { .. }
                )
            } else {
                matches!(view.lifecycle, BrowserSurfaceLifecycle::Parked)
            };
            if !allowed {
                return Err(BrowserNativeViewError::InvalidLifecycle(if reattach {
                    "view is not reattachable"
                } else {
                    "view is not parked"
                }));
            }
        }
        if self
            .active_native_view
            .is_some_and(|active| active != resource_id)
        {
            return Err(BrowserNativeViewError::ActiveViewConflict);
        }
        Ok(resource_id)
    }

    fn next_native_connection_fence(&self) -> Result<BrowserHostFence, BrowserNativeViewError> {
        let current_epoch = self
            .native_host_fence
            .connection_epoch
            .max(self.next_native_connection_epoch);
        let connection_epoch =
            current_epoch
                .checked_add(1)
                .ok_or(BrowserNativeViewError::Descriptor(
                    BrowserDtoError::Overflow("host connection epoch"),
                ))?;
        Ok(BrowserHostFence::new(
            self.native_host_fence.boot_epoch,
            connection_epoch,
        )?)
    }

    fn validate_descriptor(
        &self,
        descriptor: &BrowserSurfaceDescriptor,
    ) -> Result<ResourceId, BrowserNativeViewError> {
        self.validate_descriptor_inner(descriptor, true)
    }

    fn validate_descriptor_inner(
        &self,
        descriptor: &BrowserSurfaceDescriptor,
        reject_reconciliation: bool,
    ) -> Result<ResourceId, BrowserNativeViewError> {
        descriptor.validate()?;
        let resource_id = descriptor.identity.resource_id;
        let view = self.native_views.get(&resource_id).ok_or(
            BrowserNativeViewError::ForeignDescriptor("surface resource"),
        )?;
        let expected = &view.descriptor;
        if descriptor.identity != expected.identity {
            return Err(BrowserNativeViewError::ForeignDescriptor(
                "surface identity",
            ));
        }
        if descriptor.host_process != expected.host_process {
            return Err(BrowserNativeViewError::ForeignDescriptor("host process"));
        }
        if descriptor.child_hwnd != expected.child_hwnd {
            return Err(BrowserNativeViewError::ForeignDescriptor("child window"));
        }
        if descriptor.host_fence != expected.host_fence {
            return Err(BrowserNativeViewError::StaleDescriptor("host fence"));
        }
        if descriptor.nonce != expected.nonce {
            return Err(BrowserNativeViewError::StaleDescriptor("surface nonce"));
        }
        if descriptor.runtime_generation != expected.runtime_generation {
            return Err(BrowserNativeViewError::StaleDescriptor(
                "runtime generation",
            ));
        }
        if descriptor.bounds_epoch != expected.bounds_epoch {
            return Err(BrowserNativeViewError::StaleDescriptor("bounds epoch"));
        }
        if descriptor.focus_epoch != expected.focus_epoch {
            return Err(BrowserNativeViewError::StaleDescriptor("focus epoch"));
        }
        if descriptor.physical_bounds != expected.physical_bounds || descriptor.dpi != expected.dpi
        {
            return Err(BrowserNativeViewError::StaleDescriptor("geometry"));
        }
        if reject_reconciliation && view.reconciliation == BrowserNativeViewReconciliation::Unknown
        {
            return Err(BrowserNativeViewError::ReconciliationRequired);
        }
        Ok(resource_id)
    }

    fn validate_host_request(
        &self,
        request: &BrowserHostRequest,
    ) -> Result<ResourceId, BrowserNativeViewError> {
        let resource_id = self.validate_descriptor(&request.descriptor)?;
        let view = self
            .native_views
            .get(&resource_id)
            .ok_or(BrowserNativeViewError::MissingView)?;
        if view.host_request_lease != request.request_lease {
            return Err(BrowserNativeViewError::HostRequestLeaseMismatch);
        }
        Ok(resource_id)
    }

    fn validate_client_request(
        &self,
        request: &BrowserClientRequest,
    ) -> Result<ResourceId, BrowserNativeViewError> {
        request
            .validate()
            .map_err(BrowserNativeViewError::Descriptor)?;
        if request.client_sequence > MAX_BROWSER_CLIENT_SEQUENCE {
            return Err(BrowserNativeViewError::Descriptor(
                BrowserDtoError::OutOfRange("browser client sequence"),
            ));
        }
        let resource_id = self.validate_descriptor(&request.descriptor)?;
        let view = self
            .native_views
            .get(&resource_id)
            .ok_or(BrowserNativeViewError::MissingView)?;
        match &view.lifecycle {
            BrowserSurfaceLifecycle::Attached { client_id } if *client_id == request.client_id => {}
            BrowserSurfaceLifecycle::Attached { .. } => {
                return Err(BrowserNativeViewError::ClientMismatch)
            }
            _ => {
                return Err(BrowserNativeViewError::InvalidLifecycle(
                    "client operation requires an attached view",
                ))
            }
        }
        if view.attachment_lease.as_ref() != Some(&request.attachment_lease) {
            return Err(BrowserNativeViewError::AttachmentLeaseMismatch);
        }
        if view
            .last_client_sequence
            .is_some_and(|last| request.client_sequence <= last)
        {
            return Err(BrowserNativeViewError::StaleDescriptor("client sequence"));
        }
        Ok(resource_id)
    }

    fn bump_native_epochs_for_view(
        view: &mut BrowserNativeView,
    ) -> Result<(), BrowserNativeViewError> {
        view.descriptor.bounds_epoch = view.descriptor.bounds_epoch.next()?;
        view.descriptor.focus_epoch = view.descriptor.focus_epoch.next()?;
        Ok(())
    }

    fn native_view_receipt(
        &self,
        resource_id: ResourceId,
    ) -> Result<BrowserNativeViewReceipt, BrowserNativeViewError> {
        let view = self
            .native_views
            .get(&resource_id)
            .ok_or(BrowserNativeViewError::MissingView)?;
        Ok(BrowserNativeViewReceipt {
            descriptor: view.descriptor.clone(),
            lifecycle: view.lifecycle.clone(),
            attachment_lease: view.attachment_lease.clone(),
            attached_parent: view.attached_parent.clone(),
            focused: view.focused,
            reconciliation: view.reconciliation,
        })
    }

    pub fn ensure_workspace(
        &mut self,
        workspace_key: BrowserWorkspaceKey,
        mut snapshot: BrowserWorkspaceSnapshot,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        if let Some(existing) = self.workspaces.get(&workspace_key) {
            return Ok(BrowserWorkspaceMutation::new(existing.clone()));
        }
        let mut changed = false;
        if snapshot.tabs.is_empty() {
            let tab_id = self.generate_tab_id()?;
            snapshot.tabs.push(BrowserTabSnapshot {
                id: tab_id.clone(),
                title: String::new(),
                url: "about:blank".to_string(),
                viewport: BrowserViewport::default(),
            });
            snapshot.selected_tab_id = Some(tab_id);
            changed = true;
        } else if snapshot
            .selected_tab_id
            .as_ref()
            .is_none_or(|selected| !snapshot.tabs.iter().any(|tab| &tab.id == selected))
        {
            snapshot.selected_tab_id = snapshot.tabs.first().map(|tab| tab.id.clone());
            changed = true;
        }
        if changed {
            snapshot.advance_revision();
        }
        self.workspaces.insert(workspace_key, snapshot.clone());
        Ok(BrowserWorkspaceMutation::new(snapshot))
    }

    pub fn create_tab(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        url: impl Into<String>,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        let url = validate_browser_url(&url.into())?;
        let tab_id = self.generate_tab_id()?;
        let snapshot =
            self.workspaces
                .get_mut(workspace_key)
                .ok_or_else(|| BrowserError::CrashedView {
                    message: "browser workspace has not been ensured".to_string(),
                })?;
        snapshot.tabs.push(BrowserTabSnapshot {
            id: tab_id.clone(),
            title: String::new(),
            url,
            viewport: BrowserViewport::default(),
        });
        snapshot.selected_tab_id = Some(tab_id);
        snapshot.advance_revision();
        Ok(BrowserWorkspaceMutation::new(snapshot.clone()))
    }

    pub fn save_annotation(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        annotation: BrowserAnnotation,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        let snapshot = self.workspace_mut(workspace_key)?;
        snapshot.save_annotation(annotation)?;
        Ok(BrowserWorkspaceMutation::new(snapshot.clone()))
    }

    pub fn acknowledge_attachment_projection(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        revision: BrowserAttachmentRevision,
        pending_annotation_ids: &[String],
        tombstone_annotation_ids: &[String],
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        let snapshot = self.workspace_mut(workspace_key)?;
        snapshot.pending_annotation_ids.retain(|pending| {
            !tombstone_annotation_ids
                .iter()
                .any(|tombstone| tombstone == pending)
        });
        for annotation_id in pending_annotation_ids {
            if tombstone_annotation_ids
                .iter()
                .any(|tombstone| tombstone == annotation_id)
                || snapshot
                    .pending_annotation_ids
                    .iter()
                    .any(|pending| pending == annotation_id)
            {
                continue;
            }
            snapshot.pending_annotation_ids.push(annotation_id.clone());
        }
        snapshot.pending_annotation_revision = snapshot.pending_annotation_revision.max(revision);
        Ok(BrowserWorkspaceMutation::new(snapshot.clone()))
    }

    pub fn annotation_summaries(
        &self,
        workspace_key: &BrowserWorkspaceKey,
    ) -> Result<Vec<BrowserAnnotationSummary>, BrowserError> {
        let snapshot = self
            .workspace(workspace_key)
            .ok_or_else(missing_workspace)?;
        snapshot
            .annotations
            .iter()
            .map(|annotation| {
                let redacted = redacted_browser_annotation(annotation);
                Ok(BrowserAnnotationSummary {
                    id: annotation.id.clone(),
                    kind: annotation.kind,
                    comment: truncate_annotation_summary(&redacted.comment, 160),
                    url: truncate_annotation_summary(&redacted.url, 240),
                    resolved: annotation.resolved,
                    stale: snapshot.annotation_anchor_is_stale(&annotation.id)?,
                    screenshot: None,
                })
            })
            .collect()
    }

    pub fn annotation_details(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        annotation_id: &str,
        resources: &BrowserResourceStore,
    ) -> Result<BrowserAnnotationDetails, BrowserError> {
        let snapshot = self
            .workspace(workspace_key)
            .ok_or_else(missing_workspace)?;
        let annotation = snapshot.annotation(annotation_id)?.clone();
        let stale = snapshot.annotation_anchor_is_stale(annotation_id)?;
        let screenshot = annotation_screenshot_handle(
            resources,
            workspace_key,
            &annotation.screenshot_resource,
        )?;
        let screenshot_was_pinned = screenshot.pinned;
        let screenshot = resources.set_pinned(workspace_key, &screenshot.id, true)?;
        let annotation = redacted_browser_annotation(&annotation);
        let encoded = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "annotation": annotation,
            "stale": stale,
            "screenshot": screenshot,
        }))
        .map_err(|_| BrowserError::CrashedView {
            message: "could not encode browser annotation details".to_string(),
        });
        let details_resource = encoded.and_then(|encoded| {
            resources.put(
                workspace_key,
                BrowserResourceKind::AnnotationDetails,
                "application/json",
                encoded,
                true,
            )
        });
        let details_resource = match details_resource {
            Ok(resource) => resource,
            Err(error) => {
                if !screenshot_was_pinned {
                    let _ = resources.set_pinned(workspace_key, &screenshot.id, false);
                }
                return Err(error);
            }
        };
        Ok(BrowserAnnotationDetails {
            annotation,
            stale,
            screenshot,
            details_resource,
        })
    }

    pub fn apply_annotation_operation(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        operation: BrowserAnnotationOperation,
        annotation_id: &str,
        resources: &BrowserResourceStore,
    ) -> Result<BrowserAnnotationMutationResult, BrowserError> {
        if matches!(
            operation,
            BrowserAnnotationOperation::List | BrowserAnnotationOperation::Get
        ) {
            return Err(BrowserError::InvalidInvocation {
                field: "annotationOperation".to_string(),
            });
        }
        let annotation = self
            .workspace(workspace_key)
            .ok_or_else(missing_workspace)?
            .annotation(annotation_id)?
            .clone();
        let screenshot = annotation_screenshot_handle(
            resources,
            workspace_key,
            &annotation.screenshot_resource,
        )?;
        let screenshot_was_pinned = screenshot.pinned;
        let screenshot = resources.set_pinned(workspace_key, &screenshot.id, true)?;
        let mutation = match operation {
            BrowserAnnotationOperation::Resolve => {
                self.set_annotation_resolved(workspace_key, annotation_id, true)
            }
            BrowserAnnotationOperation::Unresolve => {
                self.set_annotation_resolved(workspace_key, annotation_id, false)
            }
            BrowserAnnotationOperation::Delete => self
                .delete_annotation(workspace_key, annotation_id)
                .map(|(mutation, _)| mutation),
            BrowserAnnotationOperation::List | BrowserAnnotationOperation::Get => unreachable!(),
        };
        let mutation = match mutation {
            Ok(mutation) => mutation,
            Err(error) => {
                if !screenshot_was_pinned {
                    let _ = resources.set_pinned(workspace_key, &screenshot.id, false);
                }
                return Err(error);
            }
        };
        Ok(BrowserAnnotationMutationResult {
            operation,
            annotation_id: annotation_id.to_string(),
            screenshot,
            mutation,
        })
    }

    pub fn set_annotation_resolved(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        annotation_id: &str,
        resolved: bool,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        let snapshot = self.workspace_mut(workspace_key)?;
        snapshot.set_annotation_resolved(annotation_id, resolved)?;
        Ok(BrowserWorkspaceMutation::new(snapshot.clone()))
    }

    pub fn delete_annotation(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        annotation_id: &str,
    ) -> Result<(BrowserWorkspaceMutation, BrowserAnnotation), BrowserError> {
        let snapshot = self.workspace_mut(workspace_key)?;
        let annotation = snapshot.delete_annotation(annotation_id)?;
        Ok((BrowserWorkspaceMutation::new(snapshot.clone()), annotation))
    }

    pub fn select_tab(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        let snapshot = self.workspace_mut(workspace_key)?;
        if !snapshot.tabs.iter().any(|tab| tab.id == tab_id) {
            return Err(missing_tab(tab_id));
        }
        if snapshot.selected_tab_id.as_deref() != Some(tab_id) {
            snapshot.selected_tab_id = Some(tab_id.to_string());
            snapshot.advance_revision();
        }
        Ok(BrowserWorkspaceMutation::new(snapshot.clone()))
    }

    pub fn close_tab(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        let existing = self
            .workspaces
            .get(workspace_key)
            .ok_or_else(|| missing_workspace())?;
        let position = existing
            .tabs
            .iter()
            .position(|tab| tab.id == tab_id)
            .ok_or_else(|| missing_tab(tab_id))?;
        let replacement_id = if existing.tabs.len() == 1 {
            Some(self.generate_tab_id()?)
        } else {
            None
        };
        let snapshot = self.workspace_mut(workspace_key)?;
        let was_selected = snapshot.selected_tab_id.as_deref() == Some(tab_id);
        snapshot.tabs.remove(position);
        if let Some(replacement_id) = replacement_id {
            snapshot.tabs.push(BrowserTabSnapshot {
                id: replacement_id.clone(),
                title: String::new(),
                url: "about:blank".to_string(),
                viewport: BrowserViewport::default(),
            });
            snapshot.selected_tab_id = Some(replacement_id);
        } else if was_selected {
            let selected_position = position.min(snapshot.tabs.len().saturating_sub(1));
            snapshot.selected_tab_id = snapshot
                .tabs
                .get(selected_position)
                .map(|tab| tab.id.clone());
        }
        snapshot.advance_revision();
        Ok(BrowserWorkspaceMutation::new(snapshot.clone()))
    }

    pub fn navigate_tab(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
        url: &str,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        let url = validate_browser_url(url)?;
        let snapshot = self.workspace_mut(workspace_key)?;
        let tab = snapshot
            .tabs
            .iter_mut()
            .find(|tab| tab.id == tab_id)
            .ok_or_else(|| missing_tab(tab_id))?;
        if tab.url != url {
            tab.url = url;
            snapshot.advance_revision();
        }
        Ok(BrowserWorkspaceMutation::new(snapshot.clone()))
    }

    pub fn update_viewport(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
        viewport: BrowserViewport,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        let snapshot = self.workspace_mut(workspace_key)?;
        let tab = snapshot
            .tabs
            .iter_mut()
            .find(|tab| tab.id == tab_id)
            .ok_or_else(|| missing_tab(tab_id))?;
        if tab.viewport != viewport {
            tab.viewport = viewport;
            snapshot.advance_revision();
        }
        Ok(BrowserWorkspaceMutation::new(snapshot.clone()))
    }

    pub fn apply_title_change(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
        title: impl Into<String>,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        let snapshot = self.workspace_mut(workspace_key)?;
        let tab = snapshot
            .tabs
            .iter_mut()
            .find(|tab| tab.id == tab_id)
            .ok_or_else(|| missing_tab(tab_id))?;
        tab.title = title.into();
        snapshot.advance_revision();
        Ok(BrowserWorkspaceMutation::new(snapshot.clone()))
    }

    pub fn apply_user_input(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        let snapshot = self.workspace_mut(workspace_key)?;
        if !snapshot.tabs.iter().any(|tab| tab.id == tab_id) {
            return Err(missing_tab(tab_id));
        }
        snapshot.advance_revision();
        Ok(BrowserWorkspaceMutation::new(snapshot.clone()))
    }

    pub fn apply_dom_mutation(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        let snapshot = self.workspace_mut(workspace_key)?;
        if !snapshot.tabs.iter().any(|tab| tab.id == tab_id) {
            return Err(missing_tab(tab_id));
        }
        snapshot.advance_revision();
        Ok(BrowserWorkspaceMutation::new(snapshot.clone()))
    }

    pub fn apply_automation_mutation(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        self.apply_dom_mutation(workspace_key, tab_id)
    }

    pub fn append_journal_entry(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        entry: super::BrowserJournalEntry,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        let snapshot = self.workspace_mut(workspace_key)?;
        snapshot.append_journal_entry(entry);
        Ok(BrowserWorkspaceMutation::new(snapshot.clone()))
    }

    pub fn apply_page_load(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        tab_id: &str,
        url: &str,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        let url = validate_browser_url(url)?;
        let snapshot = self.workspace_mut(workspace_key)?;
        let tab = snapshot
            .tabs
            .iter_mut()
            .find(|tab| tab.id == tab_id)
            .ok_or_else(|| missing_tab(tab_id))?;
        tab.url = url;
        snapshot.advance_revision();
        Ok(BrowserWorkspaceMutation::new(snapshot.clone()))
    }

    pub fn reset_workspace(&mut self, workspace_key: &BrowserWorkspaceKey) {
        self.workspaces.remove(workspace_key);
        if self.active_workspace.as_ref() == Some(workspace_key) {
            self.active_workspace = None;
        }
    }

    pub fn clear_project_workspaces(&mut self, project_id: &str) {
        self.workspaces
            .retain(|workspace_key, _| workspace_key.project_id != project_id);
        if self
            .active_workspace
            .as_ref()
            .is_some_and(|workspace_key| workspace_key.project_id == project_id)
        {
            self.active_workspace = None;
        }
    }

    pub fn workspace(
        &self,
        workspace_key: &BrowserWorkspaceKey,
    ) -> Option<&BrowserWorkspaceSnapshot> {
        self.workspaces.get(workspace_key)
    }

    pub(crate) fn workspace_keys(&self) -> Vec<BrowserWorkspaceKey> {
        let mut keys = self.workspaces.keys().cloned().collect::<Vec<_>>();
        keys.sort_by(|left, right| {
            left.project_id
                .cmp(&right.project_id)
                .then_with(|| left.ai_tab_id.cmp(&right.ai_tab_id))
        });
        keys
    }

    fn workspace_mut(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
    ) -> Result<&mut BrowserWorkspaceSnapshot, BrowserError> {
        self.workspaces
            .get_mut(workspace_key)
            .ok_or_else(missing_workspace)
    }

    pub fn selected_view_plan(
        &self,
        workspace_key: &BrowserWorkspaceKey,
    ) -> Option<BrowserViewCreationPlan> {
        let snapshot = self.workspaces.get(workspace_key)?;
        let selected = snapshot.selected_tab_id.as_deref()?;
        let tab = snapshot.tabs.iter().find(|tab| tab.id == selected)?;
        Some(BrowserViewCreationPlan {
            workspace_key: workspace_key.clone(),
            tab_id: tab.id.clone(),
            url: tab.url.clone(),
        })
    }

    pub fn project_context_key(
        &self,
        workspace_key: &BrowserWorkspaceKey,
    ) -> BrowserProjectContextKey {
        BrowserProjectContextKey {
            project_id: workspace_key.project_id.clone(),
            profile_dir: BrowserStorageLayout::new(&self.app_config_dir, &workspace_key.project_id)
                .profile_dir,
        }
    }

    pub fn set_pane_open(
        &mut self,
        workspace_key: &BrowserWorkspaceKey,
        open: bool,
    ) -> Result<BrowserWorkspaceMutation, BrowserError> {
        let snapshot =
            self.workspaces
                .get_mut(workspace_key)
                .ok_or_else(|| BrowserError::CrashedView {
                    message: "browser workspace has not been ensured".to_string(),
                })?;
        if snapshot.pane_open != open {
            snapshot.pane_open = open;
        }
        Ok(BrowserWorkspaceMutation::new(snapshot.clone()))
    }

    pub fn set_active_workspace(&mut self, workspace_key: Option<BrowserWorkspaceKey>) {
        self.active_workspace = workspace_key;
    }

    pub fn active_workspace(&self) -> Option<&BrowserWorkspaceKey> {
        self.active_workspace.as_ref()
    }

    pub fn visibility_plan(&self) -> Vec<BrowserViewVisibilityPlan> {
        let mut plans = Vec::new();
        for (workspace_key, snapshot) in &self.workspaces {
            let workspace_is_visible =
                self.active_workspace.as_ref() == Some(workspace_key) && snapshot.pane_open;
            for tab in &snapshot.tabs {
                let visible = workspace_is_visible
                    && snapshot.selected_tab_id.as_deref() == Some(tab.id.as_str());
                plans.push(BrowserViewVisibilityPlan {
                    workspace_key: workspace_key.clone(),
                    tab_id: tab.id.clone(),
                    visible,
                    memory_target: if visible {
                        BrowserMemoryTarget::Normal
                    } else {
                        BrowserMemoryTarget::Low
                    },
                });
            }
        }
        plans
    }

    pub fn profile_clear_plan(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        candidate: impl AsRef<Path>,
    ) -> Result<BrowserProfileClearPlan, BrowserError> {
        let expected =
            BrowserStorageLayout::new(&self.app_config_dir, &workspace_key.project_id).profile_dir;
        let candidate = candidate.as_ref();
        let hash_is_valid = expected
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|value| {
                value.len() == 64
                    && value.chars().all(|character| {
                        character.is_ascii_digit() || ('a'..='f').contains(&character)
                    })
            });
        if candidate != expected || !hash_is_valid {
            return Err(BrowserError::OutsideWorkspace {
                path: candidate.to_path_buf(),
            });
        }
        Ok(BrowserProfileClearPlan {
            profile_dir: expected,
        })
    }

    fn generate_tab_id(&self) -> Result<String, BrowserError> {
        loop {
            let mut random = [0_u8; 16];
            getrandom::fill(&mut random).map_err(|_| BrowserError::CrashedView {
                message: "could not generate browser tab id".to_string(),
            })?;
            let mut id = String::with_capacity(36);
            id.push_str("tab-");
            for byte in random {
                let _ = write!(id, "{byte:02x}");
            }
            if self
                .workspaces
                .values()
                .all(|snapshot| snapshot.tabs.iter().all(|tab| tab.id != id))
            {
                return Ok(id);
            }
        }
    }
}

fn annotation_screenshot_handle(
    resources: &BrowserResourceStore,
    workspace_key: &BrowserWorkspaceKey,
    resource_id: &super::BrowserResourceId,
) -> Result<BrowserResourceHandle, BrowserError> {
    let handle = resources.handle(workspace_key, resource_id)?;
    if handle.kind != BrowserResourceKind::AnnotationScreenshot || handle.mime_type != "image/png" {
        return Err(BrowserError::MissingResource {
            id: resource_id.clone(),
        });
    }
    Ok(handle)
}

fn truncate_annotation_summary(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn missing_workspace() -> BrowserError {
    BrowserError::CrashedView {
        message: "browser workspace has not been ensured".to_string(),
    }
}

fn missing_tab(_tab_id: &str) -> BrowserError {
    BrowserError::CrashedView {
        message: "browser tab does not exist".to_string(),
    }
}

pub fn validate_browser_url(url: &str) -> Result<String, BrowserError> {
    let failure = |message: &str| BrowserError::NavigationFailure {
        url: url.to_string(),
        message: message.to_string(),
    };
    if url.is_empty() || url.trim() != url || url.chars().any(char::is_whitespace) {
        return Err(failure("URL contains empty or whitespace input"));
    }
    if url.eq_ignore_ascii_case("about:blank") {
        return Ok(url.to_string());
    }
    let Some((scheme, remainder)) = url.split_once("://") else {
        return Err(failure("URL must use http, https, or about:blank"));
    };
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return Err(failure("URL scheme is not allowed"));
    }
    let authority = remainder.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('\\') {
        return Err(failure("URL must contain a valid network host"));
    }
    Ok(url.to_string())
}

pub fn unique_download_path(
    downloads_dir: impl AsRef<Path>,
    suggested_path: impl AsRef<Path>,
) -> Result<PathBuf, BrowserError> {
    let downloads_dir = super::downloads::prepare_untrusted_download_root(downloads_dir.as_ref())?;
    super::downloads::unique_path_in(&downloads_dir, suggested_path.as_ref())
}
pub use initialization::browser_user_input_initialization_script;

#[cfg(test)]
mod native_view_authority_tests {
    use super::*;
    use crate::domain::id::{BrowserContextId, ClientId, ResourceId, TaskId};
    use crate::protocol::{BrowserCoordinateSpace, BrowserLogicalBounds, BrowserPhysicalPoint};

    struct RecordingBackend {
        calls: Vec<&'static str>,
        operation_descriptors: Vec<BrowserSurfaceDescriptor>,
        on_ui_thread: bool,
        owner_matches: bool,
        actual_process: Option<BrowserHostProcessIdentity>,
        allocated_identities: Vec<BrowserSurfaceIdentity>,
        actual_attached: bool,
        actual_parent: Option<BrowserWindowHandle>,
        actual_bounds: BrowserPhysicalBounds,
        actual_focused: bool,
        partial_operation: Option<&'static str>,
        partial_seen: bool,
        rollback_fails: bool,
        allow_crash_observation: bool,
        allow_zero_residue: bool,
        crash_mutates_then_errors: bool,
        allocation_mutates_then_errors: bool,
        allocation_rollback_fails: bool,
        owner_changes_after_allocation: bool,
        process_changes_after_allocation: bool,
        owner_changes_after_first_admission_check: bool,
        process_changes_after_first_admission_check: bool,
    }

    impl Default for RecordingBackend {
        fn default() -> Self {
            Self {
                calls: Vec::new(),
                operation_descriptors: Vec::new(),
                on_ui_thread: true,
                owner_matches: true,
                actual_process: None,
                allocated_identities: Vec::new(),
                actual_attached: false,
                actual_parent: None,
                actual_bounds: BrowserPhysicalBounds::new(-16, -8, 640, 480)
                    .expect("valid default bounds"),
                actual_focused: false,
                partial_operation: None,
                partial_seen: false,
                rollback_fails: false,
                allow_crash_observation: false,
                allow_zero_residue: false,
                crash_mutates_then_errors: false,
                allocation_mutates_then_errors: false,
                allocation_rollback_fails: false,
                owner_changes_after_allocation: false,
                process_changes_after_allocation: false,
                owner_changes_after_first_admission_check: false,
                process_changes_after_first_admission_check: false,
            }
        }
    }

    impl browser_native_surface_backend_seal::Sealed for RecordingBackend {}

    impl BrowserNativeSurfaceBackend for RecordingBackend {
        fn preflight_native_view_allocation(
            &mut self,
            registration: &BrowserNativeViewRegistration,
        ) -> Result<(), String> {
            self.calls.push("allocation");
            if !self.owner_matches {
                return Err("child window owner changed".to_string());
            }
            self.allocated_identities.push(registration.identity);
            if self
                .actual_process
                .as_ref()
                .is_some_and(|actual| actual != &registration.host_process)
            {
                return Err("host PID identity was reused".to_string());
            }
            if self.allocation_mutates_then_errors {
                return Err("partial native allocation failure".to_string());
            }
            if self.owner_changes_after_allocation {
                self.owner_matches = false;
            }
            if self.process_changes_after_allocation {
                self.actual_process = Some(
                    BrowserHostProcessIdentity::new(
                        registration.host_process.pid,
                        registration.host_process.creation_time_100ns + 1,
                        "C:\\DevManager\\reused-host.exe",
                    )
                    .expect("valid reused process"),
                );
            }
            Ok(())
        }

        fn rollback_native_view_allocation(
            &mut self,
            registration: &BrowserNativeViewRegistration,
        ) -> Result<(), String> {
            self.calls.push("rollback");
            if self.allocation_rollback_fails {
                return Err("allocation rollback failed with hostile backend text".to_string());
            }
            self.allocated_identities
                .retain(|identity| *identity != registration.identity);
            Ok(())
        }

        fn preflight_native_view_operation(
            &mut self,
            descriptor: &BrowserSurfaceDescriptor,
            _parking: &BrowserWindowHandle,
        ) -> Result<(), String> {
            self.calls.push("preflight");
            self.operation_descriptors.push(descriptor.clone());
            if !self.owner_matches {
                return Err("child window owner changed".to_string());
            }
            if !self.allocated_identities.contains(&descriptor.identity) {
                return Err("surface identity changed".to_string());
            }
            if self
                .actual_process
                .as_ref()
                .is_some_and(|actual| actual != &descriptor.host_process)
            {
                return Err("host PID identity was reused".to_string());
            }
            if self.operation_descriptors.len() == 1 {
                if self.owner_changes_after_first_admission_check {
                    self.owner_matches = false;
                }
                if self.process_changes_after_first_admission_check {
                    self.actual_process = Some(
                        BrowserHostProcessIdentity::new(
                            descriptor.host_process.pid,
                            descriptor.host_process.creation_time_100ns + 1,
                            "C:\\DevManager\\post-admission-reused-host.exe",
                        )
                        .expect("valid post-admission process identity"),
                    );
                }
            }
            Ok(())
        }

        fn assert_ui_thread(&self) -> Result<(), String> {
            if self.on_ui_thread {
                Ok(())
            } else {
                Err("backend is not on its UI thread".to_string())
            }
        }

        fn park_surface(
            &mut self,
            _child: &BrowserWindowHandle,
            parking: &BrowserWindowHandle,
        ) -> Result<(), String> {
            self.calls.push("park");
            if self.partial_operation == Some("park") && !self.partial_seen {
                self.partial_seen = true;
                self.actual_attached = false;
                self.actual_parent = Some(parking.clone());
                return Err("partial park failure".to_string());
            }
            if self.rollback_fails && self.partial_seen {
                return Err("rollback park failure".to_string());
            }
            self.actual_attached = false;
            self.actual_parent = Some(parking.clone());
            Ok(())
        }

        fn attach_surface(
            &mut self,
            child: &BrowserWindowHandle,
            destination: &BrowserWindowHandle,
            bounds: BrowserPhysicalBounds,
        ) -> Result<(), String> {
            self.calls.push("attach");
            if destination == child {
                return Err("destination equals child".to_string());
            }
            if self.partial_operation == Some("attach") && !self.partial_seen {
                self.partial_seen = true;
                self.actual_attached = true;
                self.actual_parent = Some(destination.clone());
                self.actual_bounds = bounds;
                return Err("partial attach failure".to_string());
            }
            if self.rollback_fails && self.partial_seen {
                return Err("rollback attach failure".to_string());
            }
            self.actual_attached = true;
            self.actual_parent = Some(destination.clone());
            self.actual_bounds = bounds;
            Ok(())
        }

        fn set_surface_bounds(
            &mut self,
            _child: &BrowserWindowHandle,
            bounds: BrowserPhysicalBounds,
        ) -> Result<(), String> {
            self.calls.push("bounds");
            if self.partial_operation == Some("bounds") && !self.partial_seen {
                self.partial_seen = true;
                self.actual_bounds = bounds;
                return Err("partial bounds failure".to_string());
            }
            if self.rollback_fails && self.partial_seen {
                return Err("rollback bounds failure".to_string());
            }
            self.actual_bounds = bounds;
            Ok(())
        }

        fn set_surface_focus(
            &mut self,
            _child: &BrowserWindowHandle,
            focused: bool,
        ) -> Result<(), String> {
            self.calls.push("focus");
            if self.partial_operation == Some("focus") && !self.partial_seen {
                self.partial_seen = true;
                self.actual_focused = focused;
                return Err("partial focus failure".to_string());
            }
            if self.rollback_fails && self.partial_seen {
                return Err("rollback focus failure".to_string());
            }
            self.actual_focused = focused;
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
            self.calls.push("verify");
            if !self.allocated_identities.contains(&descriptor.identity) {
                return Err("surface identity changed".to_string());
            }
            if self.actual_attached != attached {
                return Err("attached postcondition mismatch".to_string());
            }
            if self.actual_bounds != bounds {
                return Err("bounds postcondition mismatch".to_string());
            }
            if self.actual_focused != focused {
                return Err("focus postcondition mismatch".to_string());
            }
            if attached {
                let destination = attached_parent.ok_or("missing attach destination")?;
                if destination == parking {
                    return Err("attach destination must not be parking".to_string());
                }
                if self.actual_parent.as_ref() != Some(destination) {
                    return Err("destination parent mismatch".to_string());
                }
            } else if attached_parent.is_some() {
                return Err("parked surface must not retain an attach destination".to_string());
            } else if self
                .actual_parent
                .as_ref()
                .is_some_and(|parent| parent != parking)
            {
                return Err("parked surface must remain parented to parking HWND".to_string());
            }
            Ok(())
        }

        fn observe_surface_crash(
            &mut self,
            descriptor: &BrowserSurfaceDescriptor,
            parking: &BrowserWindowHandle,
        ) -> Result<(), String> {
            self.calls.push("crash");
            self.preflight_native_view_operation(descriptor, parking)?;
            if self.crash_mutates_then_errors {
                self.actual_attached = false;
                return Err("partial crash observation failure".to_string());
            }
            if self.allow_crash_observation {
                Ok(())
            } else {
                Err("no live crash observation".to_string())
            }
        }

        fn observe_teardown_zero_residue(
            &mut self,
            descriptor: &BrowserSurfaceDescriptor,
            parking: &BrowserWindowHandle,
        ) -> Result<(), String> {
            self.calls.push("teardown-residue");
            if !self.on_ui_thread {
                return Err("backend is not on its UI thread".to_string());
            }
            if !self.allocated_identities.contains(&descriptor.identity) {
                return Err("surface identity changed".to_string());
            }
            let _ = parking;
            if self.allow_zero_residue && !self.actual_attached {
                Ok(())
            } else {
                Err("real runtime zero-residue observation unavailable".to_string())
            }
        }
    }

    fn registration() -> BrowserNativeViewRegistration {
        BrowserNativeViewRegistration::from_host_record(
            BrowserSurfaceIdentity {
                task_id: TaskId::new(),
                context_id: BrowserContextId::new(),
                resource_id: ResourceId::new(),
            },
            BrowserWindowHandle::from_raw(0x1001).expect("valid child handle"),
            BrowserWindowHandle::from_raw(0x2001).expect("valid parking handle"),
            BrowserHostProcessIdentity::new(41, 9_001, "C:\\DevManager\\devmanager-host.exe")
                .expect("valid host process"),
            BrowserPhysicalBounds::new(-16, -8, 640, 480).expect("valid bounds"),
            BrowserDpi::new(144, 144).expect("valid dpi"),
        )
        .expect("valid host record")
    }

    fn attach_destination() -> BrowserWindowHandle {
        BrowserWindowHandle::from_raw(0xA011).expect("destination")
    }

    fn issue(
        state: &mut BrowserHostState,
        backend: &mut RecordingBackend,
    ) -> BrowserNativeViewReceipt {
        let issued = state
            .register_native_view_with_backend(registration(), backend)
            .expect("backend proved host-owned registration");
        backend.actual_attached = false;
        backend.actual_focused = false;
        backend.actual_bounds = issued.descriptor.physical_bounds;
        backend.calls.clear();
        issued
    }

    fn client_request(
        receipt: &BrowserNativeViewReceipt,
        client_id: ClientId,
    ) -> BrowserClientRequest {
        let mut request = BrowserClientRequest::new(
            receipt.descriptor.clone(),
            client_id,
            receipt
                .attachment_lease
                .clone()
                .expect("attached view has a lease"),
        );
        request.client_sequence = receipt.descriptor.bounds_epoch.value();
        request
    }

    fn host_request(
        state: &BrowserHostState,
        receipt: &BrowserNativeViewReceipt,
    ) -> BrowserHostRequest {
        state
            .host_request(&receipt.descriptor.identity)
            .expect("host owns the request capability")
    }

    fn attached_state() -> (
        BrowserHostState,
        BrowserNativeViewReceipt,
        ClientId,
        RecordingBackend,
    ) {
        let mut state = BrowserHostState::new(std::env::temp_dir()).expect("browser host state");
        let mut backend = RecordingBackend::default();
        let issued = issue(&mut state, &mut backend);
        let client_id = ClientId::new();
        let attached = state
            .attach_native_view_with_backend(
                BrowserAttachRequest::new(issued.descriptor, client_id),
                attach_destination(),
                &mut backend,
            )
            .expect("attach succeeds");
        (state, attached, client_id, backend)
    }

    #[test]
    fn attach_records_explicit_destination_and_rejects_parking() {
        let mut state = BrowserHostState::new(std::env::temp_dir()).expect("browser host state");
        let mut backend = RecordingBackend::default();
        let issued = issue(&mut state, &mut backend);
        let parking = BrowserWindowHandle::from_raw(0x2001).expect("parking");
        assert!(
            matches!(
                state.attach_native_view_with_backend(
                    BrowserAttachRequest::new(issued.descriptor.clone(), ClientId::new()),
                    parking,
                    &mut backend,
                ),
                Err(BrowserNativeViewError::InvalidInput(_))
            ),
            "parking HWND is not an attach destination"
        );
        let destination = attach_destination();
        let attached = state
            .attach_native_view_with_backend(
                BrowserAttachRequest::new(issued.descriptor, ClientId::new()),
                destination.clone(),
                &mut backend,
            )
            .expect("attach succeeds");
        assert_eq!(attached.attached_parent.as_ref(), Some(&destination));
        let parked = state
            .park_native_view_with_backend(host_request(&state, &attached), &mut backend)
            .expect("park");
        assert_eq!(parked.attached_parent, None);
        assert_eq!(parked.lifecycle, BrowserSurfaceLifecycle::Parked);
    }

    #[test]
    fn backend_does_not_mutate_after_fallible_attach_preparation_fails() {
        let mut state = BrowserHostState::new(std::env::temp_dir()).expect("browser host state");
        let mut backend = RecordingBackend::default();
        let issued = issue(&mut state, &mut backend);
        let before = issued.clone();
        state.native_host_fence = BrowserHostFence::new(1, u64::MAX).expect("valid fence");

        let error = state
            .attach_native_view_with_backend(
                BrowserAttachRequest::new(issued.descriptor.clone(), ClientId::new()),
                attach_destination(),
                &mut backend,
            )
            .expect_err("connection fence overflow must reject before backend mutation");

        assert!(matches!(
            error,
            BrowserNativeViewError::Descriptor(BrowserDtoError::Overflow("host connection epoch"))
        ));
        assert!(
            backend.calls.is_empty(),
            "fallible attach preparation must not reach the backend"
        );
        assert_eq!(
            state
                .native_view(&before.descriptor.identity)
                .expect("view remains registered"),
            before
        );
    }

    #[test]
    fn mutating_allocation_failure_rolls_back_backend_identity_before_state_commit() {
        let mut state = BrowserHostState::new(std::env::temp_dir()).expect("browser host state");
        let registration = registration();
        let identity = registration.identity();
        let mut backend = RecordingBackend {
            allocation_mutates_then_errors: true,
            ..RecordingBackend::default()
        };

        assert_eq!(
            state
                .register_native_view_with_backend(registration, &mut backend)
                .expect_err("a mutating allocation error must be visible"),
            BrowserNativeViewError::Backend
        );
        assert!(
            backend.allocated_identities.is_empty(),
            "failed allocation must not leak a backend-owned identity"
        );
        assert!(state.native_view(&identity).is_none());
    }

    #[test]
    fn post_allocation_owner_race_rolls_back_before_host_state_commit() {
        let mut state = BrowserHostState::new(std::env::temp_dir()).expect("browser host state");
        let registration = registration();
        let identity = registration.identity();
        let mut backend = RecordingBackend {
            owner_changes_after_allocation: true,
            ..RecordingBackend::default()
        };

        assert_eq!(
            state
                .register_native_view_with_backend(registration, &mut backend)
                .expect_err("an owner race after allocation must fail closed"),
            BrowserNativeViewError::Backend
        );
        assert!(
            backend.allocated_identities.is_empty(),
            "post-admission identity failure must release allocation"
        );
        assert!(state.native_view(&identity).is_none());
    }

    #[test]
    fn post_allocation_process_race_rolls_back_before_host_state_commit() {
        let mut state = BrowserHostState::new(std::env::temp_dir()).expect("browser host state");
        let registration = registration();
        let identity = registration.identity();
        let mut backend = RecordingBackend {
            process_changes_after_allocation: true,
            ..RecordingBackend::default()
        };

        assert_eq!(
            state
                .register_native_view_with_backend(registration, &mut backend)
                .expect_err("a PID reuse race after allocation must fail closed"),
            BrowserNativeViewError::Backend
        );
        assert!(
            backend.allocated_identities.is_empty(),
            "post-admission PID failure must release allocation"
        );
        assert!(state.native_view(&identity).is_none());
    }

    #[test]
    fn final_post_allocation_identity_check_rolls_back_a_late_owner_race() {
        let mut state = BrowserHostState::new(std::env::temp_dir()).expect("browser host state");
        let registration = registration();
        let identity = registration.identity();
        let mut backend = RecordingBackend {
            owner_changes_after_first_admission_check: true,
            ..RecordingBackend::default()
        };

        assert_eq!(
            state
                .register_native_view_with_backend(registration, &mut backend)
                .expect_err("a late owner race must fail before state commit"),
            BrowserNativeViewError::Backend
        );
        assert!(backend.allocated_identities.is_empty());
        assert!(state.native_view(&identity).is_none());
    }

    #[test]
    fn final_post_allocation_identity_check_rolls_back_a_late_process_race() {
        let mut state = BrowserHostState::new(std::env::temp_dir()).expect("browser host state");
        let registration = registration();
        let identity = registration.identity();
        let mut backend = RecordingBackend {
            process_changes_after_first_admission_check: true,
            ..RecordingBackend::default()
        };

        assert_eq!(
            state
                .register_native_view_with_backend(registration, &mut backend)
                .expect_err("a late PID race must fail before state commit"),
            BrowserNativeViewError::Backend
        );
        assert!(backend.allocated_identities.is_empty());
        assert!(state.native_view(&identity).is_none());
    }

    #[test]
    fn allocation_rollback_failure_is_retained_as_a_bounded_teardown_orphan() {
        let mut state = BrowserHostState::new(std::env::temp_dir()).expect("browser host state");
        let registration = registration();
        let identity = registration.identity();
        let mut backend = RecordingBackend {
            allocation_mutates_then_errors: true,
            allocation_rollback_fails: true,
            ..RecordingBackend::default()
        };

        assert_eq!(
            state
                .register_native_view_with_backend(registration, &mut backend)
                .expect_err("an allocation rollback failure must fail closed"),
            BrowserNativeViewError::ReconciliationRequired
        );
        assert_eq!(state.native_allocation_orphan_count_for_test(), 1);
        assert!(state.native_view(&identity).is_none());
        assert!(
            backend.allocated_identities.contains(&identity),
            "a failed rollback must remain visible for teardown reconciliation"
        );
    }

    #[test]
    fn crash_observer_ui_thread_failure_marks_unknown_before_returning() {
        let (mut state, attached, _client_id, mut backend) = attached_state();
        let capability = state
            .controller_capability(&attached.descriptor.identity)
            .unwrap();
        backend.on_ui_thread = false;

        assert!(state
            .observe_native_view_crash_with_backend(capability, &mut backend)
            .is_err());
        assert_eq!(
            state
                .native_view(&attached.descriptor.identity)
                .unwrap()
                .reconciliation,
            BrowserNativeViewReconciliation::Unknown
        );
    }

    #[test]
    fn crash_observer_preflight_failure_marks_unknown_before_returning() {
        let (mut state, attached, _client_id, mut backend) = attached_state();
        let capability = state
            .controller_capability(&attached.descriptor.identity)
            .unwrap();
        backend.owner_matches = false;

        assert!(state
            .observe_native_view_crash_with_backend(capability, &mut backend)
            .is_err());
        assert_eq!(
            state
                .native_view(&attached.descriptor.identity)
                .unwrap()
                .reconciliation,
            BrowserNativeViewReconciliation::Unknown
        );
    }

    #[test]
    fn registration_requires_ui_thread_and_owner_proof_before_state() {
        let mut state = BrowserHostState::new(std::env::temp_dir()).expect("browser host state");
        let registration = registration();
        let identity = registration.identity();
        let mut backend = RecordingBackend {
            owner_matches: false,
            ..RecordingBackend::default()
        };

        assert!(state
            .register_native_view_with_backend(registration, &mut backend)
            .is_err());
        assert!(state.native_view(&identity).is_none());
        assert_eq!(backend.calls, ["allocation", "rollback"]);
    }

    #[test]
    fn host_registration_admission_rejects_duplicate_and_foreign_before_backend_allocation() {
        let mut state = BrowserHostState::new(std::env::temp_dir()).expect("browser host state");
        let mut backend = RecordingBackend::default();
        let first = registration();
        let identity = first.identity();

        state
            .register_native_view_with_backend(first.clone(), &mut backend)
            .expect("first registration succeeds");
        backend.calls.clear();

        assert_eq!(
            state
                .register_native_view_with_backend(first, &mut backend)
                .expect_err("duplicate registration must be rejected"),
            BrowserNativeViewError::DuplicateView
        );
        assert!(
            backend.calls.is_empty(),
            "duplicate admission must not allocate a native surface"
        );

        let foreign = BrowserNativeViewRegistration::from_host_record(
            BrowserSurfaceIdentity {
                task_id: TaskId::new(),
                context_id: BrowserContextId::new(),
                resource_id: ResourceId::new(),
            },
            BrowserWindowHandle::from_raw(0x3001).expect("valid child handle"),
            BrowserWindowHandle::from_raw(0x4001).expect("valid parking handle"),
            BrowserHostProcessIdentity::new(42, 9_002, "C:\\DevManager\\other-host.exe")
                .expect("valid foreign process"),
            BrowserPhysicalBounds::new(-16, -8, 640, 480).expect("valid bounds"),
            BrowserDpi::new(144, 144).expect("valid dpi"),
        )
        .expect("valid foreign registration");
        assert_eq!(
            state
                .register_native_view_with_backend(foreign, &mut backend)
                .expect_err("foreign registration must be rejected"),
            BrowserNativeViewError::ForeignDescriptor("host process")
        );
        assert!(
            backend.calls.is_empty(),
            "foreign admission must not allocate a native surface"
        );
        assert!(state.native_view(&identity).is_some());
    }

    #[test]
    fn host_registration_state_overflow_rejects_before_backend_allocation() {
        let mut state = BrowserHostState::new(std::env::temp_dir()).expect("browser host state");
        let mut backend = RecordingBackend::default();
        state.next_native_runtime_generation = u64::MAX;

        assert_eq!(
            state
                .register_native_view_with_backend(registration(), &mut backend)
                .expect_err("runtime generation exhaustion must fail closed"),
            BrowserNativeViewError::Descriptor(BrowserDtoError::Overflow("runtime generation"))
        );
        assert!(
            backend.calls.is_empty(),
            "host state rejection must not leak native allocation"
        );
    }

    #[test]
    fn partial_attach_failure_rolls_back_or_enters_unknown() {
        let mut state = BrowserHostState::new(std::env::temp_dir()).expect("browser host state");
        let mut backend = RecordingBackend::default();
        let issued = issue(&mut state, &mut backend);
        backend.partial_operation = Some("attach");
        backend.rollback_fails = true;

        let error = state
            .attach_native_view_with_backend(
                BrowserAttachRequest::new(issued.descriptor.clone(), ClientId::new()),
                attach_destination(),
                &mut backend,
            )
            .expect_err("partial attach must not be reported as healthy");

        assert_eq!(error, BrowserNativeViewError::ReconciliationRequired);
        let receipt = state.native_view(&issued.descriptor.identity).unwrap();
        assert_eq!(
            receipt.reconciliation,
            BrowserNativeViewReconciliation::Unknown
        );
        let request = host_request(&state, &receipt);
        assert_eq!(
            state.native_teardown_status(&request).unwrap(),
            BrowserTeardownStatus::Blocked(
                BrowserTeardownBlocker::NativeSurfaceReconciliationRequired
            )
        );
    }

    #[test]
    fn partial_park_failure_rolls_back_or_enters_unknown() {
        let (mut state, attached, client_id, mut backend) = attached_state();
        backend.partial_operation = Some("park");
        backend.rollback_fails = true;

        let error = state
            .park_native_view_with_backend(host_request(&state, &attached), &mut backend)
            .expect_err("partial park must not be reported as healthy");

        assert_eq!(error, BrowserNativeViewError::ReconciliationRequired);
        assert_eq!(
            state
                .native_view(&attached.descriptor.identity)
                .unwrap()
                .reconciliation,
            BrowserNativeViewReconciliation::Unknown
        );
        assert!(matches!(
            state.native_view(&attached.descriptor.identity).unwrap().lifecycle,
            BrowserSurfaceLifecycle::Attached { client_id: ref id } if *id == client_id
        ));
    }

    #[test]
    fn partial_bounds_failure_rolls_back_or_enters_unknown() {
        let (mut state, attached, client_id, mut backend) = attached_state();
        backend.partial_operation = Some("bounds");
        backend.rollback_fails = true;
        let geometry = BrowserGeometryInput::new(
            BrowserCoordinateSpace::Local,
            BrowserLogicalBounds::new(0, 0, 320, 240).unwrap(),
            BrowserPhysicalPoint::new(0, 0),
            BrowserDpi::new(96, 96).unwrap(),
        )
        .unwrap();

        let error = state
            .update_native_view_geometry_with_backend(
                client_request(&attached, client_id),
                geometry,
                &mut backend,
            )
            .expect_err("partial bounds must not be reported as healthy");

        assert_eq!(error, BrowserNativeViewError::ReconciliationRequired);
        assert_eq!(
            state
                .native_view(&attached.descriptor.identity)
                .unwrap()
                .reconciliation,
            BrowserNativeViewReconciliation::Unknown
        );
    }

    #[test]
    fn partial_focus_failure_rolls_back_or_enters_unknown() {
        let (mut state, attached, client_id, mut backend) = attached_state();
        backend.partial_operation = Some("focus");
        backend.rollback_fails = true;

        let error = state
            .update_native_view_focus_with_backend(
                client_request(&attached, client_id),
                true,
                &mut backend,
            )
            .expect_err("partial focus must not be reported as healthy");

        assert_eq!(error, BrowserNativeViewError::ReconciliationRequired);
        assert_eq!(
            state
                .native_view(&attached.descriptor.identity)
                .unwrap()
                .reconciliation,
            BrowserNativeViewReconciliation::Unknown
        );
    }

    #[test]
    fn recoverable_backend_failure_does_not_commit_authority() {
        let (mut state, attached, client_id, mut backend) = attached_state();
        let before = attached.clone();
        backend.partial_operation = Some("focus");

        let error = state
            .update_native_view_focus_with_backend(
                client_request(&attached, client_id),
                true,
                &mut backend,
            )
            .expect_err("backend failure must be visible");

        assert!(matches!(error, BrowserNativeViewError::Backend));
        assert_eq!(
            state.native_view(&before.descriptor.identity).unwrap(),
            before
        );
    }

    #[test]
    fn every_native_action_revalidates_owner_and_process_identity() {
        let (mut state, issued, client_id, mut backend) = attached_state();
        backend.owner_matches = false;
        let before = issued.clone();
        let error = state
            .update_native_view_focus_with_backend(
                client_request(&issued, client_id),
                true,
                &mut backend,
            )
            .expect_err("owner change must block focus");
        assert!(matches!(error, BrowserNativeViewError::Backend));
        assert_eq!(
            state.native_view(&before.descriptor.identity).unwrap(),
            before
        );
        assert!(!backend.calls.contains(&"focus"));

        backend.owner_matches = true;
        backend.actual_process = Some(
            BrowserHostProcessIdentity::new(41, 99_999, "C:\\DevManager\\reused.exe")
                .expect("valid reused process identity"),
        );
        let error = state
            .update_native_view_focus_with_backend(
                client_request(&issued, client_id),
                true,
                &mut backend,
            )
            .expect_err("PID reuse must block focus");
        assert!(matches!(error, BrowserNativeViewError::Backend));
        assert!(!backend.calls.contains(&"focus"));
    }

    #[test]
    fn attach_revalidates_prepared_generation_and_connection_before_mutation() {
        let mut state = BrowserHostState::new(std::env::temp_dir()).expect("browser host state");
        let mut backend = RecordingBackend::default();
        let issued = issue(&mut state, &mut backend);
        backend.operation_descriptors.clear();

        let attached = state
            .attach_native_view_with_backend(
                BrowserAttachRequest::new(issued.descriptor, ClientId::new()),
                attach_destination(),
                &mut backend,
            )
            .expect("attach succeeds");

        assert_eq!(backend.operation_descriptors.len(), 3);
        assert_eq!(
            backend.operation_descriptors[1], attached.descriptor,
            "the preflight immediately before attach must bind the new connection"
        );
        assert_ne!(
            backend.operation_descriptors[1].host_fence.connection_epoch,
            backend.operation_descriptors[0].host_fence.connection_epoch
        );
        assert_eq!(
            backend.operation_descriptors[1].runtime_generation,
            attached.descriptor.runtime_generation
        );
        assert_eq!(
            backend.operation_descriptors[1].identity,
            attached.descriptor.identity
        );
    }

    #[test]
    fn host_request_lease_rotates_and_stale_lease_cannot_use_current_descriptor() {
        let mut state = BrowserHostState::new(std::env::temp_dir()).expect("browser host state");
        let mut backend = RecordingBackend::default();
        let issued = issue(&mut state, &mut backend);
        let stale_registration_lease = host_request(&state, &issued);
        let client_id = ClientId::new();
        let attached = state
            .attach_native_view_with_backend(
                BrowserAttachRequest::new(issued.descriptor.clone(), client_id),
                attach_destination(),
                &mut backend,
            )
            .unwrap();
        let attached_request = host_request(&state, &attached);
        assert!(
            attached_request.request_lease.request_epoch()
                > stale_registration_lease.request_lease.request_epoch()
        );
        assert_eq!(
            attached_request.request_lease.connection_epoch(),
            attached.descriptor.host_fence.connection_epoch
        );
        assert_ne!(
            attached_request.request_lease,
            stale_registration_lease.request_lease
        );
        let stale_after_attach = BrowserHostRequest::new(
            attached.descriptor.clone(),
            stale_registration_lease.request_lease.clone(),
        );
        assert_eq!(
            state.native_teardown_status(&stale_after_attach),
            Err(BrowserNativeViewError::HostRequestLeaseMismatch)
        );

        let current = attached_request;
        let parked = state
            .park_native_view_with_backend(current.clone(), &mut backend)
            .unwrap();
        let stale_after_park =
            BrowserHostRequest::new(parked.descriptor.clone(), current.request_lease);
        assert_eq!(
            state.native_teardown_status(&stale_after_park),
            Err(BrowserNativeViewError::HostRequestLeaseMismatch)
        );
    }

    #[test]
    fn connection_fences_are_global_monotonic_and_never_regress_when_parking() {
        let mut state = BrowserHostState::new(std::env::temp_dir()).expect("browser host state");
        let mut backend = RecordingBackend::default();
        let first = issue(&mut state, &mut backend);
        let second = issue(&mut state, &mut backend);
        let first_client = ClientId::new();
        let first_attached = state
            .attach_native_view_with_backend(
                BrowserAttachRequest::new(first.descriptor.clone(), first_client),
                attach_destination(),
                &mut backend,
            )
            .unwrap();
        let first_fence = first_attached.descriptor.host_fence.connection_epoch;
        let first_parked = state
            .park_native_view_with_backend(host_request(&state, &first_attached), &mut backend)
            .unwrap();
        assert_eq!(
            first_parked.descriptor.host_fence.connection_epoch,
            first_fence
        );

        let second_attached = state
            .attach_native_view_with_backend(
                BrowserAttachRequest::new(second.descriptor.clone(), ClientId::new()),
                attach_destination(),
                &mut backend,
            )
            .unwrap();
        let second_fence = second_attached.descriptor.host_fence.connection_epoch;
        assert!(second_fence > first_fence);
        let second_parked = state
            .park_native_view_with_backend(host_request(&state, &second_attached), &mut backend)
            .unwrap();
        let first_reattached = state
            .reattach_native_view_with_backend(
                BrowserAttachRequest::new(first_parked.descriptor, ClientId::new()),
                attach_destination(),
                &mut backend,
            )
            .unwrap();
        assert!(
            first_reattached.descriptor.host_fence.connection_epoch > second_fence,
            "parking an older view must not lower or reuse the global fence"
        );
        assert!(second_parked.descriptor.host_fence.connection_epoch >= second_fence);
    }

    #[test]
    fn controller_observation_requires_exact_current_identity_and_generation() {
        let (mut state, attached, client_id, mut backend) = attached_state();
        let capability = state
            .controller_capability(&attached.descriptor.identity)
            .unwrap();
        assert_eq!(
            state.observe_native_view_crash_with_backend(capability.clone(), &mut backend),
            Err(BrowserNativeViewError::ReconciliationRequired)
        );
        let unknown = state.native_view(&attached.descriptor.identity).unwrap();
        assert_eq!(
            unknown.reconciliation,
            BrowserNativeViewReconciliation::Unknown
        );
        assert!(matches!(
            unknown.lifecycle,
            BrowserSurfaceLifecycle::Attached { client_id: id } if id == client_id
        ));

        backend.allow_crash_observation = true;
        let calls_after_failure = backend.calls.len();
        assert_eq!(
            state.observe_native_view_crash_with_backend(capability, &mut backend),
            Err(BrowserNativeViewError::ReconciliationRequired),
            "an unverified observation cannot later become a proven detach"
        );
        assert!(
            backend.calls.len() == calls_after_failure,
            "blocked reconciliation must not invoke the backend again"
        );
    }

    #[test]
    fn partial_crash_observation_fails_closed_into_unknown_reconciliation() {
        let (mut state, attached, _client_id, mut backend) = attached_state();
        let capability = state
            .controller_capability(&attached.descriptor.identity)
            .unwrap();
        backend.crash_mutates_then_errors = true;

        assert_eq!(
            state.observe_native_view_crash_with_backend(capability, &mut backend),
            Err(BrowserNativeViewError::ReconciliationRequired)
        );
        let receipt = state.native_view(&attached.descriptor.identity).unwrap();
        assert_eq!(
            receipt.reconciliation,
            BrowserNativeViewReconciliation::Unknown
        );
        assert!(matches!(
            receipt.lifecycle,
            BrowserSurfaceLifecycle::Attached { .. }
        ));
        let request = host_request(&state, &receipt);
        assert_eq!(
            state.native_teardown_status(&request).unwrap(),
            BrowserTeardownStatus::Blocked(
                BrowserTeardownBlocker::NativeSurfaceReconciliationRequired
            )
        );
    }

    #[test]
    fn non_mutating_crash_observation_failure_is_unknown_and_blocks_later_claims() {
        let (mut state, attached, client_id, mut backend) = attached_state();
        let capability = state
            .controller_capability(&attached.descriptor.identity)
            .unwrap();

        assert_eq!(
            state.observe_native_view_crash_with_backend(capability, &mut backend),
            Err(BrowserNativeViewError::ReconciliationRequired)
        );
        let receipt = state.native_view(&attached.descriptor.identity).unwrap();
        assert_eq!(
            receipt.reconciliation,
            BrowserNativeViewReconciliation::Unknown
        );
        assert!(matches!(
            receipt.lifecycle,
            BrowserSurfaceLifecycle::Attached { client_id: ref id } if *id == client_id
        ));
        let request = host_request(&state, &receipt);
        assert_eq!(
            state.native_teardown_status(&request).unwrap(),
            BrowserTeardownStatus::Blocked(
                BrowserTeardownBlocker::NativeSurfaceReconciliationRequired
            )
        );
        assert_eq!(
            state.detach_native_view_with_backend(
                client_request(&attached, client_id),
                &mut backend,
            ),
            Err(BrowserNativeViewError::ReconciliationRequired),
            "an unverified crash must prevent a later proven detach claim"
        );
    }

    #[test]
    fn failed_host_request_is_consumed_after_rollback_restores_native_surface() {
        let (mut state, attached, _client_id, mut backend) = attached_state();
        let before = attached.clone();
        let request = host_request(&state, &attached);
        backend.partial_operation = Some("park");

        assert_eq!(
            state
                .park_native_view_with_backend(request.clone(), &mut backend)
                .expect_err("partial park failure must be visible"),
            BrowserNativeViewError::Backend
        );
        assert_eq!(
            state.native_view(&before.descriptor.identity).unwrap(),
            before,
            "rollback must leave the native authority state unchanged"
        );
        assert!(backend.actual_attached);
        assert_eq!(backend.actual_bounds, before.descriptor.physical_bounds);
        assert_eq!(backend.actual_focused, before.focused);
        assert_eq!(
            state.native_teardown_status(&request),
            Err(BrowserNativeViewError::HostRequestLeaseMismatch),
            "a failed host request is one-shot even after successful rollback"
        );
        let calls_after_failure = backend.calls.len();
        assert_eq!(
            state
                .park_native_view_with_backend(request, &mut backend)
                .expect_err("replaying a consumed host request must fail"),
            BrowserNativeViewError::HostRequestLeaseMismatch
        );
        assert_eq!(backend.calls.len(), calls_after_failure);
    }

    #[test]
    fn global_host_epoch_exhaustion_does_not_wrap_or_regress() {
        let previous = NEXT_NATIVE_HOST_EPOCH.swap(u64::MAX - 1, Ordering::Relaxed);
        let first = std::panic::catch_unwind(next_native_host_epoch);
        let exhausted = std::panic::catch_unwind(next_native_host_epoch);
        NEXT_NATIVE_HOST_EPOCH.store(previous, Ordering::Relaxed);

        assert!(first.is_ok());
        assert!(first.unwrap().is_ok());
        assert!(
            exhausted.is_ok(),
            "global host epoch exhaustion must return a typed failure instead of panicking"
        );
        assert!(exhausted.unwrap().is_err());
    }

    #[test]
    fn host_state_epoch_exhaustion_is_typed_and_does_not_panic() {
        let previous = NEXT_NATIVE_HOST_EPOCH.swap(u64::MAX - 1, Ordering::Relaxed);
        let outcome = std::panic::catch_unwind(|| BrowserHostState::new(std::env::temp_dir()));
        NEXT_NATIVE_HOST_EPOCH.store(previous, Ordering::Relaxed);

        assert!(
            outcome.is_ok(),
            "host construction must fail closed without panicking when epochs exhaust"
        );
        assert!(outcome.unwrap().is_err());
    }

    #[test]
    fn crash_observation_cannot_survive_detach_and_reattach() {
        let (mut state, attached, client_id, mut backend) = attached_state();
        let capability = state
            .controller_capability(&attached.descriptor.identity)
            .unwrap();
        let detached = state
            .detach_native_view_with_backend(client_request(&attached, client_id), &mut backend)
            .expect("detach succeeds");
        let reattached = state
            .reattach_native_view_with_backend(
                BrowserAttachRequest::new(detached.descriptor, ClientId::new()),
                attach_destination(),
                &mut backend,
            )
            .expect("reattach succeeds");
        assert_ne!(
            reattached.descriptor.host_fence.connection_epoch,
            capability.host_fence.connection_epoch
        );
        backend.allow_crash_observation = true;
        assert!(matches!(
            state.observe_native_view_crash_with_backend(capability, &mut backend),
            Err(BrowserNativeViewError::ControllerObservationMismatch(_))
        ));
        assert_eq!(
            state
                .native_view(&reattached.descriptor.identity)
                .expect("reattached view remains current")
                .lifecycle,
            reattached.lifecycle
        );
        assert_eq!(
            backend
                .calls
                .iter()
                .filter(|call| **call == "crash")
                .count(),
            0
        );
    }

    #[test]
    fn teardown_stays_blocked_without_real_runtime_observation() {
        let mut state = BrowserHostState::new(std::env::temp_dir()).expect("browser host state");
        let mut backend = RecordingBackend::default();
        let issued = issue(&mut state, &mut backend);
        let request = host_request(&state, &issued);

        assert_eq!(
            state.native_teardown_status(&request).unwrap(),
            BrowserTeardownStatus::Blocked(
                BrowserTeardownBlocker::RealRuntimeObservationUnavailable
            )
        );
        assert_eq!(
            state.close_native_context(request),
            Err(BrowserNativeViewError::TeardownBlocked(
                BrowserTeardownBlocker::RealRuntimeObservationUnavailable
            ))
        );
    }

    #[test]
    fn teardown_ready_only_after_backend_zero_residue_observation() {
        let mut state = BrowserHostState::new(std::env::temp_dir()).expect("browser host state");
        let mut backend = RecordingBackend::default();
        let issued = issue(&mut state, &mut backend);
        let request = host_request(&state, &issued);

        assert_eq!(
            state
                .native_teardown_status_with_backend(&request, &mut backend)
                .unwrap(),
            BrowserTeardownStatus::Blocked(
                BrowserTeardownBlocker::RealRuntimeObservationUnavailable
            ),
            "recording backend must not invent zero residue"
        );

        backend.allow_zero_residue = true;
        assert_eq!(
            state
                .native_teardown_status_with_backend(&request, &mut backend)
                .unwrap(),
            BrowserTeardownStatus::Ready
        );
        let closed = state
            .close_native_context_with_backend(request, &mut backend)
            .expect("observed zero residue may close");
        assert_eq!(closed.lifecycle, BrowserSurfaceLifecycle::Closed);
        assert!(
            backend.calls.iter().any(|call| *call == "teardown-residue"),
            "close must observe residue through the backend"
        );
    }

    #[test]
    fn host_owned_backend_binds_park_attach_and_rejects_unadmitted_hwnd() {
        let mut state = BrowserHostState::new(std::env::temp_dir()).expect("browser host state");
        let mut backend = HostOwnedNativeSurfaceBackend::new_synthetic_for_test();
        let child = BrowserWindowHandle::from_raw(0x5101).expect("child");
        let parking = BrowserWindowHandle::from_raw(0x5201).expect("parking");
        let bounds = BrowserPhysicalBounds::new(1, 2, 320, 240).expect("bounds");
        backend
            .admit_host_allocation(&child, &parking, bounds)
            .expect("admit live allocation");
        let registration = BrowserNativeViewRegistration::from_host_record(
            BrowserSurfaceIdentity {
                task_id: TaskId::new(),
                context_id: BrowserContextId::new(),
                resource_id: ResourceId::new(),
            },
            child.clone(),
            parking.clone(),
            BrowserHostProcessIdentity::new(41, 9_001, "C:\\DevManager\\devmanager-host.exe")
                .expect("host process"),
            bounds,
            BrowserDpi::new(96, 96).expect("dpi"),
        )
        .expect("registration");
        let issued = state
            .register_native_view_with_backend(registration, &mut backend)
            .expect("host-owned registration");
        let client_id = ClientId::new();
        let attached = state
            .attach_native_view_with_backend(
                BrowserAttachRequest::new(issued.descriptor.clone(), client_id),
                BrowserWindowHandle::from_raw(0x5501).expect("destination"),
                &mut backend,
            )
            .expect("attach");
        assert!(matches!(
            attached.lifecycle,
            BrowserSurfaceLifecycle::Attached { .. }
        ));
        assert_eq!(
            attached.attached_parent,
            Some(BrowserWindowHandle::from_raw(0x5501).expect("destination"))
        );
        let parked = state
            .park_native_view_with_backend(host_request(&state, &attached), &mut backend)
            .expect("park");
        assert_eq!(parked.lifecycle, BrowserSurfaceLifecycle::Parked);

        let mut cold = HostOwnedNativeSurfaceBackend::new();
        let foreign_child = BrowserWindowHandle::from_raw(0x5301).expect("foreign");
        let foreign_registration = BrowserNativeViewRegistration::from_host_record(
            BrowserSurfaceIdentity {
                task_id: TaskId::new(),
                context_id: BrowserContextId::new(),
                resource_id: ResourceId::new(),
            },
            foreign_child,
            BrowserWindowHandle::from_raw(0x5401).expect("foreign parking"),
            BrowserHostProcessIdentity::new(41, 9_001, "C:\\DevManager\\devmanager-host.exe")
                .expect("host process"),
            bounds,
            BrowserDpi::new(96, 96).expect("dpi"),
        )
        .expect("foreign registration");
        assert_eq!(
            state
                .register_native_view_with_backend(foreign_registration, &mut cold)
                .expect_err("unadmitted HWND must not mint authority"),
            BrowserNativeViewError::Backend
        );
    }

    #[test]
    fn host_owned_surface_proof_and_legacy_mcp_normalize_to_task_identity() {
        let mut state = BrowserHostState::new(std::env::temp_dir()).expect("browser host state");
        let mut backend = HostOwnedNativeSurfaceBackend::new_synthetic_for_test();
        let child = BrowserWindowHandle::from_raw(0x6101).expect("child");
        let parking = BrowserWindowHandle::from_raw(0x6201).expect("parking");
        let bounds = BrowserPhysicalBounds::new(0, 0, 100, 80).expect("bounds");
        backend
            .admit_host_allocation(&child, &parking, bounds)
            .expect("admit");
        let identity = BrowserSurfaceIdentity {
            task_id: TaskId::new(),
            context_id: BrowserContextId::new(),
            resource_id: ResourceId::new(),
        };
        let registration = BrowserNativeViewRegistration::from_host_record(
            identity,
            child,
            parking,
            BrowserHostProcessIdentity::new(77, 1_001, "C:\\DevManager\\host.exe").expect("proc"),
            bounds,
            BrowserDpi::new(96, 96).expect("dpi"),
        )
        .expect("registration");
        let issued = state
            .register_native_view_with_backend(registration, &mut backend)
            .expect("register");
        assert_eq!(
            state.host_owned_surface_proof(&issued.descriptor.identity),
            Err(BrowserNativeViewError::LiveWryObservationUnavailable),
            "copied host-state descriptor is not live Wry proof"
        );
        assert_eq!(
            state.normalize_legacy_mcp_task_surface(issued.descriptor.identity.task_id),
            Some(issued.descriptor.identity)
        );
        assert_eq!(
            state.normalize_legacy_mcp_task_surface(TaskId::new()),
            None,
            "cross-task MCP must not inherit a foreign surface"
        );
        assert_eq!(
            state.require_legacy_mcp_normalized_surface(None),
            Err(LegacyMcpTaskSurfaceBlocker::WorkspaceCommandLacksTaskId)
        );
        assert_eq!(
            state.require_legacy_mcp_normalized_surface(Some(issued.descriptor.identity.task_id)),
            Err(LegacyMcpTaskSurfaceBlocker::CrossTaskOrMissingSurface),
            "live surface requires TaskId+ContextId+ResourceId, not task-only inference"
        );
        assert_eq!(
            state.require_legacy_mcp_normalized_surface(Some(TaskId::new())),
            Err(LegacyMcpTaskSurfaceBlocker::CrossTaskOrMissingSurface)
        );
    }

    #[test]
    fn host_owned_teardown_requires_closed_controller_zero_residue() {
        let mut state = BrowserHostState::new(std::env::temp_dir()).expect("browser host state");
        let mut backend = HostOwnedNativeSurfaceBackend::new_synthetic_for_test();
        let child = BrowserWindowHandle::from_raw(0x7101).expect("child");
        let parking = BrowserWindowHandle::from_raw(0x7201).expect("parking");
        let bounds = BrowserPhysicalBounds::new(0, 0, 120, 90).expect("bounds");
        backend
            .admit_host_allocation(&child, &parking, bounds)
            .expect("admit");
        let registration = BrowserNativeViewRegistration::from_host_record(
            BrowserSurfaceIdentity {
                task_id: TaskId::new(),
                context_id: BrowserContextId::new(),
                resource_id: ResourceId::new(),
            },
            child.clone(),
            parking,
            BrowserHostProcessIdentity::new(88, 2_002, "C:\\DevManager\\host.exe").expect("proc"),
            bounds,
            BrowserDpi::new(96, 96).expect("dpi"),
        )
        .expect("registration");
        let issued = state
            .register_native_view_with_backend(registration, &mut backend)
            .expect("register");
        let request = host_request(&state, &issued);
        assert_eq!(
            state
                .native_teardown_status_with_backend(&request, &mut backend)
                .unwrap(),
            BrowserTeardownStatus::Blocked(
                BrowserTeardownBlocker::RealRuntimeObservationUnavailable
            )
        );
        backend
            .mark_controller_closed(&child)
            .expect("controller closed");
        assert_eq!(
            state
                .native_teardown_status_with_backend(&request, &mut backend)
                .unwrap(),
            BrowserTeardownStatus::Ready
        );
        let closed = state
            .close_native_context_with_backend(request, &mut backend)
            .expect("zero residue close");
        assert_eq!(closed.lifecycle, BrowserSurfaceLifecycle::Closed);
    }

    #[test]
    fn production_host_owned_backend_cannot_admit_or_teardown_synthetic_hwnd() {
        let mut production = HostOwnedNativeSurfaceBackend::new();
        let child = BrowserWindowHandle::from_raw(0x7501).expect("child");
        let parking = BrowserWindowHandle::from_raw(0x7601).expect("parking");
        let bounds = BrowserPhysicalBounds::new(0, 0, 20, 20).expect("bounds");
        assert_eq!(
            production.admit_host_allocation(&child, &parking, bounds),
            Err(HostOwnedSurfaceBindError::LiveWindowRequired)
        );
        assert!(production.park_surface(&child, &parking).is_err());
    }

    #[test]
    fn host_owned_teardown_blocks_when_helper_residue_remains() {
        let mut state = BrowserHostState::new(std::env::temp_dir()).expect("browser host state");
        let mut backend = HostOwnedNativeSurfaceBackend::new_synthetic_for_test();
        let child = BrowserWindowHandle::from_raw(0x7701).expect("child");
        let parking = BrowserWindowHandle::from_raw(0x7801).expect("parking");
        let bounds = BrowserPhysicalBounds::new(0, 0, 40, 30).expect("bounds");
        backend
            .admit_host_allocation(&child, &parking, bounds)
            .expect("admit");
        let registration = BrowserNativeViewRegistration::from_host_record(
            BrowserSurfaceIdentity {
                task_id: TaskId::new(),
                context_id: BrowserContextId::new(),
                resource_id: ResourceId::new(),
            },
            child.clone(),
            parking,
            BrowserHostProcessIdentity::new(88, 2_003, "C:\\DevManager\\host.exe").expect("proc"),
            bounds,
            BrowserDpi::new(96, 96).expect("dpi"),
        )
        .expect("registration");
        let issued = state
            .register_native_view_with_backend(registration, &mut backend)
            .expect("register");
        let request = host_request(&state, &issued);
        backend
            .mark_controller_closed(&child)
            .expect("controller closed");
        backend.inject_helper_residue_for_test(&child, 2);
        assert_eq!(
            state
                .native_teardown_status_with_backend(&request, &mut backend)
                .unwrap(),
            BrowserTeardownStatus::Blocked(
                BrowserTeardownBlocker::RealRuntimeObservationUnavailable
            )
        );
    }

    #[test]
    fn exact_mcp_binding_rejects_cross_task_context_and_resource() {
        let mut state = BrowserHostState::new(std::env::temp_dir()).expect("browser host state");
        let mut backend = HostOwnedNativeSurfaceBackend::new_synthetic_for_test();
        let child = BrowserWindowHandle::from_raw(0x7901).expect("child");
        let parking = BrowserWindowHandle::from_raw(0x7a01).expect("parking");
        let bounds = BrowserPhysicalBounds::new(0, 0, 40, 30).expect("bounds");
        backend
            .admit_host_allocation(&child, &parking, bounds)
            .expect("admit");
        let identity = BrowserSurfaceIdentity {
            task_id: TaskId::new(),
            context_id: BrowserContextId::new(),
            resource_id: ResourceId::new(),
        };
        let registration = BrowserNativeViewRegistration::from_host_record(
            identity,
            child,
            parking,
            BrowserHostProcessIdentity::new(91, 3_003, "C:\\DevManager\\host.exe").expect("proc"),
            bounds,
            BrowserDpi::new(96, 96).expect("dpi"),
        )
        .expect("registration");
        let issued = state
            .register_native_view_with_backend(registration, &mut backend)
            .expect("register");
        let live = issued.descriptor.identity;
        assert_eq!(
            state.require_legacy_mcp_exact_binding(Some((
                live.task_id,
                Some(live.context_id),
                Some(live.resource_id)
            ))),
            Ok(())
        );
        assert_eq!(
            state.require_legacy_mcp_exact_binding(Some((
                TaskId::new(),
                Some(live.context_id),
                Some(live.resource_id)
            ))),
            Err(LegacyMcpTaskSurfaceBlocker::CrossTaskOrMissingSurface)
        );
        assert_eq!(
            state.require_legacy_mcp_exact_binding(Some((
                live.task_id,
                Some(BrowserContextId::new()),
                Some(live.resource_id)
            ))),
            Err(LegacyMcpTaskSurfaceBlocker::CrossTaskOrMissingSurface)
        );
        assert_eq!(
            state.require_legacy_mcp_exact_binding(Some((
                live.task_id,
                Some(live.context_id),
                Some(ResourceId::new())
            ))),
            Err(LegacyMcpTaskSurfaceBlocker::CrossTaskOrMissingSurface)
        );
        assert_eq!(
            state.require_legacy_mcp_exact_binding(Some((live.task_id, None, None))),
            Err(LegacyMcpTaskSurfaceBlocker::CrossTaskOrMissingSurface)
        );
    }

    #[test]
    fn legacy_mcp_without_bound_surface_stays_available() {
        let state = BrowserHostState::new(std::env::temp_dir()).expect("browser host state");
        assert_eq!(state.require_legacy_mcp_normalized_surface(None), Ok(()));
    }

    #[test]
    fn receipts_and_internal_requests_do_not_debug_expose_host_capabilities() {
        let mut state = BrowserHostState::new(std::env::temp_dir()).expect("browser host state");
        let mut backend = RecordingBackend::default();
        let issued = issue(&mut state, &mut backend);
        let receipt_debug = format!("{issued:?}");
        assert!(!receipt_debug.contains("BrowserHostRequestLease"));
        assert!(receipt_debug.contains("redacted"));

        let request_debug = format!("{:?}", host_request(&state, &issued));
        assert!(request_debug.contains("redacted"));
        let capability_debug = format!(
            "{:?}",
            state
                .controller_capability(&issued.descriptor.identity)
                .unwrap()
        );
        assert_eq!(capability_debug, "BrowserControllerCapability(<redacted>)");
    }
}

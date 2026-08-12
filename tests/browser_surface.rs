//! Portable browser surface contract suite.
//!
//! These tests exercise the public `BrowserSurfaceHost` / fixture / dock
//! model. They do not launch WebView2, GPUI, installed DevManager, or a
//! stock provider. Windows/WebView2 capability is labeled separately and
//! is not a passing visible-surface proof.

use devmanager::browser::{
    BoundsEpoch, BrowserDockChrome, BrowserDockFocusTarget, BrowserDockGesture, BrowserDockSurface,
    BrowserPointerDisposition, BrowserSurfaceDescriptor, BrowserSurfaceFixture,
    BrowserSurfaceFixtureError, BrowserSurfaceHost, BrowserSurfaceIdentity,
    BrowserSurfaceRegistration, ClientBinding, DpiScale, FocusEpoch, HostHwndOwnership,
    HostSurfaceRequest, HostTeardownProof, PhysicalBounds, ProcessIdentity, RuntimeGeneration,
    SurfaceAction, SurfaceAttachRequest, SurfaceAuthority, SurfaceBoundsUpdate,
    SurfaceClientRequest, SurfaceCommand, SurfaceDescriptorField, SurfaceDetachReason,
    SurfaceError, SurfaceEventKind, SurfaceFocusUpdate, SurfaceInputAction, SurfaceInputRequest,
    SurfaceLifecycle, SurfaceNonce, SurfaceOwner, SurfaceParkReason, SurfacePermission,
    SurfaceTaskSwitchRequest, SurfaceTeardownReason, SurfaceWindowHandle,
    BROWSER_SURFACE_FIXTURE_CLICK_TOKEN, BROWSER_SURFACE_FIXTURE_RETAINED_STATE,
    BROWSER_SURFACE_FIXTURE_VISIBLE_TOKEN,
};
use devmanager::domain::{
    AgentSessionId, BrowserContextId, BrowserTabId, ClientId, RequestId, ResourceId, TaskId,
};
use devmanager::protocol::{
    BrowserAttachRequest, BrowserDpi, BrowserHostFence, BrowserHostProcessIdentity,
    BrowserPhysicalBounds, BrowserRuntimeGeneration,
    BrowserSurfaceDescriptor as ProtocolDescriptor, BrowserSurfaceIdentity as ProtocolIdentity,
    BrowserSurfaceLifecycle, BrowserWindowHandle,
};
use std::path::PathBuf;

fn host_process() -> ProcessIdentity {
    ProcessIdentity::new(4_241, 1_700_000_000, "devmanager-host").expect("host process")
}

fn client_process(pid: u32) -> ProcessIdentity {
    ProcessIdentity::new(pid, 1_800_000_000 + u64::from(pid), "devmanager").expect("client process")
}

fn hwnd(raw: u64) -> SurfaceWindowHandle {
    SurfaceWindowHandle::from_raw(raw).expect("nonzero hwnd")
}

fn ownership(child: u64, parking: u64) -> HostHwndOwnership {
    HostHwndOwnership::new(hwnd(child), hwnd(parking), true, true).expect("hwnd ownership")
}

fn identity() -> BrowserSurfaceIdentity {
    BrowserSurfaceIdentity::new(
        TaskId::new(),
        AgentSessionId::new(),
        BrowserContextId::new(),
        ResourceId::new(),
    )
}

fn bounds(width: u32, height: u32) -> PhysicalBounds {
    PhysicalBounds::new(0, 0, width, height).expect("bounds")
}

fn dpi(percent: u16) -> DpiScale {
    DpiScale::new(percent).expect("dpi")
}

fn nonce(seed: u8) -> SurfaceNonce {
    let mut bytes = [seed; 16];
    bytes[0] = seed.max(1);
    SurfaceNonce::new(bytes).expect("nonce")
}

fn register(
    host: &mut BrowserSurfaceHost,
    identity: BrowserSurfaceIdentity,
    child: u64,
    parking: u64,
) -> BrowserSurfaceDescriptor {
    host.register(BrowserSurfaceRegistration {
        identity,
        hwnd_ownership: ownership(child, parking),
        nonce: nonce((child % 250) as u8 + 1),
        runtime_generation: RuntimeGeneration::initial(),
        physical_bounds: bounds(800, 600),
        dpi: dpi(100),
    })
    .expect("register surface")
}

fn attach(
    host: &mut BrowserSurfaceHost,
    descriptor: BrowserSurfaceDescriptor,
    client: ClientBinding,
) -> BrowserSurfaceDescriptor {
    host.attach(SurfaceAttachRequest { descriptor, client })
        .expect("attach")
        .descriptor
}

fn protocol_descriptor(task_id: TaskId) -> ProtocolDescriptor {
    let json = serde_json::json!({
        "identity": {
            "task_id": task_id,
            "context_id": BrowserContextId::new(),
            "resource_id": ResourceId::new(),
        },
        "childHwnd": "hwnd:4096",
        "hostProcess": {
            "pid": 4241,
            "creationTime100ns": 1_700_000_000u64,
            "executable": "devmanager-host"
        },
        "hostFence": { "bootEpoch": 1, "connectionEpoch": 1 },
        "runtimeGeneration": 1,
        "nonce": [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
        "boundsEpoch": 1,
        "focusEpoch": 1,
        "physicalBounds": { "x": 0, "y": 0, "width": 800, "height": 600 },
        "dpi": { "horizontal": 96, "vertical": 96 }
    });
    serde_json::from_value(json).expect("protocol descriptor")
}

fn manifest_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
}

#[test]
fn opaque_window_and_process_identity_reject_zero_values() {
    assert!(ProcessIdentity::new(0, 1, "devmanager-host").is_err());
    assert!(ProcessIdentity::new(1, 0, "devmanager-host").is_err());
    assert!(ProcessIdentity::new(1, 1, "").is_err());
    assert!(SurfaceWindowHandle::from_raw(0).is_err());
    assert!(SurfaceWindowHandle::from_wire("0").is_err());
    assert!(SurfaceWindowHandle::from_wire("hwnd:0").is_err());
    let handle = hwnd(0xBEEF);
    assert_eq!(handle.wire_value(), "hwnd:48879");
    assert_ne!(handle.raw_value(), 0);
    let process = host_process();
    assert_ne!(process.pid(), 0);
    assert_ne!(process.creation_time_100ns(), 0);
}

#[test]
fn host_registration_is_parked_host_owned_and_denies_automation() {
    let mut host = BrowserSurfaceHost::new(host_process());
    let identity = identity();
    let descriptor = register(&mut host, identity, 0x1001, 0x2001);
    assert_eq!(descriptor.owner(), SurfaceOwner::Host);
    assert!(!descriptor.allows_automation());
    assert!(descriptor.allows(SurfacePermission::Attach));
    assert_eq!(
        descriptor.authority(),
        SurfaceAuthority {
            task_id: identity.task_id,
            session_id: identity.session_id,
            runtime_generation: RuntimeGeneration::initial(),
        }
    );
    let snapshot = host.snapshot(identity.resource_id).expect("snapshot");
    assert_eq!(
        snapshot.lifecycle,
        SurfaceLifecycle::Parked {
            reason: SurfaceParkReason::Initial
        }
    );
    assert!(!snapshot.active);
    assert!(snapshot.context_retained);
    assert_eq!(
        host.parking_hwnd(identity.resource_id)
            .map(|h| h.raw_value()),
        Some(0x2001)
    );
    assert_ne!(
        host.parking_hwnd(identity.resource_id)
            .expect("parking")
            .raw_value(),
        descriptor.child_hwnd.raw_value()
    );
    assert!(!BrowserSurfaceHost::HOT_PATH_PROBES_PERMITTED);
    assert!(!host.hot_path_probed());
}

#[test]
fn attach_park_detach_and_reattach_advance_epochs_and_retain_context() {
    let mut host = BrowserSurfaceHost::new(host_process());
    let identity = identity();
    let registered = register(&mut host, identity, 0x1010, 0x2010);
    let client = ClientBinding::new(ClientId::new(), client_process(77));
    let attached = attach(&mut host, registered.clone(), client.clone());
    assert!(attached.bounds_epoch > registered.bounds_epoch);
    assert!(attached.focus_epoch > registered.focus_epoch);
    assert_eq!(
        host.snapshot(identity.resource_id)
            .expect("attached snapshot")
            .lifecycle,
        SurfaceLifecycle::Attached {
            client_id: client.id
        }
    );
    assert_eq!(host.active_resource_id(), Some(identity.resource_id));

    let parked = host
        .park(HostSurfaceRequest {
            descriptor: attached.clone(),
        })
        .expect("park")
        .descriptor;
    assert_eq!(
        host.snapshot(identity.resource_id)
            .expect("parked snapshot")
            .lifecycle,
        SurfaceLifecycle::Parked {
            reason: SurfaceParkReason::Explicit
        }
    );
    assert!(
        host.snapshot(identity.resource_id)
            .unwrap()
            .context_retained
    );

    let attached_again = attach(&mut host, parked, client.clone());
    let detached = host
        .detach(SurfaceClientRequest {
            descriptor: attached_again,
            client_id: client.id,
        })
        .expect("detach");
    assert_eq!(
        detached.lifecycle,
        SurfaceLifecycle::Detached {
            reason: SurfaceDetachReason::ClientRequested
        }
    );
    let reattached = host
        .reattach(SurfaceAttachRequest {
            descriptor: detached.descriptor,
            client,
        })
        .expect("reattach");
    assert!(matches!(
        reattached.lifecycle,
        SurfaceLifecycle::Attached { .. }
    ));
    assert!(
        host.snapshot(identity.resource_id)
            .unwrap()
            .context_retained
    );
}

#[test]
fn task_and_client_authority_reject_cross_task_and_foreign_client() {
    let mut host = BrowserSurfaceHost::new(host_process());
    let first = identity();
    let mut second = identity();
    second.task_id = TaskId::new();
    let first_desc = register(&mut host, first, 0x1100, 0x2100);
    let second_desc = register(&mut host, second, 0x1101, 0x2101);
    let client_a = ClientBinding::new(ClientId::new(), client_process(11));
    let client_b = ClientBinding::new(ClientId::new(), client_process(12));
    let attached = attach(&mut host, first_desc, client_a.clone());
    let conflict = host.attach(SurfaceAttachRequest {
        descriptor: second_desc.clone(),
        client: client_b.clone(),
    });
    assert!(matches!(
        conflict,
        Err(SurfaceError::ActiveSurfaceConflict { .. })
    ));
    let mismatch = host.detach(SurfaceClientRequest {
        descriptor: attached.clone(),
        client_id: client_b.id,
    });
    assert!(matches!(mismatch, Err(SurfaceError::ClientMismatch { .. })));
    assert_eq!(host.task_surface(first.task_id), Some(first.resource_id));
    assert_eq!(host.task_surface(second.task_id), Some(second.resource_id));
}

#[test]
fn stale_descriptor_and_epoch_updates_are_rejected() {
    let mut host = BrowserSurfaceHost::new(host_process());
    let identity = identity();
    let registered = register(&mut host, identity, 0x1200, 0x2200);
    let client = ClientBinding::new(ClientId::new(), client_process(33));
    let attached = attach(&mut host, registered.clone(), client.clone());
    let stale_attach = host.attach(SurfaceAttachRequest {
        descriptor: registered,
        client: client.clone(),
    });
    assert!(matches!(
        stale_attach,
        Err(SurfaceError::StaleDescriptor {
            field: SurfaceDescriptorField::BoundsEpoch | SurfaceDescriptorField::FocusEpoch
        })
    ));
    let mut stale_bounds = attached.clone();
    stale_bounds.bounds_epoch = BoundsEpoch::initial();
    let bounds_err = host.receive_bounds(SurfaceBoundsUpdate {
        descriptor: stale_bounds,
        client_id: client.id,
        client_sequence: 9,
        physical_bounds: bounds(640, 480),
        dpi: dpi(125),
    });
    assert!(matches!(
        bounds_err,
        Err(SurfaceError::StaleDescriptor {
            field: SurfaceDescriptorField::BoundsEpoch
        })
    ));
    let mut stale_focus = attached.clone();
    stale_focus.focus_epoch = FocusEpoch::initial();
    let focus_err = host.receive_focus(SurfaceFocusUpdate {
        descriptor: stale_focus,
        client_id: client.id,
        client_sequence: 9,
        focused: true,
    });
    assert!(matches!(
        focus_err,
        Err(SurfaceError::StaleDescriptor {
            field: SurfaceDescriptorField::FocusEpoch
        })
    ));
}

#[test]
fn dpi_and_bounds_matrix_updates_physical_geometry() {
    let mut host = BrowserSurfaceHost::new(host_process());
    let identity = identity();
    let registered = register(&mut host, identity, 0x1300, 0x2300);
    let client = ClientBinding::new(ClientId::new(), client_process(44));
    let mut descriptor = attach(&mut host, registered, client.clone());
    for (percent, width, height) in [
        (100, 800, 600),
        (125, 1000, 750),
        (150, 1200, 900),
        (200, 1600, 1200),
    ] {
        let receipt = host
            .receive_bounds(SurfaceBoundsUpdate {
                descriptor,
                client_id: client.id,
                client_sequence: u64::from(percent),
                physical_bounds: bounds(width, height),
                dpi: dpi(percent),
            })
            .expect("dpi matrix bounds");
        assert_eq!(receipt.descriptor.dpi.scale_percent(), percent);
        assert_eq!(receipt.descriptor.physical_bounds.width, width);
        assert_eq!(receipt.descriptor.physical_bounds.height, height);
        descriptor = receipt.descriptor;
    }
}

#[test]
fn shell_task_switch_consumes_pointer_and_requires_later_focus_for_page_input() {
    let mut host = BrowserSurfaceHost::new(host_process());
    let outgoing_id = identity();
    let mut incoming_id = identity();
    incoming_id.task_id = TaskId::new();
    let outgoing = register(&mut host, outgoing_id, 0x1400, 0x2400);
    let incoming = register(&mut host, incoming_id, 0x1401, 0x2401);
    let client = ClientBinding::new(ClientId::new(), client_process(55));
    let outgoing = attach(&mut host, outgoing, client.clone());
    let switched = host
        .task_switch(SurfaceTaskSwitchRequest {
            outgoing,
            incoming,
            client: client.clone(),
        })
        .expect("task switch");
    assert!(switched.pointer_consumed);
    assert_eq!(
        switched.outgoing.lifecycle,
        SurfaceLifecycle::Parked {
            reason: SurfaceParkReason::TaskSwitch
        }
    );
    assert!(matches!(
        switched.incoming.lifecycle,
        SurfaceLifecycle::Attached { .. }
    ));
    let input = host.receive_input(SurfaceInputRequest {
        descriptor: switched.incoming.descriptor.clone(),
        client_id: client.id,
        action: SurfaceInputAction::TrustedClick {
            x: 10,
            y: 10,
            target_token: BROWSER_SURFACE_FIXTURE_CLICK_TOKEN.to_string(),
        },
    });
    assert!(matches!(input, Err(SurfaceError::InputRequiresFocus)));
}

#[test]
fn fixture_state_is_retained_across_trusted_click_text_and_resize() {
    let mut fixture = BrowserSurfaceFixture::new();
    let before = fixture.snapshot();
    assert_eq!(before.visible_token, BROWSER_SURFACE_FIXTURE_VISIBLE_TOKEN);
    assert_eq!(
        before.retained_state,
        BROWSER_SURFACE_FIXTURE_RETAINED_STATE
    );
    assert!(before.trusted_click_token.is_none());
    fixture
        .trusted_click(BROWSER_SURFACE_FIXTURE_CLICK_TOKEN)
        .expect("trusted click");
    fixture.text_input("hello fixture").expect("text");
    fixture.resize(bounds(1024, 768), dpi(150));
    let after = fixture.snapshot();
    assert_eq!(after.visible_token, before.visible_token);
    assert_eq!(after.retained_state, before.retained_state);
    assert_eq!(
        after.trusted_click_token.as_deref(),
        Some(BROWSER_SURFACE_FIXTURE_CLICK_TOKEN)
    );
    assert_eq!(after.text_value, "hello fixture");
    assert_eq!(after.resize_token, "dm-surface-resize-1024x768@150");
    assert_eq!(
        fixture.trusted_click("wrong-token"),
        Err(BrowserSurfaceFixtureError::UnexpectedClickToken)
    );
}

#[test]
fn teardown_proof_requires_zero_helpers_and_leaves_no_live_task_surface() {
    let mut host = BrowserSurfaceHost::new(host_process());
    let identity = identity();
    let registered = register(&mut host, identity, 0x1500, 0x2500);
    let parked = host
        .park(HostSurfaceRequest {
            descriptor: registered.clone(),
        })
        .expect("park before close");
    let rejected = host.close_context(HostTeardownProof {
        descriptor: parked.descriptor.clone(),
        host_process: host_process(),
        surface_parked: true,
        controller_closed: true,
        environment_closed: true,
        helper_processes_remaining: 2,
        context_closed: true,
        reason: SurfaceTeardownReason::ContextClosed,
    });
    assert!(matches!(
        rejected,
        Err(SurfaceError::InvalidTeardownProof { .. })
    ));
    let closed = host
        .close_context(HostTeardownProof {
            descriptor: parked.descriptor,
            host_process: host_process(),
            surface_parked: true,
            controller_closed: true,
            environment_closed: true,
            helper_processes_remaining: 0,
            context_closed: true,
            reason: SurfaceTeardownReason::ContextClosed,
        })
        .expect("close context");
    assert!(matches!(
        closed.lifecycle,
        SurfaceLifecycle::Terminal {
            reason: SurfaceTeardownReason::ContextClosed
        }
    ));
    let snapshot = host.snapshot(identity.resource_id).expect("terminal");
    assert!(snapshot.is_terminal());
    assert!(!snapshot.context_retained);
    assert_eq!(host.task_surface(identity.task_id), None);
    assert_eq!(host.active_resource_id(), None);
}

#[test]
fn client_crash_detaches_and_allows_reattach_to_a_new_client() {
    let mut host = BrowserSurfaceHost::new(host_process());
    let identity = identity();
    let registered = register(&mut host, identity, 0x1600, 0x2600);
    let client = ClientBinding::new(ClientId::new(), client_process(88));
    let _ = attach(&mut host, registered, client.clone());
    let receipts = host.client_crashed(client.clone()).expect("client crash");
    assert_eq!(receipts.len(), 1);
    assert_eq!(
        receipts[0].lifecycle,
        SurfaceLifecycle::Detached {
            reason: SurfaceDetachReason::ClientCrashed
        }
    );
    let replacement = ClientBinding::new(ClientId::new(), client_process(89));
    let reattached = host
        .reattach(SurfaceAttachRequest {
            descriptor: receipts[0].descriptor.clone(),
            client: replacement,
        })
        .expect("reattach after crash");
    assert!(matches!(
        reattached.lifecycle,
        SurfaceLifecycle::Attached { .. }
    ));
}

#[test]
fn host_shutdown_requires_parked_surface_and_zero_helpers() {
    let mut host = BrowserSurfaceHost::new(host_process());
    let identity = identity();
    let registered = register(&mut host, identity, 0x1650, 0x2650);
    let client = ClientBinding::new(ClientId::new(), client_process(90));
    let attached = attach(&mut host, registered, client);
    let parked = host
        .park(HostSurfaceRequest {
            descriptor: attached,
        })
        .expect("park before host shutdown");

    let closed = host
        .close_context(HostTeardownProof {
            descriptor: parked.descriptor,
            host_process: host_process(),
            surface_parked: true,
            controller_closed: true,
            environment_closed: true,
            helper_processes_remaining: 0,
            context_closed: true,
            reason: SurfaceTeardownReason::HostShutdown,
        })
        .expect("host shutdown proof");
    assert_eq!(
        closed.lifecycle,
        SurfaceLifecycle::Terminal {
            reason: SurfaceTeardownReason::HostShutdown
        }
    );
    assert_eq!(host.task_surface(identity.task_id), None);
    assert_eq!(host.active_resource_id(), None);
}

#[test]
fn apply_action_rejects_automation_and_duplicate_request_ids() {
    let mut host = BrowserSurfaceHost::new(host_process());
    let identity = identity();
    let descriptor = register(&mut host, identity, 0x1700, 0x2700);
    let request_id = RequestId::new();
    let client = ClientBinding::new(ClientId::new(), client_process(91));
    let first = host
        .apply_action(SurfaceCommand {
            request_id,
            descriptor: descriptor.clone(),
            action: SurfaceAction::Attach {
                client: client.clone(),
            },
        })
        .expect("first action");
    assert_eq!(first.kind, SurfaceEventKind::Attached);
    let duplicate = host.apply_action(SurfaceCommand {
        request_id,
        descriptor: first.descriptor.clone(),
        action: SurfaceAction::Park,
    });
    assert!(matches!(
        duplicate,
        Err(SurfaceError::DuplicateRequest { .. })
    ));
    let automate = host.apply_action(SurfaceCommand {
        request_id: RequestId::new(),
        descriptor: first.descriptor,
        action: SurfaceAction::Automate,
    });
    assert!(matches!(
        automate,
        Err(SurfaceError::AutomationSeparatelyAuthorized)
    ));
}

#[test]
fn dock_chrome_is_native_and_shell_gestures_are_consumed_until_page_is_armed() {
    let task_id = TaskId::new();
    let mut dock = BrowserDockSurface::from_descriptor(protocol_descriptor(task_id))
        .expect("dock from descriptor");
    assert!(!BrowserDockSurface::uses_web_chrome());
    assert_eq!(
        BrowserDockSurface::required_chrome(),
        &[
            BrowserDockChrome::Tabs,
            BrowserDockChrome::Address,
            BrowserDockChrome::Status,
            BrowserDockChrome::Approvals,
            BrowserDockChrome::Artifacts,
            BrowserDockChrome::Diagnostics,
        ]
    );
    dock.attach(BrowserAttachRequest::new(
        protocol_descriptor(task_id),
        ClientId::new(),
    ))
    .expect("dock attach");
    assert!(matches!(
        dock.lifecycle(),
        BrowserSurfaceLifecycle::Attached { .. }
    ));
    assert_eq!(
        dock.classify_gesture(BrowserDockGesture::TaskSwitch),
        BrowserPointerDisposition::ConsumeShellGesture
    );
    assert_eq!(
        dock.classify_gesture(BrowserDockGesture::PageClick),
        BrowserPointerDisposition::ConsumeShellGesture
    );
    dock.arm_page_input_after_gesture().expect("arm page");
    assert_eq!(dock.focus_target(), BrowserDockFocusTarget::BrowserPage);
    assert_eq!(
        dock.classify_gesture(BrowserDockGesture::PageClick),
        BrowserPointerDisposition::ForwardToPage
    );
    dock.select_tab(BrowserTabId::new()).expect("select tab");
    assert_eq!(
        dock.admit_page_input(
            dock.generation(),
            dock.bounds_epoch(),
            dock.focus_epoch(),
            BrowserDockGesture::PageClick
        ),
        Err(devmanager::browser::BrowserDockError::PointerConsumed)
    );
}

#[test]
fn contract_fixture_server_source_never_launches_webview_or_installed_app() {
    let source = include_str!("../src/bin/browser-fixture-server.rs");
    assert!(source.contains("BROWSER_FIXTURE_SERVER_READY"));
    assert!(source.contains("/health"));
    assert!(source.contains("127.0.0.1"));
    assert!(source.contains("path traversal") || source.contains(".."));
    assert!(source.contains("fn validate_root"));
    assert!(source.contains("--isolated-parent"));
    assert!(source.contains("MAX_REQUEST_LINE_BYTES"));
    assert!(source.contains("MAX_HEADER_BYTES"));
    assert!(source.contains("MAX_BODY_BYTES"));
    assert!(!source.to_ascii_lowercase().contains("webview2"));
    assert!(!source.contains("Start-Process"));
    assert!(!source.contains("claude"));
    assert!(!source.contains("codex"));
    assert!(!source.contains("cursor"));
}

#[test]
fn contract_surface_proof_script_exists_and_stays_local() {
    let script = std::fs::read_to_string(manifest_path(
        "scripts/native-next/Invoke-BrowserSurfaceProof.ps1",
    ))
    .expect("surface proof script");
    assert!(script.contains("Set-StrictMode"));
    assert!(script.contains("-Stage"));
    assert!(script.contains("Red"));
    assert!(script.contains("Green"));
    assert!(script.contains("OutputDir"));
    assert!(script.contains("AllDpi"));
    assert!(script.contains("ClientCrash"));
    assert!(script.contains("HostRecovery"));
    assert!(script.contains("CARGO_TARGET_DIR"));
    assert!(script.contains("DEVMANAGER_PROFILE"));
    assert!(!script.contains("Start-Process"));
    assert!(!script
        .to_ascii_lowercase()
        .contains("com.userfirst.devmanager"));
}

#[cfg(windows)]
#[test]
fn windows_webview2_capability_is_labeled_and_not_claimed_by_portable_host() {
    let host_source = include_str!("../src/browser/host/windows.rs");
    assert!(
        host_source.to_ascii_lowercase().contains("webview2"),
        "Windows host module exists as capability evidence only"
    );
    let surface_source = include_str!("../src/browser/surface.rs");
    assert!(surface_source.contains("deliberately does not create a WebView2 controller"));
    assert!(
        !surface_source.contains("CreateCoreWebView2"),
        "portable surface host must not create a WebView2 controller"
    );
}

#[test]
fn protocol_window_handle_and_host_process_stay_opaque_and_nonzero() {
    assert!(BrowserWindowHandle::from_raw(0).is_err());
    let handle = BrowserWindowHandle::from_raw(0x1000).expect("hwnd");
    assert_eq!(handle.wire_value(), "hwnd:4096");
    assert!(BrowserHostProcessIdentity::new(0, 1, "devmanager-host").is_err());
    let process =
        BrowserHostProcessIdentity::new(8, 22, "devmanager-host").expect("host process dto");
    process.validate().expect("valid");
    let fence = BrowserHostFence::new(1, 2).expect("fence");
    assert!(fence.is_nonzero());
    let physical = BrowserPhysicalBounds::new(0, 0, 320, 240).expect("bounds");
    assert!(physical.contains_local_point(10, 10));
    assert!(!physical.contains_local_point(-1, 0));
    let scale = BrowserDpi::new(96, 96).expect("dpi");
    assert_eq!(scale.horizontal, 96);
    let _ = BrowserRuntimeGeneration::initial();
    let identity = ProtocolIdentity {
        task_id: TaskId::new(),
        context_id: BrowserContextId::new(),
        resource_id: ResourceId::new(),
    };
    assert_ne!(format!("{}", identity.task_id), "");
}

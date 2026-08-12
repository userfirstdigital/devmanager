use devmanager::browser::{
    BrowserSurfaceFixture, BrowserSurfaceFixtureSnapshot, BrowserSurfaceHost,
    BrowserSurfaceIdentity, BrowserSurfaceRegistration, ClientBinding, DpiScale, HostTeardownProof,
    PhysicalBounds, ProcessIdentity, RuntimeGeneration, SurfaceBoundsUpdate, SurfaceClientRequest,
    SurfaceFocusUpdate, SurfaceInputAction, SurfaceInputRequest, SurfaceLifecycle, SurfaceNonce,
    SurfaceOwner, SurfacePermission, SurfaceTaskSwitchRequest, SurfaceTeardownReason,
    SurfaceWindowHandle, TextInputError, BROWSER_SURFACE_FIXTURE_CLICK_TOKEN,
    BROWSER_SURFACE_FIXTURE_VISIBLE_TOKEN,
};
use devmanager::domain::id::{BrowserContextId, ClientId, ResourceId, TaskId};

fn process(pid: u32) -> ProcessIdentity {
    ProcessIdentity::new(
        pid,
        10_000 + u64::from(pid),
        format!("C:/devmanager/test-host-{pid}.exe"),
    )
    .expect("valid process identity")
}

fn registration(
    task_id: TaskId,
    resource_id: ResourceId,
    nonce_byte: u8,
) -> BrowserSurfaceRegistration {
    BrowserSurfaceRegistration {
        identity: BrowserSurfaceIdentity::new(task_id, BrowserContextId::new(), resource_id),
        child_hwnd: SurfaceWindowHandle::from_raw(100 + u64::from(nonce_byte)).unwrap(),
        parking_hwnd: SurfaceWindowHandle::from_raw(200 + u64::from(nonce_byte)).unwrap(),
        nonce: SurfaceNonce::new([nonce_byte; 16]).unwrap(),
        runtime_generation: RuntimeGeneration::new(1).unwrap(),
        physical_bounds: PhysicalBounds::new(0, 0, 800, 600).unwrap(),
        dpi: DpiScale::new(100).unwrap(),
    }
}

fn client(id: ClientId, pid: u32) -> ClientBinding {
    ClientBinding::new(id, process(pid))
}

#[test]
fn descriptor_is_host_bound_and_window_handle_is_an_opaque_nonzero_token() {
    assert!(SurfaceWindowHandle::from_raw(0).is_err());
    assert!(ProcessIdentity::new(0, 1, "host.exe").is_err());
    assert!(ProcessIdentity::new(1, 0, "host.exe").is_err());

    let host_process = process(10);
    let mut host = BrowserSurfaceHost::new(host_process.clone());
    let descriptor = host
        .register(registration(TaskId::new(), ResourceId::new(), 7))
        .unwrap();

    assert_eq!(descriptor.host_process, host_process);
    assert_eq!(descriptor.child_hwnd.raw_value(), 107);
    assert_eq!(descriptor.owner(), SurfaceOwner::Host);
    assert!(descriptor.allows(SurfacePermission::TrustedClick));
    assert!(descriptor.allows(SurfacePermission::TextInput));

    let wire = serde_json::to_string(&descriptor).unwrap();
    assert!(wire.contains("hwnd:107"));
    assert!(!wire.contains("0x"));
}

#[test]
fn attach_park_detach_and_reattach_preserve_context_identity_and_fence_epochs() {
    let mut host = BrowserSurfaceHost::new(process(10));
    let descriptor = host
        .register(registration(TaskId::new(), ResourceId::new(), 7))
        .unwrap();
    let first_client = client(ClientId::new(), 11);

    let attached = host
        .attach(devmanager::browser::SurfaceAttachRequest {
            descriptor,
            client: first_client.clone(),
        })
        .unwrap();
    assert!(matches!(
        attached.lifecycle,
        SurfaceLifecycle::Attached { .. }
    ));

    let parked = host
        .park(devmanager::browser::HostSurfaceRequest {
            descriptor: attached.descriptor.clone(),
        })
        .unwrap();
    assert!(matches!(parked.lifecycle, SurfaceLifecycle::Parked { .. }));
    assert_eq!(parked.descriptor.identity, attached.descriptor.identity);
    assert!(parked.descriptor.bounds_epoch > attached.descriptor.bounds_epoch);
    assert!(parked.descriptor.focus_epoch > attached.descriptor.focus_epoch);

    let detached = host.detach(SurfaceClientRequest {
        descriptor: parked.descriptor.clone(),
        client_id: first_client.id,
    });
    assert!(detached.is_err(), "a parked surface has no attached client");

    let second_client = client(ClientId::new(), 12);
    let reattached = host
        .attach(devmanager::browser::SurfaceAttachRequest {
            descriptor: parked.descriptor,
            client: second_client,
        })
        .unwrap();
    assert!(matches!(
        reattached.lifecycle,
        SurfaceLifecycle::Attached { .. }
    ));
    assert_eq!(reattached.descriptor.identity, attached.descriptor.identity);
    assert_eq!(
        reattached.descriptor.runtime_generation,
        attached.descriptor.runtime_generation
    );

    let detached = host
        .detach(SurfaceClientRequest {
            descriptor: reattached.descriptor.clone(),
            client_id: ClientId::new(),
        })
        .unwrap_err();
    assert!(matches!(
        detached,
        devmanager::browser::SurfaceError::ClientMismatch { .. }
    ));
}

#[test]
fn stale_and_foreign_descriptors_are_rejected_before_mutation() {
    let mut host = BrowserSurfaceHost::new(process(10));
    let resource_id = ResourceId::new();
    let descriptor = host
        .register(registration(TaskId::new(), resource_id, 7))
        .unwrap();
    let attachment = host
        .attach(devmanager::browser::SurfaceAttachRequest {
            descriptor: descriptor.clone(),
            client: client(ClientId::new(), 11),
        })
        .unwrap();

    let bounds = host
        .receive_bounds(SurfaceBoundsUpdate {
            descriptor: attachment.descriptor.clone(),
            client_id: match attachment.lifecycle {
                SurfaceLifecycle::Attached { client_id } => client_id,
                _ => unreachable!(),
            },
            client_sequence: 900,
            physical_bounds: PhysicalBounds::new(0, 0, 900, 700).unwrap(),
            dpi: DpiScale::new(125).unwrap(),
        })
        .unwrap();

    assert!(host
        .receive_bounds(SurfaceBoundsUpdate {
            descriptor: attachment.descriptor,
            client_id: match bounds.lifecycle {
                SurfaceLifecycle::Attached { client_id } => client_id,
                _ => unreachable!(),
            },
            client_sequence: 1,
            physical_bounds: PhysicalBounds::new(0, 0, 901, 701).unwrap(),
            dpi: DpiScale::new(150).unwrap(),
        })
        .is_err());

    let mut foreign = bounds.descriptor.clone();
    foreign.identity.resource_id = ResourceId::new();
    assert!(matches!(
        host.park(devmanager::browser::HostSurfaceRequest {
            descriptor: foreign
        }),
        Err(devmanager::browser::SurfaceError::ForeignDescriptor { .. })
    ));
    assert!(matches!(
        host.snapshot(resource_id).unwrap().lifecycle,
        SurfaceLifecycle::Attached { .. }
    ));
}

#[test]
fn bounds_and_focus_epochs_follow_host_receive_order_not_client_sequence() {
    let mut host = BrowserSurfaceHost::new(process(10));
    let descriptor = host
        .register(registration(TaskId::new(), ResourceId::new(), 7))
        .unwrap();
    let attached_client = client(ClientId::new(), 11);
    let attached = host
        .attach(devmanager::browser::SurfaceAttachRequest {
            descriptor,
            client: attached_client.clone(),
        })
        .unwrap();

    let focused = host
        .receive_focus(SurfaceFocusUpdate {
            descriptor: attached.descriptor.clone(),
            client_id: attached_client.id,
            client_sequence: 99_999,
            focused: true,
        })
        .unwrap();
    let resized = host
        .receive_bounds(SurfaceBoundsUpdate {
            descriptor: focused.descriptor.clone(),
            client_id: attached_client.id,
            client_sequence: 1,
            physical_bounds: PhysicalBounds::new(0, 0, 1024, 768).unwrap(),
            dpi: DpiScale::new(150).unwrap(),
        })
        .unwrap();
    let unfocused = host
        .receive_focus(SurfaceFocusUpdate {
            descriptor: resized.descriptor.clone(),
            client_id: attached_client.id,
            client_sequence: 0,
            focused: false,
        })
        .unwrap();

    assert!(focused.descriptor.focus_epoch > attached.descriptor.focus_epoch);
    assert_eq!(
        resized.descriptor.focus_epoch,
        focused.descriptor.focus_epoch
    );
    assert!(resized.descriptor.bounds_epoch > focused.descriptor.bounds_epoch);
    assert!(unfocused.descriptor.focus_epoch > resized.descriptor.focus_epoch);
}

#[test]
fn task_switch_parks_outgoing_surface_and_consumes_pointer_input() {
    let mut host = BrowserSurfaceHost::new(process(10));
    let outgoing = host
        .register(registration(TaskId::new(), ResourceId::new(), 7))
        .unwrap();
    let incoming = host
        .register(registration(TaskId::new(), ResourceId::new(), 8))
        .unwrap();
    let attached_client = client(ClientId::new(), 11);
    let attached = host
        .attach(devmanager::browser::SurfaceAttachRequest {
            descriptor: outgoing,
            client: attached_client.clone(),
        })
        .unwrap();
    let focused = host
        .receive_focus(SurfaceFocusUpdate {
            descriptor: attached.descriptor,
            client_id: attached_client.id,
            client_sequence: 2,
            focused: true,
        })
        .unwrap();

    let switched = host
        .task_switch(SurfaceTaskSwitchRequest {
            outgoing: focused.descriptor.clone(),
            incoming,
            client: attached_client.clone(),
        })
        .unwrap();
    assert!(switched.pointer_consumed);
    assert!(matches!(
        switched.outgoing.lifecycle,
        SurfaceLifecycle::Parked { .. }
    ));
    assert!(matches!(
        switched.incoming.lifecycle,
        SurfaceLifecycle::Attached { .. }
    ));
    assert_eq!(
        host.active_resource_id(),
        Some(switched.incoming.descriptor.identity.resource_id)
    );

    assert!(host
        .receive_input(SurfaceInputRequest {
            descriptor: focused.descriptor,
            client_id: attached_client.id,
            action: SurfaceInputAction::TrustedClick {
                x: 10,
                y: 10,
                target_token: BROWSER_SURFACE_FIXTURE_CLICK_TOKEN.to_string(),
            },
        })
        .is_err());
}

#[test]
fn client_crash_detaches_but_does_not_close_context_and_reattach_uses_new_client() {
    let mut host = BrowserSurfaceHost::new(process(10));
    let descriptor = host
        .register(registration(TaskId::new(), ResourceId::new(), 7))
        .unwrap();
    let first_client = client(ClientId::new(), 11);
    let attached = host
        .attach(devmanager::browser::SurfaceAttachRequest {
            descriptor,
            client: first_client.clone(),
        })
        .unwrap();

    let crashed = host
        .client_crashed(first_client.clone())
        .unwrap()
        .pop()
        .unwrap();
    assert!(matches!(
        crashed.lifecycle,
        SurfaceLifecycle::Detached { .. }
    ));
    let detached = host
        .snapshot(crashed.descriptor.identity.resource_id)
        .unwrap();
    assert!(!detached.is_terminal());
    assert!(detached.context_retained);

    let second_client = client(ClientId::new(), 12);
    let reattached = host
        .reattach(devmanager::browser::SurfaceAttachRequest {
            descriptor: crashed.descriptor,
            client: second_client,
        })
        .unwrap();
    assert!(matches!(
        reattached.lifecycle,
        SurfaceLifecycle::Attached { .. }
    ));
    assert_eq!(reattached.descriptor.identity, attached.descriptor.identity);
    assert_eq!(
        reattached.descriptor.runtime_generation,
        attached.descriptor.runtime_generation
    );
    assert!(reattached.descriptor.focus_epoch > attached.descriptor.focus_epoch);
}

#[test]
fn terminal_state_requires_host_context_teardown_proof_not_client_drop() {
    let host_process = process(10);
    let mut host = BrowserSurfaceHost::new(host_process.clone());
    let descriptor = host
        .register(registration(TaskId::new(), ResourceId::new(), 7))
        .unwrap();
    let attached_client = client(ClientId::new(), 11);
    let attached = host
        .attach(devmanager::browser::SurfaceAttachRequest {
            descriptor,
            client: attached_client.clone(),
        })
        .unwrap();
    let detached = host
        .detach(SurfaceClientRequest {
            descriptor: attached.descriptor,
            client_id: attached_client.id,
        })
        .unwrap();

    let incomplete = HostTeardownProof {
        descriptor: detached.descriptor.clone(),
        host_process: host_process.clone(),
        surface_parked: true,
        controller_closed: false,
        environment_closed: false,
        helper_processes_remaining: 1,
        context_closed: false,
        reason: SurfaceTeardownReason::ContextClosed,
    };
    assert!(host.close_context(incomplete).is_err());
    assert!(!host
        .snapshot(detached.descriptor.identity.resource_id)
        .unwrap()
        .is_terminal());

    let complete = HostTeardownProof {
        descriptor: detached.descriptor,
        host_process,
        surface_parked: true,
        controller_closed: true,
        environment_closed: true,
        helper_processes_remaining: 0,
        context_closed: true,
        reason: SurfaceTeardownReason::ContextClosed,
    };
    let terminal = host.close_context(complete).unwrap();
    assert!(matches!(
        terminal.lifecycle,
        SurfaceLifecycle::Terminal {
            reason: SurfaceTeardownReason::ContextClosed
        }
    ));
    assert!(host
        .reattach(devmanager::browser::SurfaceAttachRequest {
            descriptor: terminal.descriptor,
            client: client(ClientId::new(), 12),
        })
        .is_err());
}

#[test]
fn fixture_contract_is_bounded_deterministic_and_retains_state() {
    let mut fixture = BrowserSurfaceFixture::new();
    assert_eq!(
        fixture.snapshot().visible_token,
        BROWSER_SURFACE_FIXTURE_VISIBLE_TOKEN
    );

    fixture
        .trusted_click(BROWSER_SURFACE_FIXTURE_CLICK_TOKEN)
        .unwrap();
    fixture.text_input("retained text").unwrap();
    fixture.resize(
        PhysicalBounds::new(1024, 10, 900, 700).unwrap(),
        DpiScale::new(125).unwrap(),
    );

    let snapshot: BrowserSurfaceFixtureSnapshot = fixture.snapshot();
    assert_eq!(
        snapshot.trusted_click_token.as_deref(),
        Some(BROWSER_SURFACE_FIXTURE_CLICK_TOKEN)
    );
    assert_eq!(snapshot.text_value, "retained text");
    assert_eq!(snapshot.resize_token, "dm-surface-resize-900x700@125");
    assert_eq!(snapshot.retained_state, "dm-surface-retained-state-v1");

    let too_long = "x".repeat(5_000);
    assert!(matches!(
        fixture.text_input(&too_long),
        Err(TextInputError::TooLarge { .. })
    ));
}

#[test]
fn input_requires_current_focus_and_bounds_epochs() {
    let mut host = BrowserSurfaceHost::new(process(10));
    let descriptor = host
        .register(registration(TaskId::new(), ResourceId::new(), 7))
        .unwrap();
    let client = client(ClientId::new(), 11);
    let attached = host
        .attach(devmanager::browser::SurfaceAttachRequest {
            descriptor,
            client: client.clone(),
        })
        .unwrap();
    let click = SurfaceInputRequest {
        descriptor: attached.descriptor.clone(),
        client_id: client.id,
        action: SurfaceInputAction::TrustedClick {
            x: 10,
            y: 10,
            target_token: BROWSER_SURFACE_FIXTURE_CLICK_TOKEN.to_string(),
        },
    };
    assert!(host.receive_input(click.clone()).is_err());

    let focused = host
        .receive_focus(SurfaceFocusUpdate {
            descriptor: attached.descriptor,
            client_id: client.id,
            client_sequence: 1,
            focused: true,
        })
        .unwrap();
    assert!(host
        .receive_input(SurfaceInputRequest {
            descriptor: focused.descriptor,
            ..click
        })
        .is_ok());
}

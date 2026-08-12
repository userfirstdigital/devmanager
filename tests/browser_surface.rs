use devmanager::browser::{
    BrowserSurfaceFixture, BrowserSurfaceFixtureSnapshot, BrowserSurfaceHost,
    BrowserSurfaceIdentity, BrowserSurfaceRegistration, ClientBinding, DpiScale, HostHwndOwnership,
    HostTeardownProof, PhysicalBounds, ProcessIdentity, RuntimeGeneration, SurfaceAction,
    SurfaceBoundsUpdate, SurfaceClientRequest, SurfaceCommand, SurfaceError, SurfaceEventKind,
    SurfaceFocusUpdate, SurfaceInputAction, SurfaceInputRequest, SurfaceLifecycle, SurfaceNonce,
    SurfaceOwner, SurfacePermission, SurfaceTaskSwitchRequest, SurfaceTeardownReason,
    SurfaceWindowHandle, TextInputError, BROWSER_SURFACE_FIXTURE_CLICK_TOKEN,
    BROWSER_SURFACE_FIXTURE_VISIBLE_TOKEN, MAX_SURFACE_EVENTS,
};
use devmanager::domain::id::{
    AgentSessionId, BrowserContextId, ClientId, RequestId, ResourceId, TaskId,
};

fn process(pid: u32) -> ProcessIdentity {
    ProcessIdentity::new(
        pid,
        10_000 + u64::from(pid),
        format!("C:/devmanager/test-host-{pid}.exe"),
    )
    .expect("valid process identity")
}

fn hwnd_ownership(nonce_byte: u8) -> HostHwndOwnership {
    HostHwndOwnership::new(
        SurfaceWindowHandle::from_raw(100 + u64::from(nonce_byte)).unwrap(),
        SurfaceWindowHandle::from_raw(200 + u64::from(nonce_byte)).unwrap(),
        true,
        true,
    )
    .expect("valid host HWND ownership")
}

fn registration(
    task_id: TaskId,
    resource_id: ResourceId,
    nonce_byte: u8,
) -> BrowserSurfaceRegistration {
    registration_with_session(task_id, AgentSessionId::new(), resource_id, nonce_byte)
}

fn registration_with_session(
    task_id: TaskId,
    session_id: AgentSessionId,
    resource_id: ResourceId,
    nonce_byte: u8,
) -> BrowserSurfaceRegistration {
    BrowserSurfaceRegistration {
        identity: BrowserSurfaceIdentity::new(
            task_id,
            session_id,
            BrowserContextId::new(),
            resource_id,
        ),
        hwnd_ownership: hwnd_ownership(nonce_byte),
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
    assert!(!descriptor.allows_automation());
    assert_eq!(
        descriptor.authority().session_id,
        descriptor.identity.session_id
    );
    assert_eq!(
        descriptor.authority().runtime_generation,
        descriptor.runtime_generation
    );

    let wire = serde_json::to_string(&descriptor).unwrap();
    assert!(wire.contains("hwnd:107"));
    assert!(!wire.contains("hwnd:207"));
    assert!(!wire.contains("0x"));
    assert!(!wire.contains("parking"));
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
    assert!(
        host.close_context(HostTeardownProof {
            descriptor: detached.descriptor.clone(),
            host_process: host_process.clone(),
            surface_parked: true,
            controller_closed: true,
            environment_closed: true,
            helper_processes_remaining: 0,
            context_closed: true,
            reason: SurfaceTeardownReason::ContextClosed,
        })
        .is_err(),
        "teardown must fail closed until the host actually parks the surface"
    );

    let parked = host
        .park(devmanager::browser::HostSurfaceRequest {
            descriptor: detached.descriptor,
        })
        .unwrap();

    let incomplete = HostTeardownProof {
        descriptor: parked.descriptor.clone(),
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
        .snapshot(parked.descriptor.identity.resource_id)
        .unwrap()
        .is_terminal());

    let complete = HostTeardownProof {
        descriptor: parked.descriptor,
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

#[test]
fn identity_requires_exact_task_session_and_generation_authority() {
    let mut host = BrowserSurfaceHost::new(process(10));
    let task_id = TaskId::new();
    let session_id = AgentSessionId::new();
    let descriptor = host
        .register(registration_with_session(
            task_id,
            session_id,
            ResourceId::new(),
            7,
        ))
        .unwrap();
    let authority = descriptor.authority();
    assert_eq!(authority.task_id, task_id);
    assert_eq!(authority.session_id, session_id);
    assert_eq!(authority.runtime_generation, descriptor.runtime_generation);

    let mut foreign_session = descriptor.clone();
    foreign_session.identity.session_id = AgentSessionId::new();
    assert!(matches!(
        host.park(devmanager::browser::HostSurfaceRequest {
            descriptor: foreign_session
        }),
        Err(SurfaceError::ForeignDescriptor { .. })
    ));

    let mut stale_generation = descriptor.clone();
    stale_generation.runtime_generation = RuntimeGeneration::new(9).unwrap();
    assert!(matches!(
        host.park(devmanager::browser::HostSurfaceRequest {
            descriptor: stale_generation
        }),
        Err(SurfaceError::StaleDescriptor { .. })
    ));
}

#[test]
fn hwnd_ownership_is_host_parking_parent_on_ui_com_thread_and_unique() {
    assert!(HostHwndOwnership::new(
        SurfaceWindowHandle::from_raw(1).unwrap(),
        SurfaceWindowHandle::from_raw(1).unwrap(),
        true,
        true,
    )
    .is_err());
    assert!(HostHwndOwnership::new(
        SurfaceWindowHandle::from_raw(1).unwrap(),
        SurfaceWindowHandle::from_raw(2).unwrap(),
        false,
        true,
    )
    .is_err());
    assert!(HostHwndOwnership::new(
        SurfaceWindowHandle::from_raw(1).unwrap(),
        SurfaceWindowHandle::from_raw(2).unwrap(),
        true,
        false,
    )
    .is_err());

    let mut host = BrowserSurfaceHost::new(process(10));
    let first = host
        .register(registration(TaskId::new(), ResourceId::new(), 7))
        .unwrap();
    assert_eq!(
        host.parking_hwnd(first.identity.resource_id)
            .unwrap()
            .raw_value(),
        207
    );
    assert_eq!(
        host.hwnd_ownership(first.identity.resource_id)
            .unwrap()
            .child_hwnd(),
        &first.child_hwnd
    );

    let mut duplicate_child = registration(TaskId::new(), ResourceId::new(), 8);
    duplicate_child.hwnd_ownership = HostHwndOwnership::new(
        first.child_hwnd.clone(),
        SurfaceWindowHandle::from_raw(208).unwrap(),
        true,
        true,
    )
    .unwrap();
    assert!(matches!(
        host.register(duplicate_child),
        Err(SurfaceError::DuplicateHwnd { .. })
    ));
}

#[test]
fn one_live_surface_follows_its_owning_task_and_rejects_same_task_switch() {
    let mut host = BrowserSurfaceHost::new(process(10));
    let outgoing_task = TaskId::new();
    let incoming_task = TaskId::new();
    let outgoing = host
        .register(registration(outgoing_task, ResourceId::new(), 7))
        .unwrap();
    assert_eq!(
        host.task_surface(outgoing_task),
        Some(outgoing.identity.resource_id)
    );
    assert!(matches!(
        host.register(registration(outgoing_task, ResourceId::new(), 9)),
        Err(SurfaceError::TaskSurfaceConflict { task_id }) if task_id == outgoing_task
    ));

    let incoming = host
        .register(registration(incoming_task, ResourceId::new(), 8))
        .unwrap();
    let attached_client = client(ClientId::new(), 11);
    let attached = host
        .attach(devmanager::browser::SurfaceAttachRequest {
            descriptor: outgoing,
            client: attached_client.clone(),
        })
        .unwrap();
    assert_eq!(
        host.active_task_id(),
        Some(attached.descriptor.identity.task_id)
    );

    assert!(matches!(
        host.task_switch(SurfaceTaskSwitchRequest {
            outgoing: attached.descriptor.clone(),
            incoming: attached.descriptor.clone(),
            client: attached_client.clone(),
        }),
        Err(SurfaceError::ActiveSurfaceConflict { .. })
    ));

    let switched = host
        .task_switch(SurfaceTaskSwitchRequest {
            outgoing: attached.descriptor,
            incoming,
            client: attached_client,
        })
        .unwrap();
    assert!(switched.pointer_consumed);
    assert_eq!(
        host.active_task_id(),
        Some(switched.incoming.descriptor.identity.task_id)
    );
    assert_eq!(
        host.active_resource_id(),
        Some(switched.incoming.descriptor.identity.resource_id)
    );
}

#[test]
fn bounded_actions_and_events_are_catalogued_capped_and_request_fenced() {
    let mut host = BrowserSurfaceHost::new(process(10));
    let descriptor = host
        .register(registration(TaskId::new(), ResourceId::new(), 7))
        .unwrap();
    let attached_client = client(ClientId::new(), 11);
    let request_id = RequestId::new();
    let attached = host
        .apply_action(SurfaceCommand {
            request_id,
            descriptor,
            action: SurfaceAction::Attach {
                client: attached_client.clone(),
            },
        })
        .unwrap();
    assert_eq!(attached.kind, SurfaceEventKind::Attached);
    assert_eq!(attached.request_id, request_id);
    assert!(matches!(
        host.apply_action(SurfaceCommand {
            request_id,
            descriptor: attached.descriptor.clone(),
            action: SurfaceAction::UpdateFocus {
                client_id: attached_client.id,
                client_sequence: 1,
                focused: true,
            },
        }),
        Err(SurfaceError::DuplicateRequest { .. })
    ));

    for index in 0..MAX_SURFACE_EVENTS {
        host.apply_action(SurfaceCommand {
            request_id: RequestId::new(),
            descriptor: host
                .descriptor(attached.descriptor.identity.resource_id)
                .unwrap()
                .clone(),
            action: SurfaceAction::UpdateFocus {
                client_id: attached_client.id,
                client_sequence: u64::from(index as u32),
                focused: index % 2 == 0,
            },
        })
        .unwrap();
    }
    assert_eq!(host.events().len(), MAX_SURFACE_EVENTS);
    assert!(host
        .events()
        .iter()
        .all(|event| event.kind != SurfaceEventKind::Attached));
}

#[test]
fn hot_path_never_admits_process_or_window_probes() {
    assert!(!BrowserSurfaceHost::HOT_PATH_PROBES_PERMITTED);
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
            descriptor: attached.descriptor,
            client_id: attached_client.id,
            client_sequence: 1,
            focused: true,
        })
        .unwrap();
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
        .is_ok());
    assert!(!host.hot_path_probed());
}

#[test]
fn automation_remains_separately_authorized_from_the_surface() {
    let mut host = BrowserSurfaceHost::new(process(10));
    let descriptor = host
        .register(registration(TaskId::new(), ResourceId::new(), 7))
        .unwrap();
    assert!(!descriptor.allows_automation());
    assert!(matches!(
        host.apply_action(SurfaceCommand {
            request_id: RequestId::new(),
            descriptor,
            action: SurfaceAction::Automate,
        }),
        Err(SurfaceError::AutomationSeparatelyAuthorized)
    ));
    assert!(host
        .events()
        .iter()
        .all(|event| event.kind == SurfaceEventKind::Registered));
}

mod browser_dock_tests {
    use devmanager::browser::{
        BrowserDockFocusTarget, BrowserDockGesture, BrowserDockSurface, BrowserPointerDisposition,
    };
    use devmanager::domain::id::{BrowserContextId, BrowserTabId, ClientId, ResourceId, TaskId};
    use devmanager::protocol::{
        BrowserAttachRequest, BrowserPhysicalBounds, BrowserProjectionMeta, BrowserSecurityState,
        BrowserSurfaceDescriptor, BrowserSurfaceLifecycle, BrowserTabProjection,
    };
    use devmanager::ui::task_cockpit::{
        BrowserContextDock, ContextDockFocus, ContextDockLayout, TaskBrowserDockModel,
    };
    use serde_json::json;
    
    fn descriptor(task_id: TaskId, generation: u64) -> BrowserSurfaceDescriptor {
        serde_json::from_value(json!({
            "identity": {
                "taskId": task_id,
                "contextId": BrowserContextId::new(),
                "resourceId": ResourceId::new(),
            },
            "childHwnd": "hwnd:42",
            "hostProcess": {"pid": 7, "creationTime100ns": 11, "executable": "devmanager-host.exe"},
            "hostFence": {"bootEpoch": 1, "connectionEpoch": 1},
            "runtimeGeneration": generation,
            "nonce": [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            "boundsEpoch": 1,
            "focusEpoch": 1,
            "physicalBounds": {"x": 0, "y": 0, "width": 320, "height": 200},
            "dpi": {"horizontal": 96, "vertical": 96},
        }))
        .expect("host-issued descriptor")
    }
    
    fn tab(task_url: &str, title: &str) -> BrowserTabProjection {
        let tab = BrowserTabProjection {
            tab_id: BrowserTabId::new(),
            title: title.to_string(),
            url: task_url.to_string(),
            kind: devmanager::domain::browser::BrowserTabKind::Page,
            security: BrowserSecurityState::Secure,
            loading: false,
            error: None,
        };
        tab.validate().expect("tab");
        tab
    }
    
    fn projection(task_id: TaskId, context_id: BrowserContextId, tabs: Vec<BrowserTabProjection>) -> BrowserProjectionMeta {
        let selected = tabs.first().map(|tab| tab.tab_id);
        let meta = BrowserProjectionMeta {
            task_id,
            context_id,
            generation: devmanager::protocol::BrowserRuntimeGeneration::new(1).unwrap(),
            bounds_epoch: devmanager::protocol::BrowserBoundsEpoch::initial(),
            focus_epoch: devmanager::protocol::BrowserFocusEpoch::initial(),
            frame_id: 1,
            selected_tab_id: selected,
            tabs,
            progress: Some("inspecting".to_string()),
            interaction_mode: devmanager::protocol::BrowserInteractionMode::Observe,
        };
        meta.validate().expect("meta");
        meta
    }
    
    #[test]
    fn ui_chrome_is_native_and_never_web_toolbar() {
        assert!(!BrowserDockSurface::uses_web_chrome());
        assert!(!BrowserContextDock::uses_web_chrome());
        assert!(!TaskBrowserDockModel::uses_web_chrome());
        assert_eq!(BrowserDockSurface::required_chrome().len(), 6);
    }
    
    #[test]
    fn ui_task_tab_strip_and_status_come_from_projection() {
        let task_id = TaskId::new();
        let page = tab("https://example.test/", "Fixture");
        let loading = BrowserTabProjection {
            loading: true,
            error: Some("timeout".to_string()),
            security: BrowserSecurityState::Insecure,
            url: "http://example.test/".to_string(),
            ..page.clone()
        };
        loading.validate().expect("loading tab");
        let model = TaskBrowserDockModel::from_projection(&projection(
            task_id,
            BrowserContextId::new(),
            vec![loading],
        ));
        assert_eq!(model.tab_labels, vec!["Fixture".to_string()]);
        assert_eq!(model.address, "http://example.test/");
        assert_eq!(model.error.as_deref(), Some("timeout"));
        assert!(model.loading);
        assert_eq!(model.security, BrowserSecurityState::Insecure);
        assert_eq!(model.progress.as_deref(), Some("inspecting"));
    }
    
    #[test]
    fn ui_attach_detach_and_stale_generation_fail_closed() {
        let task_id = TaskId::new();
        let mut surface = BrowserDockSurface::from_descriptor(descriptor(task_id, 1)).unwrap();
        let client = ClientId::new();
        surface
            .attach(BrowserAttachRequest::new(descriptor(task_id, 1), client))
            .expect("attach");
        assert!(matches!(
            surface.lifecycle(),
            BrowserSurfaceLifecycle::Attached { .. }
        ));
        surface.detach(false).expect("detach");
        assert!(matches!(
            surface.lifecycle(),
            BrowserSurfaceLifecycle::Detached { crashed: false, .. }
        ));
        assert_eq!(
            surface.admit_page_input(2, 1, 1, BrowserDockGesture::PageClick),
            Err(devmanager::browser::BrowserDockError::StaleGeneration)
        );
    }
    
    #[test]
    fn ui_keyboard_traversal_stays_in_chrome_until_armed() {
        let task_id = TaskId::new();
        let mut surface = BrowserDockSurface::from_descriptor(descriptor(task_id, 1)).unwrap();
        surface
            .attach(BrowserAttachRequest::new(descriptor(task_id, 1), ClientId::new()))
            .unwrap();
        assert_eq!(
            surface.classify_gesture(BrowserDockGesture::PageKey),
            BrowserPointerDisposition::ConsumeShellGesture
        );
        surface.arm_page_input_after_gesture().unwrap();
        assert_eq!(surface.focus_target(), BrowserDockFocusTarget::BrowserPage);
        assert_eq!(
            surface.classify_gesture(BrowserDockGesture::PageKey),
            BrowserPointerDisposition::ForwardToPage
        );
    }
    
    #[test]
    fn ui_context_dock_resize_hides_before_new_bounds() {
        let task_id = TaskId::new();
        let surface = BrowserDockSurface::from_descriptor(descriptor(task_id, 1)).unwrap();
        let tabs = vec![tab("https://example.test/", "A")];
        let context_id = surface.task_id();
        let _ = context_id;
        let mut dock = BrowserContextDock::open(
            surface,
            projection(task_id, BrowserContextId::new(), tabs),
            ContextDockLayout::split(1000, 40).unwrap(),
        )
        .unwrap();
        dock.attach(BrowserAttachRequest::new(descriptor(task_id, 1), ClientId::new()))
            .unwrap();
        let epoch = dock
            .resize(
                1,
                ContextDockLayout::split(1200, 50).unwrap(),
                BrowserPhysicalBounds::new(0, 0, 400, 240).unwrap(),
            )
            .unwrap();
        assert_eq!(epoch, 2);
        assert_eq!(dock.layout().terminal_width, 600);
        assert!(dock.terminal_present());
    }
    
    #[test]
    fn ui_task_switch_while_form_focused_consumes_pointer() {
        let task_id = TaskId::new();
        let mut surface = BrowserDockSurface::from_descriptor(descriptor(task_id, 1)).unwrap();
        surface
            .attach(BrowserAttachRequest::new(descriptor(task_id, 1), ClientId::new()))
            .unwrap();
        surface.arm_page_input_after_gesture().unwrap();
        surface
            .switch_task(BrowserAttachRequest::new(descriptor(task_id, 1), ClientId::new()))
            .unwrap();
        assert_eq!(
            surface.admit_page_input(
                1,
                surface.bounds_epoch(),
                surface.focus_epoch(),
                BrowserDockGesture::PageClick
            ),
            Err(devmanager::browser::BrowserDockError::PointerConsumed)
        );
    }
    
    #[test]
    fn ui_popup_selection_and_address_cannot_become_page_input() {
        let task_id = TaskId::new();
        let mut surface = BrowserDockSurface::from_descriptor(descriptor(task_id, 1)).unwrap();
        surface
            .attach(BrowserAttachRequest::new(descriptor(task_id, 1), ClientId::new()))
            .unwrap();
        surface.select_tab(BrowserTabId::new()).unwrap();
        for gesture in [
            BrowserDockGesture::PopupSelect,
            BrowserDockGesture::AddressSubmit,
            BrowserDockGesture::FileChoice,
            BrowserDockGesture::PermissionAnswer,
            BrowserDockGesture::PageDrag,
        ] {
            assert_eq!(
                surface.classify_gesture(gesture),
                BrowserPointerDisposition::ConsumeShellGesture
            );
        }
    }
    
    #[test]
    fn ui_terminal_browser_focus_transition_preserves_terminal() {
        let task_id = TaskId::new();
        let surface = BrowserDockSurface::from_descriptor(descriptor(task_id, 1)).unwrap();
        let mut dock = BrowserContextDock::open(
            surface,
            projection(
                task_id,
                BrowserContextId::new(),
                vec![tab("https://example.test/", "A")],
            ),
            ContextDockLayout::split(800, 40).unwrap(),
        )
        .unwrap();
        dock.focus_terminal().unwrap();
        assert_eq!(dock.focus(), ContextDockFocus::Terminal);
        assert!(dock.terminal_present());
        assert_eq!(
            dock.classify(BrowserDockGesture::PageClick),
            BrowserPointerDisposition::ConsumeShellGesture
        );
    }
    
    #[test]
    fn ui_cross_task_descriptor_is_rejected() {
        let mut surface = BrowserDockSurface::from_descriptor(descriptor(TaskId::new(), 1)).unwrap();
        assert_eq!(
            surface.attach(BrowserAttachRequest::new(descriptor(TaskId::new(), 1), ClientId::new())),
            Err(devmanager::browser::BrowserDockError::CrossTask)
        );
    }
}


//! NativeShell-facing browser controller and visible-host proof classification.
//!
//! These tests do not launch WebView2, GPUI, fixture servers, or the installed
//! app. They pin exact Task/Agent/Context/Resource binding, generation/lease
//! fencing, and the rule that fixture-only proof cannot be visible-green.

use devmanager::browser::{
    classify_visible_host_proof, unsupported_host_status, BrowserBounds, BrowserCommand,
    BrowserGatewayBindingRef, BrowserNativeCallback, BrowserNativeCallbackKind,
    BrowserNativeControllerError, BrowserNativeDestination, BrowserNativeHostCommand,
    BrowserNativeIdentity, BrowserNativeLeaseFence, BrowserNativeShellController,
    BrowserVisibleHostProofClaim,
    BrowserVisibleHostProofClass, BrowserWorkspaceKey, BROWSER_VISIBLE_WEBVIEW2_OPT_IN_ENV,
};
use devmanager::domain::{AgentSessionId, BrowserContextId, ResourceId, TaskId};

fn identity() -> BrowserNativeIdentity {
    BrowserNativeIdentity::new(
        TaskId::new(),
        AgentSessionId::new(),
        BrowserContextId::new(),
        ResourceId::new(),
    )
}

fn workspace(project: &str, tab: &str) -> BrowserWorkspaceKey {
    BrowserWorkspaceKey::new(project, tab).expect("workspace key")
}

fn dest(raw: u64) -> BrowserNativeDestination {
    BrowserNativeDestination::from_raw(raw).expect("destination hwnd")
}

fn bounds() -> BrowserBounds {
    BrowserBounds {
        x: 0,
        y: 0,
        width: 320,
        height: 200,
    }
}

#[test]
fn bind_mints_exact_identity_generation_and_gateway_lease() {
    let controller = BrowserNativeShellController::supported();
    let identity = identity();
    let gateway = BrowserGatewayBindingRef::new("process-session-a");
    let lease = controller
        .bind(
            identity,
            workspace("project", "conversation"),
            gateway.clone(),
        )
        .expect("bind");

    assert_eq!(lease.generation(), 1);
    assert_eq!(controller.current_identity(), Some(identity));
    assert_eq!(controller.current_gateway(), Some(gateway));
    assert_eq!(controller.current_lease(), Some(lease.clone()));
    assert!(!controller.is_attached());
    let debug = format!("{lease:?}");
    assert!(debug.contains("generation"));
    assert!(!debug.contains("token"), "lease debug must not reveal the token");
}

#[test]
fn attach_requires_the_current_lease_and_exact_gateway() {
    let controller = BrowserNativeShellController::supported();
    let identity = identity();
    let gateway = BrowserGatewayBindingRef::new("process-session-a");
    let lease = controller
        .bind(
            identity,
            workspace("project", "conversation"),
            gateway.clone(),
        )
        .expect("bind");
    let destination = dest(0xB101);

    let admitted = controller
        .attach_with_gateway(&lease, &gateway, destination, bounds())
        .expect("attach");
    match admitted {
        BrowserNativeHostCommand::Attach {
            identity: bound,
            gateway: bound_gateway,
            destination: bound_dest,
            ..
        } => {
            assert_eq!(bound, identity);
            assert_eq!(bound_gateway, gateway);
            assert_eq!(bound_dest, destination);
        }
        other => panic!("expected attach command, got {other:?}"),
    }
    assert!(controller.is_attached());

    assert_eq!(
        controller.attach_with_gateway(
            &lease,
            &BrowserGatewayBindingRef::new("process-session-other"),
            destination,
            bounds(),
        ),
        Err(BrowserNativeControllerError::GatewayMismatch)
    );
    assert!(
        controller.is_attached(),
        "gateway mismatch must not retarget the live attachment"
    );
}

#[test]
fn replace_while_attached_is_rejected_until_the_old_lease_detaches() {
    let controller = BrowserNativeShellController::supported();
    let first = identity();
    let second = identity();
    let first_lease = controller
        .bind(
            first,
            workspace("project", "one"),
            BrowserGatewayBindingRef::new("process-one"),
        )
        .expect("first bind");
    controller
        .attach(&first_lease, dest(0xB201), bounds())
        .expect("first attach");

    assert_eq!(
        controller.bind(
            second,
            workspace("project", "two"),
            BrowserGatewayBindingRef::new("process-two"),
        ),
        Err(BrowserNativeControllerError::AttachedBindingMustDetach)
    );
    assert_eq!(controller.current_identity(), Some(first));
    assert_eq!(controller.current_lease(), Some(first_lease.clone()));
    assert!(
        controller.is_attached(),
        "a rejected replace must leave the live attachment on the old lease"
    );
    controller
        .attach(&first_lease, dest(0xB201), bounds())
        .expect("old lease remains attachable after rejected replace");

    controller
        .detach(&first_lease)
        .expect("detach old lease before rebind");
    assert!(!controller.is_attached());

    let second_lease = controller
        .bind(
            second,
            workspace("project", "two"),
            BrowserGatewayBindingRef::new("process-two"),
        )
        .expect("bind after detach");
    assert!(second_lease.generation() > first_lease.generation());
    assert_eq!(controller.current_identity(), Some(second));
    assert_eq!(
        controller.attach(&first_lease, dest(0xB201), bounds()),
        Err(BrowserNativeControllerError::StaleLease)
    );

    controller
        .attach(&second_lease, dest(0xB202), bounds())
        .expect("second attach");
    assert_eq!(controller.current_identity(), Some(second));
    assert!(controller.is_attached());
}

#[test]
fn attach_replace_and_detach_are_idempotent() {
    let controller = BrowserNativeShellController::supported();
    let identity = identity();
    let gateway = BrowserGatewayBindingRef::new("process-session-a");
    let workspace_key = workspace("project", "conversation");
    let first = controller
        .bind(identity, workspace_key.clone(), gateway.clone())
        .expect("bind");
    let again = controller
        .bind(identity, workspace_key, gateway)
        .expect("idempotent bind");
    assert_eq!(first, again);

    let destination = dest(0xB301);
    let first_attach = controller
        .attach(&first, destination, bounds())
        .expect("attach");
    let second_attach = controller
        .attach(&first, destination, bounds())
        .expect("idempotent attach");
    assert_eq!(first_attach, second_attach);

    let detached = controller.detach(&first).expect("detach");
    assert!(matches!(
        detached,
        BrowserNativeHostCommand::Detach { .. }
    ));
    assert!(!controller.is_attached());
    let detached_again = controller.detach(&first).expect("idempotent detach");
    assert_eq!(detached, detached_again);
}

#[test]
fn stale_callbacks_are_ignored_and_cannot_mutate_the_live_binding() {
    let controller = BrowserNativeShellController::supported();
    let first = identity();
    let second = identity();
    let first_lease = controller
        .bind(
            first,
            workspace("project", "one"),
            BrowserGatewayBindingRef::new("process-one"),
        )
        .expect("first bind");
    controller
        .attach(&first_lease, dest(0xB401), bounds())
        .expect("attach");
    controller
        .detach(&first_lease)
        .expect("detach before replacement bind");
    let second_lease = controller
        .bind(
            second,
            workspace("project", "two"),
            BrowserGatewayBindingRef::new("process-two"),
        )
        .expect("bind after detach");
    controller
        .attach(&second_lease, dest(0xB402), bounds())
        .expect("second attach");

    let stale = BrowserNativeCallback {
        generation: first_lease.generation(),
        lease: first_lease.clone(),
        kind: BrowserNativeCallbackKind::NavigationComplete,
    };
    assert!(
        controller.take_callback(stale).is_none(),
        "stale callback must be ignored"
    );
    assert_eq!(controller.current_identity(), Some(second));
    assert!(controller.is_attached());

    let live = BrowserNativeCallback {
        generation: second_lease.generation(),
        lease: second_lease,
        kind: BrowserNativeCallbackKind::NavigationComplete,
    };
    assert_eq!(
        controller.take_callback(live),
        Some(BrowserNativeCallbackKind::NavigationComplete)
    );
}

#[test]
fn mismatched_identity_or_generation_is_rejected() {
    let controller = BrowserNativeShellController::supported();
    let identity = identity();
    let lease = controller
        .bind(
            identity,
            workspace("project", "conversation"),
            BrowserGatewayBindingRef::new("process-session-a"),
        )
        .expect("bind");
    let foreign = identity();
    assert_eq!(
        controller.require_identity(&lease, foreign),
        Err(BrowserNativeControllerError::IdentityMismatch)
    );
    let mut stale_generation = lease.clone();
    stale_generation.spoil_generation_for_test();
    assert_eq!(
        controller.attach(&stale_generation, dest(0xB501), bounds()),
        Err(BrowserNativeControllerError::StaleGeneration)
    );
}

#[test]
fn unsupported_platform_controller_rejects_host_mutations() {
    let controller = BrowserNativeShellController::unsupported();
    assert!(!controller.platform_supported());
    assert!(!unsupported_host_status(std::env::consts::OS).available);

    let lease = controller
        .bind(
            identity(),
            workspace("project", "conversation"),
            BrowserGatewayBindingRef::new("process-session-a"),
        )
        .expect("bind remains an identity fence on unsupported hosts");
    assert_eq!(
        controller.attach(&lease, dest(0xB601), bounds()),
        Err(BrowserNativeControllerError::UnsupportedPlatform)
    );
    assert_eq!(
        controller.reattach(&lease, dest(0xB602), bounds()),
        Err(BrowserNativeControllerError::UnsupportedPlatform)
    );
    assert_eq!(
        controller.resize(&lease, bounds()),
        Err(BrowserNativeControllerError::UnsupportedPlatform)
    );
    assert_eq!(
        controller.focus(&lease, true),
        Err(BrowserNativeControllerError::UnsupportedPlatform)
    );
    assert_eq!(
        controller.submit_command(
            &lease,
            BrowserCommand::Navigate {
                tab_id: "page".to_string(),
                url: "https://example.test".to_string(),
            },
        ),
        Err(BrowserNativeControllerError::UnsupportedPlatform)
    );
    controller
        .detach(&lease)
        .expect("unsupported detach stays idempotent");
    assert!(!controller.is_attached());
}

#[test]
fn command_handoff_and_render_ops_require_the_live_lease() {
    let controller = BrowserNativeShellController::supported();
    let lease = controller
        .bind(
            identity(),
            workspace("project", "conversation"),
            BrowserGatewayBindingRef::new("process-session-a"),
        )
        .expect("bind");
    controller
        .attach(&lease, dest(0xB701), bounds())
        .expect("attach");

    let navigate = BrowserCommand::Navigate {
        tab_id: "page".to_string(),
        url: "https://example.test/app".to_string(),
    };
    assert!(matches!(
        controller
            .submit_command(&lease, navigate.clone())
            .expect("command"),
        BrowserNativeHostCommand::SubmitCommand { command, .. } if command == navigate
    ));
    assert!(matches!(
        controller.resize(&lease, bounds()).expect("resize"),
        BrowserNativeHostCommand::Resize { .. }
    ));
    assert!(matches!(
        controller.focus(&lease, true).expect("focus"),
        BrowserNativeHostCommand::Focus { focused: true, .. }
    ));

    controller.detach(&lease).expect("detach");
    assert_eq!(
        controller.submit_command(&lease, navigate),
        Err(BrowserNativeControllerError::Detached)
    );
}

#[test]
fn host_lease_fence_rejects_commands_from_a_replaced_binding() {
    let controller = BrowserNativeShellController::supported();
    let first = controller
        .bind(
            identity(),
            workspace("project", "one"),
            BrowserGatewayBindingRef::new("process-one"),
        )
        .expect("first bind");
    let stale_command = controller
        .bind_gateway(&first, &BrowserGatewayBindingRef::new("process-one"))
        .expect("first gateway binding");
    controller.detach(&first).expect("first detach");

    let second = controller
        .bind(
            identity(),
            workspace("project", "two"),
            BrowserGatewayBindingRef::new("process-two"),
        )
        .expect("second bind");
    let current_command = controller
        .bind_gateway(&second, &BrowserGatewayBindingRef::new("process-two"))
        .expect("second gateway binding");

    let mut fence = BrowserNativeLeaseFence::default();
    fence.admit(current_command.lease()).expect("current lease");
    assert_eq!(
        fence.admit(stale_command.lease()),
        Err(BrowserNativeControllerError::StaleGeneration)
    );
    fence.retire(current_command.lease()).expect("retire current");
    assert_eq!(fence.current(), None);
}

#[test]
fn fixture_only_proof_cannot_be_visible_green() {
    let fixture_claim = BrowserVisibleHostProofClaim {
        fixture_only: true,
        visible_claimed: true,
        opt_in_marker: true,
        observed_host_owned_webview2: true,
        observed_window_lifecycle: true,
        observed_helper_lifecycle: true,
    };
    let class = classify_visible_host_proof(fixture_claim);
    assert_eq!(class, BrowserVisibleHostProofClass::FixtureProtocolOnly);
    assert!(
        !class.is_visible_green(),
        "fixture-only execution must never report visible WebView2 success"
    );

    let protocol_only = classify_visible_host_proof(BrowserVisibleHostProofClaim {
        fixture_only: true,
        visible_claimed: false,
        opt_in_marker: false,
        observed_host_owned_webview2: false,
        observed_window_lifecycle: false,
        observed_helper_lifecycle: false,
    });
    assert_eq!(
        protocol_only,
        BrowserVisibleHostProofClass::FixtureProtocolOnly
    );
    assert!(!protocol_only.is_visible_green());

    let hold = classify_visible_host_proof(BrowserVisibleHostProofClaim {
        fixture_only: false,
        visible_claimed: true,
        opt_in_marker: false,
        observed_host_owned_webview2: false,
        observed_window_lifecycle: false,
        observed_helper_lifecycle: false,
    });
    assert_eq!(hold, BrowserVisibleHostProofClass::VisibleHold);
    assert!(!hold.is_visible_green());

    let missing_observation = classify_visible_host_proof(BrowserVisibleHostProofClaim {
        fixture_only: false,
        visible_claimed: true,
        opt_in_marker: true,
        observed_host_owned_webview2: true,
        observed_window_lifecycle: true,
        observed_helper_lifecycle: false,
    });
    assert_eq!(
        missing_observation,
        BrowserVisibleHostProofClass::VisibleHold
    );

    let visible = classify_visible_host_proof(BrowserVisibleHostProofClaim {
        fixture_only: false,
        visible_claimed: true,
        opt_in_marker: true,
        observed_host_owned_webview2: true,
        observed_window_lifecycle: true,
        observed_helper_lifecycle: true,
    });
    assert_eq!(visible, BrowserVisibleHostProofClass::VisibleGreen);
    assert!(visible.is_visible_green());
    assert_eq!(
        BROWSER_VISIBLE_WEBVIEW2_OPT_IN_ENV,
        "DEVMANAGER_BROWSER_WEBVIEW2_E2E"
    );
}

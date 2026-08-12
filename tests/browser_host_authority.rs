use devmanager::browser::BrowserNativeViewError;
use devmanager::domain::id::{BrowserContextId, ClientId, ResourceId, TaskId};
use devmanager::protocol::{
    browser_logical_to_physical, BrowserAttachmentLease, BrowserBoundsEpoch,
    BrowserCoordinateSpace, BrowserDpi, BrowserFocusEpoch, BrowserFrame, BrowserHostFence,
    BrowserLogicalBounds, BrowserPhysicalBounds, BrowserPhysicalPoint, BrowserRuntimeGeneration,
    BrowserSurfaceDescriptor, BrowserSurfaceInput, BrowserSurfaceLifecycle, BrowserSurfaceNonce,
    BrowserWindowHandle, MAX_BROWSER_FRAME_BYTES, MAX_BROWSER_TEXT_BYTES, MAX_BROWSER_TOKEN_BYTES,
};
use serde_json::json;

#[test]
fn browser_protocol_rejects_forged_stale_and_unbounded_values() {
    assert!(serde_json::from_str::<BrowserBoundsEpoch>("0").is_err());
    assert!(serde_json::from_str::<BrowserFocusEpoch>("0").is_err());
    assert!(serde_json::from_str::<BrowserRuntimeGeneration>("0").is_err());
    assert!(
        serde_json::from_str::<BrowserHostFence>(r#"{"bootEpoch":0,"connectionEpoch":1}"#).is_err()
    );
    assert!(serde_json::from_str::<BrowserDpi>(r#"{"horizontal":0,"vertical":96}"#).is_err());
    assert!(
        serde_json::from_str::<BrowserPhysicalBounds>(r#"{"x":0,"y":0,"width":0,"height":1}"#)
            .is_err()
    );
    assert!(serde_json::from_str::<BrowserWindowHandle>("\"hwnd:0\"").is_err());
    assert!(serde_json::from_str::<BrowserAttachmentLease>("\"\"").is_err());

    let unknown = json!({
        "identity": {
            "taskId": TaskId::new(),
            "contextId": BrowserContextId::new(),
            "resourceId": ResourceId::new(),
        },
        "childHwnd": "hwnd:1",
        "hostProcess": {"pid": 1, "creationTime100ns": 1, "executable": "host.exe"},
        "hostFence": {"bootEpoch": 1, "connectionEpoch": 1},
        "runtimeGeneration": 1,
        "nonce": [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        "boundsEpoch": 1,
        "focusEpoch": 1,
        "physicalBounds": {"x": 0, "y": 0, "width": 1, "height": 1},
        "dpi": {"horizontal": 96, "vertical": 96},
        "unknown": true,
    });
    assert!(serde_json::from_value::<BrowserSurfaceDescriptor>(unknown).is_err());
}

#[test]
fn browser_protocol_rejects_invalid_geometry_and_frame_payloads() {
    assert!(BrowserPhysicalBounds::new(0, 0, 0, 1).is_err());
    assert!(BrowserDpi::new(0, 96).is_err());
    assert!(browser_logical_to_physical(
        BrowserLogicalBounds::new(i64::MAX / 2, 0, 1, 1).unwrap(),
        BrowserDpi::new(3_840, 3_840).unwrap(),
        BrowserPhysicalPoint::new(0, 0),
        BrowserCoordinateSpace::Local,
    )
    .is_err());
    assert!(browser_logical_to_physical(
        BrowserLogicalBounds::new(1, 2, 3, 4).unwrap(),
        BrowserDpi::new(96, 96).unwrap(),
        BrowserPhysicalPoint::new(1, 1),
        BrowserCoordinateSpace::Local,
    )
    .is_err());
    assert!(BrowserFrame::new(1, vec![0; MAX_BROWSER_FRAME_BYTES + 1]).is_err());
    assert!(serde_json::from_value::<BrowserFrame>(json!({
        "frameId": 0,
        "bytes": [],
    }))
    .is_err());
    assert!(serde_json::from_value::<BrowserFrame>(json!({
        "frameId": 1,
        "bytes": vec![0_u8; MAX_BROWSER_FRAME_BYTES + 1],
    }))
    .is_err());
    assert!(serde_json::from_value::<BrowserSurfaceLifecycle>(json!({
        "state": "attached",
        "clientId": ClientId::new(),
        "unknown": true,
    }))
    .is_err());
    assert!(BrowserSurfaceInput::text("x".repeat(MAX_BROWSER_TEXT_BYTES + 1)).is_err());
    assert!(
        BrowserSurfaceInput::trusted_click(0, 0, "x".repeat(MAX_BROWSER_TOKEN_BYTES + 1)).is_err()
    );
}

#[test]
fn browser_geometry_preserves_negative_scaling_and_supported_steps() {
    let physical = browser_logical_to_physical(
        BrowserLogicalBounds::new(-1, -1, 1, 1).unwrap(),
        BrowserDpi::new(120, 120).unwrap(),
        BrowserPhysicalPoint::new(0, 0),
        BrowserCoordinateSpace::Local,
    )
    .unwrap();
    assert_eq!(physical.x(), -2);
    assert_eq!(physical.y(), -2);
    assert_eq!(physical.width(), 2);
    assert_eq!(physical.height(), 2);

    let bounds = BrowserLogicalBounds::new(0, 0, 320, 200).unwrap();
    for (dpi, expected_width, expected_height) in [
        (96, 320, 200),
        (120, 400, 250),
        (144, 480, 300),
        (192, 640, 400),
    ] {
        let physical = browser_logical_to_physical(
            bounds,
            BrowserDpi::new(dpi, dpi).unwrap(),
            BrowserPhysicalPoint::new(0, 0),
            BrowserCoordinateSpace::Local,
        )
        .unwrap();
        assert_eq!(physical.width(), expected_width);
        assert_eq!(physical.height(), expected_height);
    }

    assert!(BrowserPhysicalBounds::new(i32::MAX, 0, 1, 1).is_err());
    assert!(BrowserPhysicalBounds::new(0, i32::MAX, 1, 1).is_err());
}

#[test]
fn browser_bearer_debug_is_redacted() {
    let nonce: BrowserSurfaceNonce = serde_json::from_value(json!([
        171, 171, 171, 171, 171, 171, 171, 171, 171, 171, 171, 171, 171, 171, 171, 171
    ]))
    .unwrap();
    let attachment = BrowserAttachmentLease::from_wire("cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd").unwrap();

    let nonce_debug = format!("{nonce:?}");
    let attachment_debug = format!("{attachment:?}");
    assert!(!nonce_debug.contains("ab"));
    assert!(!attachment_debug.contains("cd"));
    assert!(nonce_debug.contains("redacted"));
    assert!(attachment_debug.contains("redacted"));
}

#[test]
fn browser_diagnostics_redact_native_handles_process_paths_and_attacker_text() {
    let handle = BrowserWindowHandle::from_raw(0xfeed_beef).unwrap();
    let process = devmanager::protocol::BrowserHostProcessIdentity::new(
        41,
        9_001,
        "C:\\attacker\\browser-host.exe",
    )
    .unwrap();
    let input = BrowserSurfaceInput::text("attacker supplied page text").unwrap();
    let frame = BrowserFrame::new(1, b"attacker frame bytes".to_vec()).unwrap();

    assert!(!format!("{handle:?}").contains("feed"));
    assert!(!format!("{handle}").contains("feed"));
    assert!(!format!("{process:?}").contains("attacker"));
    assert!(!format!("{input:?}").contains("attacker"));
    assert!(!format!("{frame:?}").contains("attacker"));
}

#[test]
fn browser_errors_redact_backend_text() {
    let error = BrowserNativeViewError::Backend;
    assert!(!format!("{error:?}").contains("attacker"));
    assert!(!error.to_string().contains("attacker"));
}

#[cfg(target_pointer_width = "64")]
#[test]
fn browser_window_handle_round_trips_full_supported_64_bit_values() {
    for raw in [1, isize::MAX as u64, usize::MAX as u64] {
        let handle = BrowserWindowHandle::from_raw(raw).unwrap();
        assert_eq!(handle.raw_value(), raw);
        assert_eq!(
            BrowserWindowHandle::from_wire(handle.wire_value()).unwrap(),
            handle
        );
        let decoded: BrowserWindowHandle =
            serde_json::from_value(serde_json::to_value(&handle).unwrap()).unwrap();
        assert_eq!(decoded, handle);
    }
}

#[cfg(target_pointer_width = "32")]
#[test]
fn browser_window_handle_enforces_32_bit_bounds() {
    let handle = BrowserWindowHandle::from_raw(u32::MAX as u64).unwrap();
    assert_eq!(handle.raw_value(), u32::MAX as u64);
    assert!(BrowserWindowHandle::from_raw(u32::MAX as u64 + 1).is_err());
}

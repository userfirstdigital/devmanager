use devmanager::browser::{
    BrowserAnnotation, BrowserRevision, BrowserTabSnapshot, BrowserViewport,
    BrowserWorkspaceSnapshot,
};
use serde_json::json;

fn annotation(id: &str, comment: &str) -> BrowserAnnotation {
    serde_json::from_value(json!({
        "id": id,
        "kind": "element",
        "tabId": "tab-1",
        "anchorRevision": 3,
        "comment": comment,
        "url": "https://example.test/form",
        "locator": {
            "accessibilityRole": "button",
            "accessibilityName": "Save",
            "testId": "save",
            "cssSelectors": ["[data-testid=save]", "button"]
        },
        "bounds": { "x": 10, "y": 20, "width": 120, "height": 32 },
        "viewport": { "width": 1280, "height": 720, "scalePercent": 100 },
        "screenshotResource": "resource-1",
        "computedStyles": { "display": "block" },
        "resolved": false
    }))
    .expect("valid annotation fixture")
}

fn workspace() -> BrowserWorkspaceSnapshot {
    BrowserWorkspaceSnapshot {
        revision: BrowserRevision(3),
        tabs: vec![BrowserTabSnapshot {
            id: "tab-1".to_string(),
            title: "Fixture".to_string(),
            url: "https://example.test/form".to_string(),
            viewport: BrowserViewport::default(),
        }],
        selected_tab_id: Some("tab-1".to_string()),
        ..BrowserWorkspaceSnapshot::default()
    }
}

#[test]
fn pending_annotation_ids_are_serde_defaulted_and_round_trip() {
    let legacy: BrowserWorkspaceSnapshot = serde_json::from_value(json!({
        "paneOpen": false,
        "splitPercent": 50
    }))
    .expect("legacy workspace deserializes");
    let legacy_json = serde_json::to_value(legacy).expect("workspace serializes");
    assert_eq!(legacy_json["pendingAnnotationIds"], json!([]));

    let restored: BrowserWorkspaceSnapshot = serde_json::from_value(json!({
        "pendingAnnotationIds": ["annotation-1", "annotation-2"]
    }))
    .expect("pending annotations deserialize");
    let restored_json = serde_json::to_value(restored).expect("workspace serializes");
    assert_eq!(
        restored_json["pendingAnnotationIds"],
        json!(["annotation-1", "annotation-2"])
    );
}

#[test]
fn legacy_annotations_receive_backward_compatible_anchor_defaults() {
    let legacy: BrowserAnnotation = serde_json::from_value(json!({
        "id": "legacy",
        "comment": "Legacy note",
        "url": "https://example.test/legacy",
        "locator": {},
        "bounds": { "x": 0, "y": 0, "width": 1, "height": 1 },
        "viewport": { "width": 1280, "height": 720, "scalePercent": 100 },
        "screenshotResource": "legacy-resource",
        "computedStyles": {},
        "resolved": false
    }))
    .expect("legacy annotation deserializes");

    let value = serde_json::to_value(legacy).expect("annotation serializes");
    assert_eq!(value["kind"], "element");
    assert_eq!(value["tabId"], "");
    assert_eq!(value["anchorRevision"], 0);
}

#[test]
fn save_queues_annotation_and_rejects_blank_or_duplicate_ids() {
    let mut snapshot = workspace();

    snapshot
        .save_annotation(annotation("annotation-1", "Review the save button"))
        .expect("annotation saves");
    assert_eq!(snapshot.pending_annotation_ids, vec!["annotation-1"]);
    assert_eq!(
        snapshot.annotation("annotation-1").unwrap().comment,
        "Review the save button"
    );

    let blank = snapshot
        .save_annotation(annotation("annotation-2", "  \n "))
        .expect_err("blank comments are invalid");
    assert!(blank.to_string().contains("comment"));

    let duplicate = snapshot
        .save_annotation(annotation("annotation-1", "Different"))
        .expect_err("duplicate ids are invalid");
    assert!(duplicate.to_string().contains("annotation-1"));
}

#[test]
fn resolve_unresolve_delete_and_pending_ack_are_idempotent() {
    let mut snapshot = workspace();
    snapshot
        .save_annotation(annotation("annotation-1", "First"))
        .unwrap();
    snapshot
        .save_annotation(annotation("annotation-2", "Second"))
        .unwrap();

    assert!(snapshot
        .set_annotation_resolved("annotation-1", true)
        .unwrap());
    assert!(!snapshot
        .set_annotation_resolved("annotation-1", true)
        .unwrap());
    assert!(snapshot.annotation("annotation-1").unwrap().resolved);
    assert!(snapshot
        .set_annotation_resolved("annotation-1", false)
        .unwrap());

    assert!(snapshot.remove_pending_annotation("annotation-2"));
    assert!(!snapshot.remove_pending_annotation("annotation-2"));
    snapshot.acknowledge_pending_annotations(&["annotation-1".to_string()]);
    assert!(snapshot.pending_annotation_ids.is_empty());

    let deleted = snapshot.delete_annotation("annotation-1").unwrap();
    assert_eq!(deleted.id, "annotation-1");
    assert!(snapshot.annotation("annotation-1").is_err());
    assert!(snapshot.delete_annotation("annotation-1").is_err());
}

#[test]
fn anchor_staleness_preserves_persisted_annotation_context() {
    let mut snapshot = workspace();
    snapshot
        .save_annotation(annotation("annotation-1", "Keep this context"))
        .unwrap();
    assert!(!snapshot.annotation_anchor_is_stale("annotation-1").unwrap());

    snapshot.advance_revision();
    assert!(snapshot.annotation_anchor_is_stale("annotation-1").unwrap());
    let persisted = snapshot.annotation("annotation-1").unwrap();
    assert_eq!(persisted.comment, "Keep this context");
    assert_eq!(persisted.screenshot_resource.0, "resource-1");
}

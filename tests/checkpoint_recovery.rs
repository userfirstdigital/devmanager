use devmanager::workspace::checkpoint::{CheckpointMetadataDecodeError, DurableCheckpointMetadata};

#[test]
fn external_checkpoint_projection_rejects_oversize_without_test_authority() {
    let error = DurableCheckpointMetadata::decode_json(&vec![b' '; 1024 * 1024 + 1])
        .expect_err("oversize metadata must fail closed");
    assert_eq!(error, CheckpointMetadataDecodeError::TooLarge);
}

use std::cell::Cell;

use devmanager::domain::id::ResourceId;
use devmanager::domain::operation::ResourceFence;
use devmanager::kernel::{
    CompletionDisposition, RuntimePresence, RuntimeRegistry, RuntimeRegistryError,
};

fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
    [
        0x01, 0x9a, 0x11, 0x22, 0x33, 0x44, 0x70, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        tail,
    ]
}

fn resource_id(tail: u8) -> ResourceId {
    ResourceId::from_bytes(fixed_uuid_v7(tail)).expect("resource id")
}

#[test]
fn runtime_generation_rejects_stale_completion() {
    let resource = resource_id(1);
    let mut registry = RuntimeRegistry::new();
    registry
        .install_current(ResourceFence::new(resource, 3))
        .expect("install current generation");

    let callback_count = Cell::new(0);
    let disposition = registry.apply_completion(ResourceFence::new(resource, 2), || {
        callback_count.set(callback_count.get() + 1);
    });

    assert_eq!(disposition, CompletionDisposition::Stale);
    assert_eq!(callback_count.get(), 0, "stale callback must be dropped");
}

#[test]
fn runtime_registry_rejects_duplicate_or_regressed_generation() {
    let resource = resource_id(2);
    let mut registry = RuntimeRegistry::new();
    registry
        .install_current(ResourceFence::new(resource, 7))
        .expect("install current generation");

    for proposed_generation in [7, 6] {
        assert_eq!(
            registry.install_current(ResourceFence::new(resource, proposed_generation)),
            Err(RuntimeRegistryError::GenerationNotAdvanced {
                resource_id: resource,
                current_generation: 7,
                proposed_generation,
            })
        );
    }

    assert_eq!(
        registry.current_fence(resource),
        Some(ResourceFence::new(resource, 7))
    );
}

#[test]
fn runtime_registry_generation_exhaustion_fails_closed() {
    let resource = resource_id(3);
    let mut registry = RuntimeRegistry::new();
    registry
        .install_current(ResourceFence::new(resource, u64::MAX))
        .expect("install terminal generation");

    assert_eq!(
        registry.next_generation(resource),
        Err(RuntimeRegistryError::GenerationExhausted {
            resource_id: resource,
        })
    );
    assert_eq!(
        registry.current_fence(resource),
        Some(ResourceFence::new(resource, u64::MAX))
    );
}

#[test]
fn recovering_generation_does_not_accept_completion() {
    let resource = resource_id(4);
    let fence = ResourceFence::new(resource, 11);
    let mut registry = RuntimeRegistry::new();
    registry
        .install_recovering(fence)
        .expect("install recovery candidate");

    let callback_count = Cell::new(0);
    let disposition = registry.apply_completion(fence, || {
        callback_count.set(callback_count.get() + 1);
    });
    assert_eq!(disposition, CompletionDisposition::Recovering);
    assert_eq!(callback_count.get(), 0);
    assert_eq!(
        registry.presence(resource),
        Some(RuntimePresence::Recovering)
    );

    registry
        .promote_recovered(fence)
        .expect("promote exact recovered generation");
    assert_eq!(
        registry.apply_completion(fence, || {
            callback_count.set(callback_count.get() + 1);
        }),
        CompletionDisposition::Current
    );
    assert_eq!(callback_count.get(), 1);
}

#[test]
fn replacement_generation_invalidates_prior_completion() {
    let resource = resource_id(5);
    let generation_1 = ResourceFence::new(resource, 20);
    let generation_2 = ResourceFence::new(resource, 21);
    let mut registry = RuntimeRegistry::new();
    registry
        .install_current(generation_1)
        .expect("install first generation");
    assert_eq!(registry.next_generation(resource), Ok(generation_2));
    registry
        .install_current(generation_2)
        .expect("install durable replacement generation");

    assert_eq!(
        registry.apply_completion(generation_1, || panic!("stale completion ran")),
        CompletionDisposition::Stale
    );
    assert_eq!(
        registry.apply_completion(generation_2, || {}),
        CompletionDisposition::Current
    );
}

#[test]
fn retired_generation_blocks_late_completion_and_reuse() {
    let resource = resource_id(6);
    let retired = ResourceFence::new(resource, 30);
    let replacement = ResourceFence::new(resource, 31);
    let mut registry = RuntimeRegistry::new();
    registry
        .install_current(retired)
        .expect("install current generation");
    registry.retire(retired).expect("retire exact generation");

    assert_eq!(registry.presence(resource), Some(RuntimePresence::Inactive));
    assert_eq!(
        registry.apply_completion(retired, || panic!("retired completion ran")),
        CompletionDisposition::Stale
    );
    assert_eq!(
        registry.install_current(retired),
        Err(RuntimeRegistryError::GenerationNotAdvanced {
            resource_id: resource,
            current_generation: 30,
            proposed_generation: 30,
        })
    );

    registry
        .install_current(replacement)
        .expect("install strictly newer generation");
    assert_eq!(
        registry.apply_completion(replacement, || {}),
        CompletionDisposition::Current
    );
}

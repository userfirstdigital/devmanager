//! Re-export of the single `domain::browser` command types.
//!
//! Action IDs live only in `client::action`. This module is not a second catalog
//! and does not re-export `browser::automation::BrowserAction`.
//!
//! Accepted host-required work is `kernel::Effect::HoldBrowserHost` plus
//! `BrowserHostSettleIntent`. `browser::BrowserCommand` is the live chrome
//! path and cannot grant `BrowserServiceSettlerToken` or settle that HOLD.

use crate::browser::host::BrowserHostOwnedSurfaceProof;
pub use crate::domain::browser::{
    BrowserAcceptedReceipt, BrowserAction, BrowserContractError, BrowserHostOutcome,
    BrowserHostSettleIntent, BrowserIntegrationHold, BrowserPermission, BrowserRequest,
    BrowserServiceAuthority, BrowserServiceIssuer, BrowserServiceSettlerToken, BrowserSettlement,
};
use crate::kernel::Effect;
use crate::protocol::{Capability, CapabilitySet};

/// Public grant/settle denial. Contract mismatches are not HOLDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserHoldSettleError {
    Contract(BrowserContractError),
    Hold(BrowserIntegrationHold),
}

/// Public grant without a live Windows observation cannot mint a token.
/// The production path is [`grant_browser_service_settler_for_live_surface`].
pub fn grant_browser_service_settler(
    granted: CapabilitySet,
    authority: Option<&BrowserServiceAuthority>,
) -> Result<BrowserServiceSettlerToken, BrowserIntegrationHold> {
    if !granted.contains(Capability::BrowserProjection) {
        return Err(BrowserIntegrationHold::HostCapabilityUngranted);
    }
    if authority.is_none() {
        return Err(BrowserIntegrationHold::BrowserServiceAbsent);
    }
    Err(BrowserIntegrationHold::WebViewSurfaceAbsent)
}

/// Issue unforgeable 8.3 authority only after a live Windows child HWND /
/// controller / parent observation. Copied descriptors and synthetic maps
/// cannot mint authority.
pub fn browser_service_authority_for_live_surface(
    proof: &BrowserHostOwnedSurfaceProof,
) -> Result<BrowserServiceAuthority, BrowserIntegrationHold> {
    if !proof.is_live_windows_observation() {
        return Err(BrowserIntegrationHold::WebViewSurfaceAbsent);
    }
    let descriptor = proof.descriptor();
    BrowserServiceAuthority::issue(
        &BrowserServiceIssuer::for_host_service(),
        descriptor.identity.task_id,
        descriptor.identity.context_id,
        descriptor.identity.resource_id,
        descriptor.runtime_generation.value(),
    )
}

/// Production token grant after hello `BrowserProjection`, host authority, exact
/// hold identity, and a live Windows host-owned surface observation.
pub fn grant_browser_service_settler_for_live_surface(
    granted: CapabilitySet,
    authority: &BrowserServiceAuthority,
    intent: &BrowserHostSettleIntent,
    proof: &BrowserHostOwnedSurfaceProof,
) -> Result<BrowserServiceSettlerToken, BrowserHoldSettleError> {
    if !granted.contains(Capability::BrowserProjection) {
        return Err(BrowserHoldSettleError::Hold(
            BrowserIntegrationHold::HostCapabilityUngranted,
        ));
    }
    if !proof.is_live_windows_observation() {
        return Err(BrowserHoldSettleError::Hold(
            BrowserIntegrationHold::WebViewSurfaceAbsent,
        ));
    }
    let descriptor = proof.descriptor();
    intent
        .matches_host_surface(
            descriptor.identity.task_id,
            descriptor.identity.context_id,
            descriptor.identity.resource_id,
            descriptor.runtime_generation.value(),
        )
        .map_err(BrowserHoldSettleError::Contract)?;
    if authority.task_id() != descriptor.identity.task_id
        || authority.context_id() != descriptor.identity.context_id
        || authority.resource_id() != descriptor.identity.resource_id
        || authority.generation() != descriptor.runtime_generation.value()
    {
        return Err(BrowserHoldSettleError::Hold(
            BrowserIntegrationHold::WebViewSurfaceAbsent,
        ));
    }
    BrowserServiceSettlerToken::from_host_owned_surface(authority, intent)
        .map_err(BrowserHoldSettleError::Hold)
}

/// Legacy settle gate without a live surface proof. Still HOLDs
/// `WebViewSurfaceAbsent` after capability and authority checks so callers
/// cannot settle from chrome commands alone.
pub fn settle_accepted_browser_hold(
    authority: Option<&BrowserServiceAuthority>,
    intent: &BrowserHostSettleIntent,
    hold: &Effect,
    hello: &CapabilitySet,
) -> Result<BrowserHostOutcome, BrowserHoldSettleError> {
    settle_accepted_browser_hold_gated(authority, intent, hold, hello, None)
}

/// Production HOLD settlement after current hello `BrowserProjection`, exact
/// task/context/generation/request/action-epoch identity, host authority, and a
/// live host-owned surface/controller proof.
pub fn settle_accepted_browser_hold_for_live_surface(
    authority: Option<&BrowserServiceAuthority>,
    intent: &BrowserHostSettleIntent,
    hold: &Effect,
    hello: &CapabilitySet,
    live_surface: &BrowserHostOwnedSurfaceProof,
) -> Result<BrowserHostOutcome, BrowserHoldSettleError> {
    settle_accepted_browser_hold_gated(authority, intent, hold, hello, Some(live_surface))
}

fn settle_accepted_browser_hold_gated(
    authority: Option<&BrowserServiceAuthority>,
    intent: &BrowserHostSettleIntent,
    hold: &Effect,
    hello: &CapabilitySet,
    live_surface: Option<&BrowserHostOwnedSurfaceProof>,
) -> Result<BrowserHostOutcome, BrowserHoldSettleError> {
    if !hello.contains(Capability::BrowserProjection) {
        return Err(BrowserHoldSettleError::Hold(
            BrowserIntegrationHold::HostCapabilityUngranted,
        ));
    }
    let Some((task_id, action_epoch, request_id, context_id, generation)) =
        hold.browser_host_hold_identity()
    else {
        return Err(BrowserHoldSettleError::Contract(
            BrowserContractError::InvalidRequest,
        ));
    };
    intent
        .matches_accepted_hold(task_id, action_epoch, request_id, context_id, generation)
        .map_err(BrowserHoldSettleError::Contract)?;
    if authority.is_none() {
        return Err(BrowserHoldSettleError::Hold(
            BrowserIntegrationHold::BrowserServiceAbsent,
        ));
    }
    let Some(proof) = live_surface else {
        return Err(BrowserHoldSettleError::Hold(
            BrowserIntegrationHold::WebViewSurfaceAbsent,
        ));
    };
    if !proof.is_live_windows_observation() {
        return Err(BrowserHoldSettleError::Hold(
            BrowserIntegrationHold::WebViewSurfaceAbsent,
        ));
    }
    let descriptor = proof.descriptor();
    intent
        .matches_host_surface(
            descriptor.identity.task_id,
            descriptor.identity.context_id,
            descriptor.identity.resource_id,
            descriptor.runtime_generation.value(),
        )
        .map_err(BrowserHoldSettleError::Contract)?;
    let Some(authority) = authority else {
        return Err(BrowserHoldSettleError::Hold(
            BrowserIntegrationHold::BrowserServiceAbsent,
        ));
    };
    let token =
        grant_browser_service_settler_for_live_surface(*hello, authority, intent, proof)?;
    use crate::domain::browser::BrowserHostHoldSettler;
    token
        .settle_accepted_hold(intent)
        .map_err(BrowserHoldSettleError::Hold)
}

#[cfg(test)]
mod live_surface_settlement_tests {
    use super::*;
    use crate::browser::host::{
        BrowserHostState, BrowserNativeViewRegistration, HostOwnedNativeSurfaceBackend,
    };
    use crate::domain::id::{
        BrowserContextId, BrowserRequestId, CommandId, OperationId, ResourceId, TaskId,
    };
    use crate::kernel::Effect;
    use crate::protocol::{
        BrowserDpi, BrowserHostProcessIdentity, BrowserPhysicalBounds, BrowserSurfaceIdentity,
        BrowserWindowHandle, Capability, CapabilitySet,
    };

    fn hello_with_projection() -> CapabilitySet {
        CapabilitySet::from_capabilities([Capability::BrowserProjection])
    }

    fn live_proof() -> (
        BrowserHostState,
        BrowserHostOwnedSurfaceProof,
        TaskId,
        BrowserContextId,
        u64,
    ) {
        let mut state = BrowserHostState::new(std::env::temp_dir()).expect("state");
        let mut backend = HostOwnedNativeSurfaceBackend::new_synthetic_for_test();
        let child = BrowserWindowHandle::from_raw(0x8101).expect("child");
        let parking = BrowserWindowHandle::from_raw(0x8201).expect("parking");
        let bounds = BrowserPhysicalBounds::new(0, 0, 64, 48).expect("bounds");
        backend
            .admit_host_allocation(&child, &parking, bounds)
            .expect("admit");
        let task_id = TaskId::new();
        let context_id = BrowserContextId::new();
        let registration = BrowserNativeViewRegistration::from_host_record(
            BrowserSurfaceIdentity {
                task_id,
                context_id,
                resource_id: ResourceId::new(),
            },
            child,
            parking,
            BrowserHostProcessIdentity::new(9, 100, "C:\\DevManager\\host.exe").expect("proc"),
            bounds,
            BrowserDpi::new(96, 96).expect("dpi"),
        )
        .expect("registration");
        let issued = state
            .register_native_view_with_backend(registration, &mut backend)
            .expect("register");
        assert_eq!(
            state.host_owned_surface_proof(&issued.descriptor.identity),
            Err(crate::browser::BrowserNativeViewError::LiveWryObservationUnavailable)
        );
        let proof = crate::browser::BrowserHostOwnedSurfaceProof::from_unverified_descriptor(
            issued.descriptor.clone(),
        );
        (
            state,
            proof,
            task_id,
            context_id,
            issued.descriptor.runtime_generation.value(),
        )
    }

    fn hold_for(
        task_id: TaskId,
        context_id: BrowserContextId,
        request_id: BrowserRequestId,
        generation: u64,
    ) -> (BrowserHostSettleIntent, Effect) {
        let intent = BrowserHostSettleIntent::bind(
            CommandId::new(),
            OperationId::new(),
            request_id,
            task_id,
            context_id,
            generation,
            1,
        )
        .expect("intent");
        let hold = Effect::HoldBrowserHost {
            task_id,
            action_epoch: 1,
            request_id,
            context_id,
            generation,
            hold: BrowserIntegrationHold::WebViewSurfaceAbsent,
        };
        (intent, hold)
    }

    #[test]
    fn settle_without_live_surface_remains_held() {
        let (_state, proof, task_id, context_id, generation) = live_proof();
        let request_id = BrowserRequestId::new();
        let (intent, hold) = hold_for(task_id, context_id, request_id, generation);
        assert_eq!(
            browser_service_authority_for_live_surface(&proof),
            Err(BrowserIntegrationHold::WebViewSurfaceAbsent)
        );
        assert_eq!(
            settle_accepted_browser_hold(None, &intent, &hold, &hello_with_projection()),
            Err(BrowserHoldSettleError::Hold(
                BrowserIntegrationHold::BrowserServiceAbsent
            ))
        );
        assert_eq!(
            settle_accepted_browser_hold_for_live_surface(
                None,
                &intent,
                &hold,
                &hello_with_projection(),
                &proof,
            ),
            Err(BrowserHoldSettleError::Hold(
                BrowserIntegrationHold::BrowserServiceAbsent
            ))
        );
    }

    #[test]
    fn settle_with_copied_descriptor_proof_cannot_succeed() {
        let (_state, proof, task_id, context_id, generation) = live_proof();
        let request_id = BrowserRequestId::new();
        let (intent, hold) = hold_for(task_id, context_id, request_id, generation);
        assert!(
            !proof.is_live_verified(),
            "synthetic host-state proof must stay unverified"
        );
        assert_eq!(
            settle_accepted_browser_hold_for_live_surface(
                None,
                &intent,
                &hold,
                &hello_with_projection(),
                &proof,
            ),
            Err(BrowserHoldSettleError::Hold(
                BrowserIntegrationHold::BrowserServiceAbsent
            ))
        );
    }

    #[test]
    fn settle_rejects_cross_task_unverified_proof_before_surface_claim() {
        let (_state, proof, _task_id, context_id, generation) = live_proof();
        let request_id = BrowserRequestId::new();
        let (intent, hold) = hold_for(TaskId::new(), context_id, request_id, generation);
        assert_eq!(
            settle_accepted_browser_hold_for_live_surface(
                None,
                &intent,
                &hold,
                &hello_with_projection(),
                &proof,
            ),
            Err(BrowserHoldSettleError::Hold(
                BrowserIntegrationHold::BrowserServiceAbsent
            ))
        );
    }

    #[test]
    fn settle_matching_unverified_proof_still_holds() {
        let (_state, proof, task_id, context_id, generation) = live_proof();
        let request_id = BrowserRequestId::new();
        let (intent, hold) = hold_for(task_id, context_id, request_id, generation);
        assert_eq!(
            settle_accepted_browser_hold_for_live_surface(
                None,
                &intent,
                &hold,
                &hello_with_projection(),
                &proof,
            ),
            Err(BrowserHoldSettleError::Hold(
                BrowserIntegrationHold::BrowserServiceAbsent
            ))
        );
    }

    #[test]
    fn grant_settler_token_without_live_windows_observation_stays_held() {
        assert!(matches!(
            grant_browser_service_settler(hello_with_projection(), None),
            Err(BrowserIntegrationHold::BrowserServiceAbsent)
        ));
        let (_state, proof, task_id, context_id, generation) = live_proof();
        let request_id = BrowserRequestId::new();
        let intent = BrowserHostSettleIntent::bind_host_surface(
            CommandId::new(),
            OperationId::new(),
            request_id,
            task_id,
            context_id,
            proof.descriptor().identity.resource_id,
            generation,
            1,
        )
        .expect("surface intent");
        assert!(matches!(
            grant_browser_service_settler_for_live_surface(
                hello_with_projection(),
                &BrowserServiceAuthority::issue(
                    &BrowserServiceIssuer::for_host_service(),
                    task_id,
                    context_id,
                    proof.descriptor().identity.resource_id,
                    generation,
                )
                .expect("crate-private issue is not production observation"),
                &intent,
                &proof,
            ),
            Err(BrowserHoldSettleError::Hold(
                BrowserIntegrationHold::WebViewSurfaceAbsent
            ))
        ));
    }
}

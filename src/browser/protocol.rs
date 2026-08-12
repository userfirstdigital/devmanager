//! Re-export of the single `domain::browser` command types.
//!
//! Action IDs live only in `client::action`. This module is not a second catalog
//! and does not re-export `browser::automation::BrowserAction`.
//!
//! Accepted host-required work is `kernel::Effect::HoldBrowserHost` plus
//! `BrowserHostSettleIntent`. `browser::BrowserCommand` is the live chrome
//! path and cannot grant `BrowserServiceSettlerToken` or settle that HOLD.

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

/// Public grant path. Requires authenticated host-hello `BrowserProjection`
/// *and* a future 8.3 `BrowserServiceAuthority`. `OperationSettlement` is not
/// a substitute. A missing authority never mints a token. No bool mint exists.
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

/// Accepted HOLD settlement gate. Requires current hello `BrowserProjection`
/// and exact task/context/generation/request/epoch identity. Authority is
/// issued only by the future host-owned service; without it this HOLDs
/// `BrowserServiceAbsent`. A present authority still HOLDs
/// `WebViewSurfaceAbsent`. Never succeeds. Legacy chrome commands are not
/// an input.
pub fn settle_accepted_browser_hold(
    authority: Option<&BrowserServiceAuthority>,
    intent: &BrowserHostSettleIntent,
    hold: &Effect,
    hello: &CapabilitySet,
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
    Err(BrowserHoldSettleError::Hold(
        BrowserIntegrationHold::WebViewSurfaceAbsent,
    ))
}

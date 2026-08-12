//! Task 4.8 policy helpers that sit on canonical Command/Event facts.
//!
//! This module does not own task state, roles, or a RAM journal. Durable
//! primary/specialist transitions live in `domain::command::decide` and
//! `domain::event::apply`. Session launch and destination input settlement
//! remain typed HOLDs when the missing authority is outside provider scope.
//!
//! The approved task model is exactly one Primary plus optional Specialists.
//! Native-child observation is not a second harness; subprocess evidence stays
//! behind a correlated authenticated journal receipt. Uncorrelated specialist
//! JSON is never accepted as provider truth. Free stock ingress is unavailable.

pub use crate::domain::command::{
    SpecialistPermission, SpecialistResult, SpecialistStatus, DEFAULT_MAX_TOP_LEVEL_RUNTIMES,
};
use crate::providers::journal::stock_adapter_ingress_available;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrchestrationHold {
    ProviderRuntimeAuthorityAbsent,
    ProviderJournalAbsent,
    ProviderInputAbsent,
    ProcessCapabilityUnbound,
    DuplicatePrimaryOwnership,
    UncorrelatedSpecialistResult,
}

pub fn specialist_cancel_hold() -> OrchestrationHold {
    // Cancel still requires process/runtime authority owned outside providers.
    OrchestrationHold::ProviderRuntimeAuthorityAbsent
}

pub fn specialist_write_hold() -> OrchestrationHold {
    // Writable specialist workspaces need process capability binding outside
    // the provider-owned policy seam.
    OrchestrationHold::ProcessCapabilityUnbound
}

/// Optional specialist subprocess observation remains journal-gated. This is
/// not a NativeChild role or parallel orchestration harness.
pub fn specialist_native_child_hold() -> OrchestrationHold {
    OrchestrationHold::ProviderJournalAbsent
}

/// Structured result evidence is not accepted until a stock provider emits a
/// correlated journal receipt.  Keeping this as a typed HOLD prevents a
/// caller-shaped JSON object from becoming provider truth.
pub fn specialist_structured_result_hold() -> OrchestrationHold {
    OrchestrationHold::ProviderJournalAbsent
}

pub fn validate_specialist_result(result: &SpecialistResult) -> Result<(), OrchestrationHold> {
    result
        .validate()
        .map_err(|_| OrchestrationHold::ProviderJournalAbsent)
}

/// Fail closed when a second Primary would be introduced for the same task.
pub fn ensure_single_primary(
    existing_primary: bool,
    requesting_primary: bool,
) -> Result<(), OrchestrationHold> {
    if existing_primary && requesting_primary {
        Err(OrchestrationHold::DuplicatePrimaryOwnership)
    } else {
        Ok(())
    }
}

/// Specialist structured results stay HOLD while free stock ingress is closed.
/// Authenticated journal receipts are required before acceptance.
pub fn accept_specialist_result_with_stock_ingress(
    result: &SpecialistResult,
) -> Result<(), OrchestrationHold> {
    if !stock_adapter_ingress_available() {
        return Err(specialist_structured_result_hold());
    }
    result
        .validate()
        .map_err(|_| OrchestrationHold::UncorrelatedSpecialistResult)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_primary_policy_rejects_duplicate_ownership() {
        assert!(ensure_single_primary(false, true).is_ok());
        assert_eq!(
            ensure_single_primary(true, true),
            Err(OrchestrationHold::DuplicatePrimaryOwnership)
        );
    }

    #[test]
    fn specialist_result_stays_hold_while_stock_ingress_unavailable() {
        let ok = SpecialistResult {
            role: "specialist".into(),
            status: SpecialistStatus::Completed,
            summary: "done".into(),
            evidence: Vec::new(),
            artifacts: Vec::new(),
            workspace: None,
            commit: None,
            requested_follow_up: None,
        };
        assert_eq!(
            accept_specialist_result_with_stock_ingress(&ok),
            Err(OrchestrationHold::ProviderJournalAbsent)
        );
    }
}

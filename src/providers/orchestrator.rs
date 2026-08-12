//! Task 4.8 policy helpers that sit on canonical Command/Event facts.
//!
//! This module does not own task state, roles, or a RAM journal. Durable
//! primary/specialist transitions live in `domain::command::decide` and
//! `domain::event::apply`. Session launch, journal correlation, and input
//! sequencing are typed HOLDs until those provider contracts exist.

pub use crate::domain::command::{
    SpecialistPermission, SpecialistResult, SpecialistStatus, DEFAULT_MAX_TOP_LEVEL_RUNTIMES,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrchestrationHold {
    ProviderRuntimeAuthorityAbsent,
    ProviderJournalAbsent,
    ProviderInputAbsent,
    ProcessCapabilityUnbound,
}

pub fn specialist_cancel_hold() -> OrchestrationHold {
    OrchestrationHold::ProviderRuntimeAuthorityAbsent
}

pub fn specialist_write_hold() -> OrchestrationHold {
    OrchestrationHold::ProcessCapabilityUnbound
}

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

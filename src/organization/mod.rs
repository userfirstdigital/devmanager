//! Native host organization bridges.

mod evidence;
mod local_actions;

pub use evidence::{EvidenceReference, EvidenceReferenceStore};
pub use local_actions::{
    AuthenticatedActionContext, LocalActionBridge, LocalActionReceipt, LocalActionReceiptStatus,
    LocalActionRequest,
};

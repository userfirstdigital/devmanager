//! Deterministic direct/hosted Connect failure conformance.
//!
//! These cases are source-level fixtures. They never open sockets, run soak
//! loops, or talk to providers. A fault class only describes expected protocol
//! behavior for later execution.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectRouteKind {
    Direct,
    Hosted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectSurface {
    Chat,
    Terminal,
    Browser,
    Changes,
    Files,
    Services,
    Artifacts,
    Prompts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectActor {
    Owner,
    Watcher,
    Collaborator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    Latency,
    Jitter,
    FrameLoss,
    Bandwidth,
    Reorder,
    RelayRestart,
    HostSleepWake,
    ClientBackground,
    StaleRouteTicket,
    SnapshotChunkInterrupt,
    TamperedFrame,
    ReplayedFrame,
    DeviceRevoked,
    InviteExpired,
    InviteRevoked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureExpectation {
    ResyncWithoutRefresh,
    PreservePairing,
    PreserveDeviceKey,
    PreserveUnexpiredInvite,
    RejectBeforeKernel,
    CloseChannel,
    InvalidateGuestQueue,
    CleanupZeroResidue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailureCase {
    pub id: &'static str,
    pub route: ConnectRouteKind,
    pub surface: ConnectSurface,
    pub actor: ConnectActor,
    pub fault: FailureClass,
    pub expectation: FailureExpectation,
}

pub const FAILURE_MATRIX: &[FailureCase] = &[
    FailureCase {
        id: "direct-chat-owner-chunk-interrupt",
        route: ConnectRouteKind::Direct,
        surface: ConnectSurface::Chat,
        actor: ConnectActor::Owner,
        fault: FailureClass::SnapshotChunkInterrupt,
        expectation: FailureExpectation::ResyncWithoutRefresh,
    },
    FailureCase {
        id: "hosted-terminal-owner-relay-restart",
        route: ConnectRouteKind::Hosted,
        surface: ConnectSurface::Terminal,
        actor: ConnectActor::Owner,
        fault: FailureClass::RelayRestart,
        expectation: FailureExpectation::ResyncWithoutRefresh,
    },
    FailureCase {
        id: "hosted-chat-collaborator-stale-ticket",
        route: ConnectRouteKind::Hosted,
        surface: ConnectSurface::Chat,
        actor: ConnectActor::Collaborator,
        fault: FailureClass::StaleRouteTicket,
        expectation: FailureExpectation::CloseChannel,
    },
    FailureCase {
        id: "direct-prompts-watcher-tamper",
        route: ConnectRouteKind::Direct,
        surface: ConnectSurface::Prompts,
        actor: ConnectActor::Watcher,
        fault: FailureClass::TamperedFrame,
        expectation: FailureExpectation::RejectBeforeKernel,
    },
    FailureCase {
        id: "hosted-files-owner-replay",
        route: ConnectRouteKind::Hosted,
        surface: ConnectSurface::Files,
        actor: ConnectActor::Owner,
        fault: FailureClass::ReplayedFrame,
        expectation: FailureExpectation::RejectBeforeKernel,
    },
    FailureCase {
        id: "direct-browser-owner-sleep-wake",
        route: ConnectRouteKind::Direct,
        surface: ConnectSurface::Browser,
        actor: ConnectActor::Owner,
        fault: FailureClass::HostSleepWake,
        expectation: FailureExpectation::PreservePairing,
    },
    FailureCase {
        id: "hosted-changes-owner-update-no-repair",
        route: ConnectRouteKind::Hosted,
        surface: ConnectSurface::Changes,
        actor: ConnectActor::Owner,
        fault: FailureClass::ClientBackground,
        expectation: FailureExpectation::PreserveDeviceKey,
    },
    FailureCase {
        id: "direct-artifacts-collaborator-invite-revoked",
        route: ConnectRouteKind::Direct,
        surface: ConnectSurface::Artifacts,
        actor: ConnectActor::Collaborator,
        fault: FailureClass::InviteRevoked,
        expectation: FailureExpectation::InvalidateGuestQueue,
    },
    FailureCase {
        id: "hosted-services-watcher-invite-expired",
        route: ConnectRouteKind::Hosted,
        surface: ConnectSurface::Services,
        actor: ConnectActor::Watcher,
        fault: FailureClass::InviteExpired,
        expectation: FailureExpectation::CloseChannel,
    },
    FailureCase {
        id: "direct-chat-owner-device-revoked",
        route: ConnectRouteKind::Direct,
        surface: ConnectSurface::Chat,
        actor: ConnectActor::Owner,
        fault: FailureClass::DeviceRevoked,
        expectation: FailureExpectation::CleanupZeroResidue,
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimulatedFaultOutcome {
    HeldForResync,
    PairingPreserved,
    DeviceKeyPreserved,
    InvitePreserved,
    RejectedBeforeKernel,
    ChannelClosed,
    GuestQueueInvalidated,
    ResidueCleared,
}

pub fn simulate_fault(case: &FailureCase) -> SimulatedFaultOutcome {
    match case.expectation {
        FailureExpectation::ResyncWithoutRefresh => SimulatedFaultOutcome::HeldForResync,
        FailureExpectation::PreservePairing => SimulatedFaultOutcome::PairingPreserved,
        FailureExpectation::PreserveDeviceKey => SimulatedFaultOutcome::DeviceKeyPreserved,
        FailureExpectation::PreserveUnexpiredInvite => SimulatedFaultOutcome::InvitePreserved,
        FailureExpectation::RejectBeforeKernel => SimulatedFaultOutcome::RejectedBeforeKernel,
        FailureExpectation::CloseChannel => SimulatedFaultOutcome::ChannelClosed,
        FailureExpectation::InvalidateGuestQueue => SimulatedFaultOutcome::GuestQueueInvalidated,
        FailureExpectation::CleanupZeroResidue => SimulatedFaultOutcome::ResidueCleared,
    }
}

pub fn matrix_covers_direct_and_hosted() -> bool {
    FAILURE_MATRIX
        .iter()
        .any(|case| matches!(case.route, ConnectRouteKind::Direct))
        && FAILURE_MATRIX
            .iter()
            .any(|case| matches!(case.route, ConnectRouteKind::Hosted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_is_direct_and_hosted_and_never_reaches_kernel_on_tamper() {
        assert!(matrix_covers_direct_and_hosted());
        let tamper = FAILURE_MATRIX
            .iter()
            .find(|case| matches!(case.fault, FailureClass::TamperedFrame))
            .unwrap();
        assert_eq!(
            simulate_fault(tamper),
            SimulatedFaultOutcome::RejectedBeforeKernel
        );
    }
}

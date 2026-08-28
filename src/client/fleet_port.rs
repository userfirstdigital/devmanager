//! Fleet-backed [`AsyncHostRequestPort`] for native desktop integration.
//!
//! [`FleetClientPort`] is an admission-scoped adapter over [`HostFleet`]. The
//! per-host driver remains the sole [`HostClient`] / subscription owner; this
//! type never opens a second client, bus, I/O worker, or socket clone.
//!
//! Captured admission and Hello metadata are immutable for the port lifetime:
//! reconnect must mint a fresh port. Retained command receipts are **not**
//! auto-acked — UI correlates/applies first.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::domain::command::{CommandEnvelope, CommandReceipt};
use crate::domain::id::{CommandId, TaskId};
use crate::domain::query::{QueryEnvelope, QueryReply};
use crate::domain::ClientId;
use crate::host::IpcError;
use crate::protocol::CapabilitySet;
use crate::updater::UpdateHandoffToken;
use uuid::Uuid;

use super::connection::ConnectionMetadata;
use super::fleet::{
    FleetAdmission, FleetError, FleetOwned, FleetUnsupportedKind, HostFleet, HostId, HostTaskKey,
};
use super::model::TaskInboxPreview;
use super::port::AsyncHostRequestPort;

/// Immutable admission-scoped fleet adapter for typed queries and local lifecycle.
#[derive(Clone)]
pub struct FleetClientPort {
    fleet: Arc<HostFleet>,
    admission: FleetAdmission,
    metadata: ConnectionMetadata,
    capabilities: CapabilitySet,
}

impl FleetClientPort {
    /// Validate exact live admission and capture Hello/Connect metadata once.
    pub fn new(fleet: Arc<HostFleet>, admission: FleetAdmission) -> Result<Self, FleetError> {
        fleet.validate_admission(&admission)?;
        let owned = fleet.owner_metadata(&admission.host)?;
        if owned.host != admission.host
            || owned.generation != admission.generation
            || owned.client_id != admission.client_id
        {
            return Err(FleetError::AdmissionOwnerMismatch);
        }
        let capabilities = owned.value.granted_capabilities();
        Ok(Self {
            fleet,
            admission,
            metadata: owned.value,
            capabilities,
        })
    }

    pub fn fleet(&self) -> &Arc<HostFleet> {
        &self.fleet
    }

    /// Captured admission for NativeActionRecord correlation / exact ack later.
    pub fn admission(&self) -> &FleetAdmission {
        &self.admission
    }

    pub fn host(&self) -> &HostId {
        &self.admission.host
    }

    pub fn task_id(&self) -> Option<TaskId> {
        self.admission.task_id
    }

    pub fn generation(&self) -> u64 {
        self.admission.generation
    }

    pub fn host_task_key(&self) -> Option<HostTaskKey> {
        self.admission.host_task_key()
    }

    /// Captured Hello/Connect metadata (not refreshed after reconnect).
    pub fn connection_metadata(&self) -> &ConnectionMetadata {
        &self.metadata
    }

    pub fn granted_capabilities_snapshot(&self) -> &CapabilitySet {
        &self.capabilities
    }

    /// Host-global preview via the driver-owned temporary subscription path.
    pub async fn preview_tasks(&self) -> Result<FleetOwned<TaskInboxPreview>, FleetError> {
        let owned = self.fleet.preview_tasks(&self.admission).await?;
        self.require_host_global_owned(&owned).await?;
        Ok(owned)
    }

    pub async fn detach(&self) -> Result<FleetOwned<Uuid>, FleetError> {
        let owned = self.fleet.detach(&self.admission).await?;
        self.require_host_global_owned(&owned).await?;
        Ok(owned)
    }

    pub async fn prepare_update(
        &self,
        command_id: CommandId,
        target_version: &str,
        client_build: &str,
        host_build: &str,
        allow_explicit_confirm_with_active: bool,
    ) -> Result<FleetOwned<UpdateHandoffToken>, FleetError> {
        let owned = self
            .fleet
            .prepare_update(
                &self.admission,
                command_id,
                target_version,
                client_build,
                host_build,
                allow_explicit_confirm_with_active,
            )
            .await?;
        self.require_host_global_owned(&owned).await?;
        Ok(owned)
    }

    /// Generation-fenced disconnect of the captured admission only.
    pub async fn disconnect_admitted(&self) -> Result<FleetOwned<()>, FleetError> {
        self.fleet.disconnect_admitted(&self.admission).await
    }

    fn validate_owned<T>(&self, owned: &FleetOwned<T>) -> Result<(), IpcError> {
        if owned.host != self.admission.host
            || owned.generation != self.admission.generation
            || owned.client_id != self.admission.client_id
            || owned.task_id != self.admission.task_id
        {
            return Err(IpcError::CorrelationMismatch);
        }
        Ok(())
    }

    async fn require_host_global_owned<T>(&self, owned: &FleetOwned<T>) -> Result<(), FleetError> {
        if owned.task_id.is_some()
            || owned.host != self.admission.host
            || owned.generation != self.admission.generation
            || owned.client_id != self.admission.client_id
        {
            let _ = self.fleet.disconnect_admitted(&self.admission).await;
            return Err(FleetError::AdmissionOwnerMismatch);
        }
        Ok(())
    }
}

fn map_fleet_error(error: FleetError) -> IpcError {
    match error {
        FleetError::Ipc(error) => error,
        FleetError::UnsupportedRequest(_) => IpcError::Unsupported,
        FleetError::AdmissionOwnerMismatch
        | FleetError::StaleGeneration
        | FleetError::StaleClientId
        | FleetError::HostMetadataMismatch => IpcError::Unauthorized,
        FleetError::DisconnectedReadOnly
        | FleetError::HostFenced
        | FleetError::HostNotFound
        | FleetError::WorkerGone
        | FleetError::QueueFull
        | FleetError::HostBusy
        | FleetError::HostAlreadyInstalled
        | FleetError::HostCapacityExceeded
        | FleetError::StaleReservation => IpcError::Unavailable,
        FleetError::InvalidProfile(name) => IpcError::InvalidProfile(name),
        FleetError::InvalidRemoteHostId => IpcError::Unauthorized,
        FleetError::Subscription(error) => match error {
            super::subscription::SubscriptionError::Transport(inner)
            | super::subscription::SubscriptionError::TransportAt { error: inner, .. } => inner,
            _ => IpcError::Unavailable,
        },
    }
}

#[async_trait]
impl AsyncHostRequestPort for FleetClientPort {
    fn client_id(&self) -> ClientId {
        self.admission.client_id
    }

    fn granted_capabilities(&self) -> CapabilitySet {
        self.capabilities
    }

    async fn request_query(
        &mut self,
        envelope: QueryEnvelope,
        timeout: Option<Duration>,
    ) -> Result<QueryReply, IpcError> {
        let owned = self
            .fleet
            .query_with_timeout(&self.admission, envelope, timeout)
            .await
            .map_err(map_fleet_error)?;
        if let Err(error) = self.validate_owned(&owned) {
            let _ = self.fleet.disconnect_admitted(&self.admission).await;
            return Err(error);
        }
        Ok(owned.value)
    }

    async fn request_command(
        &mut self,
        envelope: CommandEnvelope,
    ) -> Result<CommandReceipt, IpcError> {
        // Intentionally do not acknowledge retained receipts here: UI must
        // correlate/apply first, then ack through HostFleet.
        let owned = self
            .fleet
            .execute_command(&self.admission, envelope)
            .await
            .map_err(map_fleet_error)?;
        if let Err(error) = self.validate_owned(&owned) {
            let _ = self.fleet.disconnect_admitted(&self.admission).await;
            return Err(error);
        }
        Ok(owned.value)
    }

    async fn retire_request_transport(&mut self) {
        // Only the captured generation/client may disconnect; a replacement
        // host generation is left intact when this admission is stale.
        let _ = self.fleet.disconnect_admitted(&self.admission).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::connection::ClientConnection;
    use crate::client::host_client::{HostClient, HostClientConfig};
    use crate::config::paths::AppProfile;
    use crate::domain::id::RequestId;
    use crate::domain::query::{Query, QueryEnvelope};
    use crate::host::agent_connection_query_timeout;
    use crate::protocol::{
        Capability, CapabilitySet, FrameLimits, ProfileFingerprint, ServerHello, PROTOCOL_MAJOR,
        PROTOCOL_MINOR,
    };
    use std::collections::BTreeMap;
    use tokio::sync::oneshot;

    fn fixed_uuid_v7(tail: u8) -> [u8; 16] {
        [
            0x01, 0x8f, 0x60, 0xb0, 0x9c, 0x1a, 0x70, 0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, tail,
        ]
    }

    fn test_hello(profile: &str, connection_tail: u8) -> ServerHello {
        let normalized = match AppProfile::named(profile).expect("profile") {
            AppProfile::Named(name) => name,
            other => panic!("expected named, got {other:?}"),
        };
        ServerHello {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            server_build: "devmanager-host/fleet-port-test".into(),
            host_boot_id: uuid::Uuid::from_bytes(fixed_uuid_v7(0xb0)),
            connection_id: uuid::Uuid::from_bytes(fixed_uuid_v7(connection_tail)),
            profile_fingerprint: ProfileFingerprint::hash_normalized(&normalized),
            granted: CapabilitySet::from_capabilities([
                Capability::PagedSnapshots,
                Capability::TaskCockpit,
                Capability::HostShutdown,
                Capability::UpdateHandoff,
                Capability::ExplicitDetach,
            ]),
            limits: FrameLimits::v1_default(),
            reconnect_grant: None,
        }
    }

    fn local_client(profile: &str, client_tail: u8, connection_tail: u8) -> HostClient {
        let normalized = match AppProfile::named(profile).expect("profile") {
            AppProfile::Named(name) => name,
            other => panic!("expected named, got {other:?}"),
        };
        let client_id = ClientId::from_bytes(fixed_uuid_v7(client_tail)).expect("client");
        let hello = test_hello(profile, connection_tail);
        let connection = ClientConnection::inert_stub_for_test(client_id, hello.clone());
        HostClient::from_parts_for_test(
            HostClientConfig {
                named_profile: normalized,
                client_build: "devmanager/fleet-port-test".into(),
                client_id,
                requested: CapabilitySet::from_capabilities([
                    Capability::PagedSnapshots,
                    Capability::TaskCockpit,
                ]),
                limits: FrameLimits::v1_default(),
            },
            hello,
            Some(connection),
            BTreeMap::new(),
        )
    }

    #[test]
    fn constructor_captures_metadata_and_host_global_admission() {
        let host = HostId::local_profile("fleet_port_meta").unwrap();
        let fleet = Arc::new(HostFleet::new());
        fleet
            .install(host.clone(), local_client("fleet_port_meta", 0xa1, 0xa2))
            .unwrap();
        let admission = fleet.admit_host(&host).unwrap();
        assert!(admission.task_id.is_none());
        let port = FleetClientPort::new(Arc::clone(&fleet), admission.clone()).unwrap();
        assert_eq!(port.generation(), admission.generation);
        assert_eq!(port.client_id(), admission.client_id);
        assert!(port.task_id().is_none());
        assert_eq!(port.connection_metadata().protocol_major(), PROTOCOL_MAJOR);
        assert!(port
            .granted_capabilities_snapshot()
            .contains(Capability::TaskCockpit));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stale_port_retirement_cannot_disconnect_replacement() {
        let host = HostId::local_profile("fleet_port_stale").unwrap();
        let fleet = Arc::new(HostFleet::new());
        fleet
            .install(host.clone(), local_client("fleet_port_stale", 0xb1, 0xb2))
            .unwrap();
        let admission = fleet.admit_host(&host).unwrap();
        let mut port = FleetClientPort::new(Arc::clone(&fleet), admission).unwrap();
        let gen = port.generation();
        let (entered_tx, entered_rx) = oneshot::channel::<()>();
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let fleet_rec = Arc::clone(&fleet);
        let host_rec = host.clone();
        let reconnect = tokio::spawn(async move {
            fleet_rec
                .reconnect_with_factory(
                    &host_rec,
                    Box::new(move || {
                        Box::pin(async move {
                            let _ = entered_tx.send(());
                            let _ = release_rx.await;
                            Ok(local_client("fleet_port_stale", 0xb1, 0xb3))
                        })
                    }),
                )
                .await
        });
        // Generation already advanced inside begin_reconnect before factory runs.
        let _ = entered_rx.await;
        port.retire_request_transport().await;
        assert_eq!(
            fleet.generation(&host).unwrap(),
            gen + 1,
            "stale retire must not fence the replacement generation"
        );
        let _ = release_tx.send(());
        let reconnected = reconnect.await.expect("join").expect("reconnect");
        assert_ne!(reconnected.value, gen);
        assert!(fleet.is_connected(&host).unwrap());
        fleet.remove(&host).await.unwrap();
    }

    #[tokio::test]
    async fn custom_timeout_reaches_fleet_query_primitive() {
        let host = HostId::local_profile("fleet_port_timeout").unwrap();
        let fleet = Arc::new(HostFleet::new());
        fleet
            .install(host.clone(), local_client("fleet_port_timeout", 0xc1, 0xc2))
            .unwrap();
        let admission = fleet.admit_host(&host).unwrap();
        let mut port = FleetClientPort::new(Arc::clone(&fleet), admission.clone()).unwrap();
        let timeout = Some(agent_connection_query_timeout());
        let err = port
            .request_query(
                QueryEnvelope {
                    request_id: RequestId::new(),
                    client_id: admission.client_id,
                    task_id: Some(TaskId::from_bytes(fixed_uuid_v7(0xc3)).expect("task")),
                    query: Query::InspectHostQuit,
                },
                timeout,
            )
            .await
            .expect_err("scope");
        assert!(matches!(err, IpcError::Unauthorized));
        fleet.remove(&host).await.unwrap();
    }

    #[test]
    fn remote_detach_and_update_rejected_via_fleet_classify() {
        let host = HostId::remote([2; 16]).unwrap();
        let fleet = HostFleet::new();
        assert!(matches!(
            fleet.classify_request_support(&host, FleetUnsupportedKind::ExplicitDetach),
            Err(FleetError::HostNotFound) | Err(FleetError::UnsupportedRequest(_))
        ));
        assert!(matches!(
            fleet.classify_request_support(&host, FleetUnsupportedKind::PrepareUpdate),
            Err(FleetError::HostNotFound) | Err(FleetError::UnsupportedRequest(_))
        ));
    }
}

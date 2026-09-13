//! Authenticated Connect application dispatch onto the one host executor.
//!
//! The route retains the paired identity captured at admission. Query/Command
//! frames become [`ClientRequest`] values and go through
//! [`HostRequestHandle::execute`]. This module never opens a second CommandBus
//! and never translates into the legacy JSON action protocol.

use std::sync::{Arc, OnceLock, RwLock};

use async_trait::async_trait;

use crate::client::ConnectHostCommandPort;
use crate::connect::permission::{
    PermissionDecision, PermissionDenyReason, PermissionEvaluator, PermissionRequest,
};
use crate::connect::permissions::{action_for_client_request, organization_permission};
use crate::connect::{
    ChannelBinding, ConnectEnvelope, ConnectIdentityLiveState, ConnectLimits, ConnectPayload,
    ConnectPrivacyClass, ConnectRole, DeviceCredentialProof, ErrorPayload, HelloPayload,
    MAX_CONNECT_DIAGNOSTIC_BYTES,
};
use crate::domain::id::{ClientId, RequestId};
use crate::domain::query::{Query, QueryEnvelope};
use crate::domain::snapshot::SnapshotSection;
use crate::host::{HostConnectDuplex, HostRequestHandle, IpcError, OrganizationRuntime};
use crate::protocol::{
    Capability, CapabilitySet, ClientRequest, FrameLimits, NegotiatedParameters, ProtocolVersion,
    ServerMessage,
};

/// Former production HOLD text. New dispatch must never emit this fragment.
pub const CONNECT_HOLD_CALLBACK_FRAGMENT: &str =
    "unavailable until the host executor callback is bound";

pub const CONNECT_ERROR_PROTOCOL: u16 = 400;
pub const CONNECT_ERROR_UNAUTHORIZED: u16 = 401;
pub const CONNECT_ERROR_FORBIDDEN: u16 = 403;
pub const CONNECT_ERROR_CONFLICT: u16 = 409;
pub const CONNECT_ERROR_EXECUTOR_UNATTACHED: u16 = 500;

const ORG_PROMPT_UNAVAILABLE: &str = "organization projection dispatch is unavailable on this host";
const ORG_EXTENSION_SCHEMA_VERSION: u16 = crate::protocol::ORGANIZATION_SCHEMA_VERSION;
const ORG_EXTENSION_TYPE: u16 = crate::protocol::organization_extension_type(
    crate::protocol::OrganizationExtensionKind::OrganizationPrompt,
);

/// Capabilities advertised on Connect Hello.
///
/// Event replay is advertised only while the one host request lane is bound.
/// The query path then uses the host's existing bounded replay registry and
/// journal; Connect never creates a second store. Live unsolicited delivery is
/// intentionally not implied by this bit: it remains the host IPC output
/// lane's responsibility and a reconnect/resync is required when that lane is
/// unavailable.
pub fn advertised_connect_capabilities() -> CapabilitySet {
    advertised_connect_capabilities_for_host(false)
}

fn advertised_connect_capabilities_for_host(host_attached: bool) -> CapabilitySet {
    let mut capabilities = CapabilitySet::from_capabilities([
        Capability::ConnectEncryption,
        Capability::PagedSnapshots,
        Capability::OperationSettlement,
        Capability::ChunkResume,
        Capability::PromptProjection,
        Capability::ProviderInput,
        Capability::TaskCockpit,
        Capability::HostShutdown,
        Capability::ExplicitDetach,
        Capability::ManagementMetadata,
    ]);
    if host_attached {
        capabilities = CapabilitySet::from_bits(
            capabilities.bits()
                | Capability::EventReplay.bit()
                | Capability::SemanticConversation.bit()
                | Capability::BrowserProjection.bit(),
        );
    }
    // Organization is advertised only while the host-owned runtime is
    // enrolled and enabled. A standalone process must not imply a second
    // organization store or a usable organization request lane.
    if OrganizationRuntime::bound_connect_runtime()
        .is_some_and(|runtime| runtime.snapshot().capability == "enabled")
    {
        capabilities = CapabilitySet::from_bits(
            capabilities.bits()
                | Capability::GenericExtensions.bit()
                | Capability::OrganizationProjection.bit(),
        );
    }
    capabilities
}

/// Cloneable attachment for the one existing host executor.
///
/// Lock ordering: acquire only to clone the handle, then release before any
/// await (including [`HostRequestHandle::execute`]).
#[derive(Clone)]
pub struct ConnectHostRequestSlot {
    inner: Arc<RwLock<Option<Arc<dyn ConnectHostCommandPort>>>>,
}

impl std::fmt::Debug for ConnectHostRequestSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConnectHostRequestSlot")
            .field("attached", &self.get().is_some())
            .finish()
    }
}

impl Default for ConnectHostRequestSlot {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ConnectHostCommandPort for HostRequestHandle {
    async fn execute(
        &self,
        negotiated: NegotiatedParameters,
        request: ClientRequest,
    ) -> Result<ServerMessage, IpcError> {
        HostRequestHandle::execute(self, negotiated, request).await
    }

    async fn open_duplex(
        &self,
        client_id: ClientId,
    ) -> Result<Option<HostConnectDuplex>, IpcError> {
        Ok(Some(self.open_connect_duplex(client_id).await?))
    }
}

impl ConnectHostRequestSlot {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(None)),
        }
    }

    pub fn attach(&self, handle: HostRequestHandle) {
        self.attach_executor(Arc::new(handle));
    }

    pub fn attach_executor(&self, executor: Arc<dyn ConnectHostCommandPort>) {
        let mut slot = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = Some(executor);
    }

    pub fn get(&self) -> Option<Arc<dyn ConnectHostCommandPort>> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn clear(&self) {
        let mut slot = self
            .inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = None;
    }
}

fn process_host_request_slot() -> &'static ConnectHostRequestSlot {
    static SLOT: OnceLock<ConnectHostRequestSlot> = OnceLock::new();
    SLOT.get_or_init(ConnectHostRequestSlot::new)
}

/// Durable-host seam: attach the one executor handle for in-process Connect.
pub fn bind_host_request_handle(handle: HostRequestHandle) {
    process_host_request_slot().attach(handle);
}

/// Attach an IPC-backed executor owned by the native shell. The slot stores
/// only the narrow request lane and remains process-local; the implementation
/// may cross the process boundary through the existing HostClient pipe.
pub fn bind_host_executor(executor: Arc<dyn ConnectHostCommandPort>) {
    process_host_request_slot().attach_executor(executor);
}

pub fn bound_host_request_handle() -> Option<Arc<dyn ConnectHostCommandPort>> {
    process_host_request_slot().get()
}

/// Drop the process-wide executor binding before the host handle is dropped.
/// Same-process only; does not create a cross-process HostClient path.
pub fn unbind_host_request_handle() {
    process_host_request_slot().clear();
}

pub fn process_connect_host_request_slot() -> ConnectHostRequestSlot {
    process_host_request_slot().clone()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectSessionDisposition {
    Continue,
    Disconnect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NegotiatedConnect {
    limits: ConnectLimits,
    capabilities: CapabilitySet,
    privacy_class: ConnectPrivacyClass,
}

/// One authenticated Connect application session after Noise.
///
/// Hello capability/limits intersection is the complete negotiation. A later
/// Capabilities payload is optional confirmation and must match exactly.
pub struct ConnectDispatchSession {
    paired_web_client_id: String,
    bound_client_id: Option<ClientId>,
    identity_live: ConnectIdentityLiveState,
    binding: Option<ChannelBinding>,
    last_recv_sequence: u64,
    negotiated: Option<NegotiatedConnect>,
    active: bool,
    capability_ceiling: CapabilitySet,
    limit_ceiling: ConnectLimits,
    /// Opaque session-bound device proof for canonical Device-kind peers.
    device_credential: Option<DeviceCredentialProof>,
    session_epoch: Option<u64>,
    /// Host-kind Noise on the paired-cookie route during browser migration.
    legacy_host_compat: bool,
}

impl ConnectDispatchSession {
    pub fn bind_paired(
        paired_web_client_id: String,
        identity_live: ConnectIdentityLiveState,
    ) -> Self {
        Self {
            paired_web_client_id,
            bound_client_id: None,
            identity_live,
            binding: None,
            last_recv_sequence: 0,
            negotiated: None,
            active: true,
            capability_ceiling: CapabilitySet::from_bits(u64::MAX),
            limit_ceiling: ConnectLimits::v1_default(),
            device_credential: None,
            session_epoch: None,
            // Legacy Host compatibility is explicit via with_legacy_host_compat
            // after an authenticated Host claim; never the default.
            legacy_host_compat: false,
        }
    }

    /// Canonical Device path: require the opaque enrollment proof on requests.
    /// `session_epoch == 0` fails closed and does **not** enable legacy compat.
    pub fn with_device_credential(
        mut self,
        proof: DeviceCredentialProof,
        session_epoch: u64,
    ) -> Self {
        if self.negotiated.is_some() {
            return self;
        }
        if session_epoch == 0 {
            self.device_credential = None;
            self.session_epoch = None;
            self.legacy_host_compat = false;
            return self;
        }
        self.device_credential = Some(proof);
        self.session_epoch = Some(session_epoch);
        self.legacy_host_compat = false;
        self
    }

    /// Explicit Host-kind cookie-pinned compatibility (no device registration).
    pub fn with_legacy_host_compat(mut self) -> Self {
        if self.negotiated.is_none() {
            self.device_credential = None;
            self.session_epoch = None;
            self.legacy_host_compat = true;
        }
        self
    }

    /// Restrict advertised authority to operations supported by this carrier.
    /// Set before Hello; changing an active negotiation is never permitted.
    pub fn with_capability_ceiling(mut self, ceiling: CapabilitySet) -> Self {
        if self.negotiated.is_none() {
            self.capability_ceiling = self.capability_ceiling.intersection(ceiling);
        }
        self
    }

    /// Restrict negotiated transport and pagination limits for a physical
    /// carrier with a smaller record boundary (for example Noise transport).
    /// Set before Hello; changing an active negotiation is never permitted.
    pub(crate) fn with_limit_ceiling(mut self, ceiling: ConnectLimits) -> Self {
        if self.negotiated.is_none() && ceiling.validate().is_ok() {
            self.limit_ceiling = ceiling;
        }
        self
    }

    /// The pairing registry, not the browser, owns the durable command identity.
    /// An omitted Hello ID receives this ID; a conflicting supplied ID is rejected.
    pub(crate) fn with_assigned_client_id(mut self, client_id: ClientId) -> Self {
        if self.negotiated.is_none() {
            self.bound_client_id = Some(client_id);
        }
        self
    }

    pub fn bound_client_id(&self) -> Option<ClientId> {
        if self.active {
            self.bound_client_id
        } else {
            None
        }
    }

    pub fn paired_identity_bound(&self) -> bool {
        self.active && !self.paired_web_client_id.is_empty()
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    #[cfg(test)]
    pub(crate) fn legacy_host_compat_for_test(&self) -> bool {
        self.legacy_host_compat
    }

    #[cfg(test)]
    pub(crate) fn has_device_credential_for_test(&self) -> bool {
        self.device_credential.is_some()
    }

    pub fn negotiated_capabilities(&self) -> Option<CapabilitySet> {
        self.negotiated.map(|negotiated| negotiated.capabilities)
    }

    pub fn negotiated_limits(&self) -> Option<ConnectLimits> {
        self.negotiated.map(|negotiated| negotiated.limits)
    }

    pub fn channel_binding(&self) -> Option<ChannelBinding> {
        self.binding
    }

    pub fn disconnect(&mut self) {
        self.active = false;
        self.binding = None;
        self.negotiated = None;
        self.bound_client_id = None;
        self.last_recv_sequence = 0;
        self.paired_web_client_id.clear();
        self.device_credential = None;
        self.session_epoch = None;
        self.legacy_host_compat = false;
    }

    pub async fn handle_payload(
        &mut self,
        envelope: &ConnectEnvelope,
        payload: ConnectPayload,
        host: Option<&dyn ConnectHostCommandPort>,
    ) -> (ConnectPayload, ConnectSessionDisposition) {
        match self.dispatch(envelope, payload, host).await {
            Ok(payload) => (payload, ConnectSessionDisposition::Continue),
            Err(failure) => {
                if failure.disconnect {
                    self.disconnect();
                }
                (
                    typed_error(
                        failure.code,
                        failure.message,
                        envelope.request_id,
                        envelope.operation_id,
                    ),
                    if failure.disconnect {
                        ConnectSessionDisposition::Disconnect
                    } else {
                        ConnectSessionDisposition::Continue
                    },
                )
            }
        }
    }

    async fn dispatch(
        &mut self,
        envelope: &ConnectEnvelope,
        payload: ConnectPayload,
        host: Option<&dyn ConnectHostCommandPort>,
    ) -> Result<ConnectPayload, DispatchFailure> {
        if !self.active {
            return Err(DispatchFailure::fatal(
                CONNECT_ERROR_UNAUTHORIZED,
                "Connect session is closed",
            ));
        }
        if !matches!(self.identity_live, ConnectIdentityLiveState::Live) {
            return Err(DispatchFailure::fatal(
                CONNECT_ERROR_UNAUTHORIZED,
                "Connect identity is not live",
            ));
        }

        match payload {
            ConnectPayload::Hello(hello) => self.accept_hello(envelope, hello, host.is_some()),
            payload if self.negotiated.is_none() => {
                let _ = payload;
                Err(DispatchFailure::fatal(
                    CONNECT_ERROR_PROTOCOL,
                    "Connect Hello is required before Query or Command",
                ))
            }
            ConnectPayload::Capabilities(capabilities) => {
                if !self.capabilities_match_negotiated(capabilities) {
                    return Err(DispatchFailure::fatal(
                        CONNECT_ERROR_FORBIDDEN,
                        "capability set does not match the negotiated intersection",
                    ));
                }
                self.admit_post_hello_frame(envelope)?;
                Ok(ConnectPayload::Capabilities(
                    self.require_ready()?.capabilities,
                ))
            }
            ConnectPayload::Query(query) => {
                let Some(request_id) = envelope.request_id else {
                    return Err(DispatchFailure::fatal(
                        CONNECT_ERROR_PROTOCOL,
                        "Query envelope must carry request_id",
                    ));
                };
                if query.request_id != request_id {
                    return Err(DispatchFailure::fatal(
                        CONNECT_ERROR_PROTOCOL,
                        "Query request_id does not match the envelope",
                    ));
                }
                self.admit_post_hello_frame(envelope)?;
                self.dispatch_request(envelope, ClientRequest::Query(query), host)
                    .await
            }
            ConnectPayload::Command(command) => {
                self.admit_post_hello_frame(envelope)?;
                self.dispatch_request(envelope, ClientRequest::Command(command), host)
                    .await
            }
            ConnectPayload::TerminalInput(request) => {
                let Some(operation_id) = envelope.operation_id else {
                    return Err(DispatchFailure::fatal(
                        CONNECT_ERROR_PROTOCOL,
                        "TerminalInput envelope must carry operation_id",
                    ));
                };
                if operation_id.as_bytes() != request.input_id.as_bytes() {
                    return Err(DispatchFailure::fatal(
                        CONNECT_ERROR_PROTOCOL,
                        "TerminalInput input_id does not match the envelope",
                    ));
                }
                self.admit_post_hello_frame(envelope)?;
                self.dispatch_request(envelope, ClientRequest::TerminalInput(request), host)
                    .await
            }
            ConnectPayload::Extension(extension) if extension.type_id == ORG_EXTENSION_TYPE => {
                let negotiated = self.admit_post_hello_frame(envelope)?;
                self.dispatch_organization_extension(envelope, extension, negotiated)
            }
            ConnectPayload::Resync(resync) => {
                // The client reports the last durable channel sequence it can
                // prove.  It must not ask the host to advance from a future
                // position; that would turn a stale or cross-session frame
                // into an authoritative snapshot request.
                if resync.newest_sequence < resync.channel_sequence {
                    return Err(DispatchFailure::soft(
                        CONNECT_ERROR_CONFLICT,
                        "Connect resync cursor is outside the authenticated channel history",
                    ));
                }
                let negotiated = self.admit_post_hello_frame(envelope)?;
                if !negotiated.capabilities.contains(Capability::PagedSnapshots) {
                    return Err(DispatchFailure::soft(
                        CONNECT_ERROR_FORBIDDEN,
                        "Connect resync requires the paged snapshot capability",
                    ));
                }
                // Resync deliberately reuses the host's bounded snapshot
                // implementation instead of inventing a second replay store.
                // The resulting QueryReply carries the fresh page and remains
                // fenced by the same authenticated binding and negotiated
                // limits as every other request-lane operation.
                let bound = self.bound_client_id.ok_or_else(|| {
                    DispatchFailure::fatal(
                        CONNECT_ERROR_UNAUTHORIZED,
                        "Connect client identity is not bound",
                    )
                })?;
                let query = QueryEnvelope {
                    request_id: envelope.request_id.unwrap_or_else(RequestId::new),
                    client_id: bound,
                    task_id: None,
                    query: Query::SnapshotPage {
                        section: SnapshotSection::Tasks,
                        snapshot_id: None,
                        resume_cursor: None,
                    },
                };
                self.dispatch_request(envelope, ClientRequest::Query(query), host)
                    .await
            }
            ConnectPayload::EventPage(_) | ConnectPayload::SnapshotPage(_) => {
                self.admit_post_hello_frame(envelope)?;
                Err(DispatchFailure::soft(
                    CONNECT_ERROR_FORBIDDEN,
                    "Connect durable subscription is not advertised on this host",
                ))
            }
            ConnectPayload::QueryReply(_)
            | ConnectPayload::CommandReceipt(_)
            | ConnectPayload::TerminalInputAck(_)
            | ConnectPayload::OperationSettlement(_)
            | ConnectPayload::Presence(_)
            | ConnectPayload::TerminalDelta(_)
            | ConnectPayload::BrowserFrame(_)
            | ConnectPayload::PromptExtension(_)
            | ConnectPayload::BrowserExtension(_)
            | ConnectPayload::Chunk(_)
            | ConnectPayload::Error(_)
            | ConnectPayload::HostDurableOutput(_)
            | ConnectPayload::HostCriticalOutput(_)
            | ConnectPayload::HostStreamOutput(_)
            | ConnectPayload::HostConversationOutput(_)
            | ConnectPayload::Extension(_) => {
                self.admit_post_hello_frame(envelope)?;
                Err(DispatchFailure::soft(
                    CONNECT_ERROR_PROTOCOL,
                    "Connect payload kind is not accepted on the application request lane",
                ))
            }
        }
    }

    fn accept_hello(
        &mut self,
        envelope: &ConnectEnvelope,
        hello: HelloPayload,
        host_attached: bool,
    ) -> Result<ConnectPayload, DispatchFailure> {
        let binding = envelope.binding().map_err(|_| {
            DispatchFailure::fatal(CONNECT_ERROR_PROTOCOL, "invalid channel binding")
        })?;
        if envelope.sequence == 0 || envelope.sequence <= self.last_recv_sequence {
            return Err(DispatchFailure::fatal(
                CONNECT_ERROR_CONFLICT,
                "Connect sequence replay or inversion is rejected",
            ));
        }
        if self.negotiated.is_some() {
            return Err(DispatchFailure::fatal(
                CONNECT_ERROR_PROTOCOL,
                "Connect Hello is already complete",
            ));
        }
        if hello.capability_grant.is_some() {
            return Err(DispatchFailure::fatal(
                CONNECT_ERROR_FORBIDDEN,
                "client Hello cannot carry a host capability grant",
            ));
        }
        if let (Some(bound), Some(supplied)) = (self.bound_client_id, hello.client_id) {
            if bound != supplied {
                return Err(DispatchFailure::fatal(
                    CONNECT_ERROR_UNAUTHORIZED,
                    "Hello client_id does not match the bound Connect identity",
                ));
            }
        }
        hello.limits.validate().map_err(|_| {
            DispatchFailure::fatal(CONNECT_ERROR_PROTOCOL, "Hello limits are invalid")
        })?;
        if matches!(hello.privacy_class, ConnectPrivacyClass::RawContent) {
            return Err(DispatchFailure::fatal(
                CONNECT_ERROR_PROTOCOL,
                "Hello cannot advertise RawContent as the default privacy class",
            ));
        }
        let limits = self.limit_ceiling.negotiate(hello.limits).map_err(|_| {
            DispatchFailure::fatal(CONNECT_ERROR_PROTOCOL, "Hello limits cannot be negotiated")
        })?;
        let capabilities = advertised_connect_capabilities_for_host(host_attached)
            .intersection(hello.capabilities)
            .intersection(self.capability_ceiling);
        let client_id = self
            .bound_client_id
            .or(hello.client_id)
            .unwrap_or_else(ClientId::new);
        self.binding = Some(binding);
        self.last_recv_sequence = envelope.sequence;
        self.bound_client_id = Some(client_id);
        self.negotiated = Some(NegotiatedConnect {
            limits,
            capabilities,
            privacy_class: hello.privacy_class,
        });
        Ok(ConnectPayload::Hello(HelloPayload {
            capabilities,
            limits,
            privacy_class: hello.privacy_class,
            relay_url: None,
            capability_grant: None,
            client_id: Some(client_id),
        }))
    }

    fn capabilities_match_negotiated(&self, capabilities: CapabilitySet) -> bool {
        self.negotiated
            .is_some_and(|negotiated| capabilities == negotiated.capabilities)
    }

    fn admit_post_hello_frame(
        &mut self,
        envelope: &ConnectEnvelope,
    ) -> Result<NegotiatedConnect, DispatchFailure> {
        let negotiated = self.require_ready()?;
        let binding = envelope.binding().map_err(|_| {
            DispatchFailure::fatal(CONNECT_ERROR_PROTOCOL, "invalid channel binding")
        })?;
        if self.binding != Some(binding) {
            return Err(DispatchFailure::fatal(
                CONNECT_ERROR_PROTOCOL,
                "channel binding does not match the authenticated session",
            ));
        }
        if envelope.sequence == 0 || envelope.sequence <= self.last_recv_sequence {
            return Err(DispatchFailure::fatal(
                CONNECT_ERROR_CONFLICT,
                "Connect sequence replay or inversion is rejected",
            ));
        }
        if envelope.limits != negotiated.limits {
            return Err(DispatchFailure::fatal(
                CONNECT_ERROR_PROTOCOL,
                "envelope limits do not match negotiated Connect limits",
            ));
        }
        self.last_recv_sequence = envelope.sequence;
        Ok(negotiated)
    }

    fn require_ready(&self) -> Result<NegotiatedConnect, DispatchFailure> {
        self.negotiated.ok_or_else(|| {
            DispatchFailure::fatal(
                CONNECT_ERROR_PROTOCOL,
                "Connect Hello is required before Query or Command",
            )
        })
    }

    async fn dispatch_request(
        &mut self,
        envelope: &ConnectEnvelope,
        request: ClientRequest,
        host: Option<&dyn ConnectHostCommandPort>,
    ) -> Result<ConnectPayload, DispatchFailure> {
        let negotiated = self.require_ready()?;
        let bound = self.bound_client_id.ok_or_else(|| {
            DispatchFailure::fatal(
                CONNECT_ERROR_UNAUTHORIZED,
                "Connect client identity is not bound",
            )
        })?;
        bind_request_identity(&request, bound)?;
        deny_if_capability_missing(&request, negotiated.capabilities)?;
        authorize_established_request(self, &request)?;

        let Some(host) = host else {
            return Err(DispatchFailure::soft(
                CONNECT_ERROR_EXECUTOR_UNATTACHED,
                "Connect host executor is not attached",
            ));
        };

        let parameters = NegotiatedParameters {
            version: ProtocolVersion::current(),
            client_id: bound,
            capabilities: negotiated.capabilities,
            limits: frame_limits_or_default(negotiated.limits),
        };
        let message = host
            .execute(parameters, request)
            .await
            .map_err(map_ipc_error)?;
        convert_host_message(message, envelope)
    }

    fn dispatch_organization_extension(
        &self,
        envelope: &ConnectEnvelope,
        extension: crate::connect::GenericExtensionPayload,
        negotiated: NegotiatedConnect,
    ) -> Result<ConnectPayload, DispatchFailure> {
        let client_id = self.bound_client_id.ok_or_else(|| {
            DispatchFailure::fatal(
                CONNECT_ERROR_UNAUTHORIZED,
                "Connect client identity is not bound",
            )
        })?;
        if !negotiated
            .capabilities
            .contains(Capability::OrganizationProjection)
        {
            return Err(DispatchFailure::soft(
                CONNECT_ERROR_FORBIDDEN,
                "organization projection was not negotiated",
            ));
        }
        if extension.schema_version != ORG_EXTENSION_SCHEMA_VERSION {
            return Err(DispatchFailure::soft(
                CONNECT_ERROR_PROTOCOL,
                "unsupported organization extension schema",
            ));
        }
        // Generic organization requests still require normal Connect request
        // correlation. The runtime receives the exact operation id below and
        // owns replay settlement; it never receives tenant identity from the
        // remote payload.
        if envelope.request_id.is_none() {
            return Err(DispatchFailure::soft(
                CONNECT_ERROR_PROTOCOL,
                "organization request must carry request_id",
            ));
        }
        negotiated
            .limits
            .validate_payload_len(extension.payload.len())
            .map_err(|_| {
                DispatchFailure::soft(
                    CONNECT_ERROR_PROTOCOL,
                    "organization request exceeds negotiated limits",
                )
            })?;
        let mutating = organization_request_is_mutating(&extension.payload)?;
        let mut org_request = organization_permission(mutating);
        org_request.credential = match (self.device_credential.as_ref(), self.legacy_host_compat) {
            (Some(proof), false)
                if self
                    .session_epoch
                    .is_some_and(|epoch| proof.session_epoch() == epoch) =>
            {
                Some(proof.clone())
            }
            (None, true) => None,
            _ => {
                return Err(DispatchFailure::soft(
                    CONNECT_ERROR_FORBIDDEN,
                    deny_message(PermissionDenyReason::DeviceCredentialRequired),
                ));
            }
        };
        let decision =
            PermissionEvaluator::default().evaluate_transport_authenticated_owner(org_request);
        if let PermissionDecision::Denied(reason) = decision {
            return Err(DispatchFailure::soft(
                CONNECT_ERROR_FORBIDDEN,
                deny_message(reason),
            ));
        }
        if mutating && envelope.operation_id.is_none() {
            return Err(DispatchFailure::soft(
                CONNECT_ERROR_PROTOCOL,
                "organization command must carry operation_id",
            ));
        }
        let Some(runtime) = OrganizationRuntime::bound_connect_runtime() else {
            return Err(DispatchFailure::soft(
                CONNECT_ERROR_EXECUTOR_UNATTACHED,
                ORG_PROMPT_UNAVAILABLE,
            ));
        };
        // `client_id` is deliberately read only after the authenticated Hello
        // binding and is retained as the local identity fence for this route;
        // the host runtime owns the enrolled host identity and receives only
        // the outer operation id, never a tenant supplied by the client.
        let _authenticated_client_id = client_id;
        let response = runtime
            .dispatch_authenticated_connect_payload(envelope.operation_id, &extension.payload)
            .map_err(map_organization_runtime_error)?;
        negotiated
            .limits
            .validate_payload_len(response.len())
            .map_err(|_| {
                DispatchFailure::soft(
                    CONNECT_ERROR_PROTOCOL,
                    "organization response exceeds negotiated limits",
                )
            })?;
        Ok(ConnectPayload::Extension(
            crate::connect::GenericExtensionPayload {
                type_id: extension.type_id,
                schema_version: extension.schema_version,
                payload: response,
            },
        ))
    }
}

fn organization_request_is_mutating(payload: &[u8]) -> Result<bool, DispatchFailure> {
    let value: serde_json::Value = serde_json::from_slice(payload).map_err(|_| {
        DispatchFailure::soft(
            CONNECT_ERROR_PROTOCOL,
            "organization request payload is not valid JSON",
        )
    })?;
    let object = value.as_object().ok_or_else(|| {
        DispatchFailure::soft(
            CONNECT_ERROR_PROTOCOL,
            "organization request must be a tagged object",
        )
    })?;
    match (object.get("Query"), object.get("Command")) {
        (Some(_), None) => Ok(false),
        (None, Some(_)) => Ok(true),
        _ => Err(DispatchFailure::soft(
            CONNECT_ERROR_PROTOCOL,
            "organization request must contain exactly one operation",
        )),
    }
}

fn map_organization_runtime_error(error: crate::host::OrganizationRuntimeError) -> DispatchFailure {
    match error {
        crate::host::OrganizationRuntimeError::Unauthorized => DispatchFailure::soft(
            CONNECT_ERROR_FORBIDDEN,
            "organization membership does not authorize this request",
        ),
        crate::host::OrganizationRuntimeError::InvalidRequest => {
            DispatchFailure::soft(CONNECT_ERROR_PROTOCOL, "invalid organization request")
        }
        crate::host::OrganizationRuntimeError::Closed => DispatchFailure::soft(
            CONNECT_ERROR_EXECUTOR_UNATTACHED,
            "organization runtime is closed",
        ),
        crate::host::OrganizationRuntimeError::Org(_)
        | crate::host::OrganizationRuntimeError::Sync(_) => DispatchFailure::soft(
            CONNECT_ERROR_EXECUTOR_UNATTACHED,
            "organization request failed on the host",
        ),
    }
}

struct DispatchFailure {
    code: u16,
    message: &'static str,
    disconnect: bool,
}

impl DispatchFailure {
    fn fatal(code: u16, message: &'static str) -> Self {
        Self {
            code,
            message,
            disconnect: true,
        }
    }

    fn soft(code: u16, message: &'static str) -> Self {
        Self {
            code,
            message,
            disconnect: false,
        }
    }
}

fn bind_request_identity(request: &ClientRequest, bound: ClientId) -> Result<(), DispatchFailure> {
    match request {
        ClientRequest::TerminalInput(request) => {
            if request.client_id != bound {
                return Err(DispatchFailure::soft(
                    CONNECT_ERROR_UNAUTHORIZED,
                    "request client_id is not the authenticated paired identity",
                ));
            }
        }
        ClientRequest::Query(envelope) => {
            if envelope.client_id != bound {
                return Err(DispatchFailure::soft(
                    CONNECT_ERROR_UNAUTHORIZED,
                    "request client_id is not the authenticated paired identity",
                ));
            }
        }
        ClientRequest::Command(envelope) => {
            if envelope.client_id != bound {
                return Err(DispatchFailure::soft(
                    CONNECT_ERROR_UNAUTHORIZED,
                    "request client_id is not the authenticated paired identity",
                ));
            }
        }
        ClientRequest::Detach(_) => {}
    }
    Ok(())
}

fn deny_if_capability_missing(
    request: &ClientRequest,
    granted: CapabilitySet,
) -> Result<(), DispatchFailure> {
    let required = match request {
        ClientRequest::TerminalInput(_) => Some(Capability::ProviderInput),
        ClientRequest::Query(envelope) => match envelope.query {
            crate::domain::query::Query::SnapshotPage { .. }
            | crate::domain::query::Query::ReleaseSnapshot { .. } => {
                Some(Capability::PagedSnapshots)
            }
            crate::domain::query::Query::OpenEventReplay { .. }
            | crate::domain::query::Query::ContinueEventReplay { .. }
            | crate::domain::query::Query::ReleaseEventReplay { .. } => {
                Some(Capability::EventReplay)
            }
            crate::domain::query::Query::OpenArtifactContent { .. }
            | crate::domain::query::Query::ContinueArtifactContent { .. }
            | crate::domain::query::Query::ReleaseArtifactContent { .. } => {
                Some(Capability::ChunkResume)
            }
            crate::domain::query::Query::InspectHostQuit => Some(Capability::HostShutdown),
            crate::domain::query::Query::PromptLibrary(_) => Some(Capability::PromptProjection),
            crate::domain::query::Query::TaskCockpit(_) => Some(Capability::TaskCockpit),
            crate::domain::query::Query::OperationStatus { .. }
            | crate::domain::query::Query::CommandReceiptStatus { .. }
            | crate::domain::query::Query::TaskSnapshot => None,
        },
        ClientRequest::Command(envelope) => match &envelope.command {
            crate::domain::command::Command::SubmitProviderInput(_) => {
                Some(Capability::ProviderInput)
            }
            crate::domain::command::Command::ConfirmHostQuit(_) => Some(Capability::HostShutdown),
            crate::domain::command::Command::PromptLibrary(_)
            | crate::domain::command::Command::PromptChain(_) => Some(Capability::PromptProjection),
            crate::domain::command::Command::ServiceControl(_) => {
                Some(Capability::ServiceSupervisor)
            }
            crate::domain::command::Command::StartProviderSession(_) => {
                Some(Capability::ProviderInput)
            }
            crate::domain::command::Command::Browser(_) => Some(Capability::BrowserProjection),
            _ => None,
        },
        ClientRequest::Detach(_) => Some(Capability::ExplicitDetach),
    };
    if let Some(capability) = required {
        if !granted.contains(capability) {
            return Err(DispatchFailure::soft(
                CONNECT_ERROR_FORBIDDEN,
                "request requires a capability that was not negotiated",
            ));
        }
    }
    if matches!(
        request,
        ClientRequest::Query(envelope)
            if matches!(
                &envelope.query,
                crate::domain::query::Query::TaskCockpit(
                    crate::domain::cockpit::TaskCockpitQuery::OpenConversationSubscription { .. }
                        | crate::domain::cockpit::TaskCockpitQuery::ReleaseConversationSubscription { .. }
                )
            )
    ) && !granted.contains(Capability::SemanticConversation)
    {
        return Err(DispatchFailure::soft(
            CONNECT_ERROR_FORBIDDEN,
            "request requires a capability that was not negotiated",
        ));
    }
    Ok(())
}

fn authorize_established_request(
    session: &ConnectDispatchSession,
    request: &ClientRequest,
) -> Result<(), DispatchFailure> {
    if matches!(request, ClientRequest::Detach(_)) {
        return Ok(());
    }
    let Some((action, task_id)) = action_for_client_request(request) else {
        return Err(DispatchFailure::soft(
            CONNECT_ERROR_FORBIDDEN,
            "unauthorized Connect command",
        ));
    };
    let credential = match (
        session.device_credential.as_ref(),
        session.legacy_host_compat,
    ) {
        (Some(proof), false) => {
            if session
                .session_epoch
                .is_some_and(|epoch| proof.session_epoch() == epoch)
            {
                Some(proof.clone())
            } else {
                return Err(DispatchFailure::soft(
                    CONNECT_ERROR_FORBIDDEN,
                    deny_message(PermissionDenyReason::DeviceCredentialRequired),
                ));
            }
        }
        (None, true) => None,
        _ => {
            return Err(DispatchFailure::soft(
                CONNECT_ERROR_FORBIDDEN,
                deny_message(PermissionDenyReason::DeviceCredentialRequired),
            ));
        }
    };
    let decision =
        PermissionEvaluator::default().evaluate_transport_authenticated_owner(PermissionRequest {
            role: ConnectRole::PairedOwner,
            task_id,
            action,
            credential,
        });
    match decision {
        PermissionDecision::Allow => Ok(()),
        PermissionDecision::Denied(reason) => Err(DispatchFailure::soft(
            CONNECT_ERROR_FORBIDDEN,
            deny_message(reason),
        )),
    }
}

fn deny_message(reason: PermissionDenyReason) -> &'static str {
    match reason {
        PermissionDenyReason::UnknownAction => "unauthorized Connect command",
        PermissionDenyReason::WatcherReadOnly => "Watcher grants are read-only",
        PermissionDenyReason::OwnerOnly => "the action is Owner-only",
        PermissionDenyReason::DeviceCredentialRequired => {
            "PairedOwner actions require a verified device credential"
        }
        PermissionDenyReason::AnonymousPairingOnly => {
            "anonymous pairing may only redeem a bounded pairing capability"
        }
        PermissionDenyReason::IdentityNotLive => "Connect identity is not live",
        _ => "Connect permission denied",
    }
}

fn convert_host_message(
    message: ServerMessage,
    envelope: &ConnectEnvelope,
) -> Result<ConnectPayload, DispatchFailure> {
    let payload = ConnectPayload::from_host_server_message(message).map_err(|_| {
        DispatchFailure::soft(
            CONNECT_ERROR_PROTOCOL,
            "host reply is not a Connect request-lane payload",
        )
    })?;
    match &payload {
        ConnectPayload::QueryReply(reply) => {
            if let Some(request_id) = envelope.request_id {
                if reply.request_id != request_id {
                    return Err(DispatchFailure::soft(
                        CONNECT_ERROR_PROTOCOL,
                        "QueryReply request_id does not match the envelope",
                    ));
                }
            }
        }
        ConnectPayload::CommandReceipt(receipt) => {
            correlate_command_receipt(receipt, envelope)?;
        }
        ConnectPayload::TerminalInputAck(ack) => {
            let Some(operation_id) = envelope.operation_id else {
                return Err(DispatchFailure::soft(
                    CONNECT_ERROR_PROTOCOL,
                    "TerminalInputAck requires operation correlation",
                ));
            };
            if operation_id.as_bytes() != ack.input_id.as_bytes() {
                return Err(DispatchFailure::soft(
                    CONNECT_ERROR_PROTOCOL,
                    "TerminalInputAck input_id does not match the envelope",
                ));
            }
        }
        _ => {}
    }
    Ok(payload)
}

/// Accepted receipts must match a supplied envelope operation_id.
/// Rejected receipts have no operation_id; the envelope field is the authority
/// and is preserved on the sealed response envelope.
fn correlate_command_receipt(
    receipt: &crate::domain::command::CommandReceipt,
    envelope: &ConnectEnvelope,
) -> Result<(), DispatchFailure> {
    match receipt {
        crate::domain::command::CommandReceipt::Accepted { operation_id, .. } => {
            if let Some(expected) = envelope.operation_id {
                if *operation_id != expected {
                    return Err(DispatchFailure::soft(
                        CONNECT_ERROR_PROTOCOL,
                        "CommandReceipt operation_id does not match the envelope",
                    ));
                }
            }
        }
        crate::domain::command::CommandReceipt::Rejected { .. } => {}
    }
    Ok(())
}

fn map_ipc_error(error: IpcError) -> DispatchFailure {
    match error {
        IpcError::Unauthorized => DispatchFailure::soft(
            CONNECT_ERROR_UNAUTHORIZED,
            "request client_id is not the authenticated paired identity",
        ),
        IpcError::Unsupported => DispatchFailure::soft(
            CONNECT_ERROR_FORBIDDEN,
            "request is unsupported on this Connect transport",
        ),
        IpcError::UnsupportedCapability => DispatchFailure::soft(
            CONNECT_ERROR_FORBIDDEN,
            "request requires a capability that was not negotiated",
        ),
        IpcError::Unavailable | IpcError::Busy => DispatchFailure::soft(
            CONNECT_ERROR_EXECUTOR_UNATTACHED,
            "Connect host executor is not attached",
        ),
        _ => DispatchFailure::soft(CONNECT_ERROR_PROTOCOL, "host request failed closed"),
    }
}

fn frame_limits_or_default(limits: ConnectLimits) -> FrameLimits {
    limits.frame_limits()
}

fn typed_error(
    code: u16,
    message: &str,
    request_id: Option<crate::domain::id::RequestId>,
    operation_id: Option<crate::domain::id::OperationId>,
) -> ConnectPayload {
    let max = usize::try_from(MAX_CONNECT_DIAGNOSTIC_BYTES).unwrap_or(8 * 1024);
    let mut bounded = message.to_string();
    if bounded.len() > max {
        bounded.truncate(max);
    }
    if bounded.is_empty() {
        bounded.push_str("Connect error");
    }
    debug_assert!(
        !bounded.contains(CONNECT_HOLD_CALLBACK_FRAGMENT),
        "production Connect must not emit the former 503 HOLD"
    );
    ConnectPayload::Error(ErrorPayload {
        code,
        message: bounded,
        request_id,
        operation_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect::{ConnectEnvelope, SessionId};
    use crate::domain::command::{
        Command, CommandEnvelope, CommandReceipt, RejectionCode, RenameTaskIntent,
    };
    use crate::domain::id::{CommandId, EventId, OperationId, RequestId, TaskId};
    use crate::domain::query::{Query, QueryEnvelope, QueryReply};
    use crate::host::HostRequestExecutor;
    use crate::kernel::CommandBus;
    use crate::protocol::CapabilitySet;

    struct EchoTerminalPort;

    #[async_trait::async_trait]
    impl ConnectHostCommandPort for EchoTerminalPort {
        async fn execute(
            &self,
            _negotiated: NegotiatedParameters,
            request: ClientRequest,
        ) -> Result<ServerMessage, IpcError> {
            let ClientRequest::TerminalInput(request) = request else {
                return Err(IpcError::Unsupported);
            };
            Ok(ServerMessage::TerminalInputAck(
                crate::terminal::protocol::TerminalInputAck {
                    input_id: request.input_id,
                    ack: crate::terminal::protocol::InputAck::Accepted {
                        sequence: request.context.input_sequence,
                    },
                },
            ))
        }
    }

    fn binding() -> ChannelBinding {
        ChannelBinding::new(
            crate::connect::ConnectionId::new(),
            SessionId::new(),
            crate::connect::ChannelId::new(),
        )
    }

    fn hello_payload(capabilities: CapabilitySet, limits: ConnectLimits) -> HelloPayload {
        HelloPayload {
            capabilities,
            limits,
            privacy_class: ConnectPrivacyClass::LocalOnly,
            relay_url: None,
            capability_grant: None,
            client_id: None,
        }
    }

    fn envelope(
        binding: ChannelBinding,
        sequence: u64,
        request_id: Option<RequestId>,
        operation_id: Option<OperationId>,
        limits: ConnectLimits,
        payload: ConnectPayload,
    ) -> ConnectEnvelope {
        ConnectEnvelope::new(
            binding,
            payload.channel(),
            sequence,
            request_id,
            operation_id,
            limits,
            ConnectPrivacyClass::LocalOnly,
            payload,
        )
        .expect("envelope")
    }

    fn assert_not_hold(payload: &ConnectPayload) {
        if let ConnectPayload::Error(error) = payload {
            assert_ne!(
                error.code, 503,
                "production Connect must not return 503 HOLD"
            );
            assert!(
                !error.message.contains(CONNECT_HOLD_CALLBACK_FRAGMENT),
                "must not emit former callback HOLD: {}",
                error.message
            );
            assert!(
                !error.message.contains("unavailable until"),
                "must not emit former HOLD phrasing: {}",
                error.message
            );
        }
    }

    async fn complete_hello(
        session: &mut ConnectDispatchSession,
        binding: ChannelBinding,
    ) -> (ConnectLimits, ClientId) {
        let limits = ConnectLimits::v1_default();
        let payload =
            ConnectPayload::Hello(hello_payload(advertised_connect_capabilities(), limits));
        let env = envelope(binding, 1, None, None, limits, payload.clone());
        let (reply, disposition) = session.handle_payload(&env, payload, None).await;
        assert_eq!(disposition, ConnectSessionDisposition::Continue);
        match reply {
            ConnectPayload::Hello(hello) => {
                assert!(!hello.capabilities.contains(Capability::EventReplay));
                let client_id = hello.client_id.expect("Hello reply assigns client_id");
                assert_eq!(session.bound_client_id(), Some(client_id));
                (hello.limits, client_id)
            }
            other => panic!("expected Hello reply, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hello_honors_carrier_limit_ceiling_before_requests_are_admitted() {
        let binding = binding();
        let mut ceiling = ConnectLimits::v1_default();
        ceiling.max_physical_frame_bytes = 64 * 1024;
        ceiling.max_page_encoded_bytes = 48 * 1024;
        ceiling.max_chunk_bytes = 48 * 1024;
        let mut session = ConnectDispatchSession::bind_paired(
            "web-paired-owner".to_owned(),
            ConnectIdentityLiveState::Live,
        )
        .with_legacy_host_compat()
        .with_limit_ceiling(ceiling);

        let (negotiated, _) = complete_hello(&mut session, binding).await;

        assert_eq!(negotiated, ceiling);
        assert_eq!(session.negotiated_limits(), Some(ceiling));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn query_reaches_host_request_handle_and_returns_typed_query_reply() {
        let directory = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&directory.path().join("connect-query.db")).expect("bus");
        let (handle, executor) = HostRequestExecutor::start(bus);
        let binding = binding();
        let mut session = ConnectDispatchSession::bind_paired(
            "web-paired-owner".to_owned(),
            ConnectIdentityLiveState::Live,
        )
        .with_legacy_host_compat();
        let (limits, client_id) = complete_hello(&mut session, binding).await;
        let request_id = RequestId::new();
        let query = QueryEnvelope {
            request_id,
            client_id,
            task_id: None,
            query: Query::OperationStatus {
                operation_id: OperationId::new(),
            },
        };
        let payload = ConnectPayload::Query(query);
        let env = envelope(binding, 2, Some(request_id), None, limits, payload.clone());
        let (reply, disposition) = session.handle_payload(&env, payload, Some(&handle)).await;
        assert_eq!(disposition, ConnectSessionDisposition::Continue);
        assert_not_hold(&reply);
        let ConnectPayload::QueryReply(QueryReply {
            request_id: reply_id,
            ..
        }) = reply
        else {
            panic!("expected QueryReply, got {reply:?}");
        };
        assert_eq!(reply_id, request_id);
        drop(handle);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn command_reaches_host_request_handle_and_returns_typed_command_receipt() {
        let directory = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&directory.path().join("connect-command.db")).expect("bus");
        let (handle, executor) = HostRequestExecutor::start(bus);
        let binding = binding();
        let mut session = ConnectDispatchSession::bind_paired(
            "web-paired-owner".to_owned(),
            ConnectIdentityLiveState::Live,
        )
        .with_legacy_host_compat();
        let (limits, client_id) = complete_hello(&mut session, binding).await;
        let command_id = CommandId::new();
        let request_id = RequestId::new();
        let command = CommandEnvelope {
            command_id,
            client_id,
            task_id: Some(TaskId::new()),
            issued_at_ms: 1,
            expected_task_revision: None,
            // Use an ordinary command-bus mutation here. BeginCloseTask is a
            // host effect that intentionally requires a current task revision
            // before it enters the durable bus, so a made-up task would test
            // that close fence instead of Connect command dispatch.
            command: Command::RenameTask(RenameTaskIntent {
                title: "Connect dispatch probe".into(),
            }),
        };
        let payload = ConnectPayload::Command(command);
        let env = envelope(binding, 2, Some(request_id), None, limits, payload.clone());
        let (reply, disposition) = session.handle_payload(&env, payload, Some(&handle)).await;
        assert_eq!(disposition, ConnectSessionDisposition::Continue);
        assert_not_hold(&reply);
        let ConnectPayload::CommandReceipt(receipt) = reply else {
            panic!("expected CommandReceipt, got {reply:?}");
        };
        assert_eq!(receipt.command_id(), command_id);
        let _ = CommandReceipt::command_id(&receipt);
        drop(handle);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn terminal_input_reaches_host_and_returns_exact_correlated_ack() {
        use crate::domain::{AgentSessionId, ResourceId, TerminalId};
        use crate::terminal::protocol::{
            FocusEpoch, InputId, TerminalGeneration, TerminalInputContext, TerminalInputRequest,
            TerminalSessionId,
        };

        let binding = binding();
        let mut session = ConnectDispatchSession::bind_paired(
            "web-paired-owner".to_owned(),
            ConnectIdentityLiveState::Live,
        )
        .with_legacy_host_compat();
        let (limits, client_id) = complete_hello(&mut session, binding).await;
        let request = TerminalInputRequest {
            client_id,
            input_id: InputId::new(),
            terminal_id: TerminalId::new(),
            context: TerminalInputContext {
                task_id: TaskId::new(),
                agent_session_id: AgentSessionId::new(),
                resource_id: ResourceId::new(),
                runtime_generation: 2,
                resource_generation: 3,
                session_id: TerminalSessionId::new(),
                terminal_generation: TerminalGeneration::initial(),
                focus_epoch: FocusEpoch::initial(),
                action_epoch: 4,
                input_sequence: 5,
            },
            bytes: b"remote input\r".to_vec(),
        };
        let operation_id = OperationId::from_bytes(*request.input_id.as_bytes())
            .expect("input id is operation id");
        let payload = ConnectPayload::TerminalInput(request.clone());
        let env = ConnectEnvelope::new(
            binding,
            payload.channel(),
            2,
            None,
            Some(operation_id),
            limits,
            ConnectPrivacyClass::RawContent,
            payload.clone(),
        )
        .expect("terminal envelope");
        let port = EchoTerminalPort;
        let (reply, disposition) = session.handle_payload(&env, payload, Some(&port)).await;
        assert_eq!(disposition, ConnectSessionDisposition::Continue);
        let ConnectPayload::TerminalInputAck(ack) = reply else {
            panic!("expected terminal ack, got {reply:?}");
        };
        assert_eq!(ack.input_id, request.input_id);
        assert_eq!(
            ack.ack,
            crate::terminal::protocol::InputAck::Accepted { sequence: 5 }
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn missing_hello_fails_closed_without_dispatch() {
        let mut session = ConnectDispatchSession::bind_paired(
            "web-paired-owner".to_owned(),
            ConnectIdentityLiveState::Live,
        )
        .with_legacy_host_compat();
        assert!(session.bound_client_id().is_none());
        let request_id = RequestId::new();
        let payload = ConnectPayload::Query(QueryEnvelope {
            request_id,
            client_id: ClientId::new(),
            task_id: None,
            query: Query::TaskSnapshot,
        });
        let env = envelope(
            binding(),
            1,
            Some(request_id),
            None,
            ConnectLimits::v1_default(),
            payload.clone(),
        );
        let (reply, disposition) = session.handle_payload(&env, payload, None).await;
        assert_eq!(disposition, ConnectSessionDisposition::Disconnect);
        assert_not_hold(&reply);
        let ConnectPayload::Error(error) = reply else {
            panic!("expected typed error");
        };
        assert_eq!(error.code, CONNECT_ERROR_PROTOCOL);
        assert!(error.message.contains("Hello"));
        assert_eq!(error.request_id, Some(request_id));
        assert!(session.bound_client_id().is_none());
        assert!(session.channel_binding().is_none());
        assert!(session.negotiated_limits().is_none());
        assert!(!session.is_active());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn invalid_hello_grant_and_capability_mismatch_fail_closed() {
        let binding = binding();
        let mut session = ConnectDispatchSession::bind_paired(
            "web-paired-owner".to_owned(),
            ConnectIdentityLiveState::Live,
        )
        .with_legacy_host_compat();
        let mut hello = hello_payload(
            advertised_connect_capabilities(),
            ConnectLimits::v1_default(),
        );
        hello.capability_grant = Some(crate::connect::HostCapabilityGrant {
            role: crate::connect::HostConnectRole::Owner,
            task_id: "task-1".to_owned(),
            actions: vec![crate::connect::HostConnectAction::MutateTask],
        });
        let payload = ConnectPayload::Hello(hello);
        let env = envelope(
            binding,
            1,
            None,
            None,
            ConnectLimits::v1_default(),
            payload.clone(),
        );
        let (reply, disposition) = session.handle_payload(&env, payload, None).await;
        assert_eq!(disposition, ConnectSessionDisposition::Disconnect);
        assert_not_hold(&reply);
        let ConnectPayload::Error(error) = reply else {
            panic!("expected typed error");
        };
        assert_eq!(error.code, CONNECT_ERROR_FORBIDDEN);

        let mut session = ConnectDispatchSession::bind_paired(
            "web-paired-owner".to_owned(),
            ConnectIdentityLiveState::Live,
        )
        .with_legacy_host_compat();
        let (limits, _) = complete_hello(&mut session, binding).await;
        let payload = ConnectPayload::Capabilities(CapabilitySet::from_capabilities([
            Capability::OrganizationProjection,
        ]));
        let env = envelope(binding, 2, None, None, limits, payload.clone());
        let (reply, disposition) = session.handle_payload(&env, payload, None).await;
        assert_eq!(disposition, ConnectSessionDisposition::Disconnect);
        assert_not_hold(&reply);
        let ConnectPayload::Error(error) = reply else {
            panic!("expected typed error");
        };
        assert_eq!(error.code, CONNECT_ERROR_FORBIDDEN);
        assert!(!session.is_active());
        assert!(session.negotiated_limits().is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn limits_mismatch_wrong_binding_and_replay_fail_closed() {
        let channel_binding = binding();
        let mut session = ConnectDispatchSession::bind_paired(
            "web-paired-owner".to_owned(),
            ConnectIdentityLiveState::Live,
        )
        .with_legacy_host_compat();
        let (limits, client_id) = complete_hello(&mut session, channel_binding).await;
        let other_limits = ConnectLimits::try_new(
            limits.max_physical_frame_bytes,
            limits.max_reassembled_message_bytes,
            1,
            limits.max_page_encoded_bytes,
            limits.max_chunk_bytes,
            limits.max_cumulative_bytes,
        )
        .expect("smaller valid limits");
        let request_id = RequestId::new();
        let query = ConnectPayload::Query(QueryEnvelope {
            request_id,
            client_id,
            task_id: None,
            query: Query::TaskSnapshot,
        });
        let env = envelope(
            channel_binding,
            2,
            Some(request_id),
            None,
            other_limits,
            query.clone(),
        );
        let (reply, disposition) = session.handle_payload(&env, query, None).await;
        assert_eq!(disposition, ConnectSessionDisposition::Disconnect);
        assert_not_hold(&reply);
        assert!(
            matches!(reply, ConnectPayload::Error(error) if error.code == CONNECT_ERROR_PROTOCOL)
        );

        let mut session = ConnectDispatchSession::bind_paired(
            "web-paired-owner".to_owned(),
            ConnectIdentityLiveState::Live,
        )
        .with_legacy_host_compat();
        let (limits, client_id) = complete_hello(&mut session, channel_binding).await;
        let query = ConnectPayload::Query(QueryEnvelope {
            request_id,
            client_id,
            task_id: None,
            query: Query::TaskSnapshot,
        });
        let env = envelope(binding(), 2, Some(request_id), None, limits, query.clone());
        let (reply, disposition) = session.handle_payload(&env, query, None).await;
        assert_eq!(disposition, ConnectSessionDisposition::Disconnect);
        assert_not_hold(&reply);
        assert!(
            matches!(reply, ConnectPayload::Error(error) if error.code == CONNECT_ERROR_PROTOCOL)
        );

        let mut session = ConnectDispatchSession::bind_paired(
            "web-paired-owner".to_owned(),
            ConnectIdentityLiveState::Live,
        )
        .with_legacy_host_compat();
        let (limits, _) = complete_hello(&mut session, channel_binding).await;
        let hello = ConnectPayload::Hello(hello_payload(advertised_connect_capabilities(), limits));
        let env = envelope(channel_binding, 1, None, None, limits, hello.clone());
        let (reply, disposition) = session.handle_payload(&env, hello, None).await;
        assert_eq!(disposition, ConnectSessionDisposition::Disconnect);
        assert_not_hold(&reply);
        assert!(
            matches!(reply, ConnectPayload::Error(error) if error.code == CONNECT_ERROR_CONFLICT)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unauthorized_command_and_unadvertised_replay_do_not_dispatch() {
        let binding = binding();
        let mut session = ConnectDispatchSession::bind_paired(
            "web-paired-owner".to_owned(),
            ConnectIdentityLiveState::Live,
        )
        .with_legacy_host_compat();
        let (limits, bound) = complete_hello(&mut session, binding).await;
        let request_id = RequestId::new();
        let payload = ConnectPayload::Command(CommandEnvelope {
            command_id: CommandId::new(),
            client_id: ClientId::new(),
            task_id: Some(TaskId::new()),
            issued_at_ms: 1,
            expected_task_revision: None,
            command: Command::BeginCloseTask,
        });
        let env = envelope(binding, 2, Some(request_id), None, limits, payload.clone());
        let (reply, disposition) = session.handle_payload(&env, payload, None).await;
        assert_eq!(disposition, ConnectSessionDisposition::Continue);
        assert_not_hold(&reply);
        let ConnectPayload::Error(error) = reply else {
            panic!("expected typed error");
        };
        assert_eq!(error.code, CONNECT_ERROR_UNAUTHORIZED);
        assert_eq!(error.request_id, Some(request_id));
        assert!(!error.message.contains("web-paired-owner"));
        assert!(!error.message.contains("secret"));

        let payload = ConnectPayload::Query(QueryEnvelope {
            request_id,
            client_id: bound,
            task_id: None,
            query: Query::OpenEventReplay { after_sequence: 0 },
        });
        let env = envelope(binding, 3, Some(request_id), None, limits, payload.clone());
        let (reply, disposition) = session.handle_payload(&env, payload, None).await;
        assert_eq!(disposition, ConnectSessionDisposition::Continue);
        assert_not_hold(&reply);
        let ConnectPayload::Error(error) = reply else {
            panic!("expected typed error");
        };
        assert_eq!(error.code, CONNECT_ERROR_FORBIDDEN);
        assert!(error.message.contains("capability"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn paired_pin_owns_hello_identity_and_capability_ceiling() {
        let assigned = ClientId::new();
        for supplied in [None, Some(assigned), Some(ClientId::new())] {
            let mut session = ConnectDispatchSession::bind_paired(
                "paired".into(),
                ConnectIdentityLiveState::Live,
            )
            .with_legacy_host_compat()
            .with_assigned_client_id(assigned)
            .with_capability_ceiling(CapabilitySet::from_capabilities([
                Capability::PagedSnapshots,
            ]));
            let limits = ConnectLimits::v1_default();
            let mut hello = hello_payload(advertised_connect_capabilities(), limits);
            hello.client_id = supplied;
            let payload = ConnectPayload::Hello(hello);
            let env = envelope(binding(), 1, None, None, limits, payload.clone());
            let (reply, disposition) = session.handle_payload(&env, payload, None).await;
            if supplied.is_some_and(|id| id != assigned) {
                assert_eq!(disposition, ConnectSessionDisposition::Disconnect);
                assert!(
                    matches!(reply, ConnectPayload::Error(error) if error.code == CONNECT_ERROR_UNAUTHORIZED)
                );
            } else {
                assert_eq!(disposition, ConnectSessionDisposition::Continue);
                let ConnectPayload::Hello(hello) = reply else {
                    panic!("expected hello")
                };
                assert_eq!(hello.client_id, Some(assigned));
                assert!(!hello.capabilities.contains(Capability::HostShutdown));
                assert!(!hello.capabilities.contains(Capability::UpdateHandoff));
                assert_eq!(
                    hello.capabilities,
                    advertised_connect_capabilities().intersection(
                        CapabilitySet::from_capabilities([Capability::PagedSnapshots])
                    )
                );
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn production_path_never_returns_callback_hold_and_keeps_paired_identity() {
        let mut session = ConnectDispatchSession::bind_paired(
            "web-paired-owner".to_owned(),
            ConnectIdentityLiveState::Live,
        )
        .with_legacy_host_compat();
        assert!(session.bound_client_id().is_none());
        assert!(session.paired_identity_bound());
        let binding = binding();
        let (_, assigned) = complete_hello(&mut session, binding).await;
        assert_eq!(session.bound_client_id(), Some(assigned));
        assert!(session.paired_identity_bound());
        session.disconnect();
        assert!(!session.paired_identity_bound());
        assert!(session.bound_client_id().is_none());
        assert!(session.channel_binding().is_none());
        assert!(session.negotiated_limits().is_none());
        assert!(!session.is_active());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hello_assigns_or_binds_supplied_client_id_and_rejects_mismatch() {
        let binding = binding();
        let mut session = ConnectDispatchSession::bind_paired(
            "web-paired-owner".to_owned(),
            ConnectIdentityLiveState::Live,
        )
        .with_legacy_host_compat();
        let supplied = ClientId::new();
        let mut hello = hello_payload(
            advertised_connect_capabilities(),
            ConnectLimits::v1_default(),
        );
        hello.client_id = Some(supplied);
        let payload = ConnectPayload::Hello(hello);
        let env = envelope(
            binding,
            1,
            None,
            None,
            ConnectLimits::v1_default(),
            payload.clone(),
        );
        let (reply, disposition) = session.handle_payload(&env, payload, None).await;
        assert_eq!(disposition, ConnectSessionDisposition::Continue);
        let ConnectPayload::Hello(hello) = reply else {
            panic!("expected Hello reply");
        };
        assert_eq!(hello.client_id, Some(supplied));
        assert_eq!(session.bound_client_id(), Some(supplied));

        let request_id = RequestId::new();
        let limits = session.negotiated_limits().expect("negotiated");
        let query = ConnectPayload::Query(QueryEnvelope {
            request_id,
            client_id: ClientId::new(),
            task_id: None,
            query: Query::TaskSnapshot,
        });
        let env = envelope(binding, 2, Some(request_id), None, limits, query.clone());
        let (reply, disposition) = session.handle_payload(&env, query, None).await;
        assert_eq!(disposition, ConnectSessionDisposition::Continue);
        let ConnectPayload::Error(error) = reply else {
            panic!("expected typed error");
        };
        assert_eq!(error.code, CONNECT_ERROR_UNAUTHORIZED);
        assert_eq!(error.request_id, Some(request_id));
        assert_eq!(session.bound_client_id(), Some(supplied));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn duplicate_hello_and_matching_capabilities_confirmation() {
        let binding = binding();
        let mut session = ConnectDispatchSession::bind_paired(
            "web-paired-owner".to_owned(),
            ConnectIdentityLiveState::Live,
        )
        .with_legacy_host_compat();
        let (limits, _) = complete_hello(&mut session, binding).await;
        let duplicate =
            ConnectPayload::Hello(hello_payload(advertised_connect_capabilities(), limits));
        let env = envelope(binding, 2, None, None, limits, duplicate.clone());
        let (reply, disposition) = session.handle_payload(&env, duplicate, None).await;
        assert_eq!(disposition, ConnectSessionDisposition::Disconnect);
        assert_not_hold(&reply);
        assert!(matches!(
            reply,
            ConnectPayload::Error(error) if error.code == CONNECT_ERROR_PROTOCOL
        ));
        assert!(!session.is_active());
        assert!(session.bound_client_id().is_none());

        let mut session = ConnectDispatchSession::bind_paired(
            "web-paired-owner".to_owned(),
            ConnectIdentityLiveState::Live,
        )
        .with_legacy_host_compat();
        let (limits, client_id) = complete_hello(&mut session, binding).await;
        let negotiated = session.negotiated_capabilities().expect("negotiated");
        let confirm = ConnectPayload::Capabilities(negotiated);
        let env = envelope(binding, 2, None, None, limits, confirm.clone());
        let (reply, disposition) = session.handle_payload(&env, confirm, None).await;
        assert_eq!(disposition, ConnectSessionDisposition::Continue);
        assert!(matches!(reply, ConnectPayload::Capabilities(_)));

        let request_id = RequestId::new();
        let query = ConnectPayload::Query(QueryEnvelope {
            request_id,
            client_id,
            task_id: None,
            query: Query::TaskSnapshot,
        });
        let env = envelope(binding, 3, Some(request_id), None, limits, query.clone());
        let (reply, disposition) = session.handle_payload(&env, query, None).await;
        assert_eq!(disposition, ConnectSessionDisposition::Continue);
        assert_not_hold(&reply);
        assert!(matches!(
            reply,
            ConnectPayload::Error(error) if error.code == CONNECT_ERROR_EXECUTOR_UNATTACHED
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn attach_after_start_slot_clone_is_observed_and_cleared() {
        let live = ConnectHostRequestSlot::new();
        let stored = live.clone();
        assert!(live.get().is_none());
        let directory = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&directory.path().join("connect-slot-clone.db")).expect("bus");
        let (handle, executor) = HostRequestExecutor::start(bus);
        stored.attach(handle.clone());
        assert!(
            live.get().is_some(),
            "attach on a cloned slot must update the live WebState slot"
        );
        stored.clear();
        assert!(live.get().is_none());
        drop(handle);
        executor.abort();
        let _ = executor.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn process_slot_unbind_clears_clones_and_cannot_reuse_stale_handle() {
        let observed = process_connect_host_request_slot();
        let directory = tempfile::tempdir().expect("tempdir");
        let bus = CommandBus::open(&directory.path().join("connect-process-slot.db")).expect("bus");
        let (handle, executor) = HostRequestExecutor::start(bus);
        bind_host_request_handle(handle.clone());
        assert!(observed.get().is_some());
        assert!(bound_host_request_handle().is_some());
        unbind_host_request_handle();
        assert!(observed.get().is_none());
        assert!(bound_host_request_handle().is_none());
        drop(handle);
        executor.abort();
        let _ = executor.await;
    }

    #[test]
    fn command_receipt_correlation_accepted_mismatch_and_rejected_envelope_authority() {
        let binding = binding();
        let expected = OperationId::new();
        let other = OperationId::new();
        let accepted = CommandReceipt::Accepted {
            command_id: CommandId::new(),
            operation_id: other,
            task_revision: Some(1),
            event_ids: vec![EventId::new()],
            prompt_mutation: None,
        };
        let payload = ConnectPayload::CommandReceipt(accepted.clone());
        let env = envelope(
            binding,
            1,
            None,
            Some(expected),
            ConnectLimits::v1_default(),
            payload,
        );
        assert!(correlate_command_receipt(&accepted, &env).is_err());

        let rejected = CommandReceipt::Rejected {
            command_id: CommandId::new(),
            code: RejectionCode::NotFound,
            current_revision: None,
            resolution: None,
        };
        let payload = ConnectPayload::CommandReceipt(rejected.clone());
        let env = envelope(
            binding,
            1,
            None,
            Some(expected),
            ConnectLimits::v1_default(),
            payload,
        );
        assert!(
            correlate_command_receipt(&rejected, &env).is_ok(),
            "rejected receipts have no operation_id; envelope correlation is the authority"
        );
        assert!(rejected.accepted_operation_id().is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn typed_error_preserves_envelope_request_and_operation_ids() {
        let mut session = ConnectDispatchSession::bind_paired(
            "web-paired-owner".to_owned(),
            ConnectIdentityLiveState::Live,
        )
        .with_legacy_host_compat();
        let request_id = RequestId::new();
        let operation_id = OperationId::new();
        let payload = ConnectPayload::Command(CommandEnvelope {
            command_id: CommandId::new(),
            client_id: ClientId::new(),
            task_id: Some(TaskId::new()),
            issued_at_ms: 1,
            expected_task_revision: None,
            command: Command::BeginCloseTask,
        });
        let env = envelope(
            binding(),
            1,
            Some(request_id),
            Some(operation_id),
            ConnectLimits::v1_default(),
            payload.clone(),
        );
        let (reply, disposition) = session.handle_payload(&env, payload, None).await;
        assert_eq!(disposition, ConnectSessionDisposition::Disconnect);
        let ConnectPayload::Error(error) = reply else {
            panic!("expected typed error");
        };
        assert_eq!(error.code, CONNECT_ERROR_PROTOCOL);
        assert_eq!(error.request_id, Some(request_id));
        assert_eq!(error.operation_id, Some(operation_id));
    }

    #[test]
    fn advertised_capabilities_do_not_claim_live_event_replay() {
        let advertised = advertised_connect_capabilities();
        assert!(advertised.contains(Capability::ConnectEncryption));
        assert!(!advertised.contains(Capability::EventReplay));
        assert!(!advertised.contains(Capability::TerminalDeltas));
        assert!(!advertised.contains(Capability::OrganizationProjection));
    }

    #[test]
    fn bind_paired_defaults_without_legacy_and_epoch_zero_stays_fail_closed() {
        let session = ConnectDispatchSession::bind_paired(
            "web-paired-owner".to_owned(),
            ConnectIdentityLiveState::Live,
        );
        assert!(!session.legacy_host_compat_for_test());
        assert!(!session.has_device_credential_for_test());
        let legacy = ConnectDispatchSession::bind_paired(
            "web-paired-owner".to_owned(),
            ConnectIdentityLiveState::Live,
        )
        .with_legacy_host_compat();
        assert!(legacy.legacy_host_compat_for_test());
        assert!(!legacy.has_device_credential_for_test());
        // with_device_credential(proof, 0) clears credential and leaves
        // legacy_host_compat=false (see impl); bridge also closes on epoch 0.
        // Opaque DeviceCredentialProof has no public test constructor here.
    }
}

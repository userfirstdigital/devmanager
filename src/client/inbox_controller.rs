//! Native-next Inbox host controller.
//!
//! This is the execution boundary for the real desktop client: one explicit
//! [`HostClient`], one [`ClientSubscription`], and caller-driven synchronization
//! and receive.  It deliberately has no GPUI, timer, thread, environment, or
//! legacy session dependency.  A native-next shell can run these methods from
//! its background executor and hand the bounded updates to its projection.

use std::sync::{Arc, Mutex};

use crate::client::host_client::{HostClient, HostClientConfig};
use crate::client::preferences::{ClientPreferenceError, InboxPreferenceStore};
use crate::client::subscription::{
    ClientSubscription, ClientSubscriptionState, SubscriptionError, SubscriptionUpdate,
};
use crate::domain::command::{CommandEnvelope, CommandReceipt};
use crate::host::IpcError;

pub type SharedInboxSubscription = Arc<Mutex<ClientSubscription>>;

#[derive(Debug)]
pub enum InboxControllerError {
    Host(IpcError),
    Subscription(SubscriptionError),
    Preference(ClientPreferenceError),
    PoisonedSubscription,
    NotConnected,
}

impl std::fmt::Display for InboxControllerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Host(error) => write!(f, "Inbox host client failed: {error}"),
            Self::Subscription(error) => write!(f, "Inbox subscription failed: {error}"),
            Self::Preference(error) => write!(f, "Inbox preference failed: {error}"),
            Self::PoisonedSubscription => write!(f, "Inbox subscription lock is poisoned"),
            Self::NotConnected => write!(f, "Inbox host client is not connected"),
        }
    }
}

impl std::error::Error for InboxControllerError {}

impl From<IpcError> for InboxControllerError {
    fn from(error: IpcError) -> Self {
        Self::Host(error)
    }
}

impl From<SubscriptionError> for InboxControllerError {
    fn from(error: SubscriptionError) -> Self {
        Self::Subscription(error)
    }
}

impl From<ClientPreferenceError> for InboxControllerError {
    fn from(error: ClientPreferenceError) -> Self {
        Self::Preference(error)
    }
}

/// One native-next client owner for the Inbox stream.
///
/// `HostClientConfig` is supplied by the caller and therefore always carries
/// an explicit isolated profile. There is no default profile and no lookup of
/// `DEVMANAGER_PROFILE` here. Callers keep the owner on one background
/// executor and never borrow it from paint/input work.
pub struct InboxHostController {
    config: HostClientConfig,
    client: Option<HostClient>,
    subscription: SharedInboxSubscription,
    preferences: InboxPreferenceStore,
}

impl std::fmt::Debug for InboxHostController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InboxHostController")
            .field("named_profile", &self.config.named_profile)
            .field(
                "connected",
                &self.client.as_ref().is_some_and(HostClient::is_connected),
            )
            .field("subscription_state", &self.subscription_state())
            .field("preference_path", &self.preferences.path())
            .finish()
    }
}

impl InboxHostController {
    pub fn new(config: HostClientConfig, preferences: InboxPreferenceStore) -> Self {
        Self {
            config,
            client: None,
            subscription: Arc::new(Mutex::new(ClientSubscription::new())),
            preferences,
        }
    }

    pub fn subscription(&self) -> SharedInboxSubscription {
        Arc::clone(&self.subscription)
    }

    pub fn subscription_state(&self) -> ClientSubscriptionState {
        self.subscription
            .lock()
            .map(|subscription| subscription.state())
            .unwrap_or(ClientSubscriptionState::NeedsResync)
    }

    pub fn is_connected(&self) -> bool {
        self.client.as_ref().is_some_and(HostClient::is_connected)
    }

    pub fn preferences(&self) -> &InboxPreferenceStore {
        &self.preferences
    }

    /// Restore the bounded cursor bytes before the first projection attach.
    /// The caller decodes them through `UnreadCursor::decode_durable` and
    /// rejects invalid/foreign versions instead of manufacturing unread state.
    pub fn restore_unread_cursor(&self) -> Result<Option<Vec<u8>>, InboxControllerError> {
        self.preferences.load_unread_cursor().map_err(Into::into)
    }

    pub fn persist_unread_cursor(&self, cursor: Option<&[u8]>) -> Result<(), InboxControllerError> {
        self.preferences
            .save_unread_cursor(cursor)
            .map_err(Into::into)
    }

    /// Attach to the configured host and perform snapshot + race-closing
    /// replay.  Replay events remain available through the shared subscription
    /// until the projection drains them.
    pub async fn synchronize(&mut self) -> Result<(), InboxControllerError> {
        if self.client.is_none() {
            self.client = Some(HostClient::connect(self.config.clone()).await?);
        }
        let client = self
            .client
            .as_mut()
            .ok_or(InboxControllerError::NotConnected)?;
        let result = {
            let mut subscription = self
                .subscription
                .lock()
                .map_err(|_| InboxControllerError::PoisonedSubscription)?;
            subscription.synchronize(client).await
        };
        if let Err(error) = result {
            if matches!(error, SubscriptionError::Transport(_)) {
                self.client = None;
            }
            return Err(error.into());
        }
        Ok(())
    }

    /// Reconnect the authenticated client and always rebuild snapshot + replay
    /// before allowing live receive.  This makes a disconnect a bounded
    /// resync boundary rather than an attempt to apply stale events.
    pub async fn reconnect_and_synchronize(&mut self) -> Result<(), InboxControllerError> {
        if let Some(client) = self.client.as_mut() {
            if let Err(error) = client.reconnect().await {
                self.client = None;
                return Err(error.into());
            }
        } else {
            self.client = Some(HostClient::connect(self.config.clone()).await?);
        }
        self.synchronize().await
    }

    /// Drain one live update.  This is the only method that waits on host I/O;
    /// native-next calls it from its controller/task lane, never from paint or
    /// input dispatch.  Transport loss leaves the subscription fenced and the
    /// next caller must invoke `reconnect_and_synchronize`.
    pub async fn receive_one(&mut self) -> Result<SubscriptionUpdate, InboxControllerError> {
        let client = self
            .client
            .as_ref()
            .ok_or(InboxControllerError::NotConnected)?;
        let result = {
            let mut subscription = self
                .subscription
                .lock()
                .map_err(|_| InboxControllerError::PoisonedSubscription)?;
            subscription.recv_and_apply(client).await
        };
        if let Err(error) = &result {
            if matches!(error, SubscriptionError::Transport(_)) {
                self.client = None;
            }
        }
        result.map_err(Into::into)
    }

    /// Consume the real command lane on the same authenticated HostClient.
    /// Callers pass a previously captured, revision-fenced envelope from the
    /// shell dispatcher; this method does not synthesize task identity.
    pub async fn execute_command(
        &mut self,
        envelope: CommandEnvelope,
    ) -> Result<CommandReceipt, InboxControllerError> {
        let client = self
            .client
            .as_mut()
            .ok_or(InboxControllerError::NotConnected)?;
        client.execute_command(envelope).await.map_err(Into::into)
    }

    pub fn take_replay_events(
        &self,
    ) -> Result<Vec<crate::domain::event::DomainEvent>, InboxControllerError> {
        self.subscription
            .lock()
            .map(|mut subscription| subscription.take_replay_events())
            .map_err(|_| InboxControllerError::PoisonedSubscription)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::HostClientConfig;
    use crate::domain::ClientId;
    use crate::protocol::{Capability, CapabilitySet, FrameLimits};

    fn test_config(profile: &str) -> HostClientConfig {
        HostClientConfig {
            named_profile: profile.to_string(),
            client_build: "inbox-controller-test".to_string(),
            client_id: ClientId::new(),
            requested: CapabilitySet::from_capabilities([
                Capability::PagedSnapshots,
                Capability::EventReplay,
            ]),
            limits: FrameLimits::v1_default(),
        }
    }

    #[test]
    fn controller_requires_explicit_profile_and_uses_isolated_preferences() {
        let directory = tempfile::tempdir().expect("temp directory");
        let preferences = InboxPreferenceStore::at_profile_root(directory.path());
        let controller =
            InboxHostController::new(test_config("inbox-controller-test"), preferences);
        assert_eq!(
            controller.subscription_state(),
            ClientSubscriptionState::Pending
        );
        assert!(!controller.is_connected());
        assert_eq!(
            controller.restore_unread_cursor().expect("default cursor"),
            None
        );
        assert_eq!(
            controller.preferences().path().parent(),
            Some(directory.path())
        );
    }

    #[test]
    fn controller_cursor_persistence_never_uses_session_file() {
        let directory = tempfile::tempdir().expect("temp directory");
        let preferences = InboxPreferenceStore::at_profile_root(directory.path());
        let controller =
            InboxHostController::new(test_config("inbox-controller-test"), preferences);
        controller
            .persist_unread_cursor(Some(&[0x01, 0x02]))
            .expect("persist cursor");
        assert_eq!(
            controller.restore_unread_cursor().expect("restore cursor"),
            Some(vec![0x01, 0x02])
        );
        assert!(!controller
            .preferences()
            .path()
            .file_name()
            .is_some_and(|name| name == "session.json"));
    }
}

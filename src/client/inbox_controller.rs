//! Transport-free Inbox lane.
//!
//! The canonical native host runtime owns the authenticated transport. This
//! module owns only one subscription/release state, preferences, and a
//! bounded nonblocking handoff into the projection. Transport IO is supplied by a
//! borrowed [`InboxTransport`] or by already-applied subscription events.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::client::preferences::{ClientPreferenceError, InboxPreferenceStore};
use crate::client::subscription::{
    ClientSubscription, ClientSubscriptionState, SubscriptionError, SubscriptionUpdate,
};
use crate::domain::command::{CommandEnvelope, CommandReceipt};
use crate::host::IpcError;
use crate::ui::task_cockpit::{InboxRuntime, SearchProgress};

pub type SharedInboxSubscription = Arc<Mutex<ClientSubscription>>;

pub type InboxTransportFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Borrowed transport seam implemented by the canonical native host runtime.
/// The Inbox lane never stores or constructs an authenticated transport.
pub trait InboxTransport {
    fn is_connected(&self) -> bool;

    fn synchronize<'a>(
        &'a mut self,
        subscription: &'a mut ClientSubscription,
    ) -> InboxTransportFuture<'a, Result<(), SubscriptionError>>;

    fn receive_one<'a>(
        &'a self,
        subscription: &'a mut ClientSubscription,
    ) -> InboxTransportFuture<'a, Result<SubscriptionUpdate, SubscriptionError>>;

    fn release<'a>(
        &'a mut self,
        subscription: &'a mut ClientSubscription,
    ) -> InboxTransportFuture<'a, Result<(), SubscriptionError>>;

    fn reconnect<'a>(&'a mut self) -> InboxTransportFuture<'a, Result<(), IpcError>>;

    fn execute_command<'a>(
        &'a mut self,
        envelope: CommandEnvelope,
    ) -> InboxTransportFuture<'a, Result<CommandReceipt, IpcError>>;
}

#[derive(Debug)]
pub enum InboxControllerError {
    Host(IpcError),
    Subscription(SubscriptionError),
    Preference(ClientPreferenceError),
    PoisonedSubscription,
    Unattached,
    AlreadyAttached,
}

impl std::fmt::Display for InboxControllerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Host(error) => write!(f, "Inbox transport failed: {error}"),
            Self::Subscription(error) => write!(f, "Inbox subscription failed: {error}"),
            Self::Preference(error) => write!(f, "Inbox preference failed: {error}"),
            Self::PoisonedSubscription => write!(f, "Inbox subscription lock is poisoned"),
            Self::Unattached => write!(f, "Inbox lane is unattached to the canonical runtime"),
            Self::AlreadyAttached => write!(f, "Inbox lane already has a subscription"),
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InboxLaneTick {
    pub update_applied: bool,
    pub search_progress: SearchProgress,
}

/// One reusable Inbox lane. It is deliberately not a host/client owner.
pub struct InboxLane {
    subscription: SharedInboxSubscription,
    attached: bool,
    preferences: InboxPreferenceStore,
}

/// Source compatibility for callers that still use the old controller name.
/// The alias has no host client/configuration and delegates to the lane seam.
pub type InboxHostController = InboxLane;

impl std::fmt::Debug for InboxLane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InboxLane")
            .field("attached", &self.attached)
            .field("subscription_state", &self.subscription_state())
            .field("preference_path", &self.preferences.path())
            .finish()
    }
}

impl InboxLane {
    pub fn new(preferences: InboxPreferenceStore) -> Self {
        Self {
            subscription: Arc::new(Mutex::new(ClientSubscription::new())),
            attached: false,
            preferences,
        }
    }

    pub fn with_subscription(
        subscription: SharedInboxSubscription,
        preferences: InboxPreferenceStore,
    ) -> Self {
        Self {
            subscription,
            attached: true,
            preferences,
        }
    }

    pub fn attach_subscription(
        &mut self,
        subscription: SharedInboxSubscription,
    ) -> Result<(), InboxControllerError> {
        if self.attached {
            return Err(InboxControllerError::AlreadyAttached);
        }
        self.subscription = subscription;
        self.attached = true;
        Ok(())
    }

    /// Mark the lane as attached to the caller-owned canonical runtime. This
    /// does not create or store that runtime; it only closes the typed seam
    /// after composition has supplied the pending subscription.
    pub fn attach_runtime(&mut self) {
        self.attached = true;
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

    pub fn is_attached(&self) -> bool {
        self.attached
    }

    pub fn preferences(&self) -> &InboxPreferenceStore {
        &self.preferences
    }

    pub fn restore_unread_cursor(&self) -> Result<Option<Vec<u8>>, InboxControllerError> {
        self.preferences.load_unread_cursor().map_err(Into::into)
    }

    pub fn persist_unread_cursor(&self, cursor: Option<&[u8]>) -> Result<(), InboxControllerError> {
        self.preferences
            .save_unread_cursor(cursor)
            .map_err(Into::into)
    }

    /// Synchronize one subscription through the canonical runtime's borrowed
    /// transport. The lane never connects, reconnects, or stores that runtime.
    pub async fn synchronize<T: InboxTransport + ?Sized>(
        &mut self,
        transport: &mut T,
    ) -> Result<(), InboxControllerError> {
        if !self.attached || !transport.is_connected() {
            return Err(InboxControllerError::Unattached);
        }
        if self.subscription_state() != ClientSubscriptionState::Pending {
            self.retire_subscription(transport).await?;
        }
        let result = {
            let mut subscription = self
                .subscription
                .lock()
                .map_err(|_| InboxControllerError::PoisonedSubscription)?;
            transport.synchronize(&mut subscription).await
        };
        result.map_err(Into::into)
    }

    /// Reconnect and rebuild one subscription generation through the borrowed
    /// canonical runtime. Replacement is fenced before the new synchronize.
    pub async fn reconnect_and_synchronize<T: InboxTransport + ?Sized>(
        &mut self,
        transport: &mut T,
    ) -> Result<(), InboxControllerError> {
        if !self.attached {
            return Err(InboxControllerError::Unattached);
        }
        if let Err(error) = transport.reconnect().await {
            self.subscription
                .lock()
                .map_err(|_| InboxControllerError::PoisonedSubscription)?
                .retire_without_transport();
            self.subscription = Arc::new(Mutex::new(ClientSubscription::new()));
            return Err(InboxControllerError::Host(error));
        }
        self.subscription
            .lock()
            .map_err(|_| InboxControllerError::PoisonedSubscription)?
            .retire_without_transport();
        self.subscription = Arc::new(Mutex::new(ClientSubscription::new()));
        self.synchronize(transport).await
    }

    async fn retire_subscription<T: InboxTransport + ?Sized>(
        &mut self,
        transport: &mut T,
    ) -> Result<(), InboxControllerError> {
        let old = Arc::clone(&self.subscription);
        let result = if transport.is_connected() {
            let mut subscription = old
                .lock()
                .map_err(|_| InboxControllerError::PoisonedSubscription)?;
            transport.release(&mut subscription).await
        } else {
            old.lock()
                .map_err(|_| InboxControllerError::PoisonedSubscription)?
                .retire_without_transport();
            Ok(())
        };
        if let Err(error) = result {
            old.lock()
                .map_err(|_| InboxControllerError::PoisonedSubscription)?
                .retire_without_transport();
            return Err(error.into());
        }
        self.subscription = Arc::new(Mutex::new(ClientSubscription::new()));
        Ok(())
    }

    pub async fn receive_one<T: InboxTransport + ?Sized>(
        &mut self,
        transport: &T,
    ) -> Result<SubscriptionUpdate, InboxControllerError> {
        if !self.attached || !transport.is_connected() {
            return Err(InboxControllerError::Unattached);
        }
        let result = {
            let mut subscription = self
                .subscription
                .lock()
                .map_err(|_| InboxControllerError::PoisonedSubscription)?;
            transport.receive_one(&mut subscription).await
        };
        result.map_err(Into::into)
    }

    pub async fn execute_command<T: InboxTransport + ?Sized>(
        &mut self,
        transport: &mut T,
        envelope: CommandEnvelope,
    ) -> Result<CommandReceipt, InboxControllerError> {
        if !self.attached || !transport.is_connected() {
            return Err(InboxControllerError::Unattached);
        }
        transport
            .execute_command(envelope)
            .await
            .map_err(Into::into)
    }

    /// Bounded, nonblocking projection/controller handoff. Transport IO is done by
    /// the canonical runtime before supplying one applied event here; this
    /// tick only applies that event and advances at most one search page.
    pub fn tick(
        &mut self,
        runtime: &mut InboxRuntime,
        update: Option<SubscriptionUpdate>,
    ) -> Result<InboxLaneTick, InboxControllerError> {
        if !self.attached {
            return Err(InboxControllerError::Unattached);
        }
        let update_applied = update
            .map(|update| runtime.apply_subscription_update(update))
            .transpose()?
            .unwrap_or(false);
        let search_progress = runtime.tick_background_search();
        Ok(InboxLaneTick {
            update_applied,
            search_progress,
        })
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
    use crate::ui::task_cockpit::InboxRuntime;

    struct FakeTransport;

    impl InboxTransport for FakeTransport {
        fn is_connected(&self) -> bool {
            true
        }

        fn synchronize<'a>(
            &'a mut self,
            _subscription: &'a mut ClientSubscription,
        ) -> InboxTransportFuture<'a, Result<(), SubscriptionError>> {
            Box::pin(async { Err(SubscriptionError::MissingCapabilities) })
        }

        fn receive_one<'a>(
            &'a self,
            _subscription: &'a mut ClientSubscription,
        ) -> InboxTransportFuture<'a, Result<SubscriptionUpdate, SubscriptionError>> {
            Box::pin(async { Err(SubscriptionError::NotReady) })
        }

        fn release<'a>(
            &'a mut self,
            _subscription: &'a mut ClientSubscription,
        ) -> InboxTransportFuture<'a, Result<(), SubscriptionError>> {
            Box::pin(async { Ok(()) })
        }

        fn reconnect<'a>(&'a mut self) -> InboxTransportFuture<'a, Result<(), IpcError>> {
            Box::pin(async { Ok(()) })
        }

        fn execute_command<'a>(
            &'a mut self,
            _envelope: CommandEnvelope,
        ) -> InboxTransportFuture<'a, Result<CommandReceipt, IpcError>> {
            Box::pin(async { Err(IpcError::Unavailable) })
        }
    }

    #[test]
    fn lane_starts_pending_and_does_not_claim_a_runtime() {
        let directory = tempfile::tempdir().expect("temp directory");
        let preferences = InboxPreferenceStore::at_profile_root(directory.path());
        let lane = InboxLane::new(preferences);
        assert_eq!(lane.subscription_state(), ClientSubscriptionState::Pending);
        assert!(!lane.is_attached());
    }

    #[test]
    fn lane_tick_without_canonical_runtime_is_typed_unattached() {
        let directory = tempfile::tempdir().expect("temp directory");
        let preferences = InboxPreferenceStore::at_profile_root(directory.path());
        let mut lane = InboxLane::new(preferences);
        let mut runtime = InboxRuntime::new();

        let error = lane
            .tick(&mut runtime, None)
            .expect_err("a lane without the canonical runtime must not self-connect");
        assert!(matches!(error, InboxControllerError::Unattached));
    }

    #[tokio::test]
    async fn lane_uses_borrowed_transport_without_constructing_a_host_client() {
        let directory = tempfile::tempdir().expect("temp directory");
        let preferences = InboxPreferenceStore::at_profile_root(directory.path());
        let mut lane = InboxLane::new(preferences);
        lane.attach_runtime();
        let mut transport = FakeTransport;

        let error = lane
            .synchronize(&mut transport)
            .await
            .expect_err("fake transport admission should remain typed");
        assert!(matches!(
            error,
            InboxControllerError::Subscription(SubscriptionError::MissingCapabilities)
        ));
        assert_eq!(lane.subscription_state(), ClientSubscriptionState::Pending);
    }

    #[test]
    fn lane_cursor_persistence_stays_in_its_explicit_preference_store() {
        let directory = tempfile::tempdir().expect("temp directory");
        let preferences = InboxPreferenceStore::at_profile_root(directory.path());
        let lane = InboxLane::new(preferences);
        lane.persist_unread_cursor(Some(&[0x01, 0x02]))
            .expect("persist cursor");
        assert_eq!(
            lane.restore_unread_cursor().expect("restore cursor"),
            Some(vec![0x01, 0x02])
        );
        assert!(!lane
            .preferences()
            .path()
            .file_name()
            .is_some_and(|name| name == "session.json"));
    }
}

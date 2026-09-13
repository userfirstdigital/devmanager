//! Native-next Task Cockpit/client bootstrap.
//!
//! The canonical host runtime owns the authenticated transport. This boundary
//! composes that borrowed transport with one transport-free Inbox lane and one
//! projection runtime; rendering and input only read already-published state.

use crate::client::{
    ClientPreferenceError, InboxControllerError, InboxHostController, InboxLaneTick,
    InboxTransport, SharedInboxSubscription, SubscriptionUpdate,
};
use crate::domain::command::{CommandEnvelope, CommandReceipt};

use super::{InboxFilter, InboxPresentationWidth, InboxRenderModel, InboxRuntime};

/// Native-next Task Cockpit state and its caller-driven client pump.
#[derive(Debug)]
pub struct NativeNextTaskCockpit {
    controller: Option<InboxHostController>,
    runtime: InboxRuntime,
}

impl NativeNextTaskCockpit {
    /// Compose a transport-free lane with the canonical runtime without
    /// connecting. The caller owns the transport and supplies it to each
    /// pump operation.
    pub fn from_controller(
        mut controller: InboxHostController,
    ) -> Result<Self, InboxControllerError> {
        controller.attach_runtime();
        let mut runtime = InboxRuntime::new();
        match controller.restore_unread_cursor()? {
            Some(bytes) => runtime
                .restore_unread_cursor_bytes(&bytes)
                .map_err(|error| {
                    InboxControllerError::Preference(ClientPreferenceError::Decode(error))
                })?,
            None => runtime.restore_unread_cursor(crate::ui::task_cockpit::UnreadCursor::default()),
        }
        runtime.attach_live_subscription(controller.subscription());
        Ok(Self {
            controller: Some(controller),
            runtime,
        })
    }

    /// Host-free injection seam for deterministic shell tests. The injected
    /// subscription remains caller-owned and no transport is touched by
    /// paint/input work.
    pub fn from_subscription(subscription: SharedInboxSubscription) -> Self {
        let mut runtime = InboxRuntime::new();
        runtime.attach_live_subscription(subscription);
        Self {
            controller: None,
            runtime,
        }
    }

    /// Synchronize the lane using the canonical runtime's borrowed transport.
    pub async fn synchronize<T: InboxTransport + ?Sized>(
        &mut self,
        transport: &mut T,
    ) -> Result<(), InboxControllerError> {
        self.runtime.invalidate_for_resync();
        let controller = self
            .controller
            .as_mut()
            .ok_or(InboxControllerError::Unattached)?;
        controller.synchronize(transport).await?;
        self.runtime
            .attach_live_subscription(controller.subscription());
        Ok(())
    }

    /// Receive and apply one unsolicited update on the caller-driven pump
    /// lane. A single bounded search tick follows the update.
    pub async fn receive_one<T: InboxTransport + ?Sized>(
        &mut self,
        transport: &T,
    ) -> Result<SubscriptionUpdate, InboxControllerError> {
        let controller = self
            .controller
            .as_mut()
            .ok_or(InboxControllerError::Unattached)?;
        let update = match controller.receive_one(transport).await {
            Ok(update) => update,
            Err(error) => {
                self.runtime.invalidate_for_resync();
                return Err(error);
            }
        };
        controller.tick(&mut self.runtime, Some(update.clone()))?;
        Ok(update)
    }

    /// Advance one bounded search continuation from the controller/task lane.
    /// Paint and input never perform this work.
    pub fn pump_background_search(&mut self) -> bool {
        self.runtime.tick_background_search().published
    }

    /// Canonical nonblocking tick: apply at most one supplied event and request
    /// at most one continuation page.
    pub fn tick(
        &mut self,
        update: Option<SubscriptionUpdate>,
    ) -> Result<InboxLaneTick, InboxControllerError> {
        let controller = self
            .controller
            .as_mut()
            .ok_or(InboxControllerError::Unattached)?;
        controller.tick(&mut self.runtime, update)
    }

    pub async fn reconnect_and_synchronize<T: InboxTransport + ?Sized>(
        &mut self,
        transport: &mut T,
    ) -> Result<(), InboxControllerError> {
        self.runtime.invalidate_for_resync();
        let controller = self
            .controller
            .as_mut()
            .ok_or(InboxControllerError::Unattached)?;
        controller.reconnect_and_synchronize(transport).await?;
        self.runtime
            .attach_live_subscription(controller.subscription());
        Ok(())
    }

    /// Execute a captured shell command on the authenticated host runtime.
    pub async fn execute_command<T: InboxTransport + ?Sized>(
        &mut self,
        transport: &mut T,
        envelope: CommandEnvelope,
    ) -> Result<CommandReceipt, InboxControllerError> {
        let controller = self
            .controller
            .as_mut()
            .ok_or(InboxControllerError::Unattached)?;
        controller.execute_command(transport, envelope).await
    }

    pub fn render_model(&self, width: InboxPresentationWidth) -> InboxRenderModel {
        self.runtime.render_model(width)
    }

    pub fn set_filter(&mut self, filter: InboxFilter) {
        self.runtime.set_filter(filter);
    }

    pub fn runtime(&self) -> &InboxRuntime {
        &self.runtime
    }

    pub fn runtime_mut(&mut self) -> &mut InboxRuntime {
        &mut self.runtime
    }

    pub fn controller(&self) -> Option<&InboxHostController> {
        self.controller.as_ref()
    }

    pub fn controller_mut(&mut self) -> Option<&mut InboxHostController> {
        self.controller.as_mut()
    }

    pub fn persist_preferences(&self) -> Result<(), InboxControllerError> {
        let Some(controller) = self.controller.as_ref() else {
            return Err(InboxControllerError::Unattached);
        };
        self.runtime
            .persist_unread_cursor_to_controller(controller)
            .map_err(|error| InboxControllerError::Preference(ClientPreferenceError::Encode(error)))
    }
}

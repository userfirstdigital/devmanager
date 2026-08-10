//! Native-next Task Cockpit/client bootstrap.
//!
//! This boundary is intentionally separate from `app::NativeShell`.  It owns
//! one explicit Inbox host controller and hands only bounded projection state
//! to the shell.  Callers invoke the async methods from their controller/task
//! lane; rendering and input dispatch only read the already-built model.

use crate::client::{
    ClientPreferenceError, InboxControllerError, InboxHostController, SharedInboxSubscription,
    SubscriptionUpdate,
};
use crate::domain::command::{CommandEnvelope, CommandReceipt};

use super::{InboxPresentationWidth, InboxRenderModel, InboxRuntime};

/// Native-next Task Cockpit state and its caller-driven client pump.
#[derive(Debug)]
pub struct NativeNextTaskCockpit {
    controller: Option<InboxHostController>,
    runtime: InboxRuntime,
}

impl NativeNextTaskCockpit {
    /// Build the real native-next client boundary without connecting. The
    /// caller supplies an explicit profile/configuration and preference store;
    /// no environment/default profile or legacy session path is consulted.
    pub fn from_controller(controller: InboxHostController) -> Result<Self, InboxControllerError> {
        let mut runtime = InboxRuntime::new();
        match controller.restore_unread_cursor()? {
            Some(bytes) => runtime
                .restore_unread_cursor_bytes(&bytes)
                .map_err(|error| {
                    InboxControllerError::Preference(ClientPreferenceError::Decode(error))
                })?,
            None => runtime.restore_unread_cursor(crate::ui::task_cockpit::UnreadCursor::default()),
        }
        runtime.attach_host_controller(&controller);
        Ok(Self {
            controller: Some(controller),
            runtime,
        })
    }

    /// Host-free injection seam for deterministic shell tests and later entry
    /// cutover wiring. The injected subscription remains caller-owned and its
    /// transport is never touched by paint/input work.
    pub fn from_subscription(subscription: SharedInboxSubscription) -> Self {
        let mut runtime = InboxRuntime::new();
        runtime.attach_live_subscription(subscription);
        Self {
            controller: None,
            runtime,
        }
    }

    /// Connect/synchronize from the caller's controller/task lane, then hand
    /// the shared subscription to the projection. No GPUI callback awaits this
    /// method.
    pub async fn synchronize(&mut self) -> Result<(), InboxControllerError> {
        self.runtime.invalidate_for_resync();
        let controller = self
            .controller
            .as_mut()
            .ok_or(InboxControllerError::NotConnected)?;
        controller.synchronize().await?;
        self.runtime.attach_host_controller(controller);
        self.runtime.refresh_from_subscription();
        Ok(())
    }

    /// Receive and apply one unsolicited update on the caller-driven pump
    /// lane. The returned update is retained for action/revision diagnostics;
    /// the runtime consumes it incrementally after the subscription applies it
    /// to the client model.
    pub async fn receive_one(&mut self) -> Result<SubscriptionUpdate, InboxControllerError> {
        let controller = self
            .controller
            .as_mut()
            .ok_or(InboxControllerError::NotConnected)?;
        let update = match controller.receive_one().await {
            Ok(update) => update,
            Err(error) => {
                self.runtime.invalidate_for_resync();
                return Err(error);
            }
        };
        self.runtime
            .apply_subscription_update(update.clone())
            .map_err(InboxControllerError::Subscription)?;
        Ok(update)
    }

    pub async fn reconnect_and_synchronize(&mut self) -> Result<(), InboxControllerError> {
        self.runtime.invalidate_for_resync();
        let controller = self
            .controller
            .as_mut()
            .ok_or(InboxControllerError::NotConnected)?;
        controller.reconnect_and_synchronize().await?;
        self.runtime.attach_host_controller(controller);
        self.runtime.refresh_from_subscription();
        Ok(())
    }

    /// Execute a captured shell command on the authenticated host client.
    ///
    /// Command envelopes are created by the input/dispatcher lane with the
    /// task, revision, focus, and generation fences already captured. The
    /// native-next shell calls this method from its action/task lane; paint
    /// never waits on the transport.
    pub async fn execute_command(
        &mut self,
        envelope: CommandEnvelope,
    ) -> Result<CommandReceipt, InboxControllerError> {
        let controller = self
            .controller
            .as_mut()
            .ok_or(InboxControllerError::NotConnected)?;
        controller.execute_command(envelope).await
    }

    pub fn render_model(&self, width: InboxPresentationWidth) -> InboxRenderModel {
        self.runtime.render_model(width)
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
            return Err(InboxControllerError::NotConnected);
        };
        self.runtime
            .persist_unread_cursor_to_controller(controller)
            .map_err(|error| InboxControllerError::Preference(ClientPreferenceError::Encode(error)))
    }
}

use std::time::Duration;

const ACTIVE_INPUT_GUARD: Duration = Duration::from_millis(700);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterLease {
    pub owner_connection_id: u64,
    pub owner_client_id: String,
    pub owner_client_instance_id: String,
    pub generation: u64,
    pub expires_at_epoch_ms: u64,
    visible: bool,
    last_input_at_epoch_ms: Option<u64>,
    active_mutation_id: Option<String>,
    release_after_mutation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseError {
    ActiveOwner,
    NotOwner,
    Expired,
    StaleGeneration { current_generation: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "the caller must distinguish a new mutation from an existing busy marker"]
pub enum MutationBegin {
    Started(WriterLease),
    AlreadyInFlight(WriterLease),
}

#[derive(Debug)]
pub struct WriterLeaseManager {
    lease_duration_ms: u64,
    active_input_guard_ms: u64,
    generation: u64,
    current: Option<WriterLease>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControllerTarget {
    Local,
    Native(String),
}

impl ControllerTarget {
    pub fn client_id(&self) -> Option<&str> {
        match self {
            Self::Local => None,
            Self::Native(client_id) => Some(client_id),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LegacyControlClaim {
    connection_id: u64,
    client_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControllerRequest {
    Applied {
        released_lease: Option<WriterLease>,
        released_legacy_client_id: Option<String>,
    },
    Deferred,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectionRelease {
    pub released_lease: Option<WriterLease>,
    pub lease_release_deferred: bool,
    pub legacy_released: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MutationFinish {
    pub released_lease: Option<WriterLease>,
    pub controller_target: Option<ControllerTarget>,
}

#[derive(Debug)]
pub struct WebControlState {
    writer_leases: WriterLeaseManager,
    legacy_claimant: Option<LegacyControlClaim>,
    deferred_controller_target: Option<ControllerTarget>,
}

impl WebControlState {
    pub fn new(lease_duration: Duration) -> Self {
        Self {
            writer_leases: WriterLeaseManager::new(lease_duration),
            legacy_claimant: None,
            deferred_controller_target: None,
        }
    }

    pub fn writer_leases(&self) -> &WriterLeaseManager {
        &self.writer_leases
    }

    pub fn writer_leases_mut(&mut self) -> &mut WriterLeaseManager {
        &mut self.writer_leases
    }

    pub fn claim_legacy(
        &mut self,
        connection_id: u64,
        client_id: &str,
        force: bool,
        controller_client_id: Option<&str>,
    ) -> bool {
        if self.writer_leases.peek().is_some() {
            return false;
        }
        let exact_claimant = self.legacy_authorizes(connection_id, client_id);
        let available = controller_client_id.is_none()
            || (controller_client_id == Some(client_id) && exact_claimant);
        if !force && !available {
            return false;
        }
        self.legacy_claimant = Some(LegacyControlClaim {
            connection_id,
            client_id: client_id.to_string(),
        });
        true
    }

    pub fn legacy_authorizes(&self, connection_id: u64, client_id: &str) -> bool {
        self.writer_leases.peek().is_none()
            && self.legacy_claimant.as_ref().is_some_and(|claimant| {
                claimant.connection_id == connection_id && claimant.client_id == client_id
            })
    }

    pub fn legacy_claimant_client_id(&self) -> Option<&str> {
        self.legacy_claimant
            .as_ref()
            .map(|claimant| claimant.client_id.as_str())
    }

    pub fn clear_legacy_claim(&mut self, connection_id: u64, client_id: &str) -> bool {
        let exact = self.legacy_authorizes(connection_id, client_id);
        if exact {
            self.legacy_claimant = None;
        }
        exact
    }

    pub fn release_connection(&mut self, connection_id: u64, client_id: &str) -> ConnectionRelease {
        let previous_lease = self.writer_leases.peek();
        let was_busy_owner = previous_lease.as_ref().is_some_and(|lease| {
            lease.owner_connection_id == connection_id
                && lease.owner_client_id == client_id
                && lease.active_mutation_id.is_some()
        });
        let released_now = self.writer_leases.disconnect(connection_id, client_id);
        let released_lease = released_now.then_some(previous_lease).flatten();
        let legacy_released = self.legacy_claimant.as_ref().is_some_and(|claimant| {
            claimant.connection_id == connection_id && claimant.client_id == client_id
        });
        if legacy_released {
            self.legacy_claimant = None;
        }
        ConnectionRelease {
            released_lease,
            lease_release_deferred: was_busy_owner && !released_now,
            legacy_released,
        }
    }

    pub fn request_controller(&mut self, target: ControllerTarget) -> ControllerRequest {
        let busy = self.writer_leases.is_busy();
        let released_lease = self.writer_leases.invalidate();
        let released_legacy_client_id = self
            .legacy_claimant
            .take()
            .map(|claimant| claimant.client_id);
        if busy {
            self.deferred_controller_target = Some(target);
            ControllerRequest::Deferred
        } else {
            self.deferred_controller_target = None;
            ControllerRequest::Applied {
                released_lease,
                released_legacy_client_id,
            }
        }
    }

    pub fn finish_mutation(
        &mut self,
        connection_id: u64,
        client_id: &str,
        expected_generation: u64,
        mutation_id: &str,
        now_epoch_ms: u64,
    ) -> MutationFinish {
        let released_lease = self.writer_leases.finish_mutation(
            connection_id,
            client_id,
            expected_generation,
            mutation_id,
            now_epoch_ms,
        );
        let controller_target = released_lease
            .as_ref()
            .and_then(|_| self.deferred_controller_target.take());
        MutationFinish {
            released_lease,
            controller_target,
        }
    }

    pub fn reset_web(&mut self, clear_web_controller: bool) -> ControllerRequest {
        if clear_web_controller {
            if self.writer_leases.is_busy() {
                self.writer_leases.invalidate();
                self.legacy_claimant = None;
                if self.deferred_controller_target.is_none() {
                    self.deferred_controller_target = Some(ControllerTarget::Local);
                }
                ControllerRequest::Deferred
            } else {
                self.request_controller(ControllerTarget::Local)
            }
        } else {
            let busy = self.writer_leases.is_busy();
            let released_lease = self.writer_leases.invalidate();
            let released_legacy_client_id = self
                .legacy_claimant
                .take()
                .map(|claimant| claimant.client_id);
            if busy {
                ControllerRequest::Deferred
            } else {
                ControllerRequest::Applied {
                    released_lease,
                    released_legacy_client_id,
                }
            }
        }
    }
}

impl WriterLeaseManager {
    pub fn new(lease_duration: Duration) -> Self {
        Self {
            lease_duration_ms: duration_ms(lease_duration),
            active_input_guard_ms: duration_ms(ACTIVE_INPUT_GUARD),
            generation: 0,
            current: None,
        }
    }

    pub fn acquire(
        &mut self,
        connection_id: u64,
        client_id: &str,
        client_instance_id: &str,
        now_epoch_ms: u64,
    ) -> Result<WriterLease, LeaseError> {
        self.expire_if_needed(now_epoch_ms);

        if let Some(current) = self.current.as_mut() {
            let exact_owner = current.owner_connection_id == connection_id
                && current.owner_client_id == client_id
                && current.owner_client_instance_id == client_instance_id;
            if exact_owner {
                current.visible = true;
                current.expires_at_epoch_ms = now_epoch_ms.saturating_add(self.lease_duration_ms);
                return Ok(current.clone());
            }

            if current.active_mutation_id.is_some() {
                return Err(LeaseError::ActiveOwner);
            }

            let input_is_guarded = current.visible
                && current.last_input_at_epoch_ms.is_some_and(|last_input| {
                    now_epoch_ms < last_input.saturating_add(self.active_input_guard_ms)
                });
            if input_is_guarded {
                return Err(LeaseError::ActiveOwner);
            }
        }

        Ok(self.grant(connection_id, client_id, client_instance_id, now_epoch_ms))
    }

    pub fn authorize(
        &mut self,
        connection_id: u64,
        client_id: &str,
        expected_generation: u64,
        now_epoch_ms: u64,
    ) -> Result<WriterLease, LeaseError> {
        if expected_generation != self.generation {
            return Err(LeaseError::StaleGeneration {
                current_generation: self.generation,
            });
        }
        if self.current.as_ref().is_some_and(|lease| {
            lease.active_mutation_id.is_none() && now_epoch_ms >= lease.expires_at_epoch_ms
        }) {
            self.invalidate();
            return Err(LeaseError::Expired);
        }
        let Some(current) = self.current.as_mut() else {
            return Err(LeaseError::StaleGeneration {
                current_generation: self.generation,
            });
        };
        if current.owner_connection_id != connection_id || current.owner_client_id != client_id {
            return Err(LeaseError::NotOwner);
        }

        current.visible = true;
        current.last_input_at_epoch_ms = Some(now_epoch_ms);
        current.expires_at_epoch_ms = now_epoch_ms.saturating_add(self.lease_duration_ms);
        Ok(current.clone())
    }

    pub fn begin_mutation(
        &mut self,
        connection_id: u64,
        client_id: &str,
        expected_generation: u64,
        mutation_id: &str,
        now_epoch_ms: u64,
    ) -> Result<MutationBegin, LeaseError> {
        if expected_generation != self.generation {
            return Err(LeaseError::StaleGeneration {
                current_generation: self.generation,
            });
        }
        self.expire_if_needed(now_epoch_ms);
        let Some(current) = self.current.as_mut() else {
            return Err(LeaseError::Expired);
        };
        if current.owner_connection_id != connection_id || current.owner_client_id != client_id {
            return Err(LeaseError::NotOwner);
        }
        if let Some(active_mutation_id) = current.active_mutation_id.as_deref() {
            if active_mutation_id == mutation_id {
                return Ok(MutationBegin::AlreadyInFlight(current.clone()));
            }
            return Err(LeaseError::ActiveOwner);
        }

        current.visible = true;
        current.last_input_at_epoch_ms = Some(now_epoch_ms);
        current.expires_at_epoch_ms = now_epoch_ms.saturating_add(self.lease_duration_ms);
        current.active_mutation_id = Some(mutation_id.to_string());
        Ok(MutationBegin::Started(current.clone()))
    }

    pub fn finish_mutation(
        &mut self,
        connection_id: u64,
        client_id: &str,
        expected_generation: u64,
        mutation_id: &str,
        now_epoch_ms: u64,
    ) -> Option<WriterLease> {
        let current = self.current.as_mut()?;
        let exact_mutation = current.owner_connection_id == connection_id
            && current.owner_client_id == client_id
            && current.generation == expected_generation
            && current.active_mutation_id.as_deref() == Some(mutation_id);
        if !exact_mutation {
            return None;
        }
        current.active_mutation_id = None;
        current.last_input_at_epoch_ms = Some(now_epoch_ms);
        current.expires_at_epoch_ms = now_epoch_ms.saturating_add(self.lease_duration_ms);
        if current.release_after_mutation {
            return self.invalidate();
        }
        None
    }

    pub fn renew(
        &mut self,
        connection_id: u64,
        client_id: &str,
        client_instance_id: &str,
        expected_generation: u64,
        visible: bool,
        now_epoch_ms: u64,
    ) -> Result<WriterLease, LeaseError> {
        if expected_generation != self.generation {
            return Err(LeaseError::StaleGeneration {
                current_generation: self.generation,
            });
        }
        if self.current.as_ref().is_some_and(|lease| {
            lease.active_mutation_id.is_none() && now_epoch_ms >= lease.expires_at_epoch_ms
        }) {
            self.invalidate();
            return Err(LeaseError::Expired);
        }
        let Some(current) = self.current.as_mut() else {
            return Err(LeaseError::StaleGeneration {
                current_generation: self.generation,
            });
        };
        if current.owner_connection_id != connection_id
            || current.owner_client_id != client_id
            || current.owner_client_instance_id != client_instance_id
        {
            return Err(LeaseError::NotOwner);
        }
        current.visible = visible;
        current.expires_at_epoch_ms = now_epoch_ms.saturating_add(self.lease_duration_ms);
        Ok(current.clone())
    }

    pub fn set_visibility(
        &mut self,
        connection_id: u64,
        client_id: &str,
        client_instance_id: &str,
        visible: bool,
        now_epoch_ms: u64,
    ) -> Result<WriterLease, LeaseError> {
        self.expire_if_needed(now_epoch_ms);
        let Some(current) = self.current.as_mut() else {
            return Err(LeaseError::StaleGeneration {
                current_generation: self.generation,
            });
        };
        if current.owner_connection_id != connection_id
            || current.owner_client_id != client_id
            || current.owner_client_instance_id != client_instance_id
        {
            return Err(LeaseError::NotOwner);
        }
        current.visible = visible;
        Ok(current.clone())
    }

    pub fn disconnect(&mut self, connection_id: u64, client_id: &str) -> bool {
        let is_owner = self.current.as_ref().is_some_and(|current| {
            current.owner_connection_id == connection_id && current.owner_client_id == client_id
        });
        if is_owner {
            if let Some(current) = self.current.as_mut() {
                if current.active_mutation_id.is_some() {
                    current.visible = false;
                    current.release_after_mutation = true;
                    return false;
                }
            }
            self.invalidate();
        }
        is_owner
    }

    pub fn invalidate(&mut self) -> Option<WriterLease> {
        if let Some(current) = self.current.as_mut() {
            if current.active_mutation_id.is_some() {
                current.visible = false;
                current.release_after_mutation = true;
                return None;
            }
        }
        let previous = self.current.take();
        if previous.is_some() {
            self.generation = self.generation.saturating_add(1);
        }
        previous
    }

    pub fn current(&mut self, now_epoch_ms: u64) -> Option<WriterLease> {
        self.expire_if_needed(now_epoch_ms);
        self.current.clone()
    }

    pub fn peek(&self) -> Option<WriterLease> {
        self.current.clone()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn is_busy(&self) -> bool {
        self.current
            .as_ref()
            .is_some_and(|lease| lease.active_mutation_id.is_some())
    }

    fn grant(
        &mut self,
        connection_id: u64,
        client_id: &str,
        client_instance_id: &str,
        now_epoch_ms: u64,
    ) -> WriterLease {
        self.generation = self.generation.saturating_add(1);
        let lease = WriterLease {
            owner_connection_id: connection_id,
            owner_client_id: client_id.to_string(),
            owner_client_instance_id: client_instance_id.to_string(),
            generation: self.generation,
            expires_at_epoch_ms: now_epoch_ms.saturating_add(self.lease_duration_ms),
            visible: true,
            // A Resume response must not name an owner that a second visible
            // Resume can displace before the first response is even enqueued.
            // Treat the grant itself as foreground interaction for the same
            // short handoff guard used by terminal input.
            last_input_at_epoch_ms: Some(now_epoch_ms),
            active_mutation_id: None,
            release_after_mutation: false,
        };
        self.current = Some(lease.clone());
        lease
    }

    fn expire_if_needed(&mut self, now_epoch_ms: u64) {
        if self.current.as_ref().is_some_and(|lease| {
            lease.active_mutation_id.is_none() && now_epoch_ms >= lease.expires_at_epoch_ms
        }) {
            self.invalidate();
        }
    }
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_tab_cannot_write_with_the_same_install_cookie() {
        let mut leases = WriterLeaseManager::new(Duration::from_secs(8));
        let first = leases
            .acquire(1, "install-a", "tab-a", 1_000)
            .expect("first tab acquires");

        assert!(leases
            .authorize(1, "install-a", first.generation, 1_001)
            .is_ok());
        assert!(matches!(
            leases.authorize(2, "install-a", first.generation, 1_001),
            Err(LeaseError::NotOwner)
        ));
    }

    #[test]
    fn foreground_interaction_reclaims_a_hidden_lease_without_a_button() {
        let mut leases = WriterLeaseManager::new(Duration::from_secs(8));
        leases
            .acquire(1, "phone", "pwa", 1_000)
            .expect("phone acquires");
        leases
            .set_visibility(1, "phone", "pwa", false, 1_002)
            .expect("phone hides");

        let desktop = leases
            .acquire(2, "desktop", "browser", 1_003)
            .expect("foreground desktop reclaims");

        assert_eq!(desktop.owner_connection_id, 2);
    }

    #[test]
    fn recent_active_input_has_a_sub_second_handoff_guard() {
        let mut leases = WriterLeaseManager::new(Duration::from_secs(8));
        let phone = leases
            .acquire(1, "phone", "pwa", 1_000)
            .expect("phone acquires");
        leases
            .authorize(1, "phone", phone.generation, 1_100)
            .expect("phone input is authorized");

        assert!(matches!(
            leases.acquire(2, "desktop", "browser", 1_799),
            Err(LeaseError::ActiveOwner)
        ));
        assert!(leases.acquire(2, "desktop", "browser", 1_800).is_ok());
    }

    #[test]
    fn expiry_and_exact_owner_disconnect_invalidate_the_old_generation() {
        let mut leases = WriterLeaseManager::new(Duration::from_secs(8));
        let first = leases
            .acquire(1, "install-a", "tab-a", 1_000)
            .expect("first tab acquires");
        assert!(!leases.disconnect(2, "install-a"));
        assert!(leases.disconnect(1, "install-a"));
        assert!(matches!(
            leases.authorize(1, "install-a", first.generation, 1_100),
            Err(LeaseError::StaleGeneration { .. })
        ));

        let second = leases
            .acquire(2, "install-a", "tab-b", 1_101)
            .expect("second tab acquires");
        assert!(second.generation > first.generation);
        assert!(matches!(
            leases.authorize(2, "install-a", second.generation, 9_102),
            Err(LeaseError::Expired)
        ));
    }

    #[test]
    fn in_flight_mutation_blocks_handoff_and_defers_disconnect_release() {
        let mut leases = WriterLeaseManager::new(Duration::from_secs(8));
        let first = leases
            .acquire(1, "phone", "tab-a", 1_000)
            .expect("phone acquires");
        let started = leases
            .begin_mutation(1, "phone", first.generation, "mutation-a", 1_100)
            .expect("mutation starts");
        assert!(matches!(started, MutationBegin::Started(_)));

        assert!(matches!(
            leases.acquire(2, "desktop", "tab-b", 5_000),
            Err(LeaseError::ActiveOwner)
        ));
        assert!(
            !leases.disconnect(1, "phone"),
            "busy disconnect must defer invalidation"
        );
        assert!(matches!(
            leases.acquire(2, "desktop", "tab-b", 5_001),
            Err(LeaseError::ActiveOwner)
        ));

        let released = leases
            .finish_mutation(1, "phone", first.generation, "mutation-a", 5_002)
            .expect("deferred disconnect releases after callback");
        assert_eq!(released.owner_connection_id, 1);
        assert!(leases.acquire(2, "desktop", "tab-b", 5_003).is_ok());
    }

    #[test]
    fn only_the_same_in_flight_mutation_can_observe_its_busy_marker() {
        let mut leases = WriterLeaseManager::new(Duration::from_secs(8));
        let first = leases
            .acquire(1, "phone", "tab-a", 1_000)
            .expect("phone acquires");
        let started = leases
            .begin_mutation(1, "phone", first.generation, "mutation-a", 1_100)
            .expect("mutation starts");
        assert!(matches!(started, MutationBegin::Started(_)));

        let retry = leases
            .begin_mutation(1, "phone", first.generation, "mutation-a", 1_101)
            .expect("identical retry observes in-flight marker");
        assert!(matches!(retry, MutationBegin::AlreadyInFlight(_)));
        assert!(matches!(
            leases.begin_mutation(1, "phone", first.generation, "mutation-b", 1_102),
            Err(LeaseError::ActiveOwner)
        ));
    }

    #[test]
    fn native_or_restart_invalidation_cannot_erase_an_in_flight_mutation() {
        let mut leases = WriterLeaseManager::new(Duration::from_secs(8));
        let first = leases
            .acquire(1, "phone", "tab-a", 1_000)
            .expect("phone acquires");
        let started = leases
            .begin_mutation(1, "phone", first.generation, "mutation-a", 1_100)
            .expect("mutation starts");
        assert!(matches!(started, MutationBegin::Started(_)));

        assert_eq!(
            leases.invalidate(),
            None,
            "takeover/restart invalidation must defer while the PTY callback is running"
        );
        assert_eq!(
            leases.peek().map(|lease| lease.owner_connection_id),
            Some(1),
            "the busy owner remains authoritative until callback completion"
        );
        assert!(matches!(
            leases.acquire(2, "native", "desktop", 9_000),
            Err(LeaseError::ActiveOwner)
        ));

        let released = leases
            .finish_mutation(1, "phone", first.generation, "mutation-a", 9_001)
            .expect("deferred invalidation releases at callback completion");
        assert_eq!(released.owner_connection_id, 1);
        assert!(leases.peek().is_none());
        assert!(leases.generation() > first.generation);
    }

    #[test]
    fn legacy_control_is_bound_to_the_exact_connection_not_the_cookie() {
        let mut control = WebControlState::new(Duration::from_secs(8));

        assert!(control.claim_legacy(1, "same-cookie", false, None));
        assert!(control.legacy_authorizes(1, "same-cookie"));
        assert!(!control.legacy_authorizes(2, "same-cookie"));
        assert!(
            !control.release_connection(2, "same-cookie").legacy_released,
            "a same-cookie viewer cannot clear the exact claimant"
        );
        assert!(control.legacy_authorizes(1, "same-cookie"));
        assert!(control.release_connection(1, "same-cookie").legacy_released);
        assert!(!control.legacy_authorizes(1, "same-cookie"));
    }

    #[test]
    fn controller_takeover_is_deferred_until_the_busy_mutation_finishes() {
        let mut control = WebControlState::new(Duration::from_secs(8));
        let lease = control
            .writer_leases_mut()
            .acquire(1, "phone", "tab-a", 1_000)
            .expect("phone acquires");
        let started = control
            .writer_leases_mut()
            .begin_mutation(1, "phone", lease.generation, "mutation-a", 1_100)
            .expect("mutation starts");
        assert!(matches!(started, MutationBegin::Started(_)));

        assert!(matches!(
            control.request_controller(ControllerTarget::Native("desktop".to_string())),
            ControllerRequest::Deferred
        ));
        assert_eq!(
            control
                .writer_leases_mut()
                .peek()
                .map(|lease| lease.owner_connection_id),
            Some(1)
        );

        let finished = control.finish_mutation(1, "phone", lease.generation, "mutation-a", 9_000);
        assert_eq!(
            finished
                .released_lease
                .map(|lease| lease.owner_connection_id),
            Some(1)
        );
        assert_eq!(
            finished.controller_target,
            Some(ControllerTarget::Native("desktop".to_string()))
        );
    }

    #[test]
    fn restart_reset_cannot_erase_busy_marker_or_apply_cleanup_early() {
        let mut control = WebControlState::new(Duration::from_secs(8));
        let lease = control
            .writer_leases_mut()
            .acquire(1, "phone", "tab-a", 1_000)
            .expect("phone acquires");
        let started = control
            .writer_leases_mut()
            .begin_mutation(1, "phone", lease.generation, "mutation-a", 1_100)
            .expect("mutation starts");
        assert!(matches!(started, MutationBegin::Started(_)));

        assert_eq!(control.reset_web(true), ControllerRequest::Deferred);
        assert!(control.writer_leases().is_busy());
        assert_eq!(
            control
                .writer_leases()
                .peek()
                .map(|lease| lease.owner_connection_id),
            Some(1)
        );

        let finished = control.finish_mutation(1, "phone", lease.generation, "mutation-a", 9_000);
        assert!(finished.released_lease.is_some());
        assert_eq!(finished.controller_target, Some(ControllerTarget::Local));
        assert!(control.writer_leases().peek().is_none());
    }

    #[test]
    fn a_new_visible_grant_has_the_same_short_handoff_guard_as_input() {
        let mut leases = WriterLeaseManager::new(Duration::from_secs(8));
        let first = leases
            .acquire(1, "phone", "tab-a", 1_000)
            .expect("first resume acquires");

        assert!(matches!(
            leases.acquire(2, "laptop", "tab-b", 1_001),
            Err(LeaseError::ActiveOwner)
        ));
        assert_eq!(
            leases.peek().map(|lease| lease.generation),
            Some(first.generation),
            "the response owner cannot already have been displaced"
        );
        assert!(leases.acquire(2, "laptop", "tab-b", 1_700).is_ok());
    }

    #[test]
    fn nominal_expiry_does_not_expire_the_busy_owner_during_authorization() {
        let mut leases = WriterLeaseManager::new(Duration::from_secs(8));
        let lease = leases
            .acquire(1, "phone", "tab-a", 1_000)
            .expect("phone acquires");
        let started = leases
            .begin_mutation(1, "phone", lease.generation, "mutation-a", 1_100)
            .expect("mutation starts");
        assert!(matches!(started, MutationBegin::Started(_)));

        assert!(leases
            .authorize(1, "phone", lease.generation, 20_000)
            .is_ok());
        assert!(leases
            .renew(1, "phone", "tab-a", lease.generation, true, 20_001)
            .is_ok());
        assert!(leases.is_busy());
    }
}

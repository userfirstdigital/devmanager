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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseError {
    ActiveOwner,
    NotOwner,
    Expired,
    StaleGeneration { current_generation: u64 },
}

#[derive(Debug)]
pub struct WriterLeaseManager {
    lease_duration_ms: u64,
    active_input_guard_ms: u64,
    generation: u64,
    current: Option<WriterLease>,
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
        if self
            .current
            .as_ref()
            .is_some_and(|lease| now_epoch_ms >= lease.expires_at_epoch_ms)
        {
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
        if self
            .current
            .as_ref()
            .is_some_and(|lease| now_epoch_ms >= lease.expires_at_epoch_ms)
        {
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
            self.invalidate();
        }
        is_owner
    }

    pub fn invalidate(&mut self) -> Option<WriterLease> {
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
            last_input_at_epoch_ms: None,
        };
        self.current = Some(lease.clone());
        lease
    }

    fn expire_if_needed(&mut self, now_epoch_ms: u64) {
        if self
            .current
            .as_ref()
            .is_some_and(|lease| now_epoch_ms >= lease.expires_at_epoch_ms)
        {
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
}

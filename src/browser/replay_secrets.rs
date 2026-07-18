use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use zeroize::{Zeroize, Zeroizing};

pub const MAX_BROWSER_REPLAY_SECRET_INPUTS: usize = 32;
pub const MAX_BROWSER_REPLAY_SECRET_INPUT_NAME_BYTES: usize = 128;
pub const MAX_BROWSER_REPLAY_SECRET_VALUE_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserReplaySecretError {
    InvalidSubmission,
    AlreadySubmitted,
    StaleAuthority,
    ClosedStore,
    SecretUnavailable,
}

impl fmt::Display for BrowserReplaySecretError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidSubmission => "browser replay secret submission is invalid",
            Self::AlreadySubmitted => "browser replay secret submission was already accepted",
            Self::StaleAuthority => "browser replay secret authority is stale",
            Self::ClosedStore => "browser replay secret store is closed",
            Self::SecretUnavailable => "browser replay secret is unavailable",
        })
    }
}

impl std::error::Error for BrowserReplaySecretError {}

pub struct BrowserReplaySecretSubmission {
    values: Vec<(String, Zeroizing<String>)>,
}

impl BrowserReplaySecretSubmission {
    #[allow(dead_code)] // Consumed by the checkpoint-9 masked prompt boundary in a later task.
    pub(crate) fn from_user_prompt(values: Vec<(String, String)>) -> Self {
        Self {
            values: values
                .into_iter()
                .map(|(name, value)| (name, Zeroizing::new(value)))
                .collect(),
        }
    }
}

enum BrowserReplaySecretStoreStatus {
    Open,
    Installed,
    Closed,
}

struct BrowserReplaySecretStoreState {
    status: BrowserReplaySecretStoreStatus,
    values: HashMap<String, Zeroizing<String>>,
    #[cfg(test)]
    memory_clear_observer: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

pub struct BrowserReplaySecretStore {
    authority: Arc<Mutex<BrowserReplaySecretStoreState>>,
}

impl BrowserReplaySecretStore {
    pub(crate) fn new() -> Self {
        Self {
            authority: Arc::new(Mutex::new(BrowserReplaySecretStoreState {
                status: BrowserReplaySecretStoreStatus::Open,
                values: HashMap::new(),
                #[cfg(test)]
                memory_clear_observer: None,
            })),
        }
    }

    pub(crate) fn share_authority(&self) -> Self {
        Self {
            authority: Arc::clone(&self.authority),
        }
    }

    pub(crate) fn install(
        &self,
        required_names: &[String],
        submission: BrowserReplaySecretSubmission,
    ) -> Result<(), BrowserReplaySecretError> {
        let mut state = self.lock();
        match state.status {
            BrowserReplaySecretStoreStatus::Closed => {
                return Err(BrowserReplaySecretError::ClosedStore);
            }
            BrowserReplaySecretStoreStatus::Installed => {
                return Err(BrowserReplaySecretError::AlreadySubmitted);
            }
            BrowserReplaySecretStoreStatus::Open => {}
        }

        if !valid_submission(required_names, &submission.values) {
            return Err(BrowserReplaySecretError::InvalidSubmission);
        }

        state.values = submission.values.into_iter().collect();
        state.status = BrowserReplaySecretStoreStatus::Installed;
        Ok(())
    }

    pub(crate) fn submission_error(&self) -> BrowserReplaySecretError {
        match self.lock().status {
            BrowserReplaySecretStoreStatus::Open => BrowserReplaySecretError::InvalidSubmission,
            BrowserReplaySecretStoreStatus::Installed => BrowserReplaySecretError::AlreadySubmitted,
            BrowserReplaySecretStoreStatus::Closed => BrowserReplaySecretError::ClosedStore,
        }
    }

    pub(crate) fn lease(
        &self,
        input_name: &str,
    ) -> Result<BrowserReplaySecretLease, BrowserReplaySecretError> {
        let state = self.lock();
        match state.status {
            BrowserReplaySecretStoreStatus::Closed => {
                return Err(BrowserReplaySecretError::ClosedStore);
            }
            BrowserReplaySecretStoreStatus::Open => {
                return Err(BrowserReplaySecretError::SecretUnavailable);
            }
            BrowserReplaySecretStoreStatus::Installed => {}
        }
        if !state.values.contains_key(input_name) {
            return Err(BrowserReplaySecretError::SecretUnavailable);
        }
        Ok(BrowserReplaySecretLease {
            authority: Arc::clone(&self.authority),
            input_name: input_name.to_string(),
        })
    }

    pub(crate) fn close(&self) {
        let mut state = self.lock();
        if matches!(state.status, BrowserReplaySecretStoreStatus::Closed) {
            return;
        }

        #[cfg(test)]
        let observer = state.memory_clear_observer.clone();
        for value in state.values.values_mut() {
            #[cfg(test)]
            if let Some(observer) = observer.as_ref() {
                zeroize_and_observe(value, observer);
                continue;
            }
            value.zeroize();
        }
        state.values.clear();
        state.status = BrowserReplaySecretStoreStatus::Closed;
    }

    #[cfg(test)]
    pub(crate) fn observe_memory_clear_for_test(&self) -> Arc<std::sync::atomic::AtomicUsize> {
        let observer = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        self.lock().memory_clear_observer = Some(Arc::clone(&observer));
        observer
    }

    fn lock(&self) -> MutexGuard<'_, BrowserReplaySecretStoreState> {
        self.authority
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[allow(dead_code)] // The secure host lane consumes this opaque authority in a later task.
pub struct BrowserReplaySecretLease {
    authority: Arc<Mutex<BrowserReplaySecretStoreState>>,
    input_name: String,
}

impl BrowserReplaySecretLease {
    #[allow(dead_code)] // Plaintext exposure remains crate-private for the secure host lane.
    pub(crate) fn expose<T>(
        &self,
        expose: impl FnOnce(&str) -> T,
    ) -> Result<T, BrowserReplaySecretError> {
        let state = self
            .authority
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if matches!(state.status, BrowserReplaySecretStoreStatus::Closed) {
            return Err(BrowserReplaySecretError::ClosedStore);
        }
        let value = state
            .values
            .get(&self.input_name)
            .ok_or(BrowserReplaySecretError::SecretUnavailable)?;
        Ok(expose(value.as_str()))
    }
}

fn valid_submission(
    required_names: &[String],
    submitted_values: &[(String, Zeroizing<String>)],
) -> bool {
    if required_names.is_empty()
        || required_names.len() > MAX_BROWSER_REPLAY_SECRET_INPUTS
        || submitted_values.len() != required_names.len()
        || submitted_values.len() > MAX_BROWSER_REPLAY_SECRET_INPUTS
    {
        return false;
    }

    let mut required = HashSet::with_capacity(required_names.len());
    if required_names
        .iter()
        .any(|name| !valid_input_name(name) || !required.insert(name.as_str()))
    {
        return false;
    }

    let mut submitted = HashSet::with_capacity(submitted_values.len());
    submitted_values.iter().all(|(name, value)| {
        valid_input_name(name)
            && submitted.insert(name.as_str())
            && required.contains(name.as_str())
            && !value.is_empty()
            && value.len() <= MAX_BROWSER_REPLAY_SECRET_VALUE_BYTES
    })
}

fn valid_input_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_BROWSER_REPLAY_SECRET_INPUT_NAME_BYTES
        && name.trim() == name
        && !name.chars().any(char::is_control)
        && !super::automation::browser_text_contains_secret(name)
}

#[cfg(test)]
fn zeroize_and_observe(value: &mut Zeroizing<String>, observer: &std::sync::atomic::AtomicUsize) {
    use std::sync::atomic::Ordering;

    let pointer = value.as_ptr();
    let length = value.len();
    value.zeroize();

    // SAFETY: zeroizing a String retains its allocation until `value` is dropped. The
    // observer reads only the prior initialized byte range immediately after zeroization,
    // while the allocation is still exclusively held behind the store mutex.
    let bytes = unsafe { std::slice::from_raw_parts(pointer, length) };
    if bytes.iter().all(|byte| *byte == 0) {
        observer.fetch_add(1, Ordering::AcqRel);
    }
}

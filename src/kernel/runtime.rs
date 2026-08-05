use std::collections::BTreeMap;
use std::fmt;

use crate::domain::id::ResourceId;
use crate::domain::operation::ResourceFence;

/// What the kernel actually knows about an in-memory resource generation.
///
/// `Recovering` means only that a durable recipe is awaiting reconciliation.
/// It is deliberately not a claim that an operating-system process is alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimePresence {
    Recovering,
    Current,
    /// The durable generation was stopped/released and remains as a high-water
    /// mark so its late callbacks and generation reuse stay fenced out.
    Inactive,
}

/// Result of fencing an asynchronous runtime completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionDisposition {
    Current,
    Recovering,
    Stale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeRegistryError {
    UnknownResource {
        resource_id: ResourceId,
    },
    GenerationNotAdvanced {
        resource_id: ResourceId,
        current_generation: u64,
        proposed_generation: u64,
    },
    FenceMismatch {
        current: ResourceFence,
        proposed: ResourceFence,
    },
    NotRecovering {
        resource_id: ResourceId,
    },
    GenerationExhausted {
        resource_id: ResourceId,
    },
}

impl fmt::Display for RuntimeRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownResource { resource_id } => {
                write!(f, "runtime resource {resource_id} is not registered")
            }
            Self::GenerationNotAdvanced {
                resource_id,
                current_generation,
                proposed_generation,
            } => write!(
                f,
                "runtime resource {resource_id} generation {proposed_generation} must be newer than {current_generation}"
            ),
            Self::FenceMismatch { current, proposed } => write!(
                f,
                "runtime fence {:?} does not match current fence {:?}",
                proposed, current
            ),
            Self::NotRecovering { resource_id } => {
                write!(f, "runtime resource {resource_id} is not recovering")
            }
            Self::GenerationExhausted { resource_id } => {
                write!(f, "runtime resource {resource_id} generation is exhausted")
            }
        }
    }
}

impl std::error::Error for RuntimeRegistryError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RuntimeEntry {
    fence: ResourceFence,
    presence: RuntimePresence,
}

/// In-memory fence for asynchronous resource work.
///
/// This registry does not launch, stop, probe, or own operating-system
/// processes. Callers install generations only after durable state establishes
/// them, then use `apply_completion` to discard late callbacks safely.
#[derive(Debug, Default)]
pub struct RuntimeRegistry {
    entries: BTreeMap<ResourceId, RuntimeEntry>,
}

impl RuntimeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn install_recovering(&mut self, fence: ResourceFence) -> Result<(), RuntimeRegistryError> {
        self.install(fence, RuntimePresence::Recovering)
    }

    pub fn install_current(&mut self, fence: ResourceFence) -> Result<(), RuntimeRegistryError> {
        self.install(fence, RuntimePresence::Current)
    }

    fn install(
        &mut self,
        fence: ResourceFence,
        presence: RuntimePresence,
    ) -> Result<(), RuntimeRegistryError> {
        if let Some(current) = self.entries.get(&fence.resource_id) {
            if fence.runtime_generation <= current.fence.runtime_generation {
                return Err(RuntimeRegistryError::GenerationNotAdvanced {
                    resource_id: fence.resource_id,
                    current_generation: current.fence.runtime_generation,
                    proposed_generation: fence.runtime_generation,
                });
            }
        }

        self.entries
            .insert(fence.resource_id, RuntimeEntry { fence, presence });
        Ok(())
    }

    /// Marks an exactly matching recovery candidate as reconciled/current.
    pub fn promote_recovered(&mut self, fence: ResourceFence) -> Result<(), RuntimeRegistryError> {
        let entry = self.entries.get_mut(&fence.resource_id).ok_or(
            RuntimeRegistryError::UnknownResource {
                resource_id: fence.resource_id,
            },
        )?;
        if entry.fence != fence {
            return Err(RuntimeRegistryError::FenceMismatch {
                current: entry.fence,
                proposed: fence,
            });
        }
        if entry.presence != RuntimePresence::Recovering {
            return Err(RuntimeRegistryError::NotRecovering {
                resource_id: fence.resource_id,
            });
        }
        entry.presence = RuntimePresence::Current;
        Ok(())
    }

    /// Makes the exact tracked generation inactive without forgetting its
    /// high-water mark. Repeating the same retirement is harmless.
    pub fn retire(&mut self, fence: ResourceFence) -> Result<(), RuntimeRegistryError> {
        let entry = self.entries.get_mut(&fence.resource_id).ok_or(
            RuntimeRegistryError::UnknownResource {
                resource_id: fence.resource_id,
            },
        )?;
        if entry.fence != fence {
            return Err(RuntimeRegistryError::FenceMismatch {
                current: entry.fence,
                proposed: fence,
            });
        }
        entry.presence = RuntimePresence::Inactive;
        Ok(())
    }

    /// Computes—but does not install—the next generation.
    ///
    /// The caller must first make that generation durable before installing it
    /// here. This keeps the durable store as the source of truth.
    pub fn next_generation(
        &self,
        resource_id: ResourceId,
    ) -> Result<ResourceFence, RuntimeRegistryError> {
        let entry = self
            .entries
            .get(&resource_id)
            .ok_or(RuntimeRegistryError::UnknownResource { resource_id })?;
        let runtime_generation = entry
            .fence
            .runtime_generation
            .checked_add(1)
            .ok_or(RuntimeRegistryError::GenerationExhausted { resource_id })?;
        Ok(ResourceFence::new(resource_id, runtime_generation))
    }

    pub fn current_fence(&self, resource_id: ResourceId) -> Option<ResourceFence> {
        self.entries.get(&resource_id).map(|entry| entry.fence)
    }

    pub fn presence(&self, resource_id: ResourceId) -> Option<RuntimePresence> {
        self.entries.get(&resource_id).map(|entry| entry.presence)
    }

    /// Invokes `apply` only for the exact currently-live generation.
    pub fn apply_completion(
        &self,
        fence: ResourceFence,
        apply: impl FnOnce(),
    ) -> CompletionDisposition {
        let Some(entry) = self.entries.get(&fence.resource_id) else {
            return CompletionDisposition::Stale;
        };
        if entry.fence != fence {
            return CompletionDisposition::Stale;
        }
        match entry.presence {
            RuntimePresence::Recovering => CompletionDisposition::Recovering,
            RuntimePresence::Inactive => CompletionDisposition::Stale,
            RuntimePresence::Current => {
                apply();
                CompletionDisposition::Current
            }
        }
    }
}

//! Exact Connect turn, focus, and action epochs.
//!
//! Zero is never a valid epoch. These values are UX/admission fences, not
//! authority: they cannot grant permissions or create a controller lease.

use std::fmt;
use std::num::NonZeroU64;

use serde::{Deserialize, Serialize};

macro_rules! define_epoch {
    ($name:ident, $label:literal) => {
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(NonZeroU64);

        impl $name {
            pub const fn new(value: u64) -> Option<Self> {
                match NonZeroU64::new(value) {
                    Some(value) => Some(Self(value)),
                    None => None,
                }
            }

            pub const fn get(self) -> u64 {
                self.0.get()
            }

            pub const fn saturating_next(self) -> Self {
                match Self::new(self.get().saturating_add(1)) {
                    Some(next) => next,
                    None => self,
                }
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.debug_tuple($label).field(&self.get()).finish()
            }
        }
    };
}

define_epoch!(TurnEpoch, "TurnEpoch");
define_epoch!(FocusEpoch, "FocusEpoch");
define_epoch!(ActionEpoch, "ActionEpoch");

/// Provider/runtime generation. Zero is invalid; a restart must mint a new
/// nonzero generation so stale answers cannot settle.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RuntimeGeneration(NonZeroU64);

impl RuntimeGeneration {
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn get(self) -> u64 {
        self.0.get()
    }

    pub const fn saturating_next(self) -> Self {
        match Self::new(self.get().saturating_add(1)) {
            Some(next) => next,
            None => self,
        }
    }
}

impl fmt::Debug for RuntimeGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("RuntimeGeneration")
            .field(&self.get())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_epochs_are_rejected() {
        assert!(TurnEpoch::new(0).is_none());
        assert!(FocusEpoch::new(0).is_none());
        assert!(ActionEpoch::new(0).is_none());
        assert!(RuntimeGeneration::new(0).is_none());
        assert_eq!(TurnEpoch::new(1).unwrap().saturating_next().get(), 2);
    }
}

//! Host-owned publication state for the authenticated Connect browser seam.
//!
//! The web client receives only this bounded, non-secret marker.  The marker
//! is published after the listener has bound and is revoked before the owner
//! is dropped.  A monotonically increasing generation prevents a late bind
//! completion from resurrecting a stopped listener.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use serde::Serialize;

use super::envelope::{CONNECT_PROTOCOL_MAJOR, CONNECT_PROTOCOL_MINOR};

pub const CONNECT_WEB_MARKER_TRANSPORT: &str = "connect";
pub const CONNECT_WEB_MARKER_MAX_ENDPOINT_BYTES: usize = 2_048;
pub const CONNECT_WEB_MARKER_MAX_JSON_BYTES: usize = 4 * 1_024;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConnectWebTransportMarker {
    pub transport: &'static str,
    pub endpoint: String,
    pub generation: u64,
    pub protocol_major: u16,
    pub protocol_minor: u16,
}

#[derive(Clone)]
pub struct ConnectWebPublication {
    inner: Arc<ConnectWebPublicationInner>,
}

struct ConnectWebPublicationInner {
    generation: AtomicU64,
    marker: RwLock<Option<ConnectWebTransportMarker>>,
    endpoint: String,
}

impl fmt::Debug for ConnectWebPublication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConnectWebPublication")
            .field("generation", &self.generation())
            .field("published", &self.is_published())
            .finish()
    }
}

impl ConnectWebPublication {
    pub fn new(endpoint: impl Into<String>) -> Self {
        let endpoint = endpoint.into();
        assert!(
            endpoint.len() <= CONNECT_WEB_MARKER_MAX_ENDPOINT_BYTES,
            "Connect endpoint exceeds marker bound"
        );
        Self {
            inner: Arc::new(ConnectWebPublicationInner {
                generation: AtomicU64::new(1),
                marker: RwLock::new(None),
                endpoint,
            }),
        }
    }

    pub fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::Acquire)
    }

    pub fn is_published(&self) -> bool {
        self.inner
            .marker
            .read()
            .map(|marker| marker.is_some())
            .unwrap_or(false)
    }

    /// Publish the marker for the current listener generation.
    pub fn publish(&self) -> u64 {
        let generation = self.next_generation();
        self.set_marker(generation);
        generation
    }

    /// Publish only if the caller still owns the exact listener generation.
    /// This is the late-bind fence used by asynchronous listener startup.
    pub fn publish_if_generation(&self, generation: u64) -> bool {
        if generation == 0
            || self
                .inner
                .generation
                .compare_exchange(generation, generation, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return false;
        }
        self.set_marker(generation);
        true
    }

    /// Revoke publication and advance the fence before shutdown begins.
    pub fn revoke(&self) -> u64 {
        let generation = self.next_generation();
        if let Ok(mut marker) = self.inner.marker.write() {
            *marker = None;
        }
        generation
    }

    pub fn marker(&self) -> Option<ConnectWebTransportMarker> {
        self.inner.marker.read().ok()?.clone()
    }

    pub fn marker_json(&self) -> Option<String> {
        let marker = self.marker()?;
        let encoded = serde_json::to_string(&marker).ok()?;
        (encoded.len() <= CONNECT_WEB_MARKER_MAX_JSON_BYTES).then_some(encoded)
    }

    fn next_generation(&self) -> u64 {
        self.inner
            .generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
                Some(generation.saturating_add(1).max(1))
            })
            .map(|generation| generation.saturating_add(1).max(1))
            .unwrap_or_else(|generation| generation.saturating_add(1).max(1))
    }

    fn set_marker(&self, generation: u64) {
        if let Ok(mut marker) = self.inner.marker.write() {
            *marker = Some(ConnectWebTransportMarker {
                transport: CONNECT_WEB_MARKER_TRANSPORT,
                endpoint: self.inner.endpoint.clone(),
                generation,
                protocol_major: CONNECT_PROTOCOL_MAJOR,
                protocol_minor: CONNECT_PROTOCOL_MINOR,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_is_bounded_non_secret_and_generation_fenced() {
        let publication = ConnectWebPublication::new("/api/connect");
        assert!(publication.marker_json().is_none());

        let first = publication.publish();
        let marker = publication.marker().expect("published marker");
        assert_eq!(marker.transport, CONNECT_WEB_MARKER_TRANSPORT);
        assert_eq!(marker.endpoint, "/api/connect");
        assert_eq!(marker.generation, first);
        assert_eq!(marker.protocol_major, CONNECT_PROTOCOL_MAJOR);
        assert!(publication.marker_json().is_some());

        publication.revoke();
        assert!(publication.marker_json().is_none());
        assert!(!publication.publish_if_generation(first));

        let second = publication.publish();
        assert!(second > first);
        assert!(publication.publish_if_generation(second));
    }

    #[test]
    fn cloned_publications_share_the_same_shutdown_fence() {
        let publication = ConnectWebPublication::new("/api/connect");
        let clone = publication.clone();
        let generation = publication.publish();
        clone.revoke();

        assert!(!publication.is_published());
        assert!(!publication.publish_if_generation(generation));
    }
}

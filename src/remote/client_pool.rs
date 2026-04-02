use super::RemoteClientHandle;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct RemoteClientPool {
    inner: Arc<Mutex<HashMap<String, PooledRemoteClient>>>,
}

#[derive(Clone)]
struct PooledRemoteClient {
    address: String,
    port: u16,
    server_id: String,
    certificate_fingerprint: String,
    client: RemoteClientHandle,
}

impl RemoteClientPool {
    pub fn get_reusable(
        &self,
        address: &str,
        port: u16,
        server_id: Option<&str>,
        certificate_fingerprint: Option<&str>,
    ) -> Option<(String, RemoteClientHandle)> {
        let Ok(mut clients) = self.inner.lock() else {
            return None;
        };
        clients.retain(|_, entry| entry.client.disconnected_message().is_none());
        clients
            .iter()
            .find(|(_, entry)| {
                entry.address == address
                    && entry.port == port
                    && server_id.is_none_or(|value| value == entry.server_id)
                    && certificate_fingerprint
                        .is_none_or(|value| value == entry.certificate_fingerprint)
            })
            .map(|(key, entry)| (key.clone(), entry.client.clone()))
    }

    pub fn insert(
        &self,
        address: String,
        port: u16,
        server_id: String,
        certificate_fingerprint: String,
        client: RemoteClientHandle,
    ) -> String {
        let key = if !certificate_fingerprint.trim().is_empty() {
            format!("fingerprint:{certificate_fingerprint}")
        } else {
            format!("server:{server_id}")
        };
        if let Ok(mut clients) = self.inner.lock() {
            clients.insert(
                key.clone(),
                PooledRemoteClient {
                    address,
                    port,
                    server_id,
                    certificate_fingerprint,
                    client,
                },
            );
        }
        key
    }

    pub fn remove(&self, key: &str) {
        if let Ok(mut clients) = self.inner.lock() {
            clients.remove(key);
        }
    }
}

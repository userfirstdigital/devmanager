//! Redacted Task Cockpit SSH projections.
//!
//! Host catalog identity and labels may cross the wire. Hostnames, usernames,
//! ports, and credential material never do.

use crate::config::{SshAuthMode, SSHConnection};
use crate::domain::cockpit::TaskSshEndpoint;

const MAX_SSH_ENDPOINTS: usize = 32;
const MAX_SSH_LABEL_BYTES: usize = 128;

pub fn redacted_endpoints(connections: &[SSHConnection]) -> Vec<TaskSshEndpoint> {
    connections
        .iter()
        .filter_map(redacted_endpoint)
        .take(MAX_SSH_ENDPOINTS)
        .collect()
}

fn redacted_endpoint(connection: &SSHConnection) -> Option<TaskSshEndpoint> {
    if connection.id.is_empty() || connection.id.len() > 64 {
        return None;
    }
    if connection.id.contains('\0') {
        return None;
    }
    let label = if connection.label.is_empty() {
        connection.id.clone()
    } else if connection.label.len() > MAX_SSH_LABEL_BYTES {
        return None;
    } else {
        connection.label.clone()
    };
    Some(TaskSshEndpoint {
        id: connection.id.clone(),
        label,
        archived: connection.archived.as_ref().copied().unwrap_or(false),
        has_credential: has_credential(connection),
    })
}

fn has_credential(connection: &SSHConnection) -> bool {
    connection.auth.as_ref().is_some_and(|auth| {
        auth.credential_ref.as_ref().is_some()
            || matches!(
                auth.mode,
                SshAuthMode::Password | SshAuthMode::PrivateKey
            )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Nullable, SshAuth};

    #[test]
    fn redacted_endpoints_omit_hosts_usernames_and_secrets() {
        let encoded = serde_json::to_string(&redacted_endpoints(&[SSHConnection {
            id: "jump".into(),
            label: "Jump box".into(),
            host: "secret.example".into(),
            port: 22,
            username: "deploy".into(),
            auth: Nullable::Value(SshAuth {
                mode: SshAuthMode::PrivateKey,
                credential_ref: Nullable::Value("vault:ssh/jump".into()),
                extra: Default::default(),
            }),
            archived: Nullable::Value(false),
            extra: Default::default(),
        }]))
        .expect("encode");
        assert!(encoded.contains("jump"));
        assert!(encoded.contains("Jump box"));
        assert!(!encoded.contains("secret.example"));
        assert!(!encoded.contains("deploy"));
        assert!(!encoded.contains("vault:ssh/jump"));
        assert!(encoded.contains("\"has_credential\":true"));
    }
}

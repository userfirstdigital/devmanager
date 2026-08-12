//! Redacted Task Cockpit SSH projections.
//!
//! Host catalog identity and labels may cross the wire. Hostnames, usernames,
//! ports, and credential material never do.

use crate::config::{SSHConnection, SshAuthMode};
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

/// Exact catalog endpoint identity. Host/user/path/credential spellings are
/// rejected before any runtime call.
pub fn accept_exact_endpoint<'a>(
    endpoints: &'a [TaskSshEndpoint],
    endpoint_id: &str,
) -> Result<&'a TaskSshEndpoint, SshEndpointDenial> {
    if !endpoint_id_is_catalog_form(endpoint_id) {
        return Err(SshEndpointDenial::ForeignInput);
    }
    endpoints
        .iter()
        .find(|endpoint| endpoint.id == endpoint_id && !endpoint.archived)
        .ok_or(SshEndpointDenial::UnknownEndpoint)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshEndpointDenial {
    ForeignInput,
    UnknownEndpoint,
}

fn endpoint_id_is_catalog_form(endpoint_id: &str) -> bool {
    if endpoint_id.is_empty() || endpoint_id.len() > 64 {
        return false;
    }
    if endpoint_id.contains('\0')
        || endpoint_id.contains('@')
        || endpoint_id.contains(':')
        || endpoint_id.contains('/')
        || endpoint_id.contains('\\')
        || endpoint_id.contains(' ')
    {
        return false;
    }
    endpoint_id
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
        && !endpoint_id.starts_with('.')
}

fn has_credential(connection: &SSHConnection) -> bool {
    connection.auth.as_ref().is_some_and(|auth| {
        auth.credential_ref.as_ref().is_some()
            || matches!(auth.mode, SshAuthMode::Password | SshAuthMode::PrivateKey)
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

    #[test]
    fn exact_endpoint_rejects_host_user_path_and_credential_inputs() {
        let endpoints = redacted_endpoints(&[SSHConnection {
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
        }]);
        assert_eq!(
            accept_exact_endpoint(&endpoints, "jump").map(|endpoint| endpoint.id.as_str()),
            Ok("jump")
        );
        assert_eq!(
            accept_exact_endpoint(&endpoints, "deploy@secret.example"),
            Err(SshEndpointDenial::ForeignInput)
        );
        assert_eq!(
            accept_exact_endpoint(&endpoints, "C:/Users/deploy/.ssh/id_rsa"),
            Err(SshEndpointDenial::ForeignInput)
        );
        assert_eq!(
            accept_exact_endpoint(&endpoints, "vault:ssh/jump"),
            Err(SshEndpointDenial::ForeignInput)
        );
        assert_eq!(
            accept_exact_endpoint(&endpoints, "other"),
            Err(SshEndpointDenial::UnknownEndpoint)
        );
    }
}

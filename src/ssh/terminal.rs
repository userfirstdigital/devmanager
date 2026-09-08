//! Host-local launch preparation for an interactive SSH terminal.
//! Authentication prompts stay in the PTY; key bytes never enter resource recipes.
use super::credentials::{
    CredentialRef, CredentialResolver, KeyIdentity, KeyMaterialStore, RetainedKey,
};
use crate::config::{AppConfig, ConfigCommand, SshAuthMode};
use std::path::{Path, PathBuf};

pub(crate) struct InteractiveSshLaunch {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub title: String,
    pub key: Option<RetainedKey>,
}

pub(crate) fn prepare_interactive_terminal(
    config: &AppConfig,
    endpoint_id: &str,
    key_root: &Path,
) -> Result<InteractiveSshLaunch, String> {
    let endpoints = super::redacted_endpoints(&config.ssh_connections);
    super::accept_exact_endpoint(&endpoints, endpoint_id)
        .map_err(|_| "This saved SSH connection is unavailable.".to_string())?;
    let connection = config
        .ssh_connections
        .iter()
        .find(|c| c.id == endpoint_id)
        .ok_or("This saved SSH connection is unavailable.")?;
    ConfigCommand::UpdateSsh {
        connection: connection.clone(),
    }
    .validate()
    .map_err(|_| "Check the SSH hostname, username and port.".to_string())?;
    super::launch::validate_host(&connection.host).map_err(|_| "Enter a valid SSH hostname.")?;
    super::launch::validate_username(&connection.username)
        .map_err(|_| "Enter a valid SSH username.")?;
    let program = crate::diagnostics::resolve::resolve_all("ssh")
        .into_iter()
        .next()
        .ok_or("OpenSSH was not found. Install the OpenSSH client and try again.")?
        .canonicalize()
        .map_err(|_| "The OpenSSH executable is unavailable.")?;
    let mut args = vec![
        "-tt".into(),
        "-o".into(),
        "ConnectTimeout=15".into(),
        "-o".into(),
        "ServerAliveInterval=30".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
        "-p".into(),
        connection.port.to_string(),
        "-l".into(),
        connection.username.clone(),
    ];
    let mut key = None;
    if let Some(auth) = connection.auth.as_ref() {
        if auth.mode == SshAuthMode::PrivateKey {
            let reference = auth
                .credential_ref
                .as_ref()
                .ok_or("The saved SSH key is unavailable.")?;
            let reference = CredentialRef::new(reference.clone())
                .map_err(|_| "The saved SSH key reference is invalid.")?;
            let secret = super::ConfigCredentialResolver::new(config.clone())
                .resolve(&reference)
                .map_err(|_| "The saved SSH key could not be unlocked on this host.")?;
            let identity = KeyIdentity::issue(&connection.id, &reference)
                .map_err(|_| "The SSH key identity is invalid.")?;
            let store = KeyMaterialStore::new(key_root.to_path_buf())
                .map_err(|_| "The SSH key directory is unavailable.")?;
            let retained = store
                .materialize(&identity, &secret)
                .map_err(|_| "The SSH key could not be prepared.")?;
            retained
                .revalidate()
                .map_err(|_| "The SSH key changed during preparation.")?;
            args.extend(["-i".into(), retained.path_string()]);
            key = Some(retained);
        }
    }
    // End options before the validated hostname; never build a shell command string.
    args.extend(["--".into(), connection.host.clone()]);
    Ok(InteractiveSshLaunch {
        program,
        args,
        title: format!("SSH · {}", connection.label),
        key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Nullable, SSHConnection};

    #[test]
    fn interactive_ssh_rejects_foreign_archived_and_option_like_endpoints() {
        let mut config = AppConfig::default();
        config.ssh_connections.push(SSHConnection {
            id: "test-server".into(),
            label: "Test".into(),
            host: "localhost".into(),
            username: "user".into(),
            port: 22,
            ..Default::default()
        });
        let dir = tempfile::tempdir().unwrap();
        for id in ["missing", "user@localhost", "-oProxyCommand=bad"] {
            assert!(prepare_interactive_terminal(&config, id, dir.path()).is_err());
        }
        config.ssh_connections[0].archived = Nullable::Value(true);
        assert!(prepare_interactive_terminal(&config, "test-server", dir.path()).is_err());
        config.ssh_connections[0].archived = Nullable::Value(false);
        for host in ["-oProxyCommand=bad", "host with space", "host\ncommand"] {
            config.ssh_connections[0].host = host.into();
            assert!(prepare_interactive_terminal(&config, "test-server", dir.path()).is_err());
        }
    }
}

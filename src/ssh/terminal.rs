//! Host-local launch preparation for an interactive SSH terminal.
//! Key bytes never enter resource recipes: the key is materialized to a
//! locked-down file passed with `-i`, and a saved password reaches ssh only
//! through the askpass bridge (see `askpass.rs`). Every other prompt stays in
//! the PTY.
use super::askpass::AskpassSecretFile;
use super::credentials::{
    CredentialKind, CredentialRef, CredentialSecret, KeyIdentity, KeyMaterialStore, RetainedKey,
};
use crate::config::{AppConfig, ConfigCommand};
use std::path::{Path, PathBuf};

pub(crate) struct InteractiveSshLaunch {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub title: String,
    pub key: Option<RetainedKey>,
    pub password: Option<AskpassSecretFile>,
}

pub(crate) fn prepare_interactive_terminal(
    config: &AppConfig,
    endpoint_id: &str,
    config_dir: &Path,
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
    let secrets = match connection
        .auth
        .as_ref()
        .and_then(|auth| auth.credential_ref.as_ref())
    {
        Some(reference) => super::vault::load(config_dir, reference)?,
        None => super::vault::SshSecrets::default(),
    };
    let mut key = None;
    if let Some(private_key) = secrets.private_key.as_ref() {
        let secret =
            CredentialSecret::from_bytes(CredentialKind::PrivateKey, private_key.as_bytes())
                .map_err(|_| "The saved SSH key is empty or too large.")?;
        let reference = CredentialRef::new("credential:ssh-terminal-key")
            .map_err(|_| "The SSH key identity is invalid.")?;
        let identity = KeyIdentity::issue(&connection.id, &reference)
            .map_err(|_| "The SSH key identity is invalid.")?;
        let store = KeyMaterialStore::new(config_dir.join("ssh-terminal-keys"))
            .map_err(|_| "The SSH key directory is unavailable.")?;
        let retained = store
            .materialize(&identity, &secret)
            .map_err(|_| "The saved SSH key could not be prepared. Check that it is a valid OpenSSH or PEM private key.")?;
        retained
            .revalidate()
            .map_err(|_| "The SSH key changed during preparation.")?;
        args.extend(["-i".into(), retained.path_string()]);
        key = Some(retained);
    }
    #[cfg(unix)]
    let password = match secrets.password.as_ref() {
        Some(saved) => Some(
            AskpassSecretFile::write(&config_dir.join("ssh-askpass"), saved)
                .map_err(|_| "The saved SSH password could not be prepared.")?,
        ),
        None => None,
    };
    // Without an askpass bridge the password prompt simply stays in the PTY.
    #[cfg(not(unix))]
    let password: Option<AskpassSecretFile> = None;
    // End options before the validated hostname; never build a shell command string.
    args.extend(["--".into(), connection.host.clone()]);
    let (program, args) = match &password {
        Some(file) => {
            let host = std::env::current_exe()
                .and_then(|path| path.canonicalize())
                .map_err(|_| "The DevManager host executable is unavailable.")?;
            let mut wrapped = vec![
                super::askpass::EXEC_FLAG.to_string(),
                file.path().to_string_lossy().into_owned(),
                "--".into(),
                program.to_string_lossy().into_owned(),
            ];
            wrapped.extend(args);
            (host, wrapped)
        }
        None => (program, args),
    };
    Ok(InteractiveSshLaunch {
        program,
        args,
        title: format!("SSH · {}", connection.label),
        key,
        password,
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

    #[cfg(unix)]
    #[test]
    fn a_saved_password_routes_ssh_through_the_askpass_bridge() {
        let mut config = AppConfig::default();
        config.ssh_connections.push(SSHConnection {
            id: "test-server".into(),
            label: "Test".into(),
            host: "localhost".into(),
            username: "user".into(),
            port: 22,
            auth: Nullable::Value(crate::config::SshAuth {
                mode: crate::config::SshAuthMode::Password,
                credential_ref: Nullable::Value(
                    crate::config::model::encode_legacy_ssh_credential(Some("hunter2"), None)
                        .unwrap()
                        .unwrap(),
                ),
                ..Default::default()
            }),
            ..Default::default()
        });
        let dir = tempfile::tempdir().unwrap();
        let Ok(launch) = prepare_interactive_terminal(&config, "test-server", dir.path()) else {
            // No OpenSSH client on this machine; nothing to route.
            return;
        };
        assert_eq!(launch.args[0], super::super::askpass::EXEC_FLAG);
        let file = launch.password.as_ref().expect("saved password file");
        assert_eq!(launch.args[1], file.path().to_string_lossy());
        assert_eq!(launch.args[2], "--");
        assert!(launch.args.last().is_some_and(|host| host == "localhost"));
        assert!(
            !launch.args.iter().any(|arg| arg.contains("hunter2")),
            "the password never enters the launch recipe"
        );
    }
}

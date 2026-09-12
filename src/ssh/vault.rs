//! Host-owned SSH credentials for saved connections.
//!
//! `config.json` only ever carries an opaque `credential:ssh-v2-<id>.<p|k|pk>`
//! reference. The password and private key live in `ssh-credentials.json`
//! beside it, each protected by the platform secret store (DPAPI on Windows,
//! Secret Service on Linux). The reference suffix records which secrets exist,
//! so the sidebar projection can say "saved" without decrypting anything.
//! References written by the 0.4.1 migration (`credential:legacy-v1-…`) are
//! still read, and are replaced by a vault entry the next time they are saved.
use crate::config::{Nullable, SshAuth, SshAuthMode};
use crate::domain::cockpit::SshSecretEdit;
use crate::providers::settings::{protect_secret_value, reveal_secret_value};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

pub(crate) const VAULT_FILE: &str = "ssh-credentials.json";
const REFERENCE_PREFIX: &str = "credential:ssh-v2-";
const VAULT_VERSION: u32 = 1;

/// The secrets one saved connection holds. Either may be absent.
#[derive(Default)]
pub(crate) struct SshSecrets {
    pub(crate) password: Option<Zeroizing<String>>,
    pub(crate) private_key: Option<Zeroizing<String>>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VaultFile {
    version: u32,
    #[serde(default)]
    entries: BTreeMap<String, VaultEntry>,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VaultEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    private_key: Option<String>,
}

fn scope(entry_id: &str, field: &str) -> Vec<u8> {
    format!("devmanager.ssh-credential/v1/{entry_id}/{field}").into_bytes()
}

fn vault_path(config_dir: &Path) -> PathBuf {
    config_dir.join(VAULT_FILE)
}

fn parse_reference(reference: &str) -> Option<(&str, &str)> {
    let rest = reference.strip_prefix(REFERENCE_PREFIX)?;
    let (id, suffix) = rest.rsplit_once('.')?;
    (!id.is_empty() && matches!(suffix, "p" | "k" | "pk")).then_some((id, suffix))
}

/// Which secrets a saved reference holds: `(password, private_key)`.
pub(crate) fn presence(reference: &str) -> (bool, bool) {
    if let Some((_, suffix)) = parse_reference(reference) {
        return (suffix.contains('p'), suffix.contains('k'));
    }
    match crate::config::model::decode_legacy_ssh_credential(reference) {
        Ok(Some((password, private_key))) => (password.is_some(), private_key.is_some()),
        _ => (false, false),
    }
}

/// Unlock the secrets behind a saved reference. Errors are user-facing.
pub(crate) fn load(config_dir: &Path, reference: &str) -> Result<SshSecrets, String> {
    if let Some((id, _)) = parse_reference(reference) {
        let vault = read_vault(config_dir)?;
        let entry = vault.entries.get(id).ok_or(
            "The saved SSH password or key is missing on this computer. Re-enter it in Edit.",
        )?;
        let reveal = |blob: &Option<String>, field: &str| {
            blob.as_deref()
                .map(|blob| {
                    reveal_secret_value(blob, &scope(id, field)).map_err(|_| {
                        "The saved SSH password or key could not be unlocked. Check that the system keyring is unlocked."
                            .to_string()
                    })
                })
                .transpose()
        };
        return Ok(SshSecrets {
            password: reveal(&entry.password, "password")?,
            private_key: reveal(&entry.private_key, "privateKey")?,
        });
    }
    match crate::config::model::decode_legacy_ssh_credential(reference) {
        Ok(Some((password, private_key))) => Ok(SshSecrets {
            password: password.map(Zeroizing::new),
            private_key: private_key.map(Zeroizing::new),
        }),
        _ => Err("The saved SSH password or key is unreadable. Re-enter it in Edit.".into()),
    }
}

/// Apply one editor save to a connection's authentication.
///
/// Returns the new auth and, when a vault entry was replaced, its reference;
/// the caller removes that entry only after the config write has landed, so a
/// failed write never strands the connection without its old secrets.
pub(crate) fn apply_edit(
    config_dir: &Path,
    current: &Nullable<SshAuth>,
    password: &SshSecretEdit,
    private_key: &SshSecretEdit,
) -> Result<(Nullable<SshAuth>, Option<String>), String> {
    let current_ref = current
        .as_ref()
        .and_then(|auth| auth.credential_ref.as_ref())
        .cloned();
    let keeps_any =
        matches!(password, SshSecretEdit::Keep) || matches!(private_key, SshSecretEdit::Keep);
    let existing = match &current_ref {
        Some(reference) if keeps_any => load(config_dir, reference)?,
        _ => SshSecrets::default(),
    };
    let pick = |edit: &SshSecretEdit, kept: Option<Zeroizing<String>>| match edit {
        SshSecretEdit::Keep => kept,
        SshSecretEdit::Clear => None,
        SshSecretEdit::Set(value) if value.is_empty() => kept,
        SshSecretEdit::Set(value) => Some(Zeroizing::new(value.clone())),
    };
    let password = pick(password, existing.password);
    let private_key = pick(private_key, existing.private_key);
    let stale = current_ref.filter(|reference| parse_reference(reference).is_some());
    if password.is_none() && private_key.is_none() {
        // No secrets left: OpenSSH's own keys, config and agent take over. An
        // explicit Agent/Default mode without a reference is kept as chosen.
        let auth = match current.as_ref() {
            Some(auth) if auth.credential_ref.as_ref().is_none() => current.clone(),
            _ => Nullable::Absent,
        };
        return Ok((auth, stale));
    }
    let entry_id = uuid::Uuid::new_v4().simple().to_string();
    let protect = |value: &Option<Zeroizing<String>>, field: &str| {
        value
            .as_ref()
            .map(|value| {
                protect_secret_value(value, &scope(&entry_id, field)).map_err(|error| {
                    format!("the system keyring could not protect the SSH credentials: {error}")
                })
            })
            .transpose()
    };
    let entry = VaultEntry {
        password: protect(&password, "password")?,
        private_key: protect(&private_key, "privateKey")?,
    };
    let mut vault = read_vault(config_dir)?;
    vault.version = VAULT_VERSION;
    vault.entries.insert(entry_id.clone(), entry);
    write_vault(config_dir, &vault)?;
    let suffix = match (password.is_some(), private_key.is_some()) {
        (true, true) => "pk",
        (true, false) => "p",
        _ => "k",
    };
    let auth = SshAuth {
        // Key first, password as the fallback when the server asks for one --
        // the order 0.4.1 used.
        mode: if private_key.is_some() {
            SshAuthMode::PrivateKey
        } else {
            SshAuthMode::Password
        },
        credential_ref: Nullable::Value(format!("{REFERENCE_PREFIX}{entry_id}.{suffix}")),
        extra: current
            .as_ref()
            .map(|auth| auth.extra.clone())
            .unwrap_or_default(),
    };
    Ok((Nullable::Value(auth), stale))
}

/// Remove a replaced vault entry. Legacy references have no entry to remove.
pub(crate) fn remove(config_dir: &Path, reference: &str) {
    let Some((id, _)) = parse_reference(reference) else {
        return;
    };
    if let Ok(mut vault) = read_vault(config_dir) {
        if vault.entries.remove(id).is_some() {
            let _ = write_vault(config_dir, &vault);
        }
    }
}

fn read_vault(config_dir: &Path) -> Result<VaultFile, String> {
    match std::fs::read(vault_path(config_dir)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|_| "The SSH credential store is unreadable.".to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(VaultFile {
            version: VAULT_VERSION,
            entries: BTreeMap::new(),
        }),
        Err(_) => Err("The SSH credential store is unavailable.".into()),
    }
}

fn write_vault(config_dir: &Path, vault: &VaultFile) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(vault)
        .map_err(|_| "The SSH credential store could not be encoded.".to_string())?;
    let path = vault_path(config_dir);
    let temp = config_dir.join(format!(
        ".{VAULT_FILE}.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, &path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.map_err(|_| "The SSH credential store could not be written.".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy(password: Option<&str>, key: Option<&str>) -> String {
        crate::config::model::encode_legacy_ssh_credential(password, key)
            .unwrap()
            .expect("legacy reference")
    }

    #[test]
    fn presence_reads_the_reference_without_decrypting() {
        assert_eq!(presence("credential:ssh-v2-abc.p"), (true, false));
        assert_eq!(presence("credential:ssh-v2-abc.k"), (false, true));
        assert_eq!(presence("credential:ssh-v2-abc.pk"), (true, true));
        assert_eq!(presence("credential:ssh-v2-abc.x"), (false, false));
        assert_eq!(presence("credential:ssh-v2-.p"), (false, false));
        assert_eq!(presence(&legacy(Some("pw"), None)), (true, false));
        assert_eq!(presence(&legacy(None, Some("KEY"))), (false, true));
    }

    #[test]
    fn legacy_references_still_unlock_both_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = load(dir.path(), &legacy(Some("pw"), Some("KEY"))).unwrap();
        assert_eq!(secrets.password.as_deref().map(String::as_str), Some("pw"));
        assert_eq!(
            secrets.private_key.as_deref().map(String::as_str),
            Some("KEY")
        );
    }

    #[test]
    fn clearing_every_secret_drops_the_reference_and_touches_no_keyring() {
        let dir = tempfile::tempdir().unwrap();
        let current = Nullable::Value(SshAuth {
            mode: SshAuthMode::Password,
            credential_ref: Nullable::Value(legacy(Some("pw"), None)),
            ..Default::default()
        });
        let (auth, stale) = apply_edit(
            dir.path(),
            &current,
            &SshSecretEdit::Clear,
            &SshSecretEdit::Clear,
        )
        .unwrap();
        assert!(matches!(auth, Nullable::Absent));
        assert_eq!(stale, None, "legacy references have no vault entry");
        assert!(!dir.path().join(VAULT_FILE).exists());
    }

    #[test]
    fn a_missing_vault_entry_asks_the_user_to_re_enter_it() {
        let dir = tempfile::tempdir().unwrap();
        let error = load(dir.path(), "credential:ssh-v2-gone.p")
            .err()
            .expect("missing entry");
        assert!(error.contains("Re-enter"), "{error}");
    }
}

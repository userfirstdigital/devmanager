//! Linux at-rest custody: a Secret Service master key and scope-bound AEAD.
//!
//! Persisted envelopes name their immutable wallet key. Concurrent first starts
//! may create different keys; neither replaces the other or strands its data.
//! Ordinary unit tests use a process-local key and never contact a user's wallet.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
use secret_service::{EncryptionType, SecretService};
use zeroize::Zeroizing;

const MAGIC: &[u8; 8] = b"DMSSV001";
const HEADER: usize = 8 + 16 + 12;
const MAX_PLAIN: usize = 48 * 1024;
const MAX_KEYS: usize = 32;
const WALLET_DEADLINE: Duration = Duration::from_secs(3);
const PURPOSE: &str = "at-rest-master-v1";
#[cfg(debug_assertions)]
const APPLICATION: &str = "com.userfirst.devmanager.development";
#[cfg(not(debug_assertions))]
const APPLICATION: &str = "com.userfirst.devmanager";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Unavailable,
    Locked,
    MissingKey,
    Invalid,
    Busy,
    Deadline,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "desktop Secret Service wallet is unavailable",
            Self::Locked => "unlock the desktop wallet and try again",
            Self::MissingKey => "the desktop wallet no longer contains this data's encryption key",
            Self::Invalid => "protected data or its wallet key is invalid",
            Self::Busy => "desktop wallet access is busy; try again",
            Self::Deadline => "desktop wallet access timed out; unlock it and try again",
        })
    }
}

struct WalletKey {
    id: [u8; 16],
    bytes: Zeroizing<Vec<u8>>,
}

fn seal(plaintext: &[u8], scope: &[u8], key: &WalletKey) -> Result<Vec<u8>, Error> {
    if plaintext.is_empty() || plaintext.len() > MAX_PLAIN || scope.len() > MAX_PLAIN {
        return Err(Error::Invalid);
    }
    let key_cipher = LessSafeKey::new(
        UnboundKey::new(&CHACHA20_POLY1305, &key.bytes).map_err(|_| Error::Invalid)?,
    );
    let mut nonce = [0; 12];
    getrandom::fill(&mut nonce).map_err(|_| Error::Unavailable)?;
    let mut header = Vec::with_capacity(HEADER);
    header.extend_from_slice(MAGIC);
    header.extend_from_slice(&key.id);
    header.extend_from_slice(&nonce);
    let mut aad = header.clone();
    aad.extend_from_slice(scope);
    let mut ciphertext = Zeroizing::new(plaintext.to_vec());
    key_cipher
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad),
            &mut *ciphertext,
        )
        .map_err(|_| Error::Invalid)?;
    header.extend_from_slice(&ciphertext);
    Ok(header)
}

fn envelope_key_id(blob: &[u8]) -> Result<[u8; 16], Error> {
    if blob.len() <= HEADER + 16 || blob.len() > HEADER + MAX_PLAIN + 16 || &blob[..8] != MAGIC {
        return Err(Error::Invalid);
    }
    blob[8..24].try_into().map_err(|_| Error::Invalid)
}

fn open(blob: &[u8], scope: &[u8], key: &WalletKey) -> Result<Zeroizing<Vec<u8>>, Error> {
    if envelope_key_id(blob)? != key.id || scope.len() > MAX_PLAIN {
        return Err(Error::Invalid);
    }
    let cipher = LessSafeKey::new(
        UnboundKey::new(&CHACHA20_POLY1305, &key.bytes).map_err(|_| Error::Invalid)?,
    );
    let mut aad = blob[..HEADER].to_vec();
    aad.extend_from_slice(scope);
    let nonce = blob[24..HEADER].try_into().map_err(|_| Error::Invalid)?;
    let mut plaintext = Zeroizing::new(blob[HEADER..].to_vec());
    let len = cipher
        .open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad),
            &mut plaintext,
        )
        .map_err(|_| Error::Invalid)?
        .len();
    plaintext.truncate(len);
    Ok(plaintext)
}

#[derive(Default)]
struct KeyCache {
    preferred: Option<[u8; 16]>,
    keys: HashMap<[u8; 16], WalletKey>,
}

fn with_key<T>(
    id: Option<[u8; 16]>,
    operation: impl FnOnce(&WalletKey) -> Result<T, Error>,
) -> Result<T, Error> {
    static CACHE: OnceLock<Mutex<KeyCache>> = OnceLock::new();
    // Never queue an unbounded number of synchronous wallet waits behind a
    // locked desktop service. Callers keep their draft and can retry.
    let mut cache = CACHE
        .get_or_init(Mutex::default)
        .try_lock()
        .map_err(|_| Error::Busy)?;
    let wanted = id.or(cache.preferred);
    if let Some(key) = wanted.and_then(|id| cache.keys.get(&id)) {
        return operation(key);
    }
    if cache.keys.len() >= MAX_KEYS {
        return Err(Error::Invalid);
    }
    #[cfg(not(test))]
    let key = wallet_key(APPLICATION, id)?;
    #[cfg(test)]
    let key = {
        // Deliberately cannot recover across harness processes or reach an OS
        // store. The ignored OS test below exercises the real adapter directly.
        if id.is_some() {
            return Err(Error::MissingKey);
        }
        new_key()?
    };
    let key_id = key.id;
    cache.keys.insert(key_id, key);
    if id.is_none() {
        cache.preferred = Some(key_id);
    }
    operation(cache.keys.get(&key_id).ok_or(Error::MissingKey)?)
}

pub(crate) fn protect(plaintext: &[u8], scope: &[u8]) -> Result<Vec<u8>, Error> {
    if plaintext.is_empty() || plaintext.len() > MAX_PLAIN || scope.len() > MAX_PLAIN {
        return Err(Error::Invalid);
    }
    with_key(None, |key| seal(plaintext, scope, key))
}

pub(crate) fn reveal(blob: &[u8], scope: &[u8]) -> Result<Zeroizing<Vec<u8>>, Error> {
    let id = envelope_key_id(blob)?;
    with_key(Some(id), |key| open(blob, scope, key))
}

fn new_key() -> Result<WalletKey, Error> {
    let mut id = [0; 16];
    let mut bytes = Zeroizing::new(vec![0; 32]);
    getrandom::fill(&mut id).map_err(|_| Error::Unavailable)?;
    getrandom::fill(&mut bytes).map_err(|_| Error::Unavailable)?;
    Ok(WalletKey { id, bytes })
}

fn bounded_wallet<T: Send>(
    operation: impl AsyncFnOnce() -> Result<T, Error> + Send,
) -> Result<T, Error> {
    // A scoped owner works from both sync and Tokio callers. The single-thread
    // runtime and all DBus futures are dropped and the thread joined on timeout.
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| Error::Unavailable)?;
                runtime.block_on(async {
                    tokio::time::timeout(WALLET_DEADLINE, operation())
                        .await
                        .map_err(|_| Error::Deadline)?
                })
            })
            .join()
            .map_err(|_| Error::Unavailable)?
    })
}

async fn connect() -> Result<SecretService<'static>, Error> {
    let connection = zbus::connection::Builder::session()
        .map_err(|_| Error::Unavailable)?
        .method_timeout(WALLET_DEADLINE)
        .build()
        .await
        .map_err(|_| Error::Unavailable)?;
    SecretService::connect_with_existing(EncryptionType::Dh, connection)
        .await
        .map_err(|_| Error::Unavailable)
}

fn wallet_key(application: &str, id: Option<[u8; 16]>) -> Result<WalletKey, Error> {
    bounded_wallet(async || {
        let service = connect().await?;
        let id_text = id.map(|id| uuid::Uuid::from_bytes(id).to_string());
        let mut attributes = HashMap::from([("application", application), ("purpose", PURPOSE)]);
        if let Some(id) = &id_text {
            attributes.insert("key-id", id);
        }
        let found = service
            .search_items(attributes)
            .await
            .map_err(|_| Error::Unavailable)?;
        if !found.locked.is_empty() {
            return Err(Error::Locked);
        }
        if found.unlocked.len() > MAX_KEYS || (id.is_some() && found.unlocked.len() > 1) {
            return Err(Error::Invalid);
        }
        // Deterministic selection avoids generating keys on each start after a
        // concurrent initial creation. Envelopes retain the exact selected ID.
        let mut found = found.unlocked;
        found.sort_by(|a, b| a.item_path.as_str().cmp(b.item_path.as_str()));
        if let Some(item) = found.first() {
            let attrs = item
                .get_attributes()
                .await
                .map_err(|_| Error::Unavailable)?;
            let key_id = attrs
                .get("key-id")
                .and_then(|s| uuid::Uuid::parse_str(s).ok())
                .map(|id| *id.as_bytes())
                .ok_or(Error::Invalid)?;
            if attrs.get("application").map(String::as_str) != Some(application)
                || attrs.get("purpose").map(String::as_str) != Some(PURPOSE)
                || id.is_some_and(|id| key_id != id)
            {
                return Err(Error::Invalid);
            }
            let bytes = Zeroizing::new(item.get_secret().await.map_err(|_| Error::Unavailable)?);
            if bytes.len() != 32 {
                return Err(Error::Invalid);
            }
            return Ok(WalletKey { id: key_id, bytes });
        }
        if id.is_some() {
            return Err(Error::MissingKey);
        }
        let collection = service
            .get_default_collection()
            .await
            .map_err(|_| Error::Unavailable)?;
        if collection
            .is_locked()
            .await
            .map_err(|_| Error::Unavailable)?
        {
            return Err(Error::Locked);
        }
        let key = new_key()?;
        let id = uuid::Uuid::from_bytes(key.id).to_string();
        // Never replace an existing wallet key. A timed-out acknowledgement can
        // leave a recoverable unused key, but cannot destroy prior ciphertext.
        let item = collection
            .create_item(
                "DevManager encrypted data",
                HashMap::from([
                    ("application", application),
                    ("purpose", PURPOSE),
                    ("key-id", id.as_str()),
                ]),
                &key.bytes,
                false,
                "application/octet-stream",
            )
            .await
            .map_err(|_| Error::Unavailable)?;
        let observed = Zeroizing::new(item.get_secret().await.map_err(|_| Error::Unavailable)?);
        if *observed != *key.bytes {
            return Err(Error::Invalid);
        }
        Ok(key)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ciphertext_is_bound_to_key_scope_header_and_contents() {
        let key = new_key().unwrap();
        let blob = seal(b"private environment", b"profile/task/generation", &key).unwrap();
        assert_eq!(
            &*open(&blob, b"profile/task/generation", &key).unwrap(),
            b"private environment"
        );
        assert!(open(&blob, b"other-profile", &key).is_err());
        assert!(open(&blob, b"profile/task/generation", &new_key().unwrap()).is_err());
        for offset in [0, 8, 24, HEADER, blob.len() - 1] {
            let mut changed = blob.clone();
            changed[offset] ^= 1;
            assert!(open(&changed, b"profile/task/generation", &key).is_err());
        }
        assert!(envelope_key_id(&blob[..HEADER]).is_err());
        assert!(!blob
            .windows(b"private environment".len())
            .any(|s| s == b"private environment"));
        assert_ne!(
            blob,
            seal(b"private environment", b"profile/task/generation", &key).unwrap()
        );
        assert!(seal(&vec![0; MAX_PLAIN + 1], b"scope", &key).is_err());
    }

    #[test]
    fn wallet_deadline_drops_pending_work_before_return() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Dropped<'a>(&'a AtomicBool);
        impl Drop for Dropped<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = AtomicBool::new(false);
        let result: Result<(), Error> = bounded_wallet(async || {
            let _owner = Dropped(&dropped);
            std::future::pending().await
        });
        assert_eq!(result, Err(Error::Deadline));
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    #[ignore = "requires an unlocked desktop Secret Service wallet; creates and removes one uniquely owned test key"]
    fn secret_service_roundtrip_retains_exact_key_and_cleans_up() {
        let namespace = format!("com.userfirst.devmanager.test.{}", uuid::Uuid::now_v7());
        let result = std::panic::catch_unwind(|| {
            let key = wallet_key(&namespace, None).expect("unlocked desktop wallet");
            let blob = seal(b"isolated OS acceptance", b"scope", &key).unwrap();
            let restored = wallet_key(&namespace, Some(key.id)).unwrap();
            assert_eq!(
                &*open(&blob, b"scope", &restored).unwrap(),
                b"isolated OS acceptance"
            );
            assert_eq!(wallet_key(&namespace, None).unwrap().id, key.id);
            assert!(matches!(
                wallet_key(&namespace, Some([0; 16])),
                Err(Error::MissingKey)
            ));
        });
        bounded_wallet(async || {
            let service = connect().await?;
            let items = service
                .search_items(HashMap::from([
                    ("application", namespace.as_str()),
                    ("purpose", PURPOSE),
                ]))
                .await
                .map_err(|_| Error::Unavailable)?;
            assert!(items.locked.is_empty());
            assert_eq!(items.unlocked.len(), 1);
            for item in items.unlocked {
                item.delete().await.map_err(|_| Error::Unavailable)?;
            }
            let remaining = service
                .search_items(HashMap::from([("application", namespace.as_str())]))
                .await
                .map_err(|_| Error::Unavailable)?;
            assert!(remaining.locked.is_empty() && remaining.unlocked.is_empty());
            Ok(())
        })
        .expect("remove exact test wallet key");
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}

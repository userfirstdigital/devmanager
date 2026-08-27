//! OS-protected secret custody for provider environment values (Windows DPAPI).

use std::fmt;

use base64::Engine;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

#[derive(Clone, PartialEq, Eq)]
pub enum SecretCustodyError {
    Unsupported,
    ProtectFailed,
    UnprotectFailed,
    TooLarge,
    Empty,
}

impl fmt::Display for SecretCustodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported => write!(f, "secret custody is unsupported on this platform"),
            Self::ProtectFailed => write!(f, "secret protection failed"),
            Self::UnprotectFailed => write!(f, "secret unprotection failed"),
            Self::TooLarge => write!(f, "secret payload exceeds bound"),
            Self::Empty => write!(f, "secret payload is empty"),
        }
    }
}

impl fmt::Debug for SecretCustodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for SecretCustodyError {}

/// Plaintext secret / env-map bound before protection.
pub const MAX_SECRET_PLAINTEXT: usize = 16_384;
/// Encrypted blob bound after DPAPI overhead (must exceed plaintext bound).
pub const MAX_SECRET_BLOB: usize = 96 * 1024;
/// Opaque launch-environment JSON plaintext bound.
pub const MAX_LAUNCH_ENV_PLAINTEXT: usize = 48 * 1024;
const ENTROPY_LABEL: &[u8] = b"DevManagerProviderEnvSecretV1";

fn entropy_bytes(scope: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(ENTROPY_LABEL);
    hasher.update(scope);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Protect a secret for profile-scoped persistence.
pub fn protect_secret_value(plaintext: &str, scope: &[u8]) -> Result<String, SecretCustodyError> {
    if plaintext.is_empty() {
        return Err(SecretCustodyError::Empty);
    }
    if plaintext.len() > MAX_SECRET_PLAINTEXT {
        return Err(SecretCustodyError::TooLarge);
    }
    let entropy = entropy_bytes(scope);
    let blob = dpapi_protect(plaintext.as_bytes(), &entropy)?;
    if blob.len() > MAX_SECRET_BLOB {
        return Err(SecretCustodyError::TooLarge);
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(blob))
}

pub fn reveal_secret_value(
    protected_b64: &str,
    scope: &[u8],
) -> Result<Zeroizing<String>, SecretCustodyError> {
    if protected_b64.is_empty() {
        return Err(SecretCustodyError::Empty);
    }
    if protected_b64.len() > MAX_SECRET_BLOB.div_ceil(3) * 4 {
        return Err(SecretCustodyError::TooLarge);
    }
    let blob = base64::engine::general_purpose::STANDARD
        .decode(protected_b64)
        .map_err(|_| SecretCustodyError::UnprotectFailed)?;
    if blob.len() > MAX_SECRET_BLOB {
        return Err(SecretCustodyError::TooLarge);
    }
    let entropy = entropy_bytes(scope);
    let plain = dpapi_unprotect(&blob, &entropy)?;
    if plain.len() > MAX_SECRET_PLAINTEXT {
        return Err(SecretCustodyError::TooLarge);
    }
    let text = std::str::from_utf8(&plain).map_err(|_| SecretCustodyError::UnprotectFailed)?;
    Ok(Zeroizing::new(text.to_string()))
}

/// Protect an opaque byte payload (e.g. serialized launch environment map).
pub(crate) fn protect_bytes(plaintext: &[u8], scope: &[u8]) -> Result<String, SecretCustodyError> {
    if plaintext.is_empty() {
        return Err(SecretCustodyError::Empty);
    }
    if plaintext.len() > MAX_LAUNCH_ENV_PLAINTEXT {
        return Err(SecretCustodyError::TooLarge);
    }
    let entropy = entropy_bytes(scope);
    let blob = dpapi_protect(plaintext, &entropy)?;
    if blob.len() > MAX_SECRET_BLOB {
        return Err(SecretCustodyError::TooLarge);
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(blob))
}

pub(crate) fn reveal_bytes(
    protected_b64: &str,
    scope: &[u8],
) -> Result<Zeroizing<Vec<u8>>, SecretCustodyError> {
    if protected_b64.is_empty() {
        return Err(SecretCustodyError::Empty);
    }
    if protected_b64.len() > MAX_SECRET_BLOB.div_ceil(3) * 4 {
        return Err(SecretCustodyError::TooLarge);
    }
    let blob = base64::engine::general_purpose::STANDARD
        .decode(protected_b64)
        .map_err(|_| SecretCustodyError::UnprotectFailed)?;
    if blob.len() > MAX_SECRET_BLOB {
        return Err(SecretCustodyError::TooLarge);
    }
    let entropy = entropy_bytes(scope);
    let plain = dpapi_unprotect(&blob, &entropy)?;
    if plain.len() > MAX_LAUNCH_ENV_PLAINTEXT {
        return Err(SecretCustodyError::TooLarge);
    }
    Ok(plain)
}

/// Lossless OsString map codec (UTF-16LE units on Windows, raw bytes elsewhere).
pub(crate) fn encode_os_string_map(
    environment: &std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>,
) -> Result<Vec<u8>, SecretCustodyError> {
    #[derive(serde::Serialize)]
    struct Entry {
        key: Vec<u8>,
        value: Vec<u8>,
    }
    let entries: Vec<Entry> = environment
        .iter()
        .map(|(k, v)| Entry {
            key: os_string_to_bytes(k),
            value: os_string_to_bytes(v),
        })
        .collect();
    let json = serde_json::to_vec(&entries).map_err(|_| SecretCustodyError::ProtectFailed)?;
    if json.len() > MAX_LAUNCH_ENV_PLAINTEXT {
        return Err(SecretCustodyError::TooLarge);
    }
    Ok(json)
}

pub(crate) fn decode_os_string_map(
    bytes: &[u8],
) -> Result<std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>, SecretCustodyError>
{
    #[derive(serde::Deserialize)]
    struct Entry {
        key: Vec<u8>,
        value: Vec<u8>,
    }
    if bytes.len() > MAX_LAUNCH_ENV_PLAINTEXT {
        return Err(SecretCustodyError::TooLarge);
    }
    let entries: Vec<Entry> =
        serde_json::from_slice(bytes).map_err(|_| SecretCustodyError::UnprotectFailed)?;
    let mut out = std::collections::BTreeMap::new();
    for entry in entries {
        if out
            .insert(
                bytes_to_os_string(&entry.key)?,
                bytes_to_os_string(&entry.value)?,
            )
            .is_some()
        {
            return Err(SecretCustodyError::UnprotectFailed);
        }
    }
    Ok(out)
}

fn os_string_to_bytes(value: &std::ffi::OsString) -> Vec<u8> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = value.encode_wide().collect();
        let mut bytes = Vec::with_capacity(wide.len() * 2);
        for unit in wide {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        value.as_bytes().to_vec()
    }
    #[cfg(not(any(windows, unix)))]
    {
        value.to_string_lossy().into_owned().into_bytes()
    }
}

fn bytes_to_os_string(bytes: &[u8]) -> Result<std::ffi::OsString, SecretCustodyError> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        if bytes.len() % 2 != 0 {
            return Err(SecretCustodyError::UnprotectFailed);
        }
        let wide: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();
        Ok(std::ffi::OsString::from_wide(&wide))
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(std::ffi::OsString::from_vec(bytes.to_vec()))
    }
    #[cfg(not(any(windows, unix)))]
    {
        let text = std::str::from_utf8(bytes).map_err(|_| SecretCustodyError::UnprotectFailed)?;
        Ok(std::ffi::OsString::from(text))
    }
}

#[cfg(windows)]
fn dpapi_protect(plaintext: &[u8], entropy: &[u8; 32]) -> Result<Vec<u8>, SecretCustodyError> {
    use windows::core::w;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    let input = CRYPT_INTEGER_BLOB {
        cbData: plaintext.len() as u32,
        pbData: plaintext.as_ptr() as *mut u8,
    };
    let entropy_blob = CRYPT_INTEGER_BLOB {
        cbData: entropy.len() as u32,
        pbData: entropy.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    unsafe {
        CryptProtectData(
            &input,
            w!("DevManagerProviderEnvSecretV1"),
            Some(&entropy_blob as *const CRYPT_INTEGER_BLOB),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|_| SecretCustodyError::ProtectFailed)?;
        if output.pbData.is_null() || output.cbData == 0 {
            return Err(SecretCustodyError::ProtectFailed);
        }
        let copy = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(output.pbData.cast())));
        Ok(copy)
    }
}

#[cfg(windows)]
fn dpapi_unprotect(
    blob: &[u8],
    entropy: &[u8; 32],
) -> Result<Zeroizing<Vec<u8>>, SecretCustodyError> {
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    let mut input = CRYPT_INTEGER_BLOB {
        cbData: blob.len() as u32,
        pbData: blob.as_ptr() as *mut u8,
    };
    let entropy_blob = CRYPT_INTEGER_BLOB {
        cbData: entropy.len() as u32,
        pbData: entropy.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    unsafe {
        CryptUnprotectData(
            &mut input,
            None,
            Some(&entropy_blob as *const CRYPT_INTEGER_BLOB),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|_| SecretCustodyError::UnprotectFailed)?;
        if output.pbData.is_null() || output.cbData == 0 {
            return Err(SecretCustodyError::UnprotectFailed);
        }
        let copy = Zeroizing::new(
            std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec(),
        );
        let _ = LocalFree(Some(HLOCAL(output.pbData.cast())));
        Ok(copy)
    }
}

#[cfg(not(windows))]
fn dpapi_protect(_plaintext: &[u8], _entropy: &[u8; 32]) -> Result<Vec<u8>, SecretCustodyError> {
    Err(SecretCustodyError::Unsupported)
}

#[cfg(not(windows))]
fn dpapi_unprotect(
    _blob: &[u8],
    _entropy: &[u8; 32],
) -> Result<Zeroizing<Vec<u8>>, SecretCustodyError> {
    Err(SecretCustodyError::Unsupported)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn protect_reveal_roundtrip() {
        let scope = b"test-profile/claude";
        let protected = protect_secret_value("super-secret", scope).expect("protect");
        assert!(!protected.contains("super-secret"));
        let revealed = reveal_secret_value(&protected, scope).expect("reveal");
        assert_eq!(revealed.as_str(), "super-secret");
    }

    #[test]
    fn wrong_scope_fails_closed() {
        let protected = protect_secret_value("super-secret", b"scope-a").expect("protect");
        assert!(reveal_secret_value(&protected, b"scope-b").is_err());
    }

    #[test]
    fn os_string_map_roundtrip_lossless() {
        let mut map = std::collections::BTreeMap::new();
        map.insert(
            std::ffi::OsString::from("PATH"),
            std::ffi::OsString::from("C:\\tools"),
        );
        let bytes = encode_os_string_map(&map).unwrap();
        let back = decode_os_string_map(&bytes).unwrap();
        assert_eq!(map, back);
    }
}

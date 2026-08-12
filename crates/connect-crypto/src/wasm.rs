use wasm_bindgen::prelude::*;

use crate::{
    instantiate_noise_channel, ChannelRole, CredentialPurpose, CryptoPrologue,
    NoiseHandshake as NativeNoiseHandshake, NoiseHandshakeMessage, NoiseIdentityBinding,
    NoiseStaticPrivateKey, NoiseStaticPublicKey, NoiseTransport as NativeNoiseTransport,
    SealedFrame, NOISE_FIRST_PAIRING_PATTERN, NOISE_PINNED_DEVICE_PATTERN, PROTOCOL_MAJOR,
    SEALED_NONCE_BYTES, SEALED_TAG_BYTES,
};

const HOLD_ERROR: &str = "connect crypto operation failed";
const INVALID_INPUT: &str = "connect crypto input rejected";

fn redacted_error() -> JsValue {
    JsValue::from_str(HOLD_ERROR)
}

fn invalid_input() -> JsValue {
    JsValue::from_str(INVALID_INPUT)
}

fn fixed<const N: usize>(bytes: &[u8]) -> Result<[u8; N], JsValue> {
    bytes.try_into().map_err(|_| invalid_input())
}

fn purpose(value: u8) -> Result<CredentialPurpose, JsValue> {
    match value {
        1 => Ok(CredentialPurpose::OwnerPairing),
        2 => Ok(CredentialPurpose::TaskInvitation),
        _ => Err(invalid_input()),
    }
}

fn role(value: u8) -> Result<ChannelRole, JsValue> {
    match value {
        0 => Ok(ChannelRole::Initiator),
        1 => Ok(ChannelRole::Responder),
        _ => Err(invalid_input()),
    }
}

/// A Rust-owned Noise XX/IK state machine. Private material stays in native
/// Rust/wasm memory and is never converted to a string or logged.
#[wasm_bindgen]
pub struct WasmConnectHandshake {
    inner: Option<NativeNoiseHandshake>,
}

#[wasm_bindgen]
impl WasmConnectHandshake {
    #[wasm_bindgen(constructor)]
    pub fn new(
        pattern: String,
        first_pairing: bool,
        role_value: u8,
        private_key: Vec<u8>,
        local_public: Vec<u8>,
        expected_remote: Option<Vec<u8>>,
        host_public_id: Vec<u8>,
        device_public_id: Option<Vec<u8>>,
        route_id: Vec<u8>,
        session_id: Vec<u8>,
        purpose_value: u8,
        opened_at_unix: u64,
        direct_reachable: bool,
    ) -> Result<WasmConnectHandshake, JsValue> {
        let private = NoiseStaticPrivateKey::from_vault_bytes(fixed(&private_key)?)
            .map_err(|_| redacted_error())?;
        let local_public = NoiseStaticPublicKey::from_bytes(fixed(&local_public)?)
            .map_err(|_| redacted_error())?;
        let expected_remote = expected_remote.as_deref().map(fixed).transpose()?;
        let host_public_id = fixed(&host_public_id)?;
        let device_public_id = device_public_id.as_deref().map(fixed).transpose()?;
        let route_id = fixed(&route_id)?;
        let session_id = fixed(&session_id)?;
        let purpose = purpose(purpose_value)?;
        let identity = match device_public_id {
            Some(device) => NoiseIdentityBinding::host_device(host_public_id, device),
            None => NoiseIdentityBinding::host(host_public_id),
        };
        let prologue = CryptoPrologue::new(PROTOCOL_MAJOR, purpose, route_id, session_id)
            .map_err(|_| redacted_error())?;
        let channel = instantiate_noise_channel(
            &pattern,
            first_pairing,
            &private,
            local_public,
            expected_remote
                .map(NoiseStaticPublicKey::from_bytes)
                .transpose()
                .map_err(|_| redacted_error())?,
            prologue,
            role(role_value)?,
            identity,
            opened_at_unix,
            direct_reachable,
        )
        .map_err(|_| redacted_error())?;
        Ok(Self {
            inner: Some(channel),
        })
    }

    pub fn write_message(&mut self) -> Result<Vec<u8>, JsValue> {
        let inner = self.inner.as_mut().ok_or_else(redacted_error)?;
        inner
            .write_message()
            .and_then(|message| message.encode())
            .map_err(|_| redacted_error())
    }

    pub fn read_message(&mut self, encoded: &[u8]) -> Result<(), JsValue> {
        let message = NoiseHandshakeMessage::decode(encoded).map_err(|_| redacted_error())?;
        self.inner
            .as_mut()
            .ok_or_else(redacted_error)?
            .read_message(&message)
            .map_err(|_| redacted_error())
    }

    pub fn is_finished(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(NativeNoiseHandshake::is_finished)
    }

    pub fn finish(&mut self) -> Result<WasmConnectTransport, JsValue> {
        let inner = self.inner.take().ok_or_else(redacted_error)?;
        Ok(WasmConnectTransport {
            inner: Some(inner.finish().map_err(|_| redacted_error())?),
        })
    }
}

/// Rust-owned ChaChaPoly/BLAKE2s transport and native sealed-frame codec.
#[wasm_bindgen]
pub struct WasmConnectTransport {
    inner: Option<NativeNoiseTransport>,
}

#[wasm_bindgen]
impl WasmConnectTransport {
    pub fn seal(
        &mut self,
        sequence: u64,
        nonce: Vec<u8>,
        plaintext: Vec<u8>,
    ) -> Result<Vec<u8>, JsValue> {
        let nonce = fixed::<SEALED_NONCE_BYTES>(&nonce)?;
        self.inner
            .as_mut()
            .ok_or_else(redacted_error)?
            .seal(sequence, nonce, &plaintext)
            .and_then(|frame| frame.encode())
            .map_err(|_| redacted_error())
    }

    pub fn open(&mut self, encoded: &[u8]) -> Result<Vec<u8>, JsValue> {
        let frame = SealedFrame::decode(encoded).map_err(|_| redacted_error())?;
        self.inner
            .as_mut()
            .ok_or_else(redacted_error)?
            .open(&frame)
            .map_err(|_| redacted_error())
    }
}

#[wasm_bindgen]
pub fn connect_protocol_major() -> u32 {
    PROTOCOL_MAJOR as u32
}

#[wasm_bindgen]
pub fn connect_noise_pattern(first_pairing: bool) -> String {
    if first_pairing {
        NOISE_FIRST_PAIRING_PATTERN.to_owned()
    } else {
        NOISE_PINNED_DEVICE_PATTERN.to_owned()
    }
}

#[allow(dead_code)]
const _: usize = SEALED_TAG_BYTES;

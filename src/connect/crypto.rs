//! End-to-end Connect channel above a sealed-frame contract.
//!
//! Direct transport is preferred. Relay is an optional fallback that forwards
//! already-sealed frames and cannot open them. Production Noise instantiation
//! remains on HOLD.

use std::fmt;

use super::envelope::{ChannelBinding, ConnectEnvelope, SessionId};
use super::transport::ConnectRoute;
use crate::protocol::{
    instantiate_noise_channel, validate_noise_pattern, ChannelKey, ChannelRole, CredentialPurpose,
    CryptoError, CryptoHold, CryptoHoldReason, CryptoPrologue, ReplayWindow, SealedFrame,
    SourceLevelSealer, CRYPTO_PRODUCTION_READY, MAX_SESSION_AGE_SECS, PROTOCOL_MAJOR,
    SEALED_NONCE_BYTES,
};

pub use crate::protocol::{
    ChannelKey as ConnectChannelKey, ChannelRole as ConnectChannelRole,
    CredentialPurpose as ConnectCredentialPurpose, CryptoError as ConnectCryptoError,
    CryptoHold as ConnectCryptoHold, CryptoHoldReason as ConnectCryptoHoldReason,
    CryptoPrologue as ConnectCryptoPrologue, SealedFrame as ConnectSealedFrame,
    CRYPTO_PRODUCTION_READY as CONNECT_CRYPTO_PRODUCTION_READY,
    NOISE_FIRST_PAIRING_PATTERN as CONNECT_NOISE_FIRST_PAIRING_PATTERN,
    NOISE_PINNED_DEVICE_PATTERN as CONNECT_NOISE_PINNED_DEVICE_PATTERN,
};

#[derive(Clone)]
pub struct EndToEndChannel {
    role: ChannelRole,
    preferred_route: ConnectRoute,
    prologue: CryptoPrologue,
    sealer: SourceLevelSealer,
    send_sequence: u64,
    recv_window: ReplayWindow,
    opened_at_unix: u64,
}

impl EndToEndChannel {
    pub fn open_source_level(
        secret: ChannelKey,
        prologue: CryptoPrologue,
        role: ChannelRole,
        direct_reachable: bool,
        now_unix: u64,
        revoked: bool,
    ) -> Result<Self, CryptoError> {
        if revoked {
            return Err(CryptoError::RevokedKey);
        }
        Ok(Self {
            role,
            preferred_route: preferred_connect_route(direct_reachable),
            prologue,
            sealer: SourceLevelSealer::derive(&secret, prologue, role),
            send_sequence: 0,
            recv_window: ReplayWindow::new(),
            opened_at_unix: now_unix,
        })
    }

    pub fn pair_source_level(
        secret: ChannelKey,
        prologue: CryptoPrologue,
        direct_reachable: bool,
        now_unix: u64,
    ) -> Result<(Self, Self), CryptoError> {
        Ok((
            Self::open_source_level(
                secret.clone(),
                prologue,
                ChannelRole::Initiator,
                direct_reachable,
                now_unix,
                false,
            )?,
            Self::open_source_level(
                secret,
                prologue,
                ChannelRole::Responder,
                direct_reachable,
                now_unix,
                false,
            )?,
        ))
    }

    pub fn open_noise(pattern: &str, first_pairing: bool) -> Result<Self, CryptoHold> {
        match instantiate_noise_channel(pattern, first_pairing) {
            Ok(()) => Err(CryptoHold {
                reason: CryptoHoldReason::ProductionReviewRequired,
            }),
            Err(hold) => Err(hold),
        }
    }

    pub const fn role(&self) -> ChannelRole {
        self.role
    }

    pub const fn preferred_route(&self) -> ConnectRoute {
        self.preferred_route
    }

    pub const fn prologue(&self) -> CryptoPrologue {
        self.prologue
    }

    pub const fn next_send_sequence(&self) -> u64 {
        self.send_sequence.saturating_add(1)
    }

    pub fn seal(
        &mut self,
        envelope: &ConnectEnvelope,
        nonce: [u8; SEALED_NONCE_BYTES],
        now_unix: u64,
    ) -> Result<SealedFrame, CryptoError> {
        self.ensure_fresh(now_unix)?;
        let sequence = self
            .send_sequence
            .checked_add(1)
            .ok_or(CryptoError::SequenceExhausted)?;
        let plaintext = envelope
            .encode()
            .map_err(|_| CryptoError::InvalidEnvelope)?;
        let frame = self.sealer.seal(sequence, nonce, &plaintext)?;
        self.send_sequence = sequence;
        Ok(frame)
    }

    pub fn seal_bytes(
        &mut self,
        plaintext: &[u8],
        nonce: [u8; SEALED_NONCE_BYTES],
        now_unix: u64,
    ) -> Result<SealedFrame, CryptoError> {
        self.ensure_fresh(now_unix)?;
        let sequence = self
            .send_sequence
            .checked_add(1)
            .ok_or(CryptoError::SequenceExhausted)?;
        let frame = self.sealer.seal(sequence, nonce, plaintext)?;
        self.send_sequence = sequence;
        Ok(frame)
    }

    pub fn open(
        &mut self,
        frame: &SealedFrame,
        now_unix: u64,
    ) -> Result<ConnectEnvelope, CryptoError> {
        let plaintext = self.open_bytes(frame, now_unix)?;
        ConnectEnvelope::decode(&plaintext).map_err(|_| CryptoError::InvalidEnvelope)
    }

    pub fn open_bytes(
        &mut self,
        frame: &SealedFrame,
        now_unix: u64,
    ) -> Result<Vec<u8>, CryptoError> {
        self.ensure_fresh(now_unix)?;
        let plaintext = self.sealer.open(frame)?;
        self.recv_window.accept(frame.sequence())?;
        Ok(plaintext)
    }

    pub fn with_send_cursor(mut self, send_sequence: u64) -> Self {
        self.send_sequence = send_sequence;
        self
    }

    pub fn bind_session(&self, binding: ChannelBinding) -> Result<(), CryptoError> {
        if binding.session_id.as_bytes() != self.prologue.session_id() {
            return Err(CryptoError::Authenticity);
        }
        let _ = SessionId::from_bytes(self.prologue.session_id())
            .map_err(|_| CryptoError::Authenticity)?;
        Ok(())
    }

    fn ensure_fresh(&self, now_unix: u64) -> Result<(), CryptoError> {
        if now_unix.saturating_sub(self.opened_at_unix) >= MAX_SESSION_AGE_SECS {
            return Err(CryptoError::SessionExpired);
        }
        Ok(())
    }
}

impl fmt::Debug for EndToEndChannel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EndToEndChannel")
            .field("role", &self.role)
            .field("preferred_route", &self.preferred_route)
            .field("purpose", &self.prologue.purpose())
            .field("next_send_sequence", &self.next_send_sequence())
            .field("production_ready", &CRYPTO_PRODUCTION_READY)
            .finish()
    }
}

pub fn preferred_connect_route(direct_reachable: bool) -> ConnectRoute {
    if direct_reachable {
        ConnectRoute::Direct
    } else {
        ConnectRoute::Relay
    }
}

pub fn connect_prologue(
    purpose: CredentialPurpose,
    route_id: [u8; 16],
    session_id: [u8; 16],
) -> Result<CryptoPrologue, CryptoError> {
    CryptoPrologue::new(PROTOCOL_MAJOR, purpose, route_id, session_id)
}

pub fn lock_noise_pattern(pattern: &str, first_pairing: bool) -> Result<(), CryptoError> {
    validate_noise_pattern(pattern, first_pairing)
}

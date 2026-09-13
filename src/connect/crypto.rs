//! End-to-end Connect channel above a sealed-frame contract.
//!
//! Direct transport is preferred. Relay is an optional fallback that forwards
//! already-sealed frames and cannot open them.
//!
//! Production channels are bounded snow Noise XX/IK sessions. Source-level
//! HMAC sealing exists only for contract tests and is never a production opener.

use std::fmt;

use super::envelope::{ChannelBinding, ConnectEnvelope, SessionId};
use super::transport::ConnectRoute;
use crate::protocol::{
    instantiate_noise_channel, validate_noise_pattern, ChannelKey, ChannelRole, CredentialPurpose,
    CryptoError, CryptoHold, CryptoHoldReason, CryptoPrologue, NoiseCustody, NoiseHandshake,
    NoiseIdentityBinding, NoiseStaticPublicKey, NoiseTransport, ReplayWindow, SealedFrame,
    SourceLevelSealer, CRYPTO_PRODUCTION_READY, MAX_SESSION_AGE_SECS, PROTOCOL_MAJOR,
    SEALED_NONCE_BYTES,
};

pub use crate::protocol::{
    AuthenticatedPeer as ConnectAuthenticatedPeer, ChannelKey as ConnectChannelKey,
    ChannelRole as ConnectChannelRole, CredentialPurpose as ConnectCredentialPurpose,
    CryptoError as ConnectCryptoError, CryptoHold as ConnectCryptoHold,
    CryptoHoldReason as ConnectCryptoHoldReason, CryptoPrologue as ConnectCryptoPrologue,
    NoiseCustody as ConnectNoiseCustody, NoiseHandshake as ConnectNoiseHandshake,
    NoiseHandshakeMessage as ConnectNoiseHandshakeMessage,
    NoiseIdentityBinding as ConnectNoiseIdentityBinding,
    NoiseStaticPrivateKey as ConnectNoiseStaticPrivateKey,
    NoiseStaticPublicKey as ConnectNoiseStaticPublicKey, SealedFrame as ConnectSealedFrame,
    CRYPTO_PRODUCTION_READY as CONNECT_CRYPTO_PRODUCTION_READY,
    NOISE_FIRST_PAIRING_PATTERN as CONNECT_NOISE_FIRST_PAIRING_PATTERN,
    NOISE_PINNED_DEVICE_PATTERN as CONNECT_NOISE_PINNED_DEVICE_PATTERN,
};

enum ChannelCipher {
    SourceLevel(SourceLevelSealer),
    Production(NoiseTransport),
}

pub struct EndToEndChannel {
    role: ChannelRole,
    preferred_route: ConnectRoute,
    prologue: CryptoPrologue,
    cipher: ChannelCipher,
    send_sequence: u64,
    recv_window: ReplayWindow,
    opened_at_unix: u64,
}

impl EndToEndChannel {
    /// No-argument production opener. Fail-closed until vault-backed static
    /// key material is supplied through [`Self::open_production_handshake`].
    pub fn open_production(
        pattern: &str,
        first_pairing: bool,
        revoked: bool,
    ) -> Result<Self, CryptoHold> {
        if revoked {
            return Err(CryptoHold {
                reason: CryptoHoldReason::AlgorithmRejected,
            });
        }
        validate_noise_pattern(pattern, first_pairing).map_err(|_| CryptoHold {
            reason: CryptoHoldReason::AlgorithmRejected,
        })?;
        let _ = CRYPTO_PRODUCTION_READY;
        Err(CryptoHold {
            reason: CryptoHoldReason::MissingStaticKey,
        })
    }

    /// Production snow handshake using supplied custody and public identity.
    pub fn open_production_handshake(
        pattern: &str,
        first_pairing: bool,
        custody: &NoiseCustody,
        expected_remote: Option<NoiseStaticPublicKey>,
        prologue: CryptoPrologue,
        role: ChannelRole,
        identity: NoiseIdentityBinding,
        now_unix: u64,
        direct_reachable: bool,
        revoked: bool,
    ) -> Result<NoiseHandshake, CryptoHold> {
        if revoked {
            return Err(CryptoHold {
                reason: CryptoHoldReason::AlgorithmRejected,
            });
        }
        instantiate_noise_channel(
            pattern,
            first_pairing,
            custody.private(),
            custody.public(),
            expected_remote,
            prologue,
            role,
            identity,
            now_unix,
            direct_reachable,
        )
    }

    pub fn from_noise_transport(transport: NoiseTransport) -> Self {
        Self {
            role: transport.role(),
            preferred_route: preferred_connect_route(transport.direct_reachable()),
            prologue: transport.prologue(),
            opened_at_unix: transport.opened_at_unix(),
            cipher: ChannelCipher::Production(transport),
            send_sequence: 0,
            recv_window: ReplayWindow::new(),
        }
    }

    /// Test/source-level sealed channel. Not production-grade.
    pub(crate) fn open_source_level(
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
            cipher: ChannelCipher::SourceLevel(SourceLevelSealer::derive(&secret, prologue, role)),
            send_sequence: 0,
            recv_window: ReplayWindow::new(),
            opened_at_unix: now_unix,
        })
    }

    pub(crate) fn pair_source_level(
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
        Self::open_production(pattern, first_pairing, false)
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

    pub const fn is_production_grade(&self) -> bool {
        matches!(&self.cipher, ChannelCipher::Production(_))
    }

    /// Authenticated remote peer from a finished production Noise transport.
    /// Source-level channels have no peer identity and return `None`.
    pub fn authenticated_peer(&self) -> Option<ConnectAuthenticatedPeer> {
        match &self.cipher {
            ChannelCipher::Production(transport) => Some(transport.remote_peer()),
            ChannelCipher::SourceLevel(_) => None,
        }
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
        let plaintext = envelope
            .encode()
            .map_err(|_| CryptoError::InvalidEnvelope)?;
        self.seal_bytes(&plaintext, nonce, now_unix)
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
        let frame = match &mut self.cipher {
            ChannelCipher::SourceLevel(sealer) => sealer.seal(sequence, nonce, plaintext)?,
            ChannelCipher::Production(transport) => transport.seal(sequence, nonce, plaintext)?,
        };
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
        let mut probe = self.recv_window;
        probe.accept(frame.sequence())?;
        let plaintext = match &mut self.cipher {
            ChannelCipher::SourceLevel(sealer) => sealer.open(frame)?,
            ChannelCipher::Production(transport) => transport.open(frame)?,
        };
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
            .field("production_grade", &self.is_production_grade())
            .field("protocol_production_ready", &CRYPTO_PRODUCTION_READY)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        ChannelKey, ChannelRole, CredentialPurpose, MAX_HANDSHAKE_MESSAGE_BYTES,
    };

    fn complete_production_pair(
        first_pairing: bool,
        expected_for_ik: bool,
    ) -> (EndToEndChannel, EndToEndChannel) {
        let initiator_keys = NoiseCustody::generate().expect("initiator");
        let responder_keys = NoiseCustody::generate().expect("responder");
        let prologue =
            connect_prologue(CredentialPurpose::OwnerPairing, [1; 16], [2; 16]).expect("prologue");
        let pattern = if first_pairing {
            CONNECT_NOISE_FIRST_PAIRING_PATTERN
        } else {
            CONNECT_NOISE_PINNED_DEVICE_PATTERN
        };
        let initiator_expected = if expected_for_ik {
            Some(responder_keys.public())
        } else {
            None
        };
        let responder_expected = if expected_for_ik {
            Some(initiator_keys.public())
        } else {
            None
        };
        let mut initiator = EndToEndChannel::open_production_handshake(
            pattern,
            first_pairing,
            &initiator_keys,
            initiator_expected,
            prologue,
            ChannelRole::Initiator,
            NoiseIdentityBinding::host([11; 16]),
            5,
            true,
            false,
        )
        .expect("initiator handshake");
        let mut responder = EndToEndChannel::open_production_handshake(
            pattern,
            first_pairing,
            &responder_keys,
            responder_expected,
            prologue,
            ChannelRole::Responder,
            NoiseIdentityBinding::host([12; 16]),
            5,
            true,
            false,
        )
        .expect("responder handshake");
        let first = initiator.write_message().expect("first");
        responder.read_message(&first).expect("read first");
        let second = responder.write_message().expect("second");
        initiator.read_message(&second).expect("read second");
        if first_pairing {
            let third = initiator.write_message().expect("third");
            responder.read_message(&third).expect("read third");
        }
        (
            EndToEndChannel::from_noise_transport(initiator.finish().expect("initiator finish")),
            EndToEndChannel::from_noise_transport(responder.finish().expect("responder finish")),
        )
    }

    #[test]
    fn production_opener_without_custody_fails_closed_and_source_level_is_not_production() {
        assert!(CONNECT_CRYPTO_PRODUCTION_READY);
        assert!(matches!(
            EndToEndChannel::open_production(CONNECT_NOISE_FIRST_PAIRING_PATTERN, true, false),
            Err(CryptoHold {
                reason: CryptoHoldReason::MissingStaticKey
            })
        ));
        assert!(matches!(
            EndToEndChannel::open_production(CONNECT_NOISE_PINNED_DEVICE_PATTERN, false, false),
            Err(CryptoHold {
                reason: CryptoHoldReason::MissingStaticKey
            })
        ));
        assert!(
            EndToEndChannel::open_production("Noise_NN_25519_ChaChaPoly_BLAKE2s", true, false)
                .is_err()
        );
        assert!(
            EndToEndChannel::open_production(CONNECT_NOISE_FIRST_PAIRING_PATTERN, true, true)
                .is_err()
        );

        let secret = ChannelKey::from_bytes([7; 32]);
        let prologue =
            connect_prologue(CredentialPurpose::OwnerPairing, [1; 16], [2; 16]).expect("prologue");
        let source = EndToEndChannel::open_source_level(
            secret,
            prologue,
            ChannelRole::Initiator,
            true,
            10,
            false,
        )
        .expect("source-level test channel");
        assert!(!source.is_production_grade());
        assert!(source.authenticated_peer().is_none());
        assert_eq!(source.preferred_route(), ConnectRoute::Direct);
    }

    #[test]
    fn source_level_keeps_direct_and_relay_routes_distinct() {
        let secret = ChannelKey::from_bytes([7; 32]);
        let prologue =
            connect_prologue(CredentialPurpose::OwnerPairing, [1; 16], [2; 16]).expect("prologue");
        let direct = EndToEndChannel::open_source_level(
            secret.clone(),
            prologue,
            ChannelRole::Initiator,
            true,
            10,
            false,
        )
        .expect("direct");
        let relay = EndToEndChannel::open_source_level(
            secret,
            prologue,
            ChannelRole::Responder,
            false,
            10,
            false,
        )
        .expect("relay");
        assert_eq!(direct.preferred_route(), ConnectRoute::Direct);
        assert_eq!(relay.preferred_route(), ConnectRoute::Relay);
        assert!(!direct.is_production_grade());
        assert!(!relay.is_production_grade());
    }

    #[test]
    fn production_xx_and_ik_pairs_round_trip_and_reject_replay() {
        let (mut xx_a, mut xx_b) = complete_production_pair(true, false);
        assert!(xx_a.is_production_grade());
        assert!(xx_b.is_production_grade());
        assert!(xx_a.authenticated_peer().is_some());
        assert!(xx_b.authenticated_peer().is_some());
        let frame = xx_a.seal_bytes(b"xx-payload", [1; 16], 6).expect("xx seal");
        assert_eq!(xx_b.open_bytes(&frame, 6).expect("xx open"), b"xx-payload");
        assert!(matches!(
            xx_b.open_bytes(&frame, 6),
            Err(CryptoError::Replay { sequence: 1 })
        ));

        let (mut ik_a, mut ik_b) = complete_production_pair(false, true);
        let ik_frame = ik_a.seal_bytes(b"ik-payload", [2; 16], 6).expect("ik seal");
        assert_eq!(
            ik_b.open_bytes(&ik_frame, 6).expect("ik open"),
            b"ik-payload"
        );
        let _ = MAX_HANDSHAKE_MESSAGE_BYTES;
    }
}

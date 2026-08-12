# ADR 0002: Connect end-to-end transport

Date: 2026-08-11

Status: Accepted for source-level contract; **HOLD** for production hosted use.

## Decision

Connect will use one specified Noise protocol, not a new primitive:

- First owner pairing and task-invitation redemption:
  `Noise_XX_25519_ChaChaPoly_BLAKE2s`
- After the device pins the host and the host authorizes the owner device or
  guest grant: `Noise_IK_25519_ChaChaPoly_BLAKE2s`

The prologue binds `DevManagerConnect/v1`, protocol major `1`, route ID,
session ID, and credential purpose. Owner pairing and task invitation use
distinct transcript labels and cannot substitute for one another. There is no
runtime algorithm negotiation or downgrade. Sessions reconnect with a new
handshake before Noise nonce exhaustion or one-hour session age.

Direct transport is preferred. The hosted relay forwards opaque sealed frames
and short-lived routing tickets only.

## Implementation choice

The planned shared implementation is a pinned `snow` core that compiles for
native and `wasm32-unknown-unknown`. Exact `snow` and `wasm-bindgen` versions,
advisory state, WASM size, and handshake latency are **not recorded here**
because this slice cannot edit `Cargo.toml` or run Cargo/network.

Until that crate is pinned and proven, `src/protocol/crypto.rs` exposes:

- locked pattern names and prologue/replay/frame bounds
- `instantiate_noise_channel` / `EndToEndChannel::open_noise`, which return
  `CryptoHold`
- `CRYPTO_PRODUCTION_READY = false`
- a source-level HMAC-SHA256 PRF plus Encrypt-then-MAC sealer using existing
  `hmac`, `sha2`, `zeroize`, and `getrandom` dependencies

The source-level sealer exists only to prove sealed-frame bounds, replay,
purpose isolation, and relay opacity in-process. It is not a Noise
implementation and must not be enabled for hosted production.

Private host keys remain specified for the Windows credential vault. Browser
device keys remain specified for non-exportable WebCrypto wrapping. Those
stores are not implemented in this slice.

## Routing tickets

`src/connect/relay.rs` defines the host-side ticket and opaque-relay contract:

- tickets name public host/device/account IDs, audience, issuance, expiry, and
  a single-use nonce
- tickets are HMAC-signed with a relay signing key; they are not task content
- bind validates origin/audience/expiry/nonce once
- logs and `RelayObservation` carry route ID, size, status, and error class
- the Portal Node service is out of scope; this repository adds no Node runtime

## HOLD list

| ID | Reason | Unblocks |
| --- | --- | --- |
| HOLD-CRYPTO-SNOW | `snow` is not a current Cargo dependency and cannot be added without Cargo/network | Pin exact version after advisory/license review |
| HOLD-CRYPTO-WASM | Native plus `wasm32-unknown-unknown` compile, WASM size, and handshake latency are unproven | Dual-target proof in `crates/connect-crypto` |
| HOLD-CRYPTO-REVIEW | Independent security review of threat model, pairing UX, key storage, and tests has not run | Resolve critical/high findings |
| HOLD-CRYPTO-AEAD | Source-level HMAC stand-in is not ChaCha20-Poly1305 | Replace sealer with reviewed Noise transport |
| HOLD-RELAY-PORTAL | Proprietary Portal ticket/relay/presence service is a separate repository | Isolated Portal worktree; no Node runtime here |

Public hosted Connect must not ship while any HOLD above is open.

## Consequences

- Tests and fixtures in this repository can lock the prologue, sealed-frame,
  replay, and ticket contracts without a new crate.
- Reviewers can reject any attempt to treat the HMAC stand-in as production
  crypto.
- Later work adds `snow` only after Cargo.toml ownership, advisory evidence,
  and the dual-target proof exist.

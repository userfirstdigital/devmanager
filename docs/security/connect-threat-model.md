# Connect end-to-end threat model

Status: source-level contract for Phase 9 Tasks 9.5–9.6. Production hosted
rollout is on HOLD until the Noise implementation, dual-target proof, advisory
scan, and independent review in ADR 0002 are complete.

This model describes the hosted-relay and pairing surface. The local host
remains the only execution authority.

## Assets

- Raw task content: prompts, responses, terminal, browser frames, recordings,
  file bodies, full diffs, and personal prompt-library bodies.
- Long-lived host/device identities and pairing credentials.
- Task-invitation credentials, which are scoped, expiring, and never owner
  authority.
- Routing tickets that authorize a relay bind.

## Actors

- Curious or malicious Connect relay operator with logs, sockets, and ticket
  tables.
- Passive network observer on LAN or hosted path.
- Active attacker who injects, replays, reorders, or truncates frames.
- Stolen routing-ticket holder.
- Offline pairing-code guesser.
- Stolen task-invitation holder.
- Revoked or cloned device, including cleared or copied browser storage.
- Compromised host.
- Denial-of-service against pairing, bind, or relay queues.

## Controls in this slice

- Direct transport is preferred. Relay is optional NAT traversal only.
- Inner Connect envelopes are sealed before they reach relay transport.
- Relay APIs accept `SealedFrame` only. Observations expose route ID, size,
  status, and error class.
- Routing tickets are short-lived, HMAC-bound, single-use at bind, and carry
  public host/device/account IDs plus audience/expiry/nonce. They do not carry
  task bytes or private keys.
- Sealed frames have a nonzero sequence, a bounded replay window, a one-hour
  session age, and a physical-frame size cap.
- Owner-pairing and task-invitation transcripts are distinct labels and cannot
  substitute for one another.
- Runtime algorithm negotiation is rejected. The locked patterns are
  `Noise_XX_25519_ChaChaPoly_BLAKE2s` and `Noise_IK_25519_ChaChaPoly_BLAKE2s`.

## What end-to-end encryption cannot protect

- Route IDs, frame sizes, timing, presence TTL, and connect/disconnect events
  visible to the relay.
- A compromised host, which can read plaintext before seal and after open.
- Endpoint malware, stolen unlocked OS credential material, or an already
  authorized device.
- Invitation or pairing abuse after a valid credential is stolen and redeemed
  before revocation/expiry.
- Denial of service that drops, delays, or fills bounded relay queues.
- Metadata that the owner deliberately enrolls as `ManagedMetadata`.

## Explicit HOLD

The source-level sealer uses existing `hmac`/`sha2`/`zeroize`/`getrandom`
dependencies so tests can prove bounds, replay, purpose isolation, and relay
opacity without adding crates or opening the network. It is not Noise, not
ChaCha20-Poly1305, and not approved for public hosted use. See ADR 0002.

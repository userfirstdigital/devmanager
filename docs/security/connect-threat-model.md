# DevManager Connect threat model

## Scope and security goal

This model covers the Phase 9 direct/hosted Connect path: device pairing,
the host-owned WebSocket, the optional opaque relay, the shared Rust/WASM
Noise leaf, and browser storage of a device identity. The goal is to provide
confidentiality and authenticated integrity for task content and raw terminal,
browser, prompt, and artifact data while retaining the local host as the
authority for every mutation.

The model does not claim that a compromised host, compromised browser runtime,
or compromised provider account can be made safe by E2E transport.

## Assets

- Host and device static private keys.
- Pairing-code and scoped task-invitation authorization.
- Task messages, prompts, child transcripts, terminal scrollback, browser
  content, files, diffs, recordings, and provider input.
- Command/operation IDs, receipts, settlements, and grant/revocation state.
- Host/device public identities and revocation records.

## Adversaries and abuse cases

| Threat | Required protection | Residual exposure |
| --- | --- | --- |
| Curious or malicious relay | Noise-authenticated opaque frames; relay logs contain only bounded route/channel metadata | Route existence, size, timing, liveness, and DoS remain visible |
| Passive network observer | TLS where the transport requires it plus E2E Noise encryption | Traffic analysis and endpoint compromise are outside Noise |
| Frame injection, replay, reorder, or truncation | Handshake authentication, prologue binding, channel sequence, bounded replay window, reconnect/resync rules | An attacker may drop or delay traffic |
| Stolen routing token | Short-lived single-use routing ticket plus E2E device authorization | A valid currently open channel remains usable until revocation/expiry is observed |
| Guessed long-lived pairing code | Rate limits, explicit pairing exchange, host fingerprint/QR verification, and Noise handshake | A weak human code still needs a visible pairing UX and rate-limit proof |
| Stolen task invitation | Invitation-scoped Noise purpose, Task grant, expiry, revocation, and no personal-prompt authority | A redeemed grant can access its allowed Task until it expires or is revoked |
| Revoked device or cloned browser storage | Host-side revocation checked at bind and on durable commands; browser re-pair flow after storage loss | A compromised endpoint can act before revocation reaches it |
| Host compromise | OS credential vault, least-privilege local command authorization, process/resource ownership, and audit receipts | Host compromise can read plaintext before encryption and control the user session |
| Malicious provider output or browser content | Treat all provider/browser/file text as untrusted; bounded rendering and safe external-link handling | A compromised provider can still influence visible content and attempt social engineering |
| Oversized or slow input | Physical/reassembled/page/chunk limits, backpressure, rate limits, and critical-channel priority | Resource exhaustion remains a host/relay availability problem |

## Cryptographic boundary

The only browser cryptographic implementation is the generated
`connect-crypto` WASM leaf. It exposes no secret-to-string conversion and maps
failures to redacted errors. The browser runtime rejects a missing artifact,
unexpected export set, or protocol identity mismatch with the typed
`browser-e2e-transport-held` state; it never downgrades to a legacy plaintext
or JavaScript crypto path.

The prologue includes the product label, protocol major, route/session IDs, and
credential purpose. First pairing uses XX with a verified host identity;
subsequent pinned-device use uses IK. No runtime algorithm negotiation is
accepted. A new handshake is required on reconnect, before nonce exhaustion,
or after one hour.

## Key custody and artifact hygiene

- Native static private keys belong in the Windows credential vault, never in
  `config.json`, `remote.json`, logs, events, manifests, fixtures, or bundles.
- Browser device private identity belongs in non-exportable WebCrypto-backed
  IndexedDB where available. Cleared storage requires visible re-pairing; it
  must not silently create a replacement identity.
- `Build-ConnectCrypto.ps1` requires preinstalled Rust 1.94.0,
  `wasm32-unknown-unknown`, and wasm-bindgen-cli 0.2.114. It uses locked,
  offline Cargo and emits typed HOLD when any prerequisite is missing. It does
  not install packages or fetch dependencies.
- The generated artifact manifest contains only version, protocol, path, byte
  length, and SHA-256 values. A generated artifact is valid only when its
  fixed allowlist and manifest pass the embedded-bundle validator.

## Trust boundaries and authorization

1. The host kernel is authoritative for Task, agent session, device grant,
   provider, process, filesystem, browser, prompt, and approval state.
2. The browser is an untrusted presentation/client endpoint. Network pairing
   does not grant personal prompt access, filesystem access, process control,
   or approval authority.
3. The relay is an untrusted router. It must not deserialize, log, persist, or
   inspect encrypted payload bytes beyond bounded forwarding requirements.
4. Every mutation uses the host command/operation protocol and remains pending
   until a receipt and settlement prove its outcome. Disconnect/retry never
   implies that an uncertain mutation is safe to repeat.

## Verification gates

Before hosted public use, prove all of the following from a clean, isolated
checkout:

- exact pinned native and wasm32 builds;
- shared Rust/browser golden fixture, official Noise vectors, tamper, replay,
  wrong-device, revocation, sequence-exhaustion, reconnect, and corruption
  tests;
- deterministic artifact and web-bundle fingerprints;
- relay capture/log/database scan showing no fixture secrets or raw payloads;
- pairing-code rate limits, origin/CSRF/cache/CSP protections, and revocation;
- independent security review with every critical/high issue resolved.

Until those gates are recorded, release evidence must remain `HOLD` even when
fixture-only transport tests pass.

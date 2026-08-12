# ADR 0002: Rust/WASM end-to-end Connect transport

## Status

Accepted as the Phase 9 transport boundary; browser publication remains
`HOLD` until the generated artifact and an independent security review are
present. This ADR records the implementation boundary, not a claim that
authenticated hosted Connect is complete.

## Context

Connect carries task control, provider input, terminal/browser metadata, and
on-demand raw content through a potentially curious relay. The relay must be
able to route an opaque frame without learning prompt bodies, terminal bytes,
browser content, credentials, or cryptographic private keys. Native DevManager
and the browser must not grow two subtly different cryptographic
implementations.

The shared implementation is the small `crates/connect-crypto` package. It is
compiled natively and, only after its dual-target proof, as `wasm32-unknown-
unknown`. The browser ABI is byte-oriented and exposes bounded handshakes,
sealed frames, and the versioned MessagePack envelope helpers. It does not
expose a private key as text and it does not contain a JavaScript fallback.

## Decision

1. Use `Noise_XX_25519_ChaChaPoly_BLAKE2s` for first owner pairing or scoped
   task-invitation redemption. Use `Noise_IK_25519_ChaChaPoly_BLAKE2s` only
   after the device has pinned the host and the host has authorized that
   device/grant. The pairing and invitation purposes are distinct and cannot
   substitute for one another.
2. Bind `DevManagerConnect/v1`, protocol major, route/session IDs, and
   credential purpose in the prologue. Runtime algorithm negotiation and
   downgrade are rejected. Reconnect performs a fresh handshake before nonce
   exhaustion or the one-hour channel age limit.
3. Keep the host static private key in the OS credential vault. Store the
   browser device private identity through the non-exportable WebCrypto
   IndexedDB design. The relay receives only authenticated opaque frames and
   bounded routing metadata.
4. Pin Rust `1.94.0`, target `wasm32-unknown-unknown`, and
   `wasm-bindgen-cli 0.2.114`. The crate dependency is exact `=0.2.114`.
   `Build-ConnectCrypto.ps1` checks those versions, requires an already
   installed target/CLI, runs Cargo locked and offline, and never installs a
   dependency.
5. Generated files are not source-controlled TypeScript. The build script
   writes them to the ignored `web/src/connect/wasm` directory and emits a
   non-secret SHA-256 manifest. The Vite build copies the fixed allowlist to
   `web/bundle/assets/wasm`; the relative module path remains
   `./wasm/connect_crypto.js`. A clean checkout therefore remains buildable,
   while the browser loader reports the typed
   `browser-e2e-transport-held` state until the artifact exists and its
   protocol identity matches.
6. The artifact manifest is a publication/fingerprint record, not a source
   of cryptographic truth. Rust constants and the shared golden fixture remain
   authoritative for protocol identity. No key, secret, pairing code, or
   transcript material may be emitted into the artifact directory or manifest.

## Consequences

- Native and browser behavior share one reviewed Rust leaf and one golden
  runtime-identity fixture.
- Missing local toolchains fail closed as a typed `HOLD`, rather than causing
  a surprise download or silently selecting another crypto implementation.
- Vite and the Rust embedded-bundle validator understand that the WASM leaf is
  loaded by an explicit runtime path and therefore is not a static index.html
  asset. The validator still rejects an incomplete or unexpected artifact set.
- The generated binary must be produced explicitly by a release workflow; it
  is intentionally absent in source-only worktrees until that workflow runs.
- End-to-end confidentiality does not hide route IDs, frame sizes, timing,
  connection liveness, or denial-of-service effects from a relay.

## Proof still required before hosted use

- Rust/native and wasm32 release builds with the exact pinned versions.
- Rust and browser execution of the same official Noise and DevManager
  transcript/tamper/replay vectors.
- Artifact manifest and bundle fingerprint verification from a clean checkout.
- Relay opacity capture/log/database scan.
- Independent review of this ADR, the threat model, pairing UX, key custody,
  dependency advisories, and all critical/high findings.

## References

- Noise Protocol Framework revision: <https://noiseprotocol.org/noise.html>
- `crates/connect-crypto/src/wasm.rs`
- `web/src/connect/wasmArtifact.ts`
- `scripts/native-next/Build-ConnectCrypto.ps1`

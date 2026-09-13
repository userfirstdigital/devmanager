# Remote Wave 2: Host-owned real-time transport

**Status:** implementation contract; not shipped. Wave 1 does not provide a
multi-device task inbox or remote execution.

**Goal:** the owning host streams canonical task updates and accepts canonical
commands through one encrypted connection, preserving the host's existing
admission, replay, and physical-write guarantees.

**Prerequisite:** the bounded custody/route changes in
`2026-08-27-remote-wave-1.md` pass their grouped gates. No public listener is
enabled by either plan. The full design is
`../specs/2026-08-27-seamless-remote-work-design.md`.

## Settled boundaries

- Own the Connect network writer in `devmanager-host`, not the native shell.
  Forwarding unsolicited messages through the named pipe would mark them
  delivered before the remote socket flushes. Do not mislabel that cursor.
- Retain the existing `HostRequestExecutor`; do not create another command bus.
- A remote task is `(hostPublicId, taskId)`. Selection never changes execution
  ownership. Cached state cannot authorize writes.
- Preserve command IDs across reconnect and let durable host receipts deduplicate.
  Do not replay uncorrelated terminal keystrokes or silently make a new task.
- Treat current cookie-authenticated Noise XX as a bootstrap only. Pin the
  authenticated static key to the durable paired device before enabling normal
  remote execution. Subsequent connections use the existing pinned-device path.

## Task 1: Lossless output schema

Files: `src/connect/{schema,envelope}.rs`, shared fixtures and
`crates/connect-crypto/src/wire.rs`; browser decoder changes in the same wave.

Add separate statically-laned known payload kinds:

- 19 `HostDurableOutput`: exactly `ServerMessage::DurableEvent`, EventReplay grant.
- 20 `HostCriticalOutput`: exactly `ServerMessage::ResyncRequired`, EventReplay grant.
- 21 `HostStreamOutput`: exactly `ServerMessage::Stream`, resource-specific grant
  and privacy validation.

Use a bounded `HostOutputPayload { required_capabilities, message }`. Preserve
subscription IDs, event IDs, durable sequence, stream resource, generation,
payload kind, schema version and bytes. Do not convert these messages to the
existing lossy EventPage, TerminalDelta or generic Resync payloads. Reject all
request/reply/detach variants in output wrappers. Update catalog, capability
negotiation, envelope lane validation and native/WASM golden fixtures together.

## Task 2: Duplex ownership

Files: `src/host/connection.rs`, narrow crate exports and Connect host port.

Add `HostRequestHandle::open_connect_duplex(client_id)` returning
a session owning the bound request handle, `ConnectionOutputPorts`, and
`ConnectionOutputRegistration`. Generate the registration UUIDv7 on the host;
never use the client's envelope connection ID. Internally use `reconnect_from=None`;
durable replay uses the validated event cursor.

Arm registration cleanup before the first transferring await. Reuse
`with_output`, `register_output_for_connection`, and bounded critical/durable/
ephemeral capacities. Dropping the session must unregister and release all
admission owners, including cancellation before registration acknowledgement.

## Task 3: One socket writer

Files: `src/remote/web/bridge.rs` or extracted host transport module, and the
host-owned listener startup/shutdown path.

Reader admits/decodes frames and executes requests without blocking delivery of
unsolicited output. A single writer owns sealing sequence and socket writes.
For each admitted `PrioritizedOutbound`:

1. Check `should_write` and call `prepare_for_write` immediately before encoding.
2. Encode the lossless output with its negotiated capability/privacy contract.
3. Seal, write and flush the physical WebSocket.
4. Only then call `after_successful_write`.

Retain output permits through flush. In-flight successful writes still advance
delivery even if their generation was invalidated while writing. Request replies
and live output share the writer without violating durable terminal fences.
Reader/writer cancellation joins both and drops the registration. Bound socket
handshake, command deadlines and queue capacity; no detached unbounded tasks.

## Task 4: Listener and trust

Move lifetime ownership from native-shell `RemoteHostService` into the durable
host. Do not start two profile listeners or manufacture legacy projections.
Supply canonical snapshot/event/query adapters for the mobile shell before
publishing Connect availability.

The current direct bridge infers TLS from request headers and asserts SAN match
from an untrusted Host field. Replace this before LAN opt-in: actual TLS accept
metadata or an explicitly configured trusted local proxy defines the origin.
The peer address and server configuration, not Origin/Forwarded alone, establish
proxy trust. No plaintext LAN control, implicit firewall changes, self-signed
certificate click-through, or silent trust-store installation.

## Acceptance gate

- Two independent clients subscribe, receive live task events and send commands
  to the same real executor; no duplicate provider run.
- A blocked socket cannot stall another client or the request executor.
- Cancel before registration ack, during encode, during physical write, and
  after flush; verify exact cursor/permit/connection-owner cleanup.
- Replay gap, reordered event, revoked device, stale session generation and
  duplicate command all fail or recover at the intended boundary.
- Actual generated WASM decodes native output fixtures on every new lane.
- Closing the desktop client leaves a release host's authorized task running.
- Only after these gates: wire host-scoped cache/outbox and native fleet UI,
  then real two-PC and phone foreground/background acceptance.

**Not evidence of completion:** exported methods without production callers,
mocked crypto, a successful local named-pipe query, screenshots of cached chats,
or a relay connection without task updates and command receipts.

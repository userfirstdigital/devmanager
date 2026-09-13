# Remote desktop and phone work

The durable `devmanager-host` owns each task, provider, workspace and conversation
journal. Remote clients control that owner; they never launch a local replacement
because a connection is unavailable. A provider conversation ID is not a PTY ID.

## Implementation and acceptance status

This is the native-host remote path under development, not a claim that the
complete multi-PC/WAN product has shipped. See
[the design](superpowers/specs/2026-08-27-seamless-remote-work-design.md)
and [current integration wave](superpowers/plans/2026-08-27-remote-wave-5.md).

Verified with an isolated real host:

- HTTP pairing, published host identity, Noise, assigned client identity,
  canonical snapshots and native encrypted reconnect.
- Two browser viewers receiving the same real Codex prompt/reply and correlated
  provider conversation ID from a single Send click (no extra terminal Enter).
- Cached conversations and unsent drafts surviving owner outage and page reopen,
  with sending disabled until fresh authority is established.
- A phone terminal control answering a provider's pre-conversation startup
  prompt through the same runtime/epoch-fenced command path.
- Canonical browser device enrollment and same-owner process restart: automatic
  reconnect without pairing, retained history/draft, and a successful follow-up
  in the same provider conversation.
- Two encrypted host connections with the same raw task ID: a command captured
  for A still executes on A after selecting B; stopping/reconnecting A leaves B
  usable; A's stale admission is rejected after reconnect.
- The production trusted-host list/forget operations reload both identities,
  reject forgetting a changed identity, and forget exactly A without altering B.
- A TLS/WSS cross-origin browser route with pinned Noise identity, one-use route
  tickets, retained client identity on resume, and rejected wrong-Origin/reused
  tickets. The temporary CA was scoped to the test client, not OS trust.

The latest consolidated browser gate passed 586 tests (one skipped), plus the
production bundle build. Native workspace integration remains in progress;
these results are not a final union or device-acceptance gate.

Still required: interrupted first-enrollment recovery, live revocation acceptance,
unified native multi-host workspace, Connect
WAN routing and physical two-PC/phone acceptance. Loopback does not prove LAN
certificate trust, mobile sleep behavior or public WAN performance.

## Direct access without Connect

Use native **Settings → Remote access**. Read-only status does not start a
listener. Enabling is an explicit local action; a browser cannot reconfigure
listeners or establish host custody.

- Local development uses the exact loopback HTTP endpoint printed by its host.
- LAN access requires a listen address, port, advertised HTTPS origin,
  certificate and private-key files. The certificate must match the hostname
  and be trusted by the connecting device.
- Open that HTTPS origin and enter the pairing code in the form. Secrets belong
  in the POST body, not URLs, history or referrers.
- Keep the same origin after pairing. Cookies and browser custody are
  origin-bound; changing a hostname is not an automatic trust migration.

Plaintext LAN control, certificate-validation bypass and trusting arbitrary
`Forwarded` headers are not supported shortcuts. DevManager does not silently
change the firewall or install certificate trust. Do not expose the listener
directly to the public internet. Outside-home routing uses the separately
configured Connect service, not port forwarding.

## Phone behavior

The inbox has active tasks, compact Done and an archive button opening a separate
list. Task actions provide Done, explicit Restore, Rename, Archive and confirmed
Delete after archival. These use the same persisted, host-qualified command outbox
as Send; accepting a metadata action never clears the message draft.

The isolated real-browser lifecycle check exercised Done, opening Done without
restoring, explicit Restore, Rename, Archive, archive-list opening and confirmed
Delete. The follow-up UI corrections expand Done initially, derive archive
progress from canonical state, and close a deleted task only when its owning
projection confirms deletion. Those corrections passed the focused UI tests
and the rebuilt-host real-browser recheck: Done was immediately visible,
completed Archive no longer claimed to be closing, and confirmed Delete
returned to the inbox without a writable deleted-task view.

Opening or renaming Done does not restore it. Sending a message restores it atomically on
the host without an extra client-side reopen request. Archived and closing tasks
require explicit Restore before sending. Selected tasks keep their semantic history
and draft in place while the connection recovers. Terminal queries read the
exact owner's current screen. Fixed terminal keys can answer startup prompts;
raw terminal output is not stored in the durable browser cache.

The composer uses an HTML text area for selection, paste, dictation and the
software keyboard. Over HTTPS, Safari's **Share → Add to Home Screen** installs
the PWA. Its start route is `/tasks?source=pwa`.

Mobile operating systems may suspend sockets while locked. Seamlessness means
immediate cached presentation followed by automatic authenticated catch-up,
not uninterrupted background networking. Fresh input needs current host
authority; offline drafts remain editable.

## One durable command path

Direct and future WAN clients use canonical commands/queries through shared
Rust/WASM Noise. A pairing cookie is bootstrap admission, not a canonical
device credential.

Before sending, the browser persists the exact host, client and command ID.
Lost acknowledgment means **uncertain**, not permission to invent a new command.
Recovery queries that owner's durable receipt. Runtime generation, action epoch,
task revision and conversation identity stay host-validated fences.

Acceptance, physical PTY delivery and observed provider execution are separate
facts. Verify exact submitted text and a semantic response, not just a receipt.
The managed Codex TUI remains the provider; this path does not start another
app-server conversation or infer identity from transcript ordering.

## Cache and upgrades

IndexedDB stores bounded host-scoped metadata, semantic history, drafts, replay
position and exact pending commands. Cache is presentation-only and never grants
authority. A host restart must not discard drafts just because its runtime changed.

The service worker caches static assets only, not authenticated API responses,
raw terminals, file bodies or provider credentials. Private Noise bytes are
encrypted with a non-extractable WebCrypto wrapping key. Ordinary upgrades
must not rotate custody; corruption requires visible repair, not silent pairing.

Bundle activation waits while drafts or uncertain commands make it unsafe.
The embedded fingerprint covers web source, including tests: finish web edits
before building the bundle and compiling Rust.

## Development verification

Use an isolated profile and target; never restart the installed daily app.
Root `AGENTS.md` governs process, persistence and Cargo isolation.

```powershell
npm --prefix web test
npm --prefix web run build
# With an explicitly validated isolated CARGO_TARGET_DIR:
cargo check --locked --lib --bins --tests
cargo test --lib -- --test-threads=1
cargo build --locked --bin devmanager-host --example remote-host-smoke
```

Run the example from that target's `debug/examples` directory. It creates an
empty temporary workspace, profile, identity and loopback listener. Add
`--with-codex` only for a deliberate real-provider smoke. Enter stops it;
`restart` joins its exact owned process tree and restarts the same profile.
Neither operation targets the installed app.

`remote-native-ui-smoke` prepares two real remote hosts sharing a task UUID,
enrolls both in a third temporary native profile, and opens the canonical desktop
shell. It depends on native trusted-host startup integration; fixture preparation
is not an acceptance result. Build it with `devmanager-host`, copy that exact host
binary beside the example in the isolated target's `debug/examples` directory,
then run the example. Default mode starts no provider. Explicit `--with-codex`
starts providers for deliberate chat checks but sends no prompts automatically.
Closing its window tears down only its owned fixtures.

Before release, exercise independent devices, owner restart, lock/wake, network
loss, concurrent viewing, lost receipts, revocation and two owners with the same
task UUID. Measure cached paint, catch-up and rendering separately from provider
thinking. Confirm installed configuration hashes and process identity unchanged.

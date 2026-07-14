# Native Session Reconnect Design

## Problem

Opening any session in the mobile web UI currently makes the browser's atomic `Resume` request subscribe to that session's raw PTY stream. The subscription happens even when React renders a native semantic view. The host then serializes a large `sessionBootstrap` containing both an authoritative terminal screen snapshot and redundant replay bytes. In the live application this reached roughly 1.5 MB, after which the WebSocket closed and the UI entered a reconnect loop.

Claude and Codex have a second problem: their full-screen TUIs enable mouse reporting as a standing capability. The semantic projector currently interprets that capability as proof that grid interaction is required, so the first experience is forced to the raw terminal instead of the native transcript.

## Goals

- Opening a native session must request semantic history without subscribing to PTY output.
- Raw PTY traffic must exist only while the raw terminal view is mounted.
- Returning from another app must resume both native and explicitly opened raw views automatically.
- Claude and Codex must remain native-first even when their TUIs enable alternate-screen or mouse-reporting modes.
- The raw terminal fallback must remain available through the existing user-controlled mode button.
- A terminal screen snapshot must not also transport replay bytes that the browser will discard.

## Architecture

`desiredSessionKey` remains the single authority for route restoration, semantic replay, attention acknowledgement, and native focus. A new optional `rawSessionId` in the atomic `Resume` request becomes the single authority for raw PTY subscription. The server clears and rebuilds its raw subscription set from `rawSessionId` on every resume, so reconnect remains automatic and does not require legacy subscribe/focus frames.

The web store keeps `activeSessionKey` independent from `rawTerminal.activeStreamSessionId`. Selecting a session updates semantic focus and the visible-session marker used for attention and notification suppression. Mounting `RawTerminalView` additionally sets the raw stream ID and wakes the client; unmounting it clears the raw stream ID and wakes the client. This means switching back to native mode immediately stops raw output fanout without losing semantic focus.

For raw attachment, `TerminalView` already renders `screenSnapshotToAnsi(screen)` whenever the snapshot has non-zero dimensions and ignores `bootstrap.bytes`. The host will therefore encode an empty `replayBase64` when a valid screen snapshot exists. Replay bytes remain a fallback only when no usable screen snapshot exists.

Claude and Codex terminal modes are presentation details rather than automatic raw-mode requirements. Their semantic metadata will remain `rawRequired = false`; users can still explicitly pin the raw terminal. Non-AI shells retain the existing alternate-screen and mouse-reporting detection.

## Protocol and Compatibility

- Add nullable `rawSessionId` to Rust `ResumeRequest` and TypeScript `ResumeContext`.
- Keep `deny_unknown_fields`; old clients without the field deserialize to `None` through `#[serde(default)]`.
- Increment `WEB_PROTOCOL_VERSION` because the bundled client and host must agree on the new atomic resume shape.
- Validate `rawSessionId` with the existing session-ID bounds before changing subscriptions.
- Do not revive legacy `subscribeSessions`, `focusSession`, or manual resume buttons.

## Verification

- Rust wire and bridge tests prove semantic-only resume creates no PTY subscription and explicit raw resume creates exactly one.
- Rust encoding tests prove valid screen snapshots omit replay bytes while snapshot-less bootstraps retain fallback replay.
- Rust presentation tests prove AI mouse reporting stays semantic and non-AI mouse reporting remains raw.
- Web store tests prove selecting a session sends `rawSessionId: null`, mounting raw sends the PTY ID, and unmounting clears it.
- Full web tests, typecheck/build, serialized Rust tests, formatting, and Clippy run before integration.
- The replacement binary is installed locally and the live browser verifies stable native Claude/server sessions plus explicit raw-terminal fallback.

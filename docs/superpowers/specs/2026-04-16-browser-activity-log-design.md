# Browser Activity Log Design

## Context

DevManager's current web trust model stores browser access as a list of paired
records under `WebConfig.paired_clients`. The desktop UI currently surfaces
that list as a paired-browser count plus per-browser revoke actions.

This model has two problems:

1. The count is misleading because the host currently creates a new web
   `client_id` on every successful `/pair`, so the same browser can accumulate
   multiple saved records over time.
2. The data is not very useful to a human because a raw browser count does not
   answer "what connected?" or "when did it connect?"

The approved direction is to replace the raw count/list UI with a recent
browser activity log, while also introducing a stable browser identity so the
host can dedupe repeat pairings from the same browser install.

## Goals

- Deduplicate repeat pairings from the same browser install.
- Capture as much useful browser metadata as browsers can reliably provide.
- Replace the misleading paired-browser count with a recent persisted activity
  log in the desktop UI.
- Capture timestamps and IP addresses for browser activity events.
- Preserve a future path to rename or invalidate specific browser identities,
  or reset all browser access, without redesigning storage again.

## Non-Goals

- Add per-browser revoke UI in this change.
- Add "real device name" discovery from the browser; browsers generally cannot
  provide that reliably.
- Store an unbounded audit trail.
- Change the meaning of the browser pairing token. It should continue to gate
  only future pairings, not already-paired browsers.

## Product Decisions

- The Browser Access UI becomes a **log-first view** instead of a browser count.
- Browser identities are deduped using a **stable browser install ID** stored
  in the browser's `localStorage`.
- Browser names are **auto-detected first** and may be renamed later in the
  desktop app; rename UI is not part of this change.
- The host persists a **rolling recent log** of browser activity across
  restarts, capped to a recent window such as the last 100 events.
- The host captures **IP address** on pairing and on authenticated connection
  events.

## Browser Identity Model

### Stable Browser Install ID

On first load, the web app generates a random stable browser install ID and
stores it in `localStorage`.

This ID is sent during pairing and becomes the stable host-side identity key
for that browser install. The same browser install should keep the same ID
until its site storage is cleared.

### Host-Side Identity Record

Extend `PairedWebClient` so it becomes a real persisted browser identity
record rather than just a countable pair event. The record should include:

- `client_id`: stable browser identity used for cookies/auth
- `nickname`: optional user-editable label for a future desktop rename feature
- `label`: current display label; for now this is the auto-detected label
- `issued_at_epoch_ms`: first successful pair time
- `last_seen_epoch_ms`: last authenticated activity time
- `last_seen_ip`: last observed IP address
- `user_agent`: raw or normalized browser user-agent string snapshot
- `browser_family`: best-effort parsed browser family, e.g. `Chrome`, `Safari`
- `browser_version`: best-effort parsed major version or short version string
- `os_family`: best-effort parsed OS family, e.g. `Windows`, `iOS`, `Android`
- `device_class`: best-effort class such as `desktop`, `phone`, `tablet`,
  `unknown`

The host should upsert by stable browser ID instead of always pushing a new
record. That is the core dedupe fix.

### Display Label Rules

Display label priority:

1. `nickname` when present
2. auto-generated label derived from parsed metadata, e.g.:
   - `Windows Chrome`
   - `iPhone Safari`
   - `Android Chrome`
3. final fallback: `Browser`

The system must not claim to know a true machine name or personal device name
unless the user explicitly provides one later via rename UI.

## Pairing And Authentication Flow

### Pairing

The current `/pair` flow remains the entry point, but the browser now also
supplies its stable browser install ID. The host:

1. validates the browser pairing token as it does today
2. derives browser/device metadata from request headers (primarily user agent)
3. upserts the corresponding `PairedWebClient` record by stable browser ID
4. records a `paired` activity event
5. signs the remembered auth cookie for that stable browser identity

For a browser that pairs again later:

- the existing identity record is updated
- a new record is **not** appended
- the `paired` event is still logged because the action happened

### Authenticated Requests

Existing authenticated access remains cookie-based. The cookie value should map
to the stable browser identity record.

Authenticated requests such as `/api/me` should continue updating
`last_seen_epoch_ms`. They may also update `last_seen_ip` opportunistically.

### Connection Events

When the browser establishes its authenticated WebSocket session, the host
should append a browser activity event when that connection represents a real
session start, not a low-level ping.

For this change, the log should support at least:

- `paired`
- `connected`
- `reconnected`

Exact naming can be implementation-specific, but the intent is:

- `paired`: first-time or repeat successful pairing action
- `connected`: authenticated browser session established
- `reconnected`: authenticated browser session re-established after prior use

The log should remain human-readable and should not record every ping, cookie
refresh, or noisy internal retry.

## Activity Log Model

Add a persisted rolling browser activity log to `WebConfig`, for example:

- `activity_log: Vec<BrowserActivityEvent>`

Each event should capture:

- stable browser ID
- event kind
- timestamp
- IP address
- label snapshot used for display at the time
- best-effort metadata snapshot sufficient to render the row even if the
  browser identity later changes

### Retention

Persist only a recent rolling window such as the last 100 events.

When appending a new event, trim older entries beyond the cap.

This makes the feature useful as an audit/history view without turning
`remote.json` into an unbounded append-only log.

## UI Changes

### Browser Access Section

Keep the existing Browser Access controls for:

- enabling browser access
- copying/generating the browser pairing token
- showing listener URL / listener error
- showing the plain-HTTP warning

Remove:

- the raw paired-browser count

Replace that area with:

- a recent browser activity list

Each row should show:

- primary line: nickname if present, else auto label
- secondary line: event kind + IP + timestamp
- optional tertiary detail: browser/device metadata when helpful

Example rows:

- `WORK Chrome` - `reconnected from 192.168.0.14 - 2 min ago`
- `iPhone Safari` - `paired from 192.168.0.22 - Apr 16 10:42 PM`

### Trusted Access Section

Remove the browser-specific paired-browser list and per-browser revoke actions
from the desktop settings UI in this change. The section may continue to show
trusted desktop clients, but browsers should no longer be represented there as
individual action rows.

### Future Actions

The data model must support future UI for:

- rename browser identity
- invalidate one browser identity
- reset all browser access

Those actions are not part of this implementation, but the storage shape
should not block them.

## Metadata Collection Strategy

### What We Can Capture Reliably

- stable browser install ID from `localStorage`
- IP address from the server-side socket/request context
- user-agent string from request headers
- best-effort browser/OS/device classification from the user-agent

### What We Cannot Promise

- a true machine hostname
- a user-chosen device name like `WORK`
- a reliable human owner name like `Robin`

Those should only appear once a future rename feature exists.

## Compatibility And Migration

### Existing Paired Browsers

Existing saved paired-browser records should continue to work. This change must
not invalidate existing cookies merely because the storage model has richer
metadata.

Older records that lack new metadata fields should load via serde defaults and
continue authenticating normally.

### Pairing Token Semantics

Regenerating the browser pairing token should continue to affect **future**
pairings only. It should not invalidate already-paired browsers.

Future "reset browser access" behavior, when implemented, should clear browser
trust and rotate the cookie secret instead of relying only on pairing-token
rotation.

## Testing

### Rust Tests

Add focused regressions for:

- same browser install pairing twice updates one identity record instead of
  appending duplicates
- activity log persists across reload/restart
- recent log is trimmed to the configured maximum size
- regenerating the browser pairing token does not break already-paired browser
  cookies
- browser IP and metadata snapshots are persisted on pairing and/or connect

### Web Tests

Add focused tests for:

- stable browser install ID is created once and reused from `localStorage`
- pairing requests include the stable browser identity signal required by the
  host
- auto label fallback remains stable when metadata is partial or missing

### UI Tests

Update settings/UI tests so they assert:

- Browser Access shows recent browser activity instead of a paired-browser
  count
- Trusted Access no longer renders browser revoke rows
- browser activity rows display human-readable labels and timestamps

## Recommended Implementation Order

1. Introduce stable browser install ID generation/storage in the web app.
2. Extend `PairedWebClient` and `WebConfig` with richer browser identity and
   activity log fields.
3. Update `/pair` to upsert by stable browser identity and append `paired`
   events.
4. Update authenticated connect flow to append connection activity events and
   refresh `last_seen` / IP metadata.
5. Replace Browser Access count/list UI with the activity log.
6. Add/adjust regression coverage.

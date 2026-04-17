# Browser Activity Log Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the misleading paired-browser count with a deduped browser identity model and a recent browser activity log that records meaningful browser/device details and IP timestamps.

**Architecture:** Add a stable browser install ID in the web app, persist richer browser identity and rolling activity log data in the host config, upsert pairings by stable browser ID instead of appending duplicates, and replace the Browser Access paired-browser count/list UI with a recent activity log. Keep cookie-based auth semantics intact so existing paired browsers continue to work.

**Tech Stack:** Rust (`axum`, serde, existing remote/web host code), React + TypeScript + Zustand, Vitest, Rust unit tests

---

### Task 1: Browser Identity Data Model

**Files:**
- Modify: `src/remote/web/auth.rs`
- Modify: `src/remote/web/mod.rs`
- Modify: `src/remote/mod.rs`
- Test: `src/remote/web/mod.rs`

- [ ] **Step 1: Write the failing Rust tests**

```rust
#[test]
fn pair_handler_reuses_existing_browser_identity_for_same_install_id() {
    // Pair twice with the same browser install id and assert that
    // `paired_clients.len()` stays at 1 while timestamps/metadata update.
}

#[test]
fn web_activity_log_trims_to_recent_limit() {
    // Seed more than the retention limit and assert only the newest entries remain.
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test pair_handler_reuses_existing_browser_identity_for_same_install_id && cargo test web_activity_log_trims_to_recent_limit`

Expected: FAIL because stable browser IDs and activity log storage do not exist yet.

- [ ] **Step 3: Write minimal implementation**

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct BrowserActivityEvent {
    pub browser_id: String,
    pub event_kind: String,
    pub label: String,
    pub ip_address: Option<String>,
    pub event_at_epoch_ms: Option<u64>,
    pub browser_family: Option<String>,
    pub browser_version: Option<String>,
    pub os_family: Option<String>,
    pub device_class: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub struct PairedWebClient {
    pub client_id: String,
    pub browser_install_id: String,
    pub nickname: Option<String>,
    pub label: String,
    pub issued_at_epoch_ms: Option<u64>,
    pub last_seen_epoch_ms: Option<u64>,
    pub last_seen_ip: Option<String>,
    pub user_agent: Option<String>,
    pub browser_family: Option<String>,
    pub browser_version: Option<String>,
    pub os_family: Option<String>,
    pub device_class: Option<String>,
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test pair_handler_reuses_existing_browser_identity_for_same_install_id && cargo test web_activity_log_trims_to_recent_limit`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/remote/web/auth.rs src/remote/web/mod.rs src/remote/mod.rs
git commit -m "feat: add persisted browser identity model"
```

### Task 2: Pairing And Authentication Event Flow

**Files:**
- Modify: `src/remote/web/mod.rs`
- Modify: `src/remote/web/bridge.rs`
- Test: `src/remote/web/mod.rs`
- Test: `src/remote/web/bridge.rs`

- [ ] **Step 1: Write the failing Rust tests**

```rust
#[test]
fn pair_handler_records_browser_activity_with_ip_and_metadata() {
    // Pair from a known address and assert the activity log contains
    // a `paired` event with IP and derived label metadata.
}

#[test]
fn authenticated_browser_connection_records_connect_event() {
    // Register an authenticated browser connection and assert a
    // `connected` or `reconnected` activity event is appended once.
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test pair_handler_records_browser_activity_with_ip_and_metadata && cargo test authenticated_browser_connection_records_connect_event`

Expected: FAIL because pair/connect events are not logged yet.

- [ ] **Step 3: Write minimal implementation**

```rust
fn upsert_paired_web_client(/* browser install id + metadata */) -> String {
    // Reuse an existing browser identity by install id or create a new
    // one for first-time pairings.
}

fn append_browser_activity_event(/* event kind + metadata */) {
    // Push event, then trim to the rolling limit.
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test pair_handler_records_browser_activity_with_ip_and_metadata && cargo test authenticated_browser_connection_records_connect_event`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/remote/web/mod.rs src/remote/web/bridge.rs
git commit -m "feat: log browser pairing and connection activity"
```

### Task 3: Stable Browser Install ID In The Web App

**Files:**
- Create: `web/src/lib/browserIdentity.ts`
- Modify: `web/src/components/PairingGate.tsx`
- Test: `web/src/components/PairingGate.test.tsx`

- [ ] **Step 1: Write the failing web tests**

```ts
it("reuses one browser install id from localStorage", () => {
  // Call helper twice and expect the same value to be returned.
});

it("includes browser identity metadata in the pairing request", async () => {
  // Mock fetch and assert `/pair` receives the browser install id.
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `npx vitest run src/components/PairingGate.test.tsx`

Expected: FAIL because no stable browser install ID helper exists yet.

- [ ] **Step 3: Write minimal implementation**

```ts
const BROWSER_INSTALL_ID_KEY = "devmanager.browserInstallId";

export function getBrowserInstallId(): string {
  const existing = localStorage.getItem(BROWSER_INSTALL_ID_KEY);
  if (existing) return existing;
  const created = crypto.randomUUID();
  localStorage.setItem(BROWSER_INSTALL_ID_KEY, created);
  return created;
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `npx vitest run src/components/PairingGate.test.tsx`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add web/src/lib/browserIdentity.ts web/src/components/PairingGate.tsx web/src/components/PairingGate.test.tsx
git commit -m "feat: persist stable browser install identity"
```

### Task 4: Desktop Browser Access UI

**Files:**
- Modify: `src/workspace/mod.rs`
- Modify: `src/app/mod.rs`
- Test: `src/workspace/mod.rs`

- [ ] **Step 1: Write the failing UI tests**

```rust
#[test]
fn browser_access_section_shows_recent_browser_activity_instead_of_count() {
    // Assert the Browser Access section renders activity rows and
    // does not rely on a raw paired-browser count.
}

#[test]
fn trusted_access_section_no_longer_lists_browser_revoke_rows() {
    // Assert browser entries are absent from Trusted Access.
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test browser_access_section_shows_recent_browser_activity_instead_of_count && cargo test trusted_access_section_no_longer_lists_browser_revoke_rows`

Expected: FAIL because the UI still renders paired-browser count/list content.

- [ ] **Step 3: Write minimal implementation**

```rust
fields.push(FormField::info(
    "Recent browser activity",
    format!("{} recent events", draft.remote_web_activity_log.len()),
    Some("Recent browser pair/connect activity with timestamps and IPs.".to_string()),
));
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test browser_access_section_shows_recent_browser_activity_instead_of_count && cargo test trusted_access_section_no_longer_lists_browser_revoke_rows`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add src/workspace/mod.rs src/app/mod.rs
git commit -m "feat: replace browser counts with activity log ui"
```

### Task 5: Focused Verification Sweep

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Add a short local note if needed**

```md
Browser Access now shows recent browser activity instead of a paired-browser count.
```

- [ ] **Step 2: Run focused Rust verification**

Run: `cargo test pair_handler_reuses_existing_browser_identity_for_same_install_id && cargo test pair_handler_records_browser_activity_with_ip_and_metadata && cargo test authenticated_browser_connection_records_connect_event && cargo test browser_access_section_shows_recent_browser_activity_instead_of_count`

Expected: PASS

- [ ] **Step 3: Run focused web verification**

Run: `npx vitest run src/components/PairingGate.test.tsx src/store/index.test.ts`

Expected: PASS

- [ ] **Step 4: Run lints/build checks**

Run: `cargo fmt --check && cargo clippy --all-targets --all-features -- -A warnings`

Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add README.md
git commit -m "docs: note browser activity log behavior"
```

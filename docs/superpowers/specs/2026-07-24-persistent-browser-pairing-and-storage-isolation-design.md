# Persistent Browser Pairing and Storage Isolation Design

**Date:** 2026-07-24
**Status:** Approved design pending written-spec review
**Scope:** Keep the browser invite code stable until the user changes it, preserve paired browsers across releases, prevent development and test binaries from touching the installed DevManager profile, and recover the currently running production identity before restart.

## 1. Problem

DevManager currently rotates the browser pairing token after every successful pairing. The user wants one stable code that may be reused to pair additional trusted devices and changes only through an explicit security action.

The more severe defect is storage isolation. Rust web tests use the same process-global `DEVMANAGER_PROFILE` mechanism as development builds. Two slash-command route tests pair a fake browser without installing a test profile guard. Their persistence path therefore resolved to the production `%APPDATA%\com.userfirst.devmanager\remote.json`, replacing the real host identity with the `slash-route` / `slash-browser` fixture.

The installed DevManager process started before that overwrite and still holds the real host identity, cookie-signing key, and paired clients in memory. Restarting before recovery would load the fixture identity and require pairing again.

Startup compounds the risk by using `load_remote_machine_state().unwrap_or_default()`. Any parse, ACL, or I/O failure silently creates a new host identity in memory. A later save can then replace recoverable state with the new default.

## 2. Decisions

### Stable invitation code

- A successful browser pairing does not change `WebConfig.pairing_token`.
- The same code can pair multiple browser installations until the user changes it.
- **Generate new code** changes only the invitation code. Existing paired browsers, cookies, push subscriptions, and activity remain valid.
- **Reset browser access** revokes all paired browsers, clears browser push state and browser activity, rotates the cookie-signing key, and generates a new invitation code.
- Application updates, restarts, web-bundle updates, and ordinary settings saves do not rotate either the invitation code or the cookie-signing key.

The invitation code remains an authentication secret. DevManager continues to apply rate limiting and does not place the code in logs, browser storage, runtime snapshots, or diagnostic output.

### One storage scope per binary purpose

Storage selection becomes fail-safe by default:

- Installed release build with no explicit profile: production
  `%APPDATA%\com.userfirst.devmanager`
- Development/debug build with no explicit profile: isolated development profile
  `%APPDATA%\com.userfirst.devmanager-dev-debug`
- `dev-watch.ps1`: its existing explicit isolated profile
  `%APPDATA%\com.userfirst.devmanager-dev-watch`
- Rust unit-test binary: a process-unique directory under the operating-system temporary directory, never `%APPDATA%\com.userfirst.devmanager`
- Explicit `DEVMANAGER_PROFILE`: retains the existing sanitized named-profile behavior, except a test binary remains rooted under its temporary test namespace

All application persistence continues to resolve through `persistence::app_config_dir()`. Tests may select subprofiles for isolation from one another, but removing a test profile can never fall back to production.

The storage resolver exposes a pure scope-to-path function so production naming can be tested without making production the active test path.

### Remote-state load failures

The app must not silently replace a failed remote-state load with `RemoteMachineState::default()`.

- Missing `remote.json` remains a valid first-run condition and creates a new identity.
- An existing file that fails ACL repair, read, or parse produces a visible startup diagnostic.
- The failed file is not overwritten automatically.
- Browser and native remote hosting remain disabled for that run until the state is repaired. No automatic destructive reset is performed.
- Workspace loading continues independently; a remote-state problem must not erase projects or sessions.

This separates “new installation” from “existing identity could not be read.”

## 3. Recovery of the live production identity

Recovery must happen before restarting or updating the installed app:

1. Record a redacted fingerprint of the contaminated disk state.
2. Use the already-running installed DevManager to invoke **Generate new browser code** once. In the currently installed version, this persists the service’s full in-memory host configuration without revoking paired browsers.
3. Re-read `remote.json` and verify:
   - `serverId` is no longer `slash-route`;
   - no paired browser has `browserInstallId == "slash-browser"`;
   - at least the previously trusted browser identities held by the running app are present;
   - the cookie-signing key is valid;
   - the file ACL is current-user-only.
4. Do not print or retain the recovered pairing token, cookie-signing key, client IDs, or other credentials.

Rotating the invitation code once during recovery is acceptable because the current disk code belongs to the test fixture. Existing paired browser cookies remain valid.

If the running process no longer contains valid production state, recovery stops without inventing credentials. The user must pair once after an explicit reset.

## 4. Test isolation implementation

The persistence resolver will distinguish production, debug, and test execution before it considers an optional named profile.

For unit tests:

- the root includes the test process ID and a run nonce;
- the path is under `std::env::temp_dir()`;
- `TestProfileEnvGuard::without_profile()` resolves to that test root, not production;
- persistence helpers reject any test target equal to or contained within the production profile;
- test directories are cleaned through their existing guards where practical.

The two slash-command route tests also receive explicit `TestProfileGuard`s. This preserves cross-test isolation and makes the original omission visible in code review, while the resolver-level boundary provides the non-bypassable production safeguard.

No test may validate production storage by writing to it. Production path semantics are tested as pure values.

## 5. Error handling and diagnostics

- Development and test builds display their profile label so the operator can distinguish them from installed DevManager.
- A debug binary started without `DEVMANAGER_PROFILE` reports that it selected `dev-debug`.
- Remote-state load diagnostics include the path and safe error category, but never file contents or secrets.
- Persistence refuses a test write to the production profile with a clear test failure.
- Browser pairing retains the current backoff and lockout behavior when the stable code is entered incorrectly.

## 6. Verification

### Red-green regression coverage

- A repeated successful pairing with the same code succeeds and leaves the code unchanged.
- Different browser installation IDs can use the same stable code.
- Manual regeneration changes the code without invalidating paired clients.
- Reset revokes clients and changes both the code and cookie-signing key.
- A test build with no profile cannot resolve the production directory.
- A test subprofile remains beneath the temporary test root.
- The formerly contaminating slash-command route tests resolve beneath the temporary test root, and their target is provably disjoint from the production profile.
- An existing malformed remote-state file returns a startup error instead of a default identity.
- A missing remote-state file still follows first-run behavior.

### Runtime and release checks

- Run the affected web, remote, persistence, and app tests.
- Run the complete serial Rust library suite.
- Run formatting, clippy, and an all-features build.
- Hash the real production `remote.json`, run the formerly contaminating tests, and prove the hash is unchanged.
- Restart only after recovery is verified; confirm the previously paired browser reconnects without entering the code.

## 7. Alternatives considered

### Patch only the two tests

Adding two missing profile guards is small, but another test can repeat the same mistake. It does not meet the requirement that tests cannot touch production.

### Inject an in-memory persistence repository through every service

This is the strongest long-term architecture, but it expands the change across workspace, session, PID, browser, and remote persistence. The selected design establishes the safety boundary centrally without an unrelated storage rewrite.

### Selected: execution-scoped central resolver plus explicit test guards

This protects every existing caller of `app_config_dir()`, keeps the current development-profile workflow, fixes the known tests, and creates a fail-closed boundary with a bounded implementation.

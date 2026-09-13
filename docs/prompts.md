# Prompt library

DevManager keeps a **local-first personal prompt library**. Saved prompts, immutable versions, searchable recent history, and linear guided chains are host-authoritative SQLite facts. This document describes that contract and the bounded Phase 7 smoke that checks it. The smoke is fixture/local public-API verification. It is **not** a full host/GPUI/provider end-to-end.

## Authority: personal versus Connect versus organization

- **Personal/local library is authoritative on the host.** Creating or editing a saved prompt writes only to the local `PromptStore`. Signing into Connect does not upload personal prompts, apply an organization snapshot to that store, or replace local versions.
- **Connect sync is deliberate and separate.** Paired owner devices may later read the personal projection over the existing end-to-end channel. That path does not introduce a Connect persistence DTO for personal prompt bodies.
- **Published organization prompts are a different model.** `OrganizationPromptProjection` / `OrganizationPromptAdapter` cache Portal-authoritative snapshots after role, tenant, and policy checks. Those versions never become rows in the personal store.

## Exact immutable versions

Editing a saved prompt creates the next monotonically increasing version and advances the current-version pointer. Previous version bodies stay readable and are not rewritten. Version comparison uses the public diff API (`diff_versions`) on the two exact bodies loaded from the store. Diffing is comparison-only; it does not mutate stored text.

Exact version selection copies one immutable body into the composer or an equivalent public payload:

- Local: `PromptStore::get_version` / `PromptChainService::version` return the exact stored `PromptVersion` (id, body, hash). There is no public local `ComposerInsertion` type.
- Organization: `OrganizationPromptAdapter::put_in_composer` / `OrganizationPromptProjection::put_in_composer` return `ComposerInsertion { sent: false, advanced: false }`.

Selecting a version does not send to a provider and does not advance a chain.

## Manual chains

A chain is one ordered list of links to exact immutable prompt versions. `PromptChainService` exposes create/insert/move/remove and read-only `links` / `link_context`. Context reports previous and next link ids only. Insert-between uses `before_link_id` and keeps positions as a dense `0..n-1` prefix.

There is no automatic execution, cursor, scheduler, completion, branching, or advance. **Put in composer** is a read of one chosen version. Provider send is a later, explicit user action and is **not** exercised by the Phase 7 smoke.

## Safe smoke commands

Default fixture mode only. Do not point these at the installed app, `%APPDATA%\com.userfirst.devmanager`, or a production `DEVMANAGER_PROFILE`. Isolate Cargo into a unique `C:\Temp\devmanager-prompt-smoke-*` target. Do not run the complete test suite.

PowerShell (fail-closed; clears inherited profile/credential variables; never reads production `config.json` / `remote.json` / `session.json`):

```powershell
pwsh -NoProfile -File scripts/native-next/Invoke-PromptLibrarySmoke.ps1
```

Focused Rust test with an isolated target (replace the GUID):

```powershell
$target = 'C:\Temp\devmanager-prompt-smoke-worker-<guid>'
New-Item -ItemType Directory -Force -Path $target | Out-Null
$env:CARGO_TARGET_DIR = $target
Remove-Item Env:DEVMANAGER_PROFILE -ErrorAction SilentlyContinue
cargo test --offline --locked --test prompt_library_smoke -- --exact phase7_prompt_library_smoke_public_api_contract --test-threads=1
```

Authenticated mode is rejected. The script exits `0` only when the focused test passes, `1` for rejected inputs, and `2` for HOLD/failure.

## Public-API gaps recorded by this smoke

- `OwnerDeviceCapability::from_authenticated_session` is currently `Unavailable`, so `PromptLibraryRequest::exact_version` / `project_prompt_store` cannot be minted from a public session in this test. Exact local payload is exercised through `PromptStore` / `PromptChainService`.
- `PromptStore::execute` is the public command surface used by existing integration tests; it is `doc(hidden)` and is not a host CommandBus settlement API.
- Local composer insertion has no `sent`/`advanced` fields. Those flags exist only on organization `ComposerInsertion` and stay false.
- Provider launch, GPUI "Put in composer", FTS history, and restart-across-process persistence are out of scope here.
- In this worktree, `cargo test --test prompt_library_smoke` currently fails before the new test links: the library does not compile due to pre-existing errors outside these owned paths (`InMemoryIdentityPersistence` is `cfg(test)`-only but re-exported from `connect`, a private `ConnectLimits` import in `identity_store`, and a temporary-borrow error in `protocol/crypto.rs`). This smoke does not patch those files.

Provider send is not exercised.

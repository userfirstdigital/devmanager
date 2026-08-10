# Phase 4 Task 4.1 Batch B report

Status: COMPLETE 4.1b provider identity/capability boundary correction from
candidate `0bab620`. The boundary is fail-closed for authentication authority,
custom wires, executable discovery, cache freshness, redaction, and provider
kind validation. Provider-specific auth command/process integration remains
owned by corrected Task 3.7/4.1a.

## Scope

Only the Task 4.1b owned paths were changed: provider adapter/capability/
registry modules, the strict `AgentSessionFacts` validation in
`src/domain/agent.rs`, focused provider tests and fixtures, and this report.
No production profiles, session data, installed app, stock provider CLI,
merge/rebase/push/tag/publish/install, or unrelated repository path was
touched.

## TDD evidence

### RED

Focused tests were added before the production corrections. The first identity
build exposed the missing receipt-only error API. The first registry run
exposed the independent failures for adapter-provided auth, strict wire
requirements, PATH bounds/provenance, bounded caches, and redacted errors.
Those failures were addressed separately before the final green runs.

The existing Windows process-tree fixture also failed reproducibly because its
100ms timeout could kill the parent before the child PID marker was published.
The owned test timeout is now a still-bounded 1s; the production tree-kill
assertion is unchanged and the isolated test is green.

### GREEN

All commands below used the exact isolated target and single Cargo build job:

```text
CARGO_TARGET_DIR=C:\Temp\devmanager-phase41b-correction2
CARGO_BUILD_JOBS=1

cargo test --test provider_identity -- --nocapture
27 passed; 0 failed

cargo test --test provider_registry -- --nocapture
44 passed; 0 failed
```

The focused runs compile the generated Rust test harnesses under the exact
target only; they do not launch the installed DevManager.

Final gates:

```text
cargo fmt --all -- --check
CARGO_TARGET_DIR=C:\Temp\devmanager-phase41b-correction2 CARGO_BUILD_JOBS=1 cargo check --lib
git diff --check
```

The library check passed with pre-existing unrelated warnings only. No broad
suite was run.

## Delivered

- Generic `CapabilityEvidence` constructors and serde cannot create
  authenticated-subscription or auth-required authority. Only a private
  registry-receipt conversion can create auth evidence. `ProviderRegistry`
  rejects adapter-returned auth state/evidence, consumes only its correlated
  nonce/generation/provider/source/executable receipt, and keeps replay,
  stale/future, reordered, replacement, wrong-kind, wrong-executable, and
  API-key results fail-closed.
- Auth pending/accepted state and capability projections are bounded with
  deterministic oldest eviction. Receipt consumption revalidates the current
  exact executable identity and generation under one monotonic `Instant`
  freshness boundary. Caller-selected confidence is not part of the public
  receipt authority API.
- Capability, evidence, executable, nested file identity, observation, and
  cache-key wires require schema version `1`, reject omitted/duplicate/
  unknown fields, and validate cross-fields. Evidence collection admission and
  path decoding are bounded before normal object growth; native/shim
  `is_native` form is preserved through strict executable serialization.
- Production registry discovery now captures PATH once through the bounded
  `ProviderPathSnapshot` and resolves only through `ProviderDiscoveryContract`.
  Empty/relative/oversized entries, reparse ambiguity, directory replacement,
  provenance forgery, forbidden runners, wrong provider names, and wrong
  Windows `.exe`/`.cmd` forms fail closed. Controlled `.cmd` candidates require
  the exact bounded shim proof targeting the validated native identity.
- `ProviderExecutable`, provider errors, discovery errors, probe requests and
  arguments, auth invocation/receipt diagnostics, raw input, and probe results
  have redacted Debug/Display surfaces. Path, hash, nonce, token, environment,
  and raw-output sentinel coverage is exhaustive in the focused tests.
- `AgentSessionFacts` constructors, deserialization, and validation accept
  only the finite canonical provider-kind set; arbitrary strings and forged
  noncanonical public values cannot become validated session facts. Provider
  observations validate provider-kind, version, capabilities, and current
  executable identity together. Adapter capability/quota boundaries receive
  validated executable identities rather than raw paths.
- Test-only executable candidates copy the current test harness and are marked
  as metadata fixtures; they are never asserted to be stock provider tools or
  used as production authority.

## Integration seam and deferred work

The registry exposes the narrow correlated seam: begin an auth invocation for
the exact discovered identity, let the later provider-specific implementation
produce its result, accept it against that invocation, then consume the
receipt while observing (including cache hits). The generic adapter cannot
promote authentication from a capability observation.

Corrected Task 3.7/4.1a owns the provider-specific subscription-login command,
probe process lifecycle, timeout/output scrubbing, process-tree containment,
and wiring this receipt seam into the active runtime. The generic
`ProviderProbeKind::AuthStatus` transport primitive remains non-authoritative
until that integration replaces it with provider-specific validated evidence.
This task therefore fails closed with an explicit untrusted-auth error rather
than adding a permissive placeholder.

Batch C packaging and legacy UI/runtime string migration remain deferred. No
provider session identity is inferred here; exact resume must continue to use
only correlated current-generation provider session identity and must not fall
back to a fresh conversation after an exact-resume failure.

## Review and handoff

The complete owned diff was reviewed after the final focused runs and gates.
The final commit is the precise Task 4.1b correction commit reported in the
handoff. No review subagent or external provider process was used.

## Sharpening the Axe

Worked: separating generic capability projection, receipt issuance,
identity-bound receipt consumption, and wire serialization into independently
tested authority boundaries. Wasted time: shared host Cargo activity made the
cold target build and the deliberately slow cache-bound test look stalled;
the exact target/process check prevented duplicate builds. Earlier improvement:
the existing 100ms process-tree fixture timeout was scheduler-sensitive and
should have been isolated before treating its failure as a code regression.

Authority updated: this Task 4.1b report now records the consolidated rule
that stable cache state is never authentication authority; authority requires
an issued, correlated, current-identity receipt and a one-shot fail-closed
consumption path, with strict versioned wires around both sides. No AGENTS,
global memory, or separate persistent guidance update was warranted because
the project instructions already cover typed identity, redaction, cold-build
isolation, and verification requirements.

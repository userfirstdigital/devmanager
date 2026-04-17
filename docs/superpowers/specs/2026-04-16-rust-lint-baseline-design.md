# Rust Lint Baseline Design

## Context

DevManager currently has no dedicated Rust lint workflow. The existing GitHub Actions setup is release-oriented, and a strict `cargo clippy --all-targets --all-features -- -D warnings` pass currently fails with a large backlog of existing findings. That makes a repo-wide strict Clippy gate a separate cleanup project rather than a small best-practices change.

## Goals

- Add a Rust-only lint baseline.
- Make the lint baseline CI-blocking.
- Keep the baseline realistic for the current repository state.
- Provide an easy local command so contributors can run the same check before pushing.

## Non-Goals

- Fix the entire existing Clippy backlog.
- Add JavaScript or TypeScript linting in this change.
- Turn the release packaging workflow into the main day-to-day lint gate.

## Options Considered

### 1. Dedicated lint workflow with a curated baseline

Create a lightweight GitHub Actions workflow that runs on `push` and `pull_request`, checks formatting with `cargo fmt --check`, and runs `cargo clippy` against a small set of high-signal denied lints that the repo is already clean on.

Pros:
- Fast feedback outside the release pipeline.
- Blocks regressions without forcing a repo-wide cleanup first.
- Easy to expand later.

Cons:
- Does not enforce all Clippy warnings yet.

### 2. Reuse the existing release workflow

Add Rust linting into `release.yml`.

Pros:
- Fewer files.

Cons:
- Slower feedback.
- Runs in a release-focused pipeline instead of a normal developer pipeline.
- Less clear ownership of linting versus packaging.

### 3. Full strict Clippy gate

Require `cargo clippy --all-targets --all-features -- -D warnings`.

Pros:
- Strongest end state.

Cons:
- Too much scope for this change because the repo currently has a large existing Clippy backlog.

## Approved Design

Implement Option 1.

### Policy

Add a small Rust lint policy in `Cargo.toml` using project-level Clippy lints that are high signal and low churn for this codebase:

- `clippy::dbg_macro = deny`
- `clippy::todo = deny`

This establishes explicit project policy without pretending the repository is ready for a full `-D warnings` gate.

### Local Developer Workflow

Add `.cargo/config.toml` with a `cargo lint` alias that runs the Rust lint command used in CI. Contributors should be able to run one command locally and get the same result as automation.

### CI

Add a new `.github/workflows/lint.yml` workflow that runs on `push` and `pull_request` and includes:

- `cargo fmt --check`
- `cargo lint`

This keeps lint enforcement separate from release packaging and makes the baseline CI-blocking in normal development flow.

### Documentation

Add a short README note documenting the local lint command so future contributors know the expected Rust lint workflow.

## Testing

- Run `cargo fmt --check`.
- Run the new local lint command.
- Confirm the new workflow YAML is syntactically valid by keeping it simple and consistent with existing GitHub Actions patterns in the repo.

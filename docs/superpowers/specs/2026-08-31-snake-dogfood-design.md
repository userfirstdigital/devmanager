# Snake Game and DevManager Dogfood Design

## Purpose

Build a small full-stack Snake game by using the new native DevManager as the
real development environment. The exercise is also a release-candidate dogfood
run: two provider-backed tasks remain visible and active at the same time while
the workflow is inspected for correctness, usability, accessibility, latency,
resource use, and visual quality.

The work produces two outcomes:

1. A working, tested Snake game in a new multi-folder DevManager project.
2. Reproduced and verified DevManager fixes for defects encountered during the
   workflow.

## Project Shape

Create one DevManager project named `Snake Game` with two folders:

- `C:\Code\userfirst\snake-game\backend`
- `C:\Code\userfirst\snake-game\frontend`

The folders are independent Git repositories so the exercise verifies
DevManager's multi-folder repository model. DevManager must preserve the folder
identity of each task and show the correct repository state for each pane.

Two tasks will be opened simultaneously in the recursive split workspace:

- **Backend:** Codex, GPT-5.6 Sol, Low thinking.
- **Frontend:** Claude, Opus, Low thinking.

Both panes stay rendered and continue updating when they are not focused. A
click anywhere in a pane focuses its exact task. Opening another task replaces
the focused pane unless the user explicitly opens an additional pane.

## Game Architecture

### Backend

Use Node.js with built-in modules only. The backend owns:

- `GET /api/health` for readiness.
- `GET /api/scores` for the top scores.
- `POST /api/scores` to validate and record a score.
- JSON-file persistence using atomic replacement.
- Static-file delivery from the dependency-free frontend directory in local
  development.

Score submissions contain a trimmed player name, score, board dimensions, and
duration. The server validates types and bounds, limits leaderboard size, and
returns structured JSON errors. It binds to loopback by default and does not
request firewall access.

### Frontend

Use dependency-free HTML, CSS, and JavaScript. The frontend owns:

- A responsive canvas board with crisp grid-aligned rendering.
- Arrow-key and WASD movement.
- Touch swipe controls.
- Start, pause, resume, restart, and game-over states.
- Current score, local high score, speed, and server leaderboard.
- Accessible controls, visible focus states, reduced-motion support, and a
  color-safe dark visual theme.

Game rules live in a pure engine module. Rendering, input, timing, storage, and
leaderboard networking remain adapters around that engine.

## Data Flow

The frontend advances the pure game state on a fixed tick. It renders the
returned state and never mutates the engine model directly. Food consumption
grows the snake, increases the score, and gradually reduces the tick interval
within a safe minimum. Wall or self collision ends the run.

At game over, the frontend shows the result immediately. A valid score can then
be submitted to the backend. Network failure never blocks local play; it leaves
the result visible with a concise retry action. The backend writes accepted
scores atomically and returns the sorted bounded leaderboard.

## Testing

Both repositories use Node's built-in test runner to avoid dependency and
installation delays.

Backend tests cover:

- Health and leaderboard responses.
- Score validation and rejection cases.
- Ordering, trimming, maximum retained entries, and persistence recovery.
- Loopback binding and clean shutdown.

Frontend tests cover:

- Movement and opposite-direction rejection.
- Growth, scoring, food placement, and speed progression.
- Wall and self collision.
- Pause/resume and deterministic seeded state.
- Leaderboard client success and failure behavior.

Browser acceptance covers keyboard and touch input, responsive layout, focus
visibility, game-over/restart, score submission, and reload persistence.

## DevManager Dogfood Workflow

All meaningful implementation prompts are sent through the new DevManager.
The backend and frontend tasks are created from the `Snake Game` project,
configured with their required provider/model/thinking settings, and shown in
separate panes at the same time. The run exercises:

- Project creation with two folders and two repositories.
- New-task creation with project-only prompting.
- Provider, model, thinking, and access selection.
- Immediate message appearance, streaming output, and automatic task titles.
- Independent conversation rendering for focused and unfocused panes.
- Pane focus, replacement, addition, resizing, compact mode, and persistence.
- Terminal loading, immediate input, ANSI rendering, selection, copy/paste,
  image paste where supported, and wheel scrolling.
- Git status and commit surfaces for the correct folder.
- Waiting/thinking/done status, audible completion, Done/restore, archive, and
  reopen behavior.
- Restart and exact conversation resume without a fresh provider session.

## DevManager Defect Loop

Maintain a bounded observation ledger during the run. Record:

- Functional errors and incorrect state transitions.
- UI hierarchy, spacing, legibility, affordance, focus, and accessibility gaps.
- Latency from click or send to visible acknowledgement and first streamed
  output.
- Idle CPU, unnecessary polling, repeated requests, and avoidable rendering.
- Opportunities to simplify the workflow without hiding capability.

For each material issue:

1. Reproduce it in the live native app and capture the exact state transition.
2. Trace the owning component and establish the root cause.
3. Add the smallest failing automated regression test.
4. Implement one root-cause fix.
5. Run the focused test and repeat the original Computer Use gesture against
   the rebuilt app.

Do not bundle unrelated refactors or claim a visual issue fixed from source or
screenshots alone. Cosmetic refinements must preserve keyboard interaction,
accessibility semantics, and multi-host/multi-folder identity.

## Performance Acceptance

The run records, at minimum:

- New task click to visible empty composer.
- Send click to local message paint.
- Send click to provider acknowledgement and first streamed output.
- Task switch to cached conversation paint.
- Terminal open to first provider prompt.
- Physical key press to terminal paint.
- Idle CPU and child-process count after the app settles.

No fixed sleep is used as product coordination. Loading indicators appear only
while bounded asynchronous work is genuinely outstanding. Background polling
must back off or remain event-driven when idle.

## Completion Criteria

The work is complete only when:

- The frontend and backend repositories pass their focused and full tests.
- The game is playable by keyboard and touch, and score persistence works.
- Both DevManager task panes simultaneously show their own conversation and
  correct project folder.
- Required model and thinking settings are visible and remain changeable.
- Conversation, terminal, Git, lifecycle, notification, restart, and pane
  workflows pass the live acceptance sequence.
- Every material DevManager issue found in this run is fixed and retested, or
  is explicitly documented with evidence and a concrete remaining blocker.
- DevManager's complete Rust library suite and integration check pass once at
  the end in an isolated Cargo target.
- Installed DevManager v0.4.1 and its production profile remain untouched.

## Commit Boundaries

The backend and frontend repositories receive their own focused commits.
DevManager fixes are committed separately in the DevManager repository, with
the design and implementation history kept reviewable. Nothing is pushed or
installed over v0.4.1 without a separate explicit request.

# Snake Game Dogfood Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build and locally verify a full-stack Snake game through two simultaneous native DevManager tasks while reproducing, fixing, and retesting material DevManager defects encountered in the workflow.

**Architecture:** One DevManager project contains independent backend and frontend Git repositories. The Node.js backend provides health and persistent leaderboard APIs and serves the dependency-free frontend; the frontend isolates deterministic game rules from canvas rendering, input, timing, storage, and networking. DevManager remains the orchestration surface, while each discovered DevManager defect enters a separate evidence-led TDD cycle before it is changed.

**Tech Stack:** Node.js built-in HTTP, filesystem, URL, crypto, and test modules; HTML; CSS; ES modules; Canvas 2D; native DevManager GPUI/Rust.

**Spec:** `docs/superpowers/specs/2026-08-31-snake-dogfood-design.md`

## Global Constraints

- Keep all repositories local; do not push any Git remote.
- Keep installed DevManager v0.4.1 and its production profile untouched.
- Create one `Snake Game` DevManager project with `backend` and `frontend` folders as independent Git repositories.
- Use Codex GPT-5.6 Sol with Low thinking for the backend task.
- Use Claude Opus with Low thinking for the frontend task.
- Keep both task panes visible and updating simultaneously.
- Use Node.js built-in modules only; do not install packages.
- Bind the backend to loopback by default.
- Write a failing test and observe the expected failure before every production behavior change.
- Fix only DevManager issues that are reproduced and traced to a root cause during this workflow.
- Run complete expensive Rust gates once at the end in an isolated Cargo target.

---

### Task 1: Create the Multi-Folder Project and Two Live Tasks

**Files:**
- Create: `C:\Code\userfirst\snake-game\backend\.gitignore`
- Create: `C:\Code\userfirst\snake-game\frontend\.gitignore`
- Create: `C:\Code\userfirst\snake-game\backend\README.md`
- Create: `C:\Code\userfirst\snake-game\frontend\README.md`

**Interfaces:**
- Consumes: Native DevManager project, task, provider, model, thinking, pane, and folder selection controls.
- Produces: One `Snake Game` project containing two repository folders and two simultaneous task panes with exact provider ownership.

- [ ] **Step 1: Create the folder roots and local repositories**

Create `backend` and `frontend`, initialize each as an independent Git repository on branch `main`, and add `.gitignore` entries for `node_modules/`, `.tmp/`, coverage output, runtime score data, and OS/editor files.

- [ ] **Step 2: Add minimal repository descriptions**

Backend `README.md` must state that it owns loopback HTTP, leaderboard validation, persistence, and frontend serving. Frontend `README.md` must state that it owns the pure game engine, canvas UI, input, local score, and leaderboard client.

- [ ] **Step 3: Commit each repository baseline**

Run in each repository:

```powershell
git add .gitignore README.md
git commit -m "Initialize Snake game repository"
```

Expected: both repositories have clean `main` branches with no remotes.

- [ ] **Step 4: Create the DevManager project through the native UI**

Create project `Snake Game`, attach both repository folders, and verify the rail identifies `Snake Game` rather than a generic `project` label.

- [ ] **Step 5: Create and configure both empty tasks through the native UI**

Create `Snake backend` in the backend folder and select Codex / GPT-5.6 Sol / Low. Create `Snake frontend` in the frontend folder and select Claude / Opus / Low. Open them in separate simultaneous panes and verify each pane displays the correct task and folder before sending a message.

### Task 2: Backend Leaderboard Domain and Persistence

**Files:**
- Create: `C:\Code\userfirst\snake-game\backend\package.json`
- Create: `C:\Code\userfirst\snake-game\backend\src\scores.js`
- Create: `C:\Code\userfirst\snake-game\backend\test\scores.test.js`

**Interfaces:**
- Consumes: Score input `{ name, score, boardWidth, boardHeight, durationMs }`.
- Produces: `normalizeScore(input, now) -> ScoreEntry`, `sortAndLimitScores(entries, limit) -> ScoreEntry[]`, `createScoreStore({ filePath, limit, now }) -> { list(), add(input) }`.

- [ ] **Step 1: Write failing score normalization tests**

Test that `normalizeScore` trims a 1-20 character name, accepts integer scores from 0 through 1,000,000, requires board dimensions from 8 through 100 and duration from 0 through 86,400,000 ms, assigns an ISO timestamp, and throws an error with `code === "INVALID_SCORE"` for every invalid field.

- [ ] **Step 2: Run normalization tests and verify RED**

Run:

```powershell
node --test test/scores.test.js
```

Expected: FAIL because `src/scores.js` does not exist or its exports are missing.

- [ ] **Step 3: Implement score normalization and bounded ordering**

Implement immutable normalized entries and sort by descending score, ascending duration, then ascending creation time. Keep only the configured limit.

- [ ] **Step 4: Run normalization tests and verify GREEN**

Run `node --test test/scores.test.js`.

Expected: PASS with no warnings.

- [ ] **Step 5: Write failing persistence tests**

Use a process-unique temporary directory. Verify that `add` atomically persists, `list` survives store recreation, a missing file returns an empty list, and malformed JSON is surfaced as an error with `code === "SCORE_DATA_CORRUPT"` without overwriting the file.

- [ ] **Step 6: Run persistence tests and verify RED**

Expected: FAIL because `createScoreStore` persistence is absent.

- [ ] **Step 7: Implement atomic JSON persistence**

Write to a sibling temporary file, flush and close it, then rename it over the target. Serialize store mutations through one promise chain and clean only the exact temporary file on failure.

- [ ] **Step 8: Run all backend score tests and commit**

Run `node --test test/scores.test.js`, then:

```powershell
git add package.json src/scores.js test/scores.test.js
git commit -m "Add tested leaderboard persistence"
```

### Task 3: Backend HTTP API and Static Delivery

**Files:**
- Create: `C:\Code\userfirst\snake-game\backend\src\server.js`
- Create: `C:\Code\userfirst\snake-game\backend\test\server.test.js`
- Modify: `C:\Code\userfirst\snake-game\backend\package.json`

**Interfaces:**
- Consumes: `createScoreStore`, optional `{ host, port, scoreFile, frontendRoot }`.
- Produces: `createApp(options) -> { start(), stop(), address }`; routes `GET /api/health`, `GET /api/scores`, `POST /api/scores`.

- [ ] **Step 1: Write failing health and leaderboard route tests**

Start on host `127.0.0.1` and port `0`. Assert health returns `200` and `{ "status": "ok" }`, scores returns an array, unsupported API routes return structured `404`, and every response has a JSON content type.

- [ ] **Step 2: Run server tests and verify RED**

Run `node --test test/server.test.js`.

Expected: FAIL because `createApp` is absent.

- [ ] **Step 3: Implement the minimal bounded HTTP server**

Limit request bodies to 16 KiB. Return `400 INVALID_JSON`, `422 INVALID_SCORE`, `404 NOT_FOUND`, or `500 INTERNAL_ERROR` without exposing filesystem paths. Track sockets and implement bounded clean shutdown.

- [ ] **Step 4: Run route tests and verify GREEN**

Run `node --test test/server.test.js`.

Expected: PASS.

- [ ] **Step 5: Write failing static delivery tests**

Verify `/` serves `index.html`, module and CSS files have correct content types, path traversal returns `404`, missing assets return `404`, and API routes never fall through to static files.

- [ ] **Step 6: Run static tests and verify RED**

Expected: FAIL because static delivery is absent.

- [ ] **Step 7: Implement safe frontend delivery**

Resolve request paths beneath the configured frontend root, reject any resolved escape, and stream regular files only. Add `npm test` and `npm start` scripts.

- [ ] **Step 8: Run the complete backend suite and commit**

Run `npm test`, then:

```powershell
git add package.json src/server.js test/server.test.js
git commit -m "Add Snake leaderboard HTTP server"
```

### Task 4: Frontend Pure Game Engine

**Files:**
- Create: `C:\Code\userfirst\snake-game\frontend\package.json`
- Create: `C:\Code\userfirst\snake-game\frontend\src\game.js`
- Create: `C:\Code\userfirst\snake-game\frontend\test\game.test.js`

**Interfaces:**
- Consumes: `createGame({ width, height, random })`, directions `up|down|left|right`, and deterministic `random()` values in `[0, 1)`.
- Produces: `queueDirection(state, direction) -> state`, `tick(state) -> state`, `togglePause(state) -> state`, and state fields `snake`, `direction`, `queuedDirection`, `food`, `score`, `status`, `tickMs`.

- [ ] **Step 1: Write failing creation and movement tests**

Verify a deterministic centered three-segment snake, in-bounds food, one-cell movement, immutability, and rejection of an immediate opposite direction.

- [ ] **Step 2: Run engine tests and verify RED**

Run `node --test test/game.test.js`.

Expected: FAIL because the engine module is absent.

- [ ] **Step 3: Implement creation, direction queuing, and movement**

Keep all state transitions pure. Permit at most one legal direction change per tick so two rapid key presses cannot reverse into the snake.

- [ ] **Step 4: Run movement tests and verify GREEN**

Run `node --test test/game.test.js`.

Expected: PASS for creation and movement.

- [ ] **Step 5: Write failing growth, collision, and pause tests**

Verify food grows by one segment, score increases by 10, tick speed decreases by 4 ms to a 60 ms floor, food never occupies the snake, wall/self collision changes status to `game-over`, and paused games do not advance.

- [ ] **Step 6: Run new engine tests and verify RED**

Expected: FAIL for missing growth, collision, or pause behavior.

- [ ] **Step 7: Implement remaining engine transitions**

Use bounded board scans when random food candidates collide repeatedly; return a `won` state if the snake fills the board.

- [ ] **Step 8: Run the complete engine suite and commit**

Run `npm test`, then:

```powershell
git add package.json src/game.js test/game.test.js
git commit -m "Add deterministic Snake game engine"
```

### Task 5: Frontend Leaderboard Client and Controller

**Files:**
- Create: `C:\Code\userfirst\snake-game\frontend\src\leaderboard.js`
- Create: `C:\Code\userfirst\snake-game\frontend\src\controller.js`
- Create: `C:\Code\userfirst\snake-game\frontend\test\leaderboard.test.js`
- Create: `C:\Code\userfirst\snake-game\frontend\test\controller.test.js`

**Interfaces:**
- Consumes: `fetch`, storage with `getItem/setItem`, clock functions, and pure game-engine functions.
- Produces: `createLeaderboardClient({ fetchImpl, baseUrl })`, `createGameController({ engine, storage, now })`, and controller events for state, score submission, retry, and local high score.

- [ ] **Step 1: Write failing leaderboard client tests**

Verify successful list and submit parsing, URL construction, JSON headers, structured server-error propagation, and a stable `NETWORK_UNAVAILABLE` error for transport failure.

- [ ] **Step 2: Run leaderboard tests and verify RED**

Run `node --test test/leaderboard.test.js`.

Expected: FAIL because the client is absent.

- [ ] **Step 3: Implement the leaderboard client and verify GREEN**

Implement only `list()` and `submit(score)`, then rerun the focused test.

- [ ] **Step 4: Write failing controller tests**

Verify start, pause/resume, restart, direction input, fixed-tick advancement, local high-score persistence, one score submission per completed run, and retry after a failed submission.

- [ ] **Step 5: Run controller tests and verify RED**

Expected: FAIL because the controller is absent.

- [ ] **Step 6: Implement the controller and verify GREEN**

Keep DOM and canvas references out of the controller. Expose `subscribe(listener)` and return an unsubscribe function.

- [ ] **Step 7: Run the complete frontend unit suite and commit**

Run `npm test`, then:

```powershell
git add src/leaderboard.js src/controller.js test/leaderboard.test.js test/controller.test.js
git commit -m "Add Snake controller and leaderboard client"
```

### Task 6: Responsive Canvas Interface

**Files:**
- Create: `C:\Code\userfirst\snake-game\frontend\index.html`
- Create: `C:\Code\userfirst\snake-game\frontend\styles.css`
- Create: `C:\Code\userfirst\snake-game\frontend\src\render.js`
- Create: `C:\Code\userfirst\snake-game\frontend\src\main.js`
- Create: `C:\Code\userfirst\snake-game\frontend\test\render.test.js`

**Interfaces:**
- Consumes: Controller snapshots and a Canvas 2D-like context.
- Produces: `renderGame(context, state, viewport)`, keyboard/touch adapters, accessible status text, and the playable page.

- [ ] **Step 1: Write failing renderer geometry tests**

Using a recording context, verify cell size is an integer, the board is centered, snake/food draw calls remain within the board, the head is visually distinct, and game-over/pause overlays are emitted without mutating state.

- [ ] **Step 2: Run renderer tests and verify RED**

Run `node --test test/render.test.js`.

Expected: FAIL because `renderGame` is absent.

- [ ] **Step 3: Implement canvas rendering and verify GREEN**

Use device-pixel-ratio scaling, crisp integer geometry, high-contrast colors, and reduced-motion-safe transitions.

- [ ] **Step 4: Build the semantic page and interaction adapters**

Use real buttons for start/pause/restart/submit, a labeled player-name input, a polite live status region, visible focus rings, and a leaderboard list. Map Arrow/WASD keys and touch swipes without preventing unrelated page scrolling outside the board.

- [ ] **Step 5: Run frontend tests and perform browser acceptance**

Run `npm test`, start the backend, open the loopback URL, and verify keyboard play, swipe play, responsive resizing, pause/resume, collision, restart, score submit, retry messaging, local high-score reload, and reduced-motion rendering.

- [ ] **Step 6: Commit the interface**

```powershell
git add index.html styles.css src/render.js src/main.js test/render.test.js
git commit -m "Build responsive Snake game interface"
```

### Task 7: Live DevManager Observation and Root-Cause Fix Cycles

**Files:**
- Modify only after reproduction: exact owning files under `src/` and their colocated Rust tests.
- Update: `C:\Code\userfirst\devmanager\docs\superpowers\specs\2026-08-31-snake-dogfood-design.md` only if live evidence changes the approved contract.

**Interfaces:**
- Consumes: Computer Use evidence from the two live task panes and process/resource measurements.
- Produces: One regression test and one focused commit per root-cause DevManager fix.

- [ ] **Step 1: Exercise and record the full native workflow**

Measure task creation, immediate message paint, provider acknowledgement, first stream, task switching, cached conversation paint, terminal load, terminal key paint, idle CPU, and child-process count. Exercise pane focus/replacement/addition/resizing/compact mode, model/thinking changes, terminal selection/copy/paste/scroll, Git status/commit, lifecycle actions, notifications, restart, and exact resume.

- [ ] **Step 2: Classify each observation before changing code**

For every material defect, record reproduction steps, expected behavior, actual behavior, task/provider/folder identity, timing, and the component boundary where evidence diverges. Discard non-reproducible guesses.

- [ ] **Step 3: Run one TDD cycle per confirmed defect**

Identify the production symbol whose behavior must change, add the smallest test that fails for the reproduced reason, run only that test to verify RED, implement the minimal root-cause fix, rerun to GREEN, then repeat the exact live gesture against the rebuilt isolated app.

- [ ] **Step 4: Commit each verified DevManager fix separately**

Stage only the defect's source and tests. Run `git diff --cached --check` and commit with a message naming the behavior, not the symptom. Do not push.

### Task 8: Final Integration and Release Evidence

**Files:**
- Modify: `C:\Code\userfirst\snake-game\backend\README.md`
- Modify: `C:\Code\userfirst\snake-game\frontend\README.md`

**Interfaces:**
- Consumes: Completed game repositories, DevManager fixes, and live acceptance observations.
- Produces: Reproducible local run instructions and a precise completion ledger.

- [ ] **Step 1: Run both game suites and integrated browser acceptance**

Run `npm test` in each repository. Start the backend, play a complete game, submit a score, reload, and verify server and local persistence.

- [ ] **Step 2: Document exact local commands and commit**

Add Node version expectation, test commands, start command, loopback URL, repository responsibilities, and troubleshooting limited to known errors. Commit README changes separately in each repository.

- [ ] **Step 3: Run the final DevManager Rust gates once**

Resolve an isolated target beneath an exact `C:\Temp\devmanager-*` directory and print it before Cargo. Build `devmanager-process-test-helper`, run `cargo test --lib -- --test-threads=1`, then run `cargo check --locked --lib --bins --tests`. Verify no Cargo, rustc, linker, or test harness remains.

- [ ] **Step 4: Repeat the complete live native acceptance matrix**

Use the rebuilt development executable and isolated development profile. Confirm the installed v0.4.1 process and production config/remote hashes remain unchanged. Do not interact with security dialogs; stop for user handoff if one appears.

- [ ] **Step 5: Produce the final completion ledger**

Separate game code, DevManager code, automated tests, live desktop acceptance, remote-readiness implications, local commits, and anything not verified. Report local commit IDs and explicitly state that no remote was pushed.

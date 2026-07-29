# Terminal Tab Click Isolation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prevent a sidebar tab-selection gesture from forwarding an orphaned mouse release into a newly activated terminal.

**Architecture:** Track the terminal session ID and mouse button that own an active terminal press. Route mouse-up and mouse-up-out through one release helper that consumes the owner and returns protocol bytes only for the same session and button.

**Tech Stack:** Rust, GPUI mouse events, existing terminal mouse protocol encoder, Cargo unit tests.

## Global Constraints

- A pointer gesture that begins in the sidebar must never become terminal input.
- Legitimate terminal clicks and drag-out releases must continue to work.
- Local and remote terminal input paths must share the same ownership decision.
- Do not change sidebar activation timing or terminal focus behavior.
- Run only focused test targets until the final verification gate.
- Rust test execution must remain under the test-only temporary configuration root and must not touch the installed DevManager profile.

---

## File Structure

- Modify `src/app/mod.rs`: own terminal mouse-gesture state, authorize releases, and contain focused unit tests alongside the existing mouse protocol tests.
- No new production module is warranted because the state and both GPUI handlers already live in `NativeShell` in this file.

### Task 1: Require terminal releases to match their press owner

**Files:**
- Modify: `src/app/mod.rs:439-511`
- Modify: `src/app/mod.rs:13420-13785`
- Test: `src/app/mod.rs:20077-20100`

**Interfaces:**
- Produces: `TerminalMousePressOwner { session_id: String, button: MouseButton }`
- Produces: `terminal_mouse_release_report(owner: &mut Option<TerminalMousePressOwner>, session_id: Option<&str>, mode: TerminalModeSnapshot, cell: TerminalGridPosition, button: MouseButton, modifiers: Modifiers) -> Option<Vec<u8>>`
- Consumes: existing `mouse_button_report(...) -> Option<Vec<u8>>`

- [ ] **Step 1: Write the failing release-ownership tests**

Add these tests beside `sgr_mouse_reports_include_modifier_bits`:

```rust
#[test]
fn terminal_mouse_release_rejects_orphaned_or_mismatched_press_owner() {
    let mode = crate::terminal::session::TerminalModeSnapshot {
        mouse_report_click: true,
        sgr_mouse: true,
        ..Default::default()
    };
    let cell = TerminalGridPosition { row: 3, column: 4 };

    let mut owner = None;
    assert_eq!(
        terminal_mouse_release_report(
            &mut owner,
            Some("claude-2"),
            mode,
            cell,
            MouseButton::Left,
            Modifiers::default(),
        ),
        None
    );

    owner = Some(TerminalMousePressOwner {
        session_id: "claude-1".to_string(),
        button: MouseButton::Left,
    });
    assert_eq!(
        terminal_mouse_release_report(
            &mut owner,
            Some("claude-2"),
            mode,
            cell,
            MouseButton::Left,
            Modifiers::default(),
        ),
        None
    );
    assert!(owner.is_none());

    owner = Some(TerminalMousePressOwner {
        session_id: "claude-2".to_string(),
        button: MouseButton::Right,
    });
    assert_eq!(
        terminal_mouse_release_report(
            &mut owner,
            Some("claude-2"),
            mode,
            cell,
            MouseButton::Left,
            Modifiers::default(),
        ),
        None
    );
    assert!(owner.is_none());
}

#[test]
fn terminal_mouse_release_accepts_matching_press_owner_once() {
    let mode = crate::terminal::session::TerminalModeSnapshot {
        mouse_report_click: true,
        sgr_mouse: true,
        ..Default::default()
    };
    let mut owner = Some(TerminalMousePressOwner {
        session_id: "claude-2".to_string(),
        button: MouseButton::Left,
    });

    let report = terminal_mouse_release_report(
        &mut owner,
        Some("claude-2"),
        mode,
        TerminalGridPosition { row: 3, column: 4 },
        MouseButton::Left,
        Modifiers::default(),
    );

    assert_eq!(report, Some(b"\x1b[<0;5;4m".to_vec()));
    assert!(owner.is_none());
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```powershell
$env:DEVMANAGER_PROFILE='terminal-click-isolation-test'
cargo test --lib terminal_mouse_release_ -- --nocapture
```

Expected: compilation fails because `TerminalMousePressOwner` and
`terminal_mouse_release_report` do not exist. The installed
`C:\Users\micro\AppData\Local\DevManager\devmanager.exe` process remains
untouched.

- [ ] **Step 3: Add the minimal gesture owner and release helper**

Add beside the existing terminal selection and scrollbar state types:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
struct TerminalMousePressOwner {
    session_id: String,
    button: MouseButton,
}
```

Add beside `mouse_button_report`:

```rust
fn terminal_mouse_release_report(
    owner: &mut Option<TerminalMousePressOwner>,
    session_id: Option<&str>,
    mode: crate::terminal::session::TerminalModeSnapshot,
    cell: TerminalGridPosition,
    button: MouseButton,
    modifiers: Modifiers,
) -> Option<Vec<u8>> {
    let matches = owner.as_ref().is_some_and(|owner| {
        Some(owner.session_id.as_str()) == session_id && owner.button == button
    });
    *owner = None;
    if !matches {
        return None;
    }
    mouse_button_report(mode, cell, button, modifiers, false)
}
```

Add `terminal_mouse_press_owner: Option<TerminalMousePressOwner>` beside
`last_terminal_mouse_report` in `NativeShell` and initialize it to `None`.
Clear it in the existing window-deactivation and terminal-focus-out paths.

- [ ] **Step 4: Record a press only after terminal mouse-down is forwarded**

In the terminal mouse-capture branch of `handle_terminal_mouse_down`, clear
any stale owner before encoding. After resolving the session and forwarding
the press bytes, record:

```rust
self.terminal_mouse_press_owner = Some(TerminalMousePressOwner {
    session_id,
    button: event.button,
});
```

Keep `last_terminal_mouse_report` unchanged for motion deduplication.

- [ ] **Step 5: Gate both terminal release handlers**

In `handle_terminal_mouse_up` and `handle_terminal_mouse_up_out`, resolve the
current session ID and call `terminal_mouse_release_report`. Forward only the
returned bytes. If the helper returns `None`, clear
`last_terminal_mouse_report` and return without writing to the local PTY or
remote transport.

When terminal mouse capture is inactive, clear
`terminal_mouse_press_owner` before continuing into normal text-selection
completion.

- [ ] **Step 6: Run focused tests and verify GREEN**

Run:

```powershell
$env:DEVMANAGER_PROFILE='terminal-click-isolation-test'
cargo test --lib terminal_mouse_release_ -- --nocapture
cargo test --lib sgr_mouse_reports_include_modifier_bits -- --nocapture
```

Expected: all three focused tests pass.

- [ ] **Step 7: Run the coherent Rust verification gate**

Run:

```powershell
$env:DEVMANAGER_PROFILE='terminal-click-isolation-test'
cargo test --lib
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

Expected: every command exits successfully. Re-check that no test harness
remains running and that the installed DevManager PID and start time are
unchanged.

- [ ] **Step 8: Review and commit the implementation**

Run:

```powershell
git diff --check
git diff -- src/app/mod.rs
git status --short
git add src/app/mod.rs docs/superpowers/plans/2026-07-29-terminal-tab-click-isolation.md
git commit -m "fix: isolate terminal clicks from sidebar navigation"
```

Review requirement: the diff must contain only the gesture owner, release
gate, focused tests, and this implementation plan.

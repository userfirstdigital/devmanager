# Terminal Tab Click Isolation Design

## Problem

DevManager activates sidebar terminal rows on mouse-down. That activation
immediately replaces the terminal surface with the newly selected session while
the same physical pointer gesture is still in progress.

The terminal surface forwards mouse-up and mouse-up-out events to a terminal
application whenever terminal mouse reporting is enabled. It currently does so
without proving that the active terminal received the matching mouse-down.
Consequently, a sidebar click can activate a waiting Claude or Codex terminal
and then deliver an orphaned mouse-release sequence to it. Interactive prompts
may interpret that sequence as selecting the option at the corresponding row.

The behavior is intermittent because it requires the newly selected terminal
to have mouse reporting enabled and to be showing an interactive prompt when
the release arrives.

## Goals

- A pointer gesture that begins in the sidebar must never become terminal
  input.
- A terminal mouse release is forwarded only to the same terminal session that
  received the matching press, for the same mouse button.
- Legitimate terminal clicks and drag-out releases continue to work.
- Local and remote terminal input paths use the same ownership decision.
- Development and tests remain isolated from the installed DevManager profile.

## Non-goals

- Changing sidebar selection from mouse-down to mouse-up.
- Disabling terminal mouse reporting for Claude, Codex, SSH, or server
  terminals.
- Adding a timing delay or a one-shot suppression timeout.
- Changing terminal keyboard focus behavior after a sidebar selection.

## Design

DevManager will track the active terminal mouse press as a small gesture owner:
the resolved terminal session ID and mouse button that received the press.

When a terminal mouse-down is successfully encoded and forwarded, DevManager
records that owner. Mouse-up and mouse-up-out consume the owner and emit a
release only when the currently resolved session ID and released button match
it. A missing or mismatched owner is treated as an orphaned release and is
discarded without writing bytes to either the local PTY or the remote terminal
transport.

The gesture owner is cleared when terminal focus is lost, window activation is
lost, input becomes unavailable, or a release completes. This prevents a press
from one session authorizing a release into a different session after a tab
change.

The existing last-mouse-report state remains responsible for motion
deduplication. Gesture ownership is separate because deduplication state does
not identify the session that owns the physical press.

## Alternatives Considered

### Suppress the next release after a tab switch

This is smaller but depends on event timing. If no orphaned release arrives,
the flag can suppress a later legitimate terminal click. It also does not model
which session owns the gesture.

### Activate sidebar rows on mouse-up

This avoids this particular event ordering but changes interaction semantics
across every sidebar control and does not protect against other surfaces that
can replace the active terminal during a pointer gesture.

### Disable mouse reporting in AI terminals

This prevents interactive terminal mouse use entirely and removes expected
functionality to avoid one routing defect.

## Testing

Focused unit tests will exercise the real release-authorization decision:

- a release without a recorded terminal press produces no terminal bytes;
- a release for a different session produces no terminal bytes;
- a release for a different button produces no terminal bytes;
- a matching session and button produces the expected terminal release bytes
  and consumes the gesture owner.

Existing terminal mouse encoding tests remain responsible for protocol formats
and modifier behavior. The focused Rust test will run under the test-only
temporary configuration root and will not access the installed application's
`config.json`, `session.json`, or `remote.json`.

## Success Criteria

Switching to a Claude or Codex terminal from the sidebar cannot select an
interactive prompt option. Direct clicks begun inside that terminal continue
to work, including a release outside the terminal after a drag.

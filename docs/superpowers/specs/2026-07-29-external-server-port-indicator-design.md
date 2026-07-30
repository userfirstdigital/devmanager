# External Server Port Indicator Design

## Problem

DevManager's sidebar server indicators currently describe only commands with
live DevManager sessions. The background port snapshot is restricted to ports
belonging to those sessions, so a configured API or web server started outside
DevManager appears stopped even while its port is listening.

The indicator derivation already compares a listening PID with the complete
DevManager-managed process tree. The missing information is a cached status for
configured ports whose DevManager command is stopped, exited, crashed, failed,
or has never been started.

## Goals

- Show a blue dot when a configured port is listening but its PID is not owned
  by the corresponding DevManager session.
- Preserve green for a listening port owned by DevManager.
- Preserve orange while DevManager is starting or running a command whose
  managed process tree is not yet listening.
- Detect external listeners even when the corresponding DevManager session is
  absent, stopped, exited, crashed, or failed.
- Keep all operating-system port inspection outside rendering and other hot
  paths.
- Batch every configured port into one snapshot and prevent overlapping scans.
- Keep development and tests isolated from the installed DevManager profile and
  process.

## Non-goals

- Performing HTTP health checks or claiming that an application protocol is
  healthy.
- Looking up or displaying the external process name.
- Managing, stopping, or adopting the external process.
- Adding an independent port-monitoring service.
- Changing indicators for commands without configured ports.

## Terminology

Blue means **external listener**: the configured TCP port is in the operating
system's listening table, but the listener PID is outside the corresponding
DevManager session's tracked process tree. It proves port occupancy, not HTTP
or application health.

## Design

### Background snapshot

`sync_server_port_snapshot` will collect `tracked_server_ports`, which contains
every unique configured command port, instead of `live_server_ports`. It will
continue to launch one `ports_service::snapshot_ports` call on GPUI's background
executor and retain the existing `refresh_in_flight` guard.

On Windows the service reads the IPv4 and IPv6 listener tables once and filters
the requested ports in memory. On other platforms it executes one `lsof`
snapshot for the entire set. The render path will continue to read only the
cached `HashMap<u16, PortStatus>` and will never perform an operating-system or
process-name query.

The refresh interval will be adaptive:

- one second when any configured server command has a live DevManager session;
- three seconds when all configured server commands are inactive and the scan
  exists only to discover external listeners.

A configured-port change clears the last-check timestamp, prunes obsolete
statuses, and triggers one fresh background snapshot. Snapshot failures retain
the previous successful statuses and become eligible for the next scheduled
attempt; they do not block or alter rendering.

### Indicator state

`ServerIndicatorState` will gain `External`. Its precedence will be:

1. A `Starting` DevManager session remains `Unready` (orange).
2. A `Stopping` DevManager session retains the textual stopping state.
3. A `Running` session with a configured port is:
   - `Ready` (green) when its tracked process tree owns the listener;
   - `External` (blue) when another process owns the listener;
   - `Unready` (orange) when nothing is listening.
4. An absent, stopped, exited, crashed, or failed session is `External` (blue)
   when the configured port has an external listener.
5. Otherwise existing stopped, exited, crashed, and failed presentation is
   preserved.
6. A running command without a configured port remains `Ready`.

The external color will be a dedicated readable blue theme token rather than
reusing the primary-action indigo. Blue is rendered as the same six-pixel dot
used by the green, orange, and stopped states, with no extra label or animation.

## Alternatives Considered

### Separate external-server monitor

An independent monitor could scan inactive ports while the existing snapshot
continued to scan live ports. This duplicates operating-system queries, caches,
timers, and ownership rules for no additional user-visible capability.

### Probe ports during indicator rendering

Deriving status directly from the operating system would make the view current
at render time, but it would put system calls or child-process execution in a
hot UI path and repeat work across renders and commands.

### Scan only visible sidebar rows

Visibility-based sampling would reduce the requested port set but couple
runtime truth to UI expansion state. On Windows the dominant work is reading
the listener tables, not filtering a small set of configured ports, so this
complexity provides little benefit.

## Testing

Focused unit tests will prove:

- a configured external listener produces `External` without a session;
- stopped, exited, crashed, and failed sessions become `External` when another
  process owns the configured port;
- a running managed owner remains `Ready`;
- a running session with an external owner becomes `External`;
- a running session with no listener remains `Unready`;
- `Starting` and `Stopping` retain their existing precedence;
- commands without ports retain existing behavior;
- every configured port is tracked once, including inactive commands;
- the refresh interval is one second with a live server and three seconds when
  all servers are inactive;
- `External` maps to the dedicated blue dot color and has no text label.

The complete Rust library suite will run serially as
`cargo test --lib -- --test-threads=1`. Before and after verification, the
installed DevManager PID/start time and production `config.json` and
`remote.json` hashes will be compared. Cargo, rustc, and generated test
executables will be confirmed stopped after the run.

## Success Criteria

Within about three seconds of an externally started server beginning to listen,
the configured sidebar row turns blue. When DevManager owns the listener it is
green, and when DevManager is still starting its command it is orange. Port
sampling performs no operating-system work on the render thread, remains
batched, never overlaps, and does not affect the installed DevManager process
or profile.

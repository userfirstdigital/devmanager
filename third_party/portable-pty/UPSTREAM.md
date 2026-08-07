# portable-pty patch provenance

- Crate: `portable-pty` 0.9.0 (MIT)
- Upstream repository: <https://github.com/wezterm/wezterm>
- Exact crates.io source commit: `f8921727a11b9f8b073e8c24821d72fd41283500`
- Patch purpose: expose a Windows-only, type-state ConPTY launch that creates a
  root suspended and atomically assigns it to a caller-owned Job Object before
  returning either process handle to DevManager.

The ordinary `SlavePty::spawn_command` contract remains unchanged. The patch is
kept within the standalone 0.9.0 crate source so it can be submitted upstream
without carrying or replacing the rest of WezTerm. Remove this local path
override after a released upstream version provides the same boundary.

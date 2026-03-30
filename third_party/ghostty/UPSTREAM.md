# Ghostty Vendor Notes

This directory contains vendored shell-integration resources copied from the
upstream Ghostty repository:

- Upstream repository: `https://github.com/ghostty-org/ghostty`
- Source path: `src/shell-integration/`
- Downloaded into this repo: `2026-03-30`
- Local purpose: native shell integration for terminal prompt marks and related
  shell UX in DevManager

Commit pinning was not captured at download time. If these files are refreshed,
record the upstream commit hash here and keep the copied file list below in sync.

Copied files:

- `shell-integration/README.md`
- `shell-integration/bash/bash-preexec.sh`
- `shell-integration/bash/devmanager.bashrc`
- `shell-integration/bash/ghostty.bash`
- `shell-integration/fish/vendor_conf.d/ghostty-shell-integration.fish`
- `shell-integration/nushell/vendor/autoload/ghostty.nu`
- `shell-integration/zsh/.zshenv`
- `shell-integration/zsh/ghostty-integration`

License notes:

- Upstream Ghostty shell-integration files retain their original headers.
- Some upstream files note GPLv3 inheritance from Kitty-derived integration
  code. Keep those headers intact when syncing updates.

# DevManager wrapper for the vendored Ghostty bash integration.
# It keeps normal interactive startup behavior simple while loading Ghostty's
# prompt markers from the real upstream script.

if [ -f "$HOME/.bashrc" ]; then
  . "$HOME/.bashrc"
fi

if [ -n "${GHOSTTY_RESOURCES_DIR:-}" ] && [ -f "$GHOSTTY_RESOURCES_DIR/shell-integration/bash/ghostty.bash" ]; then
  . "$GHOSTTY_RESOURCES_DIR/shell-integration/bash/ghostty.bash"
fi
# DevManager wrapper for the vendored Ghostty bash integration.
# It keeps normal interactive startup behavior simple while loading Ghostty's
# prompt markers from the real upstream script.
#
# Because bash's --rcfile flag is incompatible with --login, this wrapper
# emulates login shell behavior by sourcing the first available login
# profile before the normal interactive startup file (~/.bashrc).

# Source login profile (first match wins, per bash(1) INVOCATION).
for _devmanager_rcfile in "$HOME/.bash_profile" "$HOME/.bash_login" "$HOME/.profile"; do
  if [ -r "$_devmanager_rcfile" ]; then
    . "$_devmanager_rcfile"
    break
  fi
done
unset _devmanager_rcfile

# Source interactive config. If the login profile already sourced ~/.bashrc,
# this is a no-op in well-written configs. If it didn't, the user still
# gets their interactive setup (aliases, prompt, completions, etc.).
if [ -f "$HOME/.bashrc" ]; then
  . "$HOME/.bashrc"
fi

if [ -n "${GHOSTTY_RESOURCES_DIR:-}" ] && [ -f "$GHOSTTY_RESOURCES_DIR/shell-integration/bash/ghostty.bash" ]; then
  . "$GHOSTTY_RESOURCES_DIR/shell-integration/bash/ghostty.bash"
fi

// Shared preferred PTY size — updated after every successful fitAddon.fit().
// Used by all PTY creation paths so new sessions start near the real terminal
// dimensions, avoiding a visible reflow on first mount.

let preferredCols = 120;
let preferredRows = 30;

export function getPreferredPtySize(): { cols: number; rows: number } {
  return { cols: preferredCols, rows: preferredRows };
}

export function setPreferredPtySize(cols: number, rows: number) {
  if (cols > 0 && rows > 0) {
    preferredCols = cols;
    preferredRows = rows;
  }
}

/** Check if viewport is at the bottom of the terminal buffer */
export function isAtBottom(terminal: { buffer: { active: { viewportY: number; baseY: number } } }): boolean {
  const b = terminal.buffer.active;
  return b.viewportY >= b.baseY;
}

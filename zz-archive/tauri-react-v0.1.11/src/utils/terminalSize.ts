// Shared preferred PTY size — updated after every successful fitAddon.fit().
// Used by all PTY creation paths so new sessions start near the real terminal
// dimensions, avoiding a visible reflow on first mount.

let preferredCols = 120;
let preferredRows = 30;

export interface TerminalScrollState {
  pinnedToBottom: boolean;
  viewportY: number;
}

interface ScrollStateTerminal {
  buffer: { active: { viewportY: number; baseY: number } };
  scrollToBottom: () => void;
  scrollToLine: (line: number) => void;
}

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

export function snapshotTerminalScrollState(
  terminal: { buffer: { active: { viewportY: number; baseY: number } } },
  pinnedToBottom = isAtBottom(terminal),
): TerminalScrollState {
  return {
    pinnedToBottom,
    viewportY: terminal.buffer.active.viewportY,
  };
}

export function restoreTerminalScrollState(
  terminal: ScrollStateTerminal,
  viewportElement: HTMLElement | null,
  state: TerminalScrollState,
) {
  if (state.pinnedToBottom) {
    terminal.scrollToBottom();
    if (viewportElement) {
      const syncViewport = () => {
        viewportElement.scrollTop = Math.max(
          viewportElement.scrollHeight - viewportElement.clientHeight,
          0,
        );
      };
      syncViewport();
      requestAnimationFrame(syncViewport);
    }
    return;
  }

  const targetViewportY = Math.max(
    0,
    Math.min(state.viewportY, terminal.buffer.active.baseY),
  );
  terminal.scrollToLine(targetViewportY);
}

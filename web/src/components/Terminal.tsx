import { useEffect, useRef } from "react";
import { Terminal as XTerm } from "@xterm/xterm";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { CanvasAddon } from "@xterm/addon-canvas";
import "@xterm/xterm/css/xterm.css";
import { useStore } from "../store";
import type {
  TerminalCellSnapshot,
  TerminalScreenSnapshot,
} from "../api/types";
import type { SessionBootstrapFrame, SessionOutputFrame } from "../api/ws";

interface TerminalProps {
  sessionId: string;
}

// Theme cribbed verbatim from zz-archive/.../InteractiveTerminal.tsx so the
// web UI feels identical to the native desktop app. Any future palette
// tweaks should land in both places.
const XTERM_THEME = {
  background: "#09090b",
  foreground: "#c8c8cd",
  cursor: "#e4e4e7",
  selectionBackground: "#3f3f46",
  black: "#18181b",
  red: "#ef4444",
  green: "#22c55e",
  yellow: "#eab308",
  blue: "#3b82f6",
  magenta: "#a855f7",
  cyan: "#06b6d4",
  white: "#e4e4e7",
  brightBlack: "#52525b",
  brightRed: "#f87171",
  brightGreen: "#4ade80",
  brightYellow: "#facc15",
  brightBlue: "#60a5fa",
  brightMagenta: "#c084fc",
  brightCyan: "#22d3ee",
  brightWhite: "#fafafa",
} as const;

const FONT_FAMILY =
  '"Cascadia Code", "Fira Code", "JetBrains Mono", Consolas, monospace';

const BLANK_CELL: TerminalCellSnapshot = {
  character: " ",
  zero_width: [],
  foreground: 0xc8c8cd,
  background: 0x09090b,
  bold: false,
  dim: false,
  italic: false,
  underline: false,
  undercurl: false,
  strike: false,
  hidden: false,
  has_hyperlink: false,
  default_background: true,
};

function rgbParts(color: number): [number, number, number] {
  return [(color >> 16) & 0xff, (color >> 8) & 0xff, color & 0xff];
}

function cellStyleKey(cell: TerminalCellSnapshot): string {
  return [
    cell.foreground,
    cell.background,
    cell.bold,
    cell.dim,
    cell.italic,
    cell.underline,
    cell.undercurl,
    cell.strike,
    cell.hidden,
    cell.default_background,
  ].join("|");
}

function cellStyleAnsi(cell: TerminalCellSnapshot): string {
  const [fr, fg, fb] = rgbParts(cell.foreground);
  const codes = ["0", `38;2;${fr};${fg};${fb}`];
  if (cell.bold) codes.push("1");
  if (cell.dim) codes.push("2");
  if (cell.italic) codes.push("3");
  if (cell.underline || cell.undercurl) codes.push("4");
  if (cell.strike) codes.push("9");
  if (cell.hidden) codes.push("8");
  if (cell.default_background) {
    codes.push("49");
  } else {
    const [br, bg, bb] = rgbParts(cell.background);
    codes.push(`48;2;${br};${bg};${bb}`);
  }
  return `\u001b[${codes.join(";")}m`;
}

function screenSnapshotToAnsi(screen: TerminalScreenSnapshot): string {
  let out = "\u001b[0m\u001b[?25l\u001b[H\u001b[2J";
  for (let row = 0; row < screen.rows; row++) {
    out += `\u001b[${row + 1};1H`;
    let lastStyle = "";
    for (let col = 0; col < screen.cols; col++) {
      const cell = screen.lines[row]?.[col] ?? BLANK_CELL;
      const nextStyle = cellStyleKey(cell);
      if (nextStyle !== lastStyle) {
        out += cellStyleAnsi(cell);
        lastStyle = nextStyle;
      }
      out += cell.hidden ? " " : cell.character;
      if (cell.zero_width.length > 0) {
        out += cell.zero_width.join("");
      }
    }
  }
  out += "\u001b[0m";
  if (screen.cursor) {
    out += `\u001b[${screen.cursor.row + 1};${screen.cursor.column + 1}H\u001b[?25h`;
  }
  return out;
}

function refreshTerminal(terminal: XTerm): void {
  requestAnimationFrame(() => {
    try {
      terminal.refresh(0, terminal.rows - 1);
    } catch {}
  });
}

function applyBootstrapToTerminal(
  terminal: XTerm,
  bootstrap: SessionBootstrapFrame,
): void {
  const bootstrapPayload =
    bootstrap.screen.rows > 0 && bootstrap.screen.cols > 0
      ? screenSnapshotToAnsi(bootstrap.screen)
      : bootstrap.bytes;
  if (
    (bootstrapPayload instanceof Uint8Array && bootstrapPayload.byteLength === 0) ||
    (typeof bootstrapPayload === "string" && bootstrapPayload.length === 0)
  ) {
    return;
  }
  terminal.write(bootstrapPayload, () => refreshTerminal(terminal));
}

export function TerminalView({ sessionId }: TerminalProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const terminalRef = useRef<XTerm | null>(null);
  const client = useStore((s) => s.client);
  const subscribeTerminal = useStore((s) => s.subscribeTerminal);
  const subscribeBootstrap = useStore((s) => s.subscribeBootstrap);
  const drainBootstrap = useStore((s) => s.drainBootstrap);
  const sendInput = useStore((s) => s.sendInput);
  const youHaveControl = useStore(
    (s) => s.snapshot?.youHaveControl ?? false,
  );
  // The host's PTY dimensions ARE the authoritative size. The browser xterm
  // mirrors them exactly — it never resizes the PTY — so the native desktop
  // app's terminal rendering stays stable when a browser client is connected.
  // When the native window resizes, `runtimeState.sessions[id].dimensions`
  // updates via delta and we'll `terminal.resize(cols, rows)` to match.
  const hostCols = useStore(
    (s) =>
      s.snapshot?.runtimeState?.sessions?.[sessionId]?.dimensions?.cols ?? 100,
  );
  const hostRows = useStore(
    (s) =>
      s.snapshot?.runtimeState?.sessions?.[sessionId]?.dimensions?.rows ?? 30,
  );
  const controlRef = useRef(youHaveControl);
  controlRef.current = youHaveControl;

  useEffect(() => {
    let cancelled = false;
    let cleanup: (() => void) | null = null;
    let retryTimer: number | null = null;

    // Effects run AFTER React commits, so the container is already in the
    // DOM and measurable. We don't need requestAnimationFrame — that's only
    // needed for StrictMode's development-mode double-invoke, which we
    // deliberately disabled for xterm's sake. If the container happens to
    // be zero-sized on the first tick (rare — only during layout thrash),
    // fall back to a setTimeout retry rather than rAF so background tabs
    // don't get throttled to death by Chrome.
    const mount = () => {
      if (cancelled) return;
      const container = containerRef.current;
      if (
        !container ||
        !container.isConnected ||
        container.clientWidth === 0 ||
        container.clientHeight === 0
      ) {
        retryTimer = window.setTimeout(mount, 50);
        return;
      }

      const terminal = new XTerm({
        theme: XTERM_THEME,
        fontFamily: FONT_FAMILY,
        fontSize: 13,
        lineHeight: 1.3,
        scrollback: 10000,
        cursorStyle: "bar",
        cursorBlink: true,
        convertEol: false,
        smoothScrollDuration: 0,
        // Start the xterm at the host's PTY dimensions, NOT at whatever the
        // browser's container happens to be. `terminal.resize()` below keeps
        // it in sync when the host's dimensions change via delta.
        cols: hostCols,
        rows: hostRows,
      });

      terminal.loadAddon(new WebLinksAddon());
      terminal.open(container);
      // Canvas renderer is far more reliable than xterm 5.5's DOM renderer
      // when pumping large ANSI replays through `.write()`. Load AFTER open()
      // so xterm has a core render service to swap from. Swallow failures so
      // headless/unusual environments can still fall back to DOM.
      try {
        terminal.loadAddon(new CanvasAddon());
      } catch {}

      // Suppress xterm's automatic replies to terminal-query CSI sequences.
      // The host replays raw PTY bytes including things like `\x1b[6n`
      // (Device Status Report cursor position). If we let xterm auto-answer
      // those via its `onData` event, we'd end up sending `\x1b[<row>;<col>R`
      // through `sendInput` into the host shell's stdin — bash readline
      // rejects the escape prefix and the trailing `R` falls through as a
      // literal character prepended to the next command (the user's bug
      // report: `Rnpx` instead of `npx`). Registering handlers that return
      // `true` tells xterm "I handled this" so it skips its default reply.
      //
      //   `n` → DSR (Device Status Report) — cursor pos, operating status
      //   `c` → DA  (Device Attributes)    — primary / secondary
      //   `q` → XTVERSION et al            — terminal identification
      const suppressCsi = () => true;
      try {
        terminal.parser.registerCsiHandler({ final: "n" }, suppressCsi);
        terminal.parser.registerCsiHandler({ final: "c" }, suppressCsi);
        terminal.parser.registerCsiHandler({ final: "q" }, suppressCsi);
      } catch {}

      terminalRef.current = terminal;
      (window as unknown as { __dmTerm?: unknown }).__dmTerm = terminal;

      // Re-send the subscription once the terminal is actually mounted.
      // This gives the host another chance to eagerly push bootstrap bytes
      // if the earlier sidebar-level subscribe happened before the PTY was
      // fully ready.
      client?.send({ type: "subscribeSessions", sessionIds: [sessionId] });

      // Copy-on-Ctrl+C when there's a selection, paste handled by browser
      // default on Ctrl+V. Matches the archive's custom handler at
      // InteractiveTerminal.tsx lines 304-327.
      terminal.attachCustomKeyEventHandler((event) => {
        if (event.type !== "keydown") return true;
        if (event.ctrlKey && !event.shiftKey && event.key === "c") {
          const selection = terminal.getSelection();
          if (selection && selection.length > 0) {
            navigator.clipboard?.writeText(selection).catch(() => {});
            terminal.clearSelection();
            return false;
          }
        }
        return true;
      });

      // Forward user keystrokes to the host — but only when this client
      // holds control. Viewer-mode input silently drops.
      const dataDisposable = terminal.onData((text) => {
        if (!controlRef.current) return;
        sendInput(sessionId, text);
      });
      // We intentionally do NOT hook `terminal.onResize` to `sendResize`.
      // The browser is a passive mirror of the host's PTY size; if we
      // forwarded resize events the PTY would flip between the native's
      // desired cols/rows and the browser's computed cols/rows on every
      // render, desyncing Claude Code's absolute-position TUI drawing.

      // Subscribe to live output frames.
      const unsubscribe = subscribeTerminal(
        sessionId,
        (frame: SessionOutputFrame) => {
          terminal.write(frame.bytes);
        },
      );
      const unsubscribeBootstrap = subscribeBootstrap(sessionId, (bootstrap) => {
        applyBootstrapToTerminal(terminal, bootstrap);
      });

      // Drain any bootstrap scrollback that arrived before mount, then
      // schedule a refresh in the next frame so xterm's render service has
      // a chance to mark every row dirty and re-paint.
      const bootstrap = drainBootstrap(sessionId);
      if (bootstrap) {
        applyBootstrapToTerminal(terminal, bootstrap);
      }

      // Focus after first paint.
      const focusTimer = window.setTimeout(() => {
        if (!cancelled) terminal.focus();
      }, 50);

      cleanup = () => {
        clearTimeout(focusTimer);
        unsubscribeBootstrap();
        unsubscribe();
        dataDisposable.dispose();
        terminal.dispose();
        terminalRef.current = null;
      };
    };

    mount();

    return () => {
      cancelled = true;
      if (retryTimer !== null) clearTimeout(retryTimer);
      cleanup?.();
    };
    // We deliberately do NOT put `hostCols`/`hostRows` in this dep array;
    // resizing is handled by a separate effect below so we don't tear down
    // the whole xterm on every dimension delta.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [
    sessionId,
    client,
    subscribeTerminal,
    subscribeBootstrap,
    drainBootstrap,
    sendInput,
  ]);

  // Keep the xterm in lockstep with the host's PTY dimensions. Runs on the
  // same tick React applies the delta-merged snapshot, so the browser render
  // always paints against the same cols/rows the host believes the PTY has.
  useEffect(() => {
    const terminal = terminalRef.current;
    if (!terminal) return;
    if (terminal.cols !== hostCols || terminal.rows !== hostRows) {
      try {
        terminal.resize(hostCols, hostRows);
      } catch {}
    }
  }, [hostCols, hostRows]);

  return (
    <div
      ref={containerRef}
      className="flex-1 min-h-0 bg-[#09090b]"
      style={{ padding: 0 }}
    />
  );
}

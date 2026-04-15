import { useState } from "react";
import { useStore } from "../store";

interface MobileKeyRowProps {
  sessionId: string;
}

/**
 * Touch-friendly helper row that sits above the mobile keyboard and emits
 * the common modifier/control characters an xterm terminal needs but an
 * iOS/Android keyboard does not offer. Matches the classic WebSSH / gotty /
 * Blink Shell layout.
 *
 * `Ctrl` is sticky: tap once, the next tapped letter gets translated into
 * its ASCII control code (`^X`).
 */
export function MobileKeyRow({ sessionId }: MobileKeyRowProps) {
  const sendInput = useStore((s) => s.sendInput);
  const [ctrlArmed, setCtrlArmed] = useState(false);

  const send = (text: string) => sendInput(sessionId, text);

  const onKey = (label: string, payload: string) => {
    if (label === "Ctrl") {
      setCtrlArmed((prev) => !prev);
      return;
    }
    if (ctrlArmed && /^[a-zA-Z]$/.test(payload)) {
      const code = payload.toUpperCase().charCodeAt(0) - 64;
      send(String.fromCharCode(code));
      setCtrlArmed(false);
      return;
    }
    send(payload);
  };

  const btnBase =
    "h-10 min-w-[36px] px-2 rounded text-xs font-mono text-zinc-100 bg-zinc-700 active:bg-zinc-600";

  return (
    <div className="md:hidden flex items-center gap-1 overflow-x-auto px-2 py-2 bg-zinc-800 border-t border-zinc-700 scrollbar-none">
      <button
        type="button"
        className={btnBase}
        onClick={() => onKey("Esc", "\u001b")}
      >
        Esc
      </button>
      <button
        type="button"
        className={btnBase}
        onClick={() => onKey("Tab", "\t")}
      >
        Tab
      </button>
      <button
        type="button"
        className={`${btnBase} ${ctrlArmed ? "bg-indigo-600" : ""}`}
        onClick={() => onKey("Ctrl", "")}
      >
        Ctrl
      </button>
      {["C", "D", "Z", "L"].map((letter) => (
        <button
          key={letter}
          type="button"
          className={btnBase}
          onClick={() => onKey(letter, letter.toLowerCase())}
        >
          {letter}
        </button>
      ))}
      <button
        type="button"
        className={btnBase}
        onClick={() => onKey("Up", "\u001bOA")}
      >
        ↑
      </button>
      <button
        type="button"
        className={btnBase}
        onClick={() => onKey("Down", "\u001bOB")}
      >
        ↓
      </button>
      <button
        type="button"
        className={btnBase}
        onClick={() => onKey("Left", "\u001bOD")}
      >
        ←
      </button>
      <button
        type="button"
        className={btnBase}
        onClick={() => onKey("Right", "\u001bOC")}
      >
        →
      </button>
      <button
        type="button"
        className={btnBase}
        onClick={() => onKey("Pipe", "|")}
      >
        |
      </button>
      <button
        type="button"
        className={btnBase}
        onClick={() => onKey("Slash", "/")}
      >
        /
      </button>
      <button
        type="button"
        className={btnBase}
        onClick={() => onKey("Tilde", "~")}
      >
        ~
      </button>
    </div>
  );
}

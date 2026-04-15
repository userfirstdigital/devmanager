import { useEffect, useRef, useState } from "react";
import { useStore } from "../store";
import { pickMobileKeysForWidth } from "./mobileKeyLayout";

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
  const rowRef = useRef<HTMLDivElement>(null);
  const [availableWidth, setAvailableWidth] = useState(() =>
    typeof window !== "undefined" ? window.innerWidth : 0,
  );

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
    "h-8 min-w-[28px] px-1 rounded text-[10px] font-mono whitespace-nowrap text-zinc-100 bg-zinc-700 active:bg-zinc-600 shrink-0";

  useEffect(() => {
    const element = rowRef.current;
    if (!element) {
      return;
    }

    const updateWidth = (nextWidth?: number) => {
      const measured = nextWidth ?? element.clientWidth;
      setAvailableWidth(Math.round(measured));
    };

    updateWidth();

    if (typeof ResizeObserver !== "undefined") {
      const observer = new ResizeObserver((entries) => {
        updateWidth(entries[0]?.contentRect.width);
      });
      observer.observe(element);
      return () => observer.disconnect();
    }

    const onResize = () => updateWidth();
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, []);

  const keys = pickMobileKeysForWidth(availableWidth);

  return (
    <div
      ref={rowRef}
      className="md:hidden flex items-center justify-between gap-0.5 overflow-hidden px-1 py-1.5 bg-zinc-800 border-t border-zinc-700"
    >
      {keys.map((key) => (
        <button
          key={key.label}
          type="button"
          className={`${btnBase} ${
            key.label === "Ctrl" && ctrlArmed ? "bg-indigo-600" : ""
          } ${key.label === "Enter" ? "min-w-[38px]" : ""}`}
          onClick={() => onKey(key.label, key.payload)}
        >
          {key.label === "Up"
            ? "↑"
            : key.label === "Down"
              ? "↓"
              : key.label === "Right"
                ? "→"
              : key.label}
        </button>
      ))}
    </div>
  );
}

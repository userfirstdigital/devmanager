import { describe, expect, it, vi } from "vitest";
import {
  classifyTerminalInputKind,
  forwardTerminalTextPaste,
} from "./terminalInput";

describe("terminal input provenance", () => {
  it("keeps printable and IME text distinct from terminal control bytes", () => {
    for (const text of ["hello", "café", "漢字", "\u0301", "👩‍💻"]) {
      expect(classifyTerminalInputKind(text)).toBe("text");
    }

    for (const text of ["\r", "\n", "\t", "\x03", "\x1b[A", "\x7f", "\u0085"]) {
      expect(classifyTerminalInputKind(text)).toBe("bytes");
    }
  });

  it("forwards a clipboard text paste exactly once as Paste provenance", () => {
    const preventDefault = vi.fn();
    const send = vi.fn();
    const handled = forwardTerminalTextPaste(
      {
        clipboardData: {
          getData: (kind: string) => (kind === "text/plain" ? "pasted text" : ""),
        },
        preventDefault,
      },
      send,
    );

    expect(handled).toBe(true);
    expect(preventDefault).toHaveBeenCalledTimes(1);
    expect(send).toHaveBeenCalledTimes(1);
    expect(send).toHaveBeenCalledWith("pasted text", "paste");
  });
});

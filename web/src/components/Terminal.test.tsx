// @vitest-environment jsdom

import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useStore } from "../store";
import { TerminalView } from "./Terminal";

vi.mock("@xterm/addon-web-links", () => ({
  WebLinksAddon: class {},
}));

vi.mock("@xterm/addon-canvas", () => ({
  CanvasAddon: class {},
}));

vi.mock("@xterm/xterm", () => {
  class Terminal {
    rows: number;
    cols: number;
    options: { fontSize?: number };
    element: HTMLElement | null = null;
    parser = {
      registerCsiHandler: vi.fn(() => ({ dispose() {} })),
    };
    private readonly dataListeners = new Set<(text: string) => void>();

    constructor(options: { rows?: number; cols?: number; fontSize?: number }) {
      this.rows = options.rows ?? 30;
      this.cols = options.cols ?? 100;
      this.options = { fontSize: options.fontSize };
    }

    loadAddon() {}

    open(container: HTMLElement) {
      const element = document.createElement("div");
      element.className = "xterm";
      element.getBoundingClientRect = () =>
        ({
          width: 800,
          height: 600,
          top: 0,
          right: 800,
          bottom: 600,
          left: 0,
          x: 0,
          y: 0,
          toJSON() {},
        }) as DOMRect;
      const textarea = document.createElement("textarea");
      element.appendChild(textarea);
      container.appendChild(element);
      this.element = element;

      // Match xterm's real browser path: Terminal.open() installs the same
      // paste wrapper on its textarea and terminal element. Clipboard.ts then
      // emits the clipboard's text through onData even when preventDefault was
      // already called by an ancestor.
      const handleXtermPaste = (event: ClipboardEvent) => {
        event.stopPropagation();
        const text = event.clipboardData?.getData("text/plain") ?? "";
        this.emitData(text.replace(/\r?\n/g, "\r"));
        textarea.value = "";
      };
      textarea.addEventListener("paste", handleXtermPaste);
      element.addEventListener("paste", handleXtermPaste);
      textarea.addEventListener("keydown", (event) => {
        if (event.key.length === 1) {
          this.emitData(event.key);
        }
      });
    }

    private emitData(text: string) {
      for (const listener of this.dataListeners) {
        listener(text);
      }
    }

    onData(listener: (text: string) => void) {
      this.dataListeners.add(listener);
      return { dispose: () => this.dataListeners.delete(listener) };
    }

    attachCustomKeyEventHandler() {}
    getSelection() {
      return "";
    }
    clearSelection() {}
    write(_payload: string | Uint8Array, callback?: () => void) {
      callback?.();
    }
    refresh() {}
    resize(cols: number, rows: number) {
      this.cols = cols;
      this.rows = rows;
    }
    focus() {}
    dispose() {
      this.element?.remove();
      this.element = null;
      this.dataListeners.clear();
    }
  }

  return { Terminal };
});

function configureTerminalStore() {
  const sendInput = vi.fn();
  const pasteImage = vi.fn();
  useStore.setState({
    snapshot: {
      appState: {
        config: { projects: [], sshConnections: [] },
        open_tabs: [],
      },
      runtimeState: {
        sessions: {
          "pty-a": {
            session_id: "pty-a",
            stable_session_key: "tab:tab-a",
            pid: 1,
            status: "Running",
            session_kind: "claude",
            command_id: null,
            project_id: "project-a",
            tab_id: "tab-a",
            exit_code: null,
            title: null,
            dimensions: {
              cols: 100,
              rows: 30,
              cell_width: 8,
              cell_height: 16,
            },
          },
        },
      },
      portStatuses: {},
      controllerClientId: null,
      youHaveControl: true,
      serverId: "server-a",
    },
    subscribeTerminal: vi.fn(() => () => {}),
    subscribeBootstrap: vi.fn(() => () => {}),
    drainBootstrap: vi.fn(() => null),
    drainTerminalFrames: vi.fn(() => []),
    sendInput,
    pasteImage,
  });
  return { sendInput, pasteImage };
}

function dispatchClipboardPaste(
  target: HTMLElement,
  text: string,
  items: Array<{ type: string; getAsFile: () => File | null }>,
) {
  const event = new Event("paste", {
    bubbles: true,
    cancelable: true,
  }) as ClipboardEvent;
  Object.defineProperty(event, "clipboardData", {
    value: {
      getData: (type: string) => (type === "text/plain" ? text : ""),
      items,
    },
  });
  fireEvent(target, event);
}

beforeEach(() => {
  vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockReturnValue(800);
  vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockReturnValue(600);
  vi.stubGlobal(
    "requestAnimationFrame",
    (callback: FrameRequestCallback) => {
      callback(0);
      return 1;
    },
  );
  vi.stubGlobal("cancelAnimationFrame", vi.fn());
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe("TerminalView clipboard provenance", () => {
  it("delivers one clipboard text event as one Paste without xterm onData replay", () => {
    const { sendInput } = configureTerminalStore();
    const view = render(<TerminalView sessionId="pty-a" />);
    const textarea = view.container.querySelector("textarea");
    expect(textarea).not.toBeNull();

    dispatchClipboardPaste(textarea!, "pasted text", [
      { type: "text/plain", getAsFile: () => null },
    ]);

    expect(sendInput.mock.calls).toEqual([
      ["pty-a", "pasted text", "paste"],
    ]);
  });

  it("leaves ordinary xterm key input on the Text path", () => {
    const { sendInput } = configureTerminalStore();
    const view = render(<TerminalView sessionId="pty-a" />);
    const textarea = view.container.querySelector("textarea");
    expect(textarea).not.toBeNull();

    fireEvent.keyDown(textarea!, { key: "a" });

    expect(sendInput.mock.calls).toEqual([["pty-a", "a", "text"]]);
  });

  it("delivers one image clipboard event only through pasteImage", async () => {
    const { sendInput, pasteImage } = configureTerminalStore();
    const view = render(<TerminalView sessionId="pty-a" />);
    const textarea = view.container.querySelector("textarea");
    expect(textarea).not.toBeNull();
    const file = new File([new Uint8Array([1, 2, 3])], "clip.png", {
      type: "image/png",
    });

    dispatchClipboardPaste(textarea!, "image fallback text", [
      { type: "image/png", getAsFile: () => file },
    ]);

    await waitFor(() => {
      expect(pasteImage).toHaveBeenCalledTimes(1);
    });
    expect(pasteImage).toHaveBeenCalledWith("pty-a", {
      mimeType: "image/png",
      fileName: "clip.png",
      dataBase64: "AQID",
    });
    expect(sendInput).not.toHaveBeenCalled();
  });
});

import type { WebTerminalInputKind } from "../api/types";

const TERMINAL_CONTROL = /[\u0000-\u001f\u007f-\u009f]/u;

export function classifyTerminalInputKind(text: string): WebTerminalInputKind {
  return text.length > 0 && !TERMINAL_CONTROL.test(text) ? "text" : "bytes";
}

interface TerminalClipboardEvent {
  clipboardData: Pick<DataTransfer, "getData"> | null;
  preventDefault(): void;
}

export function forwardTerminalTextPaste(
  event: TerminalClipboardEvent,
  send: (text: string, inputKind: WebTerminalInputKind) => void,
): boolean {
  const text = event.clipboardData?.getData("text/plain") ?? "";
  if (text.length === 0) return false;
  event.preventDefault();
  send(text, "paste");
  return true;
}

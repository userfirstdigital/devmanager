import type { SemanticAdapterHealth, WebSessionKind } from "../api/types";

export interface ViewModeInput {
  adapterHealth: SemanticAdapterHealth;
  ai: boolean;
  gridInteractionRequired: boolean;
  pinned: boolean;
}

export type SessionViewMode = "semantic" | "terminal";

export function resolveViewMode(input: ViewModeInput): SessionViewMode {
  return input.gridInteractionRequired || input.pinned ? "terminal" : "semantic";
}

export type NativeSessionView = "ai" | "server" | "command";

export function resolveNativeSessionView(
  kind: WebSessionKind,
  interactiveShell: boolean,
): NativeSessionView {
  if (kind === "claude" || kind === "codex") return "ai";
  if (kind === "server" && !interactiveShell) return "server";
  return "command";
}

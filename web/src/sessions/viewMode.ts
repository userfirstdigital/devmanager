import type { SemanticAdapterHealth } from "../api/types";

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

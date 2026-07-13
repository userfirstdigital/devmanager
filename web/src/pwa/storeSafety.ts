import type { UpdateSafetyState } from "./register";

interface ComposerSafetyStore {
  drafts?: Record<string, string>;
  pendingMutations?: Record<string, unknown>;
}

export function readStoreUpdateSafetyState(
  state: unknown,
): UpdateSafetyState {
  if (typeof state !== "object" || state === null) {
    return { hasDraft: true, pendingMutations: 0 };
  }
  const composerState = state as ComposerSafetyStore;
  if (!composerState.drafts || !composerState.pendingMutations) {
    return { hasDraft: true, pendingMutations: 0 };
  }
  return {
    hasDraft: Object.values(composerState.drafts).some(
      (text) => text.trim() !== "",
    ),
    pendingMutations: Object.keys(composerState.pendingMutations).length,
  };
}

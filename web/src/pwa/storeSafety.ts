import { hasExactDraftHandoff } from "../drafts/draftStore";
import type { UpdateSafetyState } from "./register";

interface ComposerSafetyStore {
  runtimeInstanceId?: string | null;
  drafts?: Record<string, string>;
  compatibleDraftHandoffTargetBuildId?: string | null;
  pendingMutations?: Record<string, unknown>;
  composerSafety?: Record<
    string,
    { selectedAttachments?: number; attachmentLoads?: number }
  >;
}

export function readStoreUpdateSafetyState(
  state: unknown,
): UpdateSafetyState {
  if (typeof state !== "object" || state === null) {
    return {
      hasDraft: true,
      pendingMutations: 0,
      selectedAttachments: 0,
      attachmentLoads: 0,
    };
  }
  const composerState = state as ComposerSafetyStore;
  if (
    !composerState.drafts ||
    !composerState.pendingMutations ||
    !composerState.composerSafety
  ) {
    return {
      hasDraft: true,
      pendingMutations: 0,
      selectedAttachments: 0,
      attachmentLoads: 0,
    };
  }
  const safetyValues = Object.values(composerState.composerSafety);
  const hasDraft = Object.values(composerState.drafts).some(
    (text) => text !== "",
  );
  const exactHandoffReady =
    hasDraft &&
    typeof composerState.compatibleDraftHandoffTargetBuildId === "string" &&
    typeof composerState.runtimeInstanceId === "string" &&
    hasExactDraftHandoff(
      composerState.compatibleDraftHandoffTargetBuildId,
      composerState.runtimeInstanceId,
      composerState.drafts,
    );
  return {
    // Preserve the exact text. Whitespace can be intentional terminal input.
    hasDraft: hasDraft && !exactHandoffReady,
    pendingMutations: Object.keys(composerState.pendingMutations).length,
    selectedAttachments: safetyValues.reduce(
      (total, safety) => total + Math.max(0, safety.selectedAttachments ?? 0),
      0,
    ),
    attachmentLoads: safetyValues.reduce(
      (total, safety) => total + Math.max(0, safety.attachmentLoads ?? 0),
      0,
    ),
  };
}

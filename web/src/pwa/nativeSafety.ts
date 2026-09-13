import type { UpdateSafetyState } from "./register";

export type NativeProjectionSafetyView = {
  drafts: ReadonlyMap<unknown, unknown>;
  outbox: ReadonlyMap<unknown, unknown>;
};

let activeNativeSafety: UpdateSafetyState | null = null;

export function publishNativeUpdateSafetyState(
  state: UpdateSafetyState | null,
): void {
  activeNativeSafety = state;
}

export function readNativeUpdateSafetyState(): UpdateSafetyState | null {
  return activeNativeSafety;
}

function draftBlocks(view: NativeProjectionSafetyView): boolean {
  return [...view.drafts.values()].some((draft) => {
    // Unknown shapes fail closed; an explicitly cleared draft is not work.
    return (
      !draft ||
      typeof draft !== "object" ||
      !("text" in draft) ||
      (draft as { text: unknown }).text !== ""
    );
  });
}

/**
 * Native cache data is canonical for the Connect shell. Until its hydration
 * completes, a PWA replacement cannot prove it will preserve drafts/outbox.
 * Multi-host fleets aggregate across ALL registered hosts — any unresolved
 * hydrate or any draft/uncertain outbox blocks destructive reload.
 */
export function nativeUpdateSafetyState(
  view:
    | NativeProjectionSafetyView
    | null
    | readonly NativeProjectionSafetyView[],
  hydrationKnown: boolean,
): UpdateSafetyState {
  if (!hydrationKnown || view === null) {
    return { hasDraft: true, pendingMutations: 1 };
  }
  const views = Array.isArray(view) ? view : [view];
  if (views.length === 0) {
    return { hasDraft: true, pendingMutations: 1 };
  }
  let hasDraft = false;
  let pendingMutations = 0;
  for (const item of views) {
    if (draftBlocks(item)) hasDraft = true;
    // Every durable native outbox state is action continuity, including
    // uncertain and client-mismatch records that must not be erased by reload.
    pendingMutations += item.outbox.size;
  }
  return { hasDraft, pendingMutations };
}

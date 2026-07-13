// @vitest-environment jsdom

import { beforeEach, describe, expect, it } from "vitest";
import { stageDraftHandoff } from "../drafts/draftStore";
import { useStore } from "../store";
import { readStoreUpdateSafetyState } from "./storeSafety";

describe("readStoreUpdateSafetyState", () => {
  beforeEach(() => {
    localStorage.clear();
    sessionStorage.clear();
    useStore.setState(useStore.getInitialState(), true);
  });

  it("reads live non-empty drafts and pending mutations", () => {
    expect(
      readStoreUpdateSafetyState({
        drafts: { first: "  ", second: "keep this" },
        pendingMutations: { second: { mutationId: "mutation-1" } },
        composerSafety: {},
      }),
    ).toEqual({
      hasDraft: true,
      pendingMutations: 1,
      selectedAttachments: 0,
      attachmentLoads: 0,
    });
  });

  it("treats raw whitespace drafts as unsaved input", () => {
    expect(
      readStoreUpdateSafetyState({
        drafts: { first: " \n\t" },
        pendingMutations: {},
        composerSafety: {},
      }),
    ).toEqual({
      hasDraft: true,
      pendingMutations: 0,
      selectedAttachments: 0,
      attachmentLoads: 0,
    });
  });

  it("allows only a verified compatibility handoff to cover draft safety", () => {
    const handedOffState = {
      runtimeInstanceId: "runtime-a",
      drafts: { first: "  preserve exactly\n" },
      pendingMutations: {},
      composerSafety: {},
      compatibleDraftHandoffReady: true,
    };

    expect(readStoreUpdateSafetyState(handedOffState).hasDraft).toBe(true);
    expect(
      stageDraftHandoff("runtime-a", handedOffState.drafts),
    ).toBe(true);
    expect(
      readStoreUpdateSafetyState(handedOffState),
    ).toEqual({
      hasDraft: false,
      pendingMutations: 0,
      selectedAttachments: 0,
      attachmentLoads: 0,
    });

    expect(
      readStoreUpdateSafetyState({
        ...handedOffState,
        pendingMutations: { first: {} },
        composerSafety: {
          first: { selectedAttachments: 1, attachmentLoads: 1 },
        },
        compatibleDraftHandoffReady: true,
      }),
    ).toEqual({
      hasDraft: false,
      pendingMutations: 1,
      selectedAttachments: 1,
      attachmentLoads: 1,
    });
  });

  it("reads the actual Zustand composer state for every unsafe input kind", () => {
    const store = useStore.getState();
    store.setDraft("tab:a", "  \n");
    store.setComposerSafety("tab:a", {
      selectedAttachments: 1,
      attachmentLoads: 0,
    });
    store.setComposerSafety("tab:b", {
      selectedAttachments: 0,
      attachmentLoads: 1,
    });
    useStore.setState({
      pendingMutations: {
        "tab:c": {
          mutationId: "mutation-1",
          stableSessionKey: "tab:c",
          text: "send",
          attachments: [],
        },
      },
    });

    expect(readStoreUpdateSafetyState(useStore.getState())).toEqual({
      hasDraft: true,
      pendingMutations: 1,
      selectedAttachments: 1,
      attachmentLoads: 1,
    });
  });

  it("fails closed until the composer store fields are available", () => {
    expect(readStoreUpdateSafetyState({})).toEqual({
      hasDraft: true,
      pendingMutations: 0,
      selectedAttachments: 0,
      attachmentLoads: 0,
    });
  });
});

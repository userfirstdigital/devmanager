import { describe, expect, it } from "vitest";
import { readStoreUpdateSafetyState } from "./storeSafety";

describe("readStoreUpdateSafetyState", () => {
  it("reads live non-empty drafts and pending mutations", () => {
    expect(
      readStoreUpdateSafetyState({
        drafts: { first: "  ", second: "keep this" },
        pendingMutations: { second: { mutationId: "mutation-1" } },
      }),
    ).toEqual({ hasDraft: true, pendingMutations: 1 });
  });

  it("treats whitespace drafts and an empty journal as safe", () => {
    expect(
      readStoreUpdateSafetyState({
        drafts: { first: " \n\t" },
        pendingMutations: {},
      }),
    ).toEqual({ hasDraft: false, pendingMutations: 0 });
  });

  it("fails closed until the composer store fields are available", () => {
    expect(readStoreUpdateSafetyState({})).toEqual({
      hasDraft: true,
      pendingMutations: 0,
    });
  });
});

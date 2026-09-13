import { describe, expect, it } from "vitest";

import { nativeUpdateSafetyState } from "./nativeSafety";

function view(overrides: Record<string, unknown> = {}) {
  return {
    drafts: new Map(),
    outbox: new Map(),
    ...overrides,
  };
}

describe("nativeUpdateSafetyState", () => {
  it("fails closed before the native cache hydrate outcome is known", () => {
    expect(nativeUpdateSafetyState(null, false)).toEqual({
      hasDraft: true,
      pendingMutations: 1,
    });
  });

  it("blocks an update for every persisted native draft and outbox status", () => {
    expect(
      nativeUpdateSafetyState(
        view({
          drafts: new Map([["task", { text: "offline draft" }]]),
          outbox: new Map([
            ["pending", { status: "pending" }],
            ["inflight", { status: "in_flight" }],
            ["uncertain", { status: "uncertain" }],
            ["blocked", { status: "blocked_client_mismatch" }],
          ]),
        }),
        true,
      ),
    ).toEqual({ hasDraft: true, pendingMutations: 4 });
  });

  it("permits activation only after a known empty native projection", () => {
    expect(nativeUpdateSafetyState(view(), true)).toEqual({
      hasDraft: false,
      pendingMutations: 0,
    });
  });

  it("aggregates drafts and outbox across every registered host", () => {
    expect(
      nativeUpdateSafetyState(
        [
          view({
            drafts: new Map([["a", { text: "" }]]),
            outbox: new Map([["pending-a", { status: "pending" }]]),
          }),
          view({
            drafts: new Map([["b", { text: "still drafting" }]]),
            outbox: new Map([
              ["uncertain-b", { status: "uncertain" }],
              ["mismatch-b", { status: "blocked_client_mismatch" }],
            ]),
          }),
        ],
        true,
      ),
    ).toEqual({ hasDraft: true, pendingMutations: 3 });
  });

  it("fails closed when any host hydrate is still unresolved", () => {
    expect(
      nativeUpdateSafetyState([view(), view()], false),
    ).toEqual({ hasDraft: true, pendingMutations: 1 });
  });
});

import { describe, expect, it } from "vitest";

import {
  createUpdateContinuity,
  observePeerBundle,
  pairingRotated,
} from "./updateContinuity";

describe("update continuity", () => {
  const pairing = {
    pairingCodeGeneration: 3,
    hostIdentityFingerprint: "host",
    deviceKeyFingerprint: "device",
  };

  it("pauses mutations on bundle mismatch without implying pairing rotation", () => {
    const state = {
      ...createUpdateContinuity("bundle-a", pairing, 1, 0),
      localDraft: "keep",
    };
    const next = observePeerBundle(state, 1, 0, "bundle-b");
    expect(next.mutationsPaused).toBe(true);
    expect(next.reloadRequired).toBe(true);
    expect(next.localDraft).toBe("keep");
    expect(next.pairing).toEqual(pairing);
    expect(pairingRotated(pairing, pairing)).toBe(false);
  });
});

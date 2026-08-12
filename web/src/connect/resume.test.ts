import { describe, expect, it } from "vitest";

import { staleResumeRequiresRefresh } from "./resume";

describe("stale resume refresh", () => {
  it("keeps a matching runtime resume online", () => {
    expect(
      staleResumeRequiresRefresh({
        hardReset: false,
        seenRuntimeInstanceId: "runtime-1",
        resumeRuntimeInstanceId: "runtime-1",
      }),
    ).toBe(false);
  });

  it("forces a safe refresh on hard reset or a different host runtime", () => {
    expect(
      staleResumeRequiresRefresh({
        hardReset: true,
        seenRuntimeInstanceId: "runtime-1",
        resumeRuntimeInstanceId: "runtime-1",
      }),
    ).toBe(true);
    expect(
      staleResumeRequiresRefresh({
        hardReset: false,
        seenRuntimeInstanceId: "runtime-1",
        resumeRuntimeInstanceId: "runtime-2",
      }),
    ).toBe(true);
  });
});

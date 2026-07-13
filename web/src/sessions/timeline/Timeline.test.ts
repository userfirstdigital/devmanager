import { describe, expect, it } from "vitest";

import { preserveTimelineAnchor } from "./Timeline";

describe("timeline scroll anchoring", () => {
  it("keeps the same visible event fixed when replay inserts content above it", () => {
    expect(
      preserveTimelineAnchor({
        scrollTop: 620,
        previousAnchorOffset: 24,
        nextAnchorOffset: 184,
      }),
    ).toBe(780);
  });

  it("never creates a negative scroll position", () => {
    expect(
      preserveTimelineAnchor({
        scrollTop: 12,
        previousAnchorOffset: 90,
        nextAnchorOffset: 10,
      }),
    ).toBe(0);
  });
});

import { describe, expect, it } from "vitest";

import { resolveViewMode } from "./viewMode";

describe("raw terminal fallback", () => {
  it("stays native when an AI adapter degrades", () => {
    expect(resolveViewMode({ adapterHealth: "healthy", ai: true, gridInteractionRequired: false, pinned: false })).toBe("semantic");
    expect(resolveViewMode({ adapterHealth: "degraded", ai: true, gridInteractionRequired: false, pinned: false })).toBe("semantic");
  });

  it("uses xterm only for terminal-grid interactions or an explicit pin", () => {
    expect(resolveViewMode({ adapterHealth: "degraded", ai: true, gridInteractionRequired: true, pinned: false })).toBe("terminal");
    expect(resolveViewMode({ adapterHealth: "healthy", ai: false, gridInteractionRequired: false, pinned: true })).toBe("terminal");
  });
});

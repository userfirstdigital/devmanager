import { describe, expect, it } from "vitest";

import {
  normalizeReturnBehavior,
  normalizeTerminalPreference,
} from "./inputPreference";

describe("mobile input preferences", () => {
  it("defaults to multiline-safe Return and automatic terminal fallback", () => {
    expect(normalizeReturnBehavior(null)).toBe("newline");
    expect(normalizeReturnBehavior("unexpected")).toBe("newline");
    expect(normalizeTerminalPreference(null)).toBe("automatic");
    expect(normalizeTerminalPreference("unexpected")).toBe("automatic");
  });

  it("accepts only supported explicit choices", () => {
    expect(normalizeReturnBehavior("send")).toBe("send");
    expect(normalizeTerminalPreference("raw")).toBe("raw");
  });
});

import { describe, expect, it } from "vitest";

import {
  classifyClientAction,
  isIdempotentClientAction,
} from "./actions";

describe("idempotent vs non-idempotent connect actions", () => {
  it("treats resume and composer submit as retry-safe", () => {
    expect(classifyClientAction("resume")).toBe("idempotent");
    expect(classifyClientAction("composerSubmit")).toBe("idempotent");
    expect(isIdempotentClientAction("resume")).toBe(true);
    expect(isIdempotentClientAction("composerSubmit")).toBe(true);
  });

  it("never classifies a sent request or raw terminal write as idempotent", () => {
    expect(classifyClientAction("request")).toBe("nonIdempotent");
    expect(classifyClientAction("rawTerminal")).toBe("nonIdempotent");
    expect(isIdempotentClientAction("request")).toBe(false);
    expect(isIdempotentClientAction("rawTerminal")).toBe(false);
  });
});

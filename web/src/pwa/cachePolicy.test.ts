import { describe, expect, it } from "vitest";
import { isNetworkOnlyPath } from "./cachePolicy";

describe("isNetworkOnlyPath", () => {
  it.each(["/api", "/api/health", "/api/ws", "/pair"])(
    "keeps %s on the network",
    (path) => {
      expect(isNetworkOnlyPath(path)).toBe(true);
    },
  );

  it.each(["/", "/sessions", "/session/tab/test", "/pairing-help"])(
    "allows app-shell handling for %s",
    (path) => {
      expect(isNetworkOnlyPath(path)).toBe(false);
    },
  );
});

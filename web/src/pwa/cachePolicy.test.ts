import { describe, expect, it } from "vitest";
import { isNetworkOnlyPath } from "./cachePolicy";

describe("isNetworkOnlyPath", () => {
  it.each(["/api", "/api/health", "/api/ws", "/api/connect", "/pair", "/pair/legacy"])(
    "keeps %s on the network",
    (path) => {
      expect(isNetworkOnlyPath(path)).toBe(true);
    },
  );

  it.each(["/", "/tasks", "/tasks/tab%3Atest", "/pairing-help"])(
    "allows app-shell handling for %s",
    (path) => {
      expect(isNetworkOnlyPath(path)).toBe(false);
    },
  );
});

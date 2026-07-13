import { describe, expect, it } from "vitest";

import {
  hrefForRoute,
  isCanonicalRouteLocation,
  parseRoute,
  routeForSessionKey,
  routesEqual,
} from "./router";

describe("app router", () => {
  it("parses every canonical route and ignores the query string", () => {
    expect(parseRoute("/sessions?source=pwa")).toEqual({ name: "sessions" });
    expect(parseRoute("/projects")).toEqual({ name: "projects" });
    expect(parseRoute("/projects/project%20one")).toEqual({
      name: "project",
      projectId: "project one",
    });
    expect(parseRoute("/session/tab/abc")).toEqual({
      name: "session",
      kind: "tab",
      id: "abc",
    });
    expect(parseRoute("/session/server/dev%2Fweb")).toEqual({
      name: "session",
      kind: "server",
      id: "dev/web",
    });
    expect(parseRoute("/settings")).toEqual({ name: "settings" });
  });

  it("maps unknown, malformed, and unsafe routes to Sessions", () => {
    expect(parseRoute("/")).toEqual({ name: "sessions" });
    expect(parseRoute("/unknown")).toEqual({ name: "sessions" });
    expect(parseRoute("/session/pty/ephemeral")).toEqual({ name: "sessions" });
    expect(parseRoute("/session/tab/%E0%A4%A")).toEqual({ name: "sessions" });
    expect(parseRoute("https://example.test/settings")).toEqual({ name: "sessions" });
  });

  it("round-trips encoded stable identifiers", () => {
    const route = routeForSessionKey("server:dev/web #1");
    expect(route).toEqual({ name: "session", kind: "server", id: "dev/web #1" });
    const href = hrefForRoute(route);
    expect(href).toBe("/session/server/dev%2Fweb%20%231");
    expect(parseRoute(href)).toEqual(route);
    expect(routesEqual(parseRoute(href), route)).toBe(true);
  });

  it("canonicalizes fallback and manifest entry URLs after routing", () => {
    const sessions = { name: "sessions" } as const;
    expect(isCanonicalRouteLocation(sessions, "/")).toBe(false);
    expect(isCanonicalRouteLocation(sessions, "/sessions?source=pwa")).toBe(false);
    expect(isCanonicalRouteLocation(sessions, "/sessions")).toBe(true);
  });
});

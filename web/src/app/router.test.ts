import { describe, expect, it } from "vitest";

import {
  hrefForRoute,
  isCanonicalRouteLocation,
  parseRoute,
  routeForStableSessionKey,
  routesEqual,
} from "./router";

describe("app router", () => {
  it("parses every canonical route and ignores the query string", () => {
    expect(parseRoute("/tasks?source=pwa")).toEqual({ name: "tasks" });
    expect(parseRoute("/projects")).toEqual({ name: "projects" });
    expect(parseRoute("/projects/project%20one")).toEqual({
      name: "project",
      projectId: "project one",
    });
    expect(parseRoute("/tasks/tab%3Aabc")).toEqual({
      name: "task",
      taskId: "tab:abc",
    });
    expect(parseRoute("/tasks/server%3Adev%2Fweb")).toEqual({
      name: "task",
      taskId: "server:dev/web",
    });
    expect(parseRoute("/tasks/tab%3Aabc/terminal")).toEqual({
      name: "task",
      taskId: "tab:abc",
      resource: "terminal",
    });
    expect(parseRoute("/settings")).toEqual({ name: "settings" });
  });

  it("maps unknown, malformed, and unsafe routes to Tasks", () => {
    expect(parseRoute("/")).toEqual({ name: "tasks" });
    expect(parseRoute("/unknown")).toEqual({ name: "tasks" });
    expect(parseRoute("/tasks/pty/ephemeral")).toEqual({ name: "tasks" });
    expect(parseRoute("/tasks/%E0%A4%A")).toEqual({ name: "tasks" });
    expect(parseRoute("https://example.test/settings")).toEqual({
      name: "tasks",
    });
  });

  it("round-trips encoded stable identifiers", () => {
    const route = routeForStableSessionKey("server:dev/web #1");
    expect(route).toEqual({ name: "task", taskId: "server:dev/web #1" });
    const href = hrefForRoute(route);
    expect(href).toBe("/tasks/server%3Adev%2Fweb%20%231");
    expect(parseRoute(href)).toEqual(route);
    expect(routesEqual(parseRoute(href), route)).toBe(true);
  });

  it("canonicalizes fallback and manifest entry URLs after routing", () => {
    const tasks = { name: "tasks" } as const;
    expect(isCanonicalRouteLocation(tasks, "/")).toBe(false);
    expect(isCanonicalRouteLocation(tasks, "/tasks?source=pwa")).toBe(false);
    expect(isCanonicalRouteLocation(tasks, "/tasks")).toBe(true);
  });
});

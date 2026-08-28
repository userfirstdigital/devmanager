import { describe, expect, it } from "vitest";

import {
  hrefForRoute,
  isCanonicalRouteLocation,
  parseRoute,
  routeForStableSessionKey,
  routeForTaskId,
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

  it("matches native task-link bounds and stable key grammar", () => {
    for (const key of ["foo", "tab:", "pty:abc", "tab:x\n", "tab:x\u0085", `tab:${"é".repeat(511)}`]) {
      expect(parseRoute(`/tasks/${encodeURIComponent(key)}`)).toEqual({ name: "tasks" });
      expect(routeForTaskId(key)).toEqual({ name: "tasks" });
      expect(hrefForRoute({ name: "task", taskId: key })).toBe("/tasks");
    }
    for (const path of ["/tasks//tab%3Aabc", "/tasks/tab%3Aabc/", "/tasks/tab%3Aabc//terminal"]) {
      expect(parseRoute(path)).toEqual({ name: "tasks" });
    }
    const boundary = `tab:${"é".repeat(510)}`;
    expect(parseRoute(hrefForRoute(routeForTaskId(boundary)))).toEqual({ name: "task", taskId: boundary });
    expect(routeForTaskId("tab:\ud800")).toEqual({ name: "tasks" });
    expect(hrefForRoute({ name: "task", taskId: "tab:\ud800" })).toBe("/tasks");
    expect(parseRoute(hrefForRoute(routeForTaskId("tab:😀")))).toEqual({ name: "task", taskId: "tab:😀" });
  });

  it("canonicalizes fallback and manifest entry URLs after routing", () => {
    const tasks = { name: "tasks" } as const;
    expect(isCanonicalRouteLocation(tasks, "/")).toBe(false);
    expect(isCanonicalRouteLocation(tasks, "/tasks?source=pwa")).toBe(false);
    expect(isCanonicalRouteLocation(tasks, "/tasks")).toBe(true);
  });
});

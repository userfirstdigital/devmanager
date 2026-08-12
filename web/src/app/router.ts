import type { StableSessionKey } from "../api/types";
import type { TaskId, TaskResource } from "../tasks/taskId";
import {
  DEFAULT_TASK_RESOURCE,
  isTaskResource,
  taskIdFromStableSessionKey,
  taskIdToStableSessionKey,
} from "../tasks/taskId";

export type AppDestination = "tasks" | "projects" | "settings";

export type AppRoute =
  | { name: "tasks" }
  | { name: "projects" }
  | { name: "project"; projectId: string }
  | { name: "task"; taskId: TaskId; resource?: TaskResource }
  | { name: "settings" };

export const TASKS_ROUTE: AppRoute = { name: "tasks" };

function decodeSegment(value: string | undefined): string | null {
  if (!value) return null;
  try {
    const decoded = decodeURIComponent(value);
    return decoded.length > 0 && !decoded.includes("\0") ? decoded : null;
  } catch {
    return null;
  }
}

export function parseRoute(input: string): AppRoute {
  if (!input.startsWith("/")) return TASKS_ROUTE;
  const pathname = input.split(/[?#]/u, 1)[0] ?? "/";
  const segments = pathname.split("/").filter(Boolean);

  if (segments.length === 1 && segments[0] === "tasks") {
    return { name: "tasks" };
  }
  if (segments.length === 1 && segments[0] === "projects") {
    return { name: "projects" };
  }
  if (segments.length === 2 && segments[0] === "projects") {
    const projectId = decodeSegment(segments[1]);
    return projectId ? { name: "project", projectId } : TASKS_ROUTE;
  }
  if (segments.length >= 2 && segments[0] === "tasks") {
    const taskId = decodeSegment(segments[1]);
    if (!taskId) return TASKS_ROUTE;
    if (segments.length === 2) {
      return { name: "task", taskId };
    }
    if (segments.length === 3 && isTaskResource(segments[2])) {
      return { name: "task", taskId, resource: segments[2] };
    }
    return TASKS_ROUTE;
  }
  if (segments.length === 1 && segments[0] === "settings") {
    return { name: "settings" };
  }
  return TASKS_ROUTE;
}

export function hrefForRoute(route: AppRoute): string {
  switch (route.name) {
    case "tasks":
      return "/tasks";
    case "projects":
      return "/projects";
    case "project":
      return `/projects/${encodeURIComponent(route.projectId)}`;
    case "task": {
      const base = `/tasks/${encodeURIComponent(route.taskId)}`;
      const resource = route.resource ?? DEFAULT_TASK_RESOURCE;
      return resource === DEFAULT_TASK_RESOURCE ? base : `${base}/${resource}`;
    }
    case "settings":
      return "/settings";
  }
}

export function routeForTaskId(
  taskId: TaskId,
  resource: TaskResource = DEFAULT_TASK_RESOURCE,
): AppRoute {
  if (!taskId || taskId.includes("\0")) return TASKS_ROUTE;
  return resource === DEFAULT_TASK_RESOURCE
    ? { name: "task", taskId }
    : { name: "task", taskId, resource };
}

/** Map a host stable session key onto the Task Cockpit route. */
export function routeForStableSessionKey(
  stableSessionKey: StableSessionKey,
  resource: TaskResource = DEFAULT_TASK_RESOURCE,
): AppRoute {
  return routeForTaskId(taskIdFromStableSessionKey(stableSessionKey), resource);
}

export function taskIdForRoute(route: AppRoute): TaskId | null {
  return route.name === "task" ? route.taskId : null;
}

export function stableSessionKeyForRoute(
  route: AppRoute,
): StableSessionKey | null {
  const taskId = taskIdForRoute(route);
  return taskId ? taskIdToStableSessionKey(taskId) : null;
}

export function destinationForRoute(route: AppRoute): AppDestination | null {
  switch (route.name) {
    case "tasks":
      return "tasks";
    case "projects":
    case "project":
      return "projects";
    case "settings":
      return "settings";
    case "task":
      return null;
  }
}

export function routesEqual(left: AppRoute, right: AppRoute): boolean {
  return hrefForRoute(left) === hrefForRoute(right);
}

export function isCanonicalRouteLocation(
  route: AppRoute,
  pathnameAndSearch: string,
): boolean {
  return pathnameAndSearch === hrefForRoute(route);
}

export function currentBrowserRoute(): AppRoute {
  if (typeof window === "undefined") return TASKS_ROUTE;
  return parseRoute(`${window.location.pathname}${window.location.search}`);
}

export function navigateBrowser(
  route: AppRoute,
  options: { replace?: boolean } = {},
): void {
  if (typeof window === "undefined") return;
  const href = hrefForRoute(route);
  if (options.replace) window.history.replaceState(null, "", href);
  else window.history.pushState(null, "", href);
  window.dispatchEvent(new PopStateEvent("popstate"));
}

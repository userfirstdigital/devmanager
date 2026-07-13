import type { StableSessionKey } from "../api/types";

export type AppDestination = "sessions" | "projects" | "settings";
export type SessionRouteKind = "server" | "tab";

export type AppRoute =
  | { name: "sessions" }
  | { name: "projects" }
  | { name: "project"; projectId: string }
  | { name: "session"; kind: SessionRouteKind; id: string }
  | { name: "settings" };

export const SESSIONS_ROUTE: AppRoute = { name: "sessions" };

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
  if (!input.startsWith("/")) return SESSIONS_ROUTE;
  const pathname = input.split(/[?#]/u, 1)[0] ?? "/";
  const segments = pathname.split("/").filter(Boolean);

  if (segments.length === 1 && segments[0] === "sessions") {
    return { name: "sessions" };
  }
  if (segments.length === 1 && segments[0] === "projects") {
    return { name: "projects" };
  }
  if (segments.length === 2 && segments[0] === "projects") {
    const projectId = decodeSegment(segments[1]);
    return projectId ? { name: "project", projectId } : SESSIONS_ROUTE;
  }
  if (
    segments.length === 3 &&
    segments[0] === "session" &&
    (segments[1] === "server" || segments[1] === "tab")
  ) {
    const id = decodeSegment(segments[2]);
    return id
      ? { name: "session", kind: segments[1], id }
      : SESSIONS_ROUTE;
  }
  if (segments.length === 1 && segments[0] === "settings") {
    return { name: "settings" };
  }
  return SESSIONS_ROUTE;
}

export function hrefForRoute(route: AppRoute): string {
  switch (route.name) {
    case "sessions":
      return "/sessions";
    case "projects":
      return "/projects";
    case "project":
      return `/projects/${encodeURIComponent(route.projectId)}`;
    case "session":
      return `/session/${route.kind}/${encodeURIComponent(route.id)}`;
    case "settings":
      return "/settings";
  }
}

export function routeForSessionKey(stableSessionKey: StableSessionKey): AppRoute {
  const separator = stableSessionKey.indexOf(":");
  if (separator <= 0 || separator === stableSessionKey.length - 1) {
    return SESSIONS_ROUTE;
  }
  const kind = stableSessionKey.slice(0, separator);
  const id = stableSessionKey.slice(separator + 1);
  if (kind !== "server" && kind !== "tab") return SESSIONS_ROUTE;
  return { name: "session", kind, id };
}

export function stableSessionKeyForRoute(
  route: AppRoute,
): StableSessionKey | null {
  return route.name === "session" ? `${route.kind}:${route.id}` : null;
}

export function destinationForRoute(route: AppRoute): AppDestination | null {
  switch (route.name) {
    case "sessions":
      return "sessions";
    case "projects":
    case "project":
      return "projects";
    case "settings":
      return "settings";
    case "session":
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
  if (typeof window === "undefined") return SESSIONS_ROUTE;
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

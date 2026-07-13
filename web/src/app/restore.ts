import type { WebWorkspaceSnapshot } from "../api/types";
import {
  hrefForRoute,
  parseRoute,
  stableSessionKeyForRoute,
  type AppRoute,
} from "./router";

const ROUTE_STORAGE_KEY = "devmanager-installed-route-v1";
const ROUTE_STORAGE_VERSION = 1;

export interface SavedRoute {
  runtimeInstanceId: string;
  route: AppRoute;
}

function defaultStorage(): Storage | undefined {
  try {
    return globalThis.localStorage;
  } catch {
    return undefined;
  }
}

interface SavedRouteEnvelope extends SavedRoute {
  version: number;
}

export function isStandaloneDisplayMode(): boolean {
  if (typeof window === "undefined") return false;
  const iosNavigator = navigator as Navigator & { standalone?: boolean };
  return (
    window.matchMedia?.("(display-mode: standalone)").matches === true ||
    iosNavigator.standalone === true
  );
}

export function isInstalledLaunchEligible(
  pathname: string,
  search: string,
): boolean {
  if (pathname === "/") return true;
  if (pathname !== "/sessions") return false;
  return new URLSearchParams(search).get("source") === "pwa";
}

export function readSavedRoute(
  storage?: Pick<Storage, "getItem">,
): SavedRoute | null {
  const availableStorage = storage ?? defaultStorage();
  if (!availableStorage) return null;
  try {
    const raw = availableStorage.getItem(ROUTE_STORAGE_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as Partial<SavedRouteEnvelope>;
    if (
      parsed.version !== ROUTE_STORAGE_VERSION ||
      typeof parsed.runtimeInstanceId !== "string" ||
      typeof parsed.route !== "object" ||
      parsed.route === null
    ) {
      return null;
    }
    const route = parseRoute(hrefForRoute(parsed.route as AppRoute));
    return { runtimeInstanceId: parsed.runtimeInstanceId, route };
  } catch {
    return null;
  }
}

export function writeSavedRoute(
  saved: SavedRoute,
  storage?: Pick<Storage, "setItem">,
): void {
  try {
    (storage ?? defaultStorage())?.setItem(
      ROUTE_STORAGE_KEY,
      JSON.stringify({ version: ROUTE_STORAGE_VERSION, ...saved }),
    );
  } catch {
    // Route restoration is a convenience. Storage denial never blocks the app.
  }
}

export function clearSavedRoute(
  storage?: Pick<Storage, "removeItem">,
): void {
  try {
    (storage ?? defaultStorage())?.removeItem(ROUTE_STORAGE_KEY);
  } catch {
    // Storage may be disabled or full.
  }
}

function routeExists(
  route: AppRoute,
  snapshot: WebWorkspaceSnapshot,
): boolean {
  const sessionKey = stableSessionKeyForRoute(route);
  if (sessionKey) {
    return snapshot.sessions.some(
      (session) => session.stableSessionKey === sessionKey,
    );
  }
  if (route.name === "project") {
    return snapshot.projects.some((project) => project.id === route.projectId);
  }
  return true;
}

export function resolveColdStart(
  initialRoute: AppRoute,
  saved: SavedRoute | null,
  context: {
    standalone: boolean;
    launchEligible: boolean;
    snapshot: WebWorkspaceSnapshot;
    notificationRuntimeInstanceId?: string | null;
    notificationRoute?: AppRoute | null;
  },
): AppRoute {
  if (
    context.notificationRuntimeInstanceId !== undefined &&
    context.notificationRuntimeInstanceId !== null
  ) {
    if (
      context.notificationRuntimeInstanceId !==
      context.snapshot.runtimeInstanceId
    ) {
      return { name: "sessions" };
    }
    const notificationRoute = context.notificationRoute ?? {
      name: "sessions",
    };
    return routeExists(notificationRoute, context.snapshot)
      ? notificationRoute
      : { name: "sessions" };
  }
  if (!context.standalone || !context.launchEligible || !saved) {
    return initialRoute;
  }
  if (saved.runtimeInstanceId !== context.snapshot.runtimeInstanceId) {
    return { name: "sessions" };
  }
  return routeExists(saved.route, context.snapshot)
    ? saved.route
    : { name: "sessions" };
}

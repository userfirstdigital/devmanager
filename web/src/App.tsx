import { TerminalSquare } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { AppShell } from "./app/AppShell";
import {
  currentBrowserRoute,
  hrefForRoute,
  isCanonicalRouteLocation,
  navigateBrowser,
  parseRoute,
  routesEqual,
  stableSessionKeyForRoute,
  type AppRoute,
} from "./app/router";
import {
  isInstalledLaunchEligible,
  isStandaloneDisplayMode,
  readSavedRoute,
  resolveColdStart,
  writeSavedRoute,
} from "./app/restore";
import { PairingGate } from "./components/PairingGate";
import { makeDemoEvents, makeDemoWorkspace } from "./dev/demoWorkspace";
import { clearOtherRuntimes, pruneDrafts } from "./drafts/draftStore";
import { bindAppLifecycle } from "./platform/lifecycle";
import { ProjectScreen } from "./projects/ProjectScreen";
import { ProjectsScreen } from "./projects/ProjectsScreen";
import {
  NOTIFICATION_ROUTE_QUERY,
  NOTIFICATION_RUNTIME_QUERY,
  reconcilePushNotificationsOnForeground,
} from "./pwa/notifications";
import { SessionScreen } from "./sessions/SessionScreen";
import { SessionsScreen } from "./sessions/SessionsScreen";
import { SettingsScreen } from "./settings/SettingsScreen";
import { useStore } from "./store";

const DEMO_MODE =
  import.meta.env.DEV &&
  typeof window !== "undefined" &&
  new URLSearchParams(window.location.search).get("demo") === "1";
const DEMO_WORKSPACE = DEMO_MODE ? makeDemoWorkspace() : null;

function routeExists(
  route: AppRoute,
  workspace: NonNullable<ReturnType<typeof useStore.getState>["workspace"]>,
): boolean {
  const key = stableSessionKeyForRoute(route);
  if (key) return workspace.sessions.some((session) => session.stableSessionKey === key);
  if (route.name === "project") {
    return workspace.projects.some((project) => project.id === route.projectId);
  }
  return true;
}

function LoadingHost({ offline }: { offline: boolean }) {
  return (
    <main className="dm-launch-state">
      <span className="dm-launch-logo" aria-hidden="true"><TerminalSquare size={30} /></span>
      <h1>DevManager</h1>
      <p>{offline ? "Waiting for the DevManager host…" : "Connecting to your workspace…"}</p>
      <span className="dm-native-spinner" aria-hidden="true" />
    </main>
  );
}

export function App() {
  const init = useStore((state) => state.init);
  const hostStatus = useStore((state) => state.status);
  const hostWorkspace = useStore((state) => state.workspace);
  const foregroundConnection = useStore((state) => state.foregroundConnection);
  const setConnectionVisibility = useStore((state) => state.setConnectionVisibility);
  const setActiveSession = useStore((state) => state.setActiveSession);
  const activeSessionKey = useStore((state) => state.activeSessionKey);
  const pendingRoute = useStore((state) => state.pendingRoute);
  const lastError = useStore((state) => state.lastError);
  const clearLastError = useStore((state) => state.clearLastError);
  const [route, setRoute] = useState<AppRoute>(() => currentBrowserRoute());
  const initialRoute = useRef(route);
  const savedRoute = useRef(readSavedRoute());
  const standalone = useRef(isStandaloneDisplayMode());
  const launchEligible = useRef(
    typeof window !== "undefined" &&
      isInstalledLaunchEligible(window.location.pathname, window.location.search),
  );
  const notificationRuntimeInstanceId = useRef(
    typeof window === "undefined"
      ? null
      : new URLSearchParams(window.location.search).get(
          NOTIFICATION_RUNTIME_QUERY,
        ),
  );
  const notificationRoute = useRef<AppRoute | null>(
    (() => {
      if (typeof window === "undefined") return null;
      const encodedRoute = new URLSearchParams(window.location.search).get(
        NOTIFICATION_ROUTE_QUERY,
      );
      if (!encodedRoute) return null;
      const parsed = parseRoute(encodedRoute);
      return parsed.name === "session" ? parsed : null;
    })(),
  );
  const resolvedRuntime = useRef<string | null>(null);
  const lastPersistedRoute = useRef<string | null>(null);
  const status = DEMO_MODE ? ({ kind: "open" } as const) : hostStatus;
  const workspace = DEMO_WORKSPACE ?? hostWorkspace;

  useEffect(() => {
    if (!DEMO_MODE) init();
  }, [init]);

  useEffect(() => {
    if (DEMO_MODE) return;
    const foreground = () => {
      foregroundConnection();
      void reconcilePushNotificationsOnForeground().catch(() => undefined);
    };
    void reconcilePushNotificationsOnForeground().catch(() => undefined);
    return bindAppLifecycle({
      foreground,
      setVisibility: setConnectionVisibility,
    });
  }, [foregroundConnection, setConnectionVisibility]);

  useEffect(() => {
    const onPopState = () => setRoute(parseRoute(`${window.location.pathname}${window.location.search}`));
    window.addEventListener("popstate", onPopState);
    return () => window.removeEventListener("popstate", onPopState);
  }, []);

  const moveTo = useCallback(
    (nextRoute: AppRoute, options: { replace?: boolean } = {}) => {
      if (routesEqual(route, nextRoute) && !options.replace) return;
      navigateBrowser(nextRoute, options);
      setRoute(nextRoute);
    },
    [route],
  );

  useEffect(() => {
    if (!workspace) return;

    clearOtherRuntimes(workspace.runtimeInstanceId);
    pruneDrafts(
      workspace.runtimeInstanceId,
      new Set(
        workspace.sessions.flatMap((session) =>
          session.stableSessionKey ? [session.stableSessionKey] : [],
        ),
      ),
    );

    if (resolvedRuntime.current === null) {
      const resolved = resolveColdStart(initialRoute.current, savedRoute.current, {
        standalone: standalone.current,
        launchEligible: launchEligible.current,
        snapshot: workspace,
        notificationRuntimeInstanceId: notificationRuntimeInstanceId.current,
        notificationRoute: notificationRoute.current,
      });
      const validated = routeExists(resolved, workspace) ? resolved : { name: "sessions" } as AppRoute;
      resolvedRuntime.current = workspace.runtimeInstanceId;
      const browserLocation = `${window.location.pathname}${window.location.search}`;
      if (
        !routesEqual(route, validated) ||
        !isCanonicalRouteLocation(validated, browserLocation)
      ) {
        moveTo(validated, { replace: true });
      }
      else setActiveSession(stableSessionKeyForRoute(validated));
      if (standalone.current) {
        writeSavedRoute({ runtimeInstanceId: workspace.runtimeInstanceId, route: validated });
      }
      return;
    }

    if (resolvedRuntime.current !== workspace.runtimeInstanceId) {
      resolvedRuntime.current = workspace.runtimeInstanceId;
      lastPersistedRoute.current = null;
      const freshRoute: AppRoute = { name: "sessions" };
      if (!routesEqual(route, freshRoute)) moveTo(freshRoute, { replace: true });
      if (standalone.current) {
        writeSavedRoute({ runtimeInstanceId: workspace.runtimeInstanceId, route: freshRoute });
      }
      return;
    }

    if (
      !routeExists(route, workspace) &&
      pendingRoute !== hrefForRoute(route)
    ) {
      moveTo(route.name === "project" ? { name: "projects" } : { name: "sessions" }, { replace: true });
    }
  }, [moveTo, pendingRoute, route, setActiveSession, workspace]);

  useEffect(() => {
    if (!workspace || resolvedRuntime.current !== workspace.runtimeInstanceId) return;
    const desiredSessionKey = stableSessionKeyForRoute(route);
    if (
      (desiredSessionKey === null || routeExists(route, workspace)) &&
      activeSessionKey !== desiredSessionKey
    ) {
      setActiveSession(desiredSessionKey);
    }
    if (standalone.current && routeExists(route, workspace)) {
      const persistenceKey = `${workspace.runtimeInstanceId}:${hrefForRoute(route)}`;
      if (lastPersistedRoute.current !== persistenceKey) {
        lastPersistedRoute.current = persistenceKey;
        writeSavedRoute({ runtimeInstanceId: workspace.runtimeInstanceId, route });
      }
    }
  }, [activeSessionKey, route, setActiveSession, workspace]);

  const attentionCount = useMemo(
    () =>
      workspace?.sessions.filter(
        (session) => session.attention === "needsInput" || session.attention === "failed",
      ).length ?? 0,
    [workspace],
  );

  if (status.kind === "unauthorized") return <PairingGate />;
  if (!workspace) return <LoadingHost offline={status.kind === "closed"} />;

  let screen;
  switch (route.name) {
    case "sessions":
      screen = <SessionsScreen workspace={workspace} onNavigate={moveTo} />;
      break;
    case "projects":
      screen = <ProjectsScreen workspace={workspace} onNavigate={moveTo} />;
      break;
    case "project":
      screen = (
        <ProjectScreen
          workspace={workspace}
          projectId={route.projectId}
          connected={status.kind === "open"}
          onNavigate={moveTo}
        />
      );
      break;
    case "settings":
      screen = <SettingsScreen status={status} />;
      break;
    case "session":
      screen = (
        <SessionScreen
          route={route}
          workspace={workspace}
          status={status}
          onNavigate={moveTo}
          demoEvents={
            DEMO_MODE
              ? makeDemoEvents(stableSessionKeyForRoute(route) ?? "")
              : undefined
          }
        />
      );
      break;
  }

  return (
    <AppShell
      route={route}
      status={status}
      attentionCount={attentionCount}
      lastError={lastError}
      onDismissError={clearLastError}
      onNavigate={moveTo}
    >
      {screen}
    </AppShell>
  );
}

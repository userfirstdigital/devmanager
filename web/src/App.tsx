import { ArrowLeft, TerminalSquare } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { AppShell } from "./app/AppShell";
import {
  currentBrowserRoute,
  hrefForRoute,
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
import { MobileKeyRow } from "./components/MobileKeyRow";
import { TerminalView } from "./components/Terminal";
import { makeDemoWorkspace } from "./dev/demoWorkspace";
import { bindAppLifecycle } from "./platform/lifecycle";
import { ProjectScreen } from "./projects/ProjectScreen";
import { ProjectsScreen } from "./projects/ProjectsScreen";
import { SessionsScreen } from "./sessions/SessionsScreen";
import { describeSession } from "./sessions/sessionModel";
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

function LegacySessionSurface({
  route,
  workspace,
  onNavigate,
}: {
  route: Extract<AppRoute, { name: "session" }>;
  workspace: NonNullable<ReturnType<typeof useStore.getState>["workspace"]>;
  onNavigate(route: AppRoute): void;
}) {
  const key = stableSessionKeyForRoute(route);
  const summary = workspace.sessions.find((session) => session.stableSessionKey === key);
  const item = summary ? describeSession(workspace, summary) : null;

  if (!summary || !item) return null;

  return (
    <section className="dm-screen dm-session-detail-screen" aria-labelledby="session-title">
      <header className="dm-session-header">
        <button type="button" className="dm-nav-back dm-session-back" onClick={() => onNavigate({ name: "sessions" })}>
          <ArrowLeft size={21} aria-hidden="true" />
          Sessions
        </button>
        <div className="dm-session-title-block">
          <h1 id="session-title">{item.label}</h1>
          <p>{item.projectName} · {item.stateLabel}</p>
        </div>
        <span className="dm-header-balance" aria-hidden="true" />
      </header>
      {DEMO_MODE ? (
        <div className="dm-screen-scroll">
          <div className="dm-native-empty">
            <span className="dm-native-empty-icon" aria-hidden="true">
              <TerminalSquare size={28} />
            </span>
            <h2>Session ready</h2>
            <p>The semantic conversation timeline mounts here in the complete mobile interface.</p>
          </div>
        </div>
      ) : (
        <div className="dm-legacy-terminal-wrap">
          <TerminalView sessionId={summary.sessionId} />
          <MobileKeyRow sessionId={summary.sessionId} />
        </div>
      )}
    </section>
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
  const [route, setRoute] = useState<AppRoute>(() => currentBrowserRoute());
  const initialRoute = useRef(route);
  const savedRoute = useRef(readSavedRoute());
  const standalone = useRef(isStandaloneDisplayMode());
  const launchEligible = useRef(
    typeof window !== "undefined" &&
      isInstalledLaunchEligible(window.location.pathname, window.location.search),
  );
  const resolvedRuntime = useRef<string | null>(null);
  const lastPersistedRoute = useRef<string | null>(null);
  const status = DEMO_MODE ? ({ kind: "open" } as const) : hostStatus;
  const workspace = DEMO_WORKSPACE ?? hostWorkspace;

  useEffect(() => {
    if (!DEMO_MODE) init();
  }, [init]);

  useEffect(
    () =>
      DEMO_MODE
        ? undefined
        : bindAppLifecycle({
            foreground: foregroundConnection,
            setVisibility: setConnectionVisibility,
          }),
    [foregroundConnection, setConnectionVisibility],
  );

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

    if (resolvedRuntime.current === null) {
      const resolved = resolveColdStart(initialRoute.current, savedRoute.current, {
        standalone: standalone.current,
        launchEligible: launchEligible.current,
        snapshot: workspace,
      });
      const validated = routeExists(resolved, workspace) ? resolved : { name: "sessions" } as AppRoute;
      resolvedRuntime.current = workspace.runtimeInstanceId;
      if (!routesEqual(route, validated)) moveTo(validated, { replace: true });
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
      screen = <ProjectScreen workspace={workspace} projectId={route.projectId} onNavigate={moveTo} />;
      break;
    case "settings":
      screen = <SettingsScreen status={status} />;
      break;
    case "session":
      screen = (
        <LegacySessionSurface
          route={route}
          workspace={workspace}
          onNavigate={moveTo}
        />
      );
      break;
  }

  return (
    <AppShell route={route} status={status} attentionCount={attentionCount} onNavigate={moveTo}>
      {screen}
    </AppShell>
  );
}

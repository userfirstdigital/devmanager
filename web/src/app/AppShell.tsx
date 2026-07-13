import {
  FolderKanban,
  MonitorSmartphone,
  Settings2,
  Sparkles,
} from "lucide-react";
import type { ReactNode } from "react";

import type { WsStatus } from "../api/ws";
import {
  destinationForRoute,
  type AppDestination,
  type AppRoute,
} from "./router";

interface DestinationItem {
  id: AppDestination;
  label: string;
  icon: typeof Sparkles;
  route: AppRoute;
}

const DESTINATIONS: DestinationItem[] = [
  {
    id: "sessions",
    label: "Sessions",
    icon: Sparkles,
    route: { name: "sessions" },
  },
  {
    id: "projects",
    label: "Projects",
    icon: FolderKanban,
    route: { name: "projects" },
  },
  {
    id: "settings",
    label: "Settings",
    icon: Settings2,
    route: { name: "settings" },
  },
];

export interface AppShellProps {
  route: AppRoute;
  status: WsStatus;
  attentionCount: number;
  onNavigate(route: AppRoute): void;
  children: ReactNode;
}

function connectionLabel(status: WsStatus): string {
  switch (status.kind) {
    case "open":
      return "Connected";
    case "connecting":
      return "Reconnecting";
    case "closed":
      return "Offline";
    case "unauthorized":
      return "Pairing required";
    case "idle":
      return "Starting";
  }
}

export function AppShell({
  route,
  status,
  attentionCount,
  onNavigate,
  children,
}: AppShellProps) {
  const destination = destinationForRoute(route);
  const isSession = route.name === "session";

  return (
    <div className="dm-app-shell" data-session-detail={isSession || undefined}>
      <aside className="dm-sidebar" aria-label="App navigation">
        <div className="dm-brand">
          <span className="dm-brand-icon" aria-hidden="true">
            <MonitorSmartphone size={20} strokeWidth={1.8} />
          </span>
          <span>
            <strong>DevManager</strong>
            <small>{connectionLabel(status)}</small>
          </span>
        </div>
        <nav className="dm-sidebar-nav">
          {DESTINATIONS.map((item) => {
            const Icon = item.icon;
            const active = destination === item.id;
            return (
              <button
                key={item.id}
                type="button"
                className="dm-sidebar-link"
                aria-current={active ? "page" : undefined}
                onClick={() => onNavigate(item.route)}
              >
                <Icon size={20} strokeWidth={1.8} aria-hidden="true" />
                <span>{item.label}</span>
                {item.id === "sessions" && attentionCount > 0 ? (
                  <span className="dm-nav-badge" aria-label={`${attentionCount} sessions need attention`}>
                    {Math.min(attentionCount, 99)}
                  </span>
                ) : null}
              </button>
            );
          })}
        </nav>
        <div className="dm-sidebar-status" data-status={status.kind}>
          <span className="dm-connection-dot" aria-hidden="true" />
          {connectionLabel(status)}
        </div>
      </aside>

      <main className="dm-app-stage">{children}</main>

      {!isSession ? (
        <nav className="dm-tab-bar" aria-label="App navigation">
          {DESTINATIONS.map((item) => {
            const Icon = item.icon;
            const active = destination === item.id;
            return (
              <button
                key={item.id}
                type="button"
                className="dm-tab-item"
                aria-current={active ? "page" : undefined}
                onClick={() => onNavigate(item.route)}
              >
                <span className="dm-tab-icon-wrap">
                  <Icon size={23} strokeWidth={active ? 2.15 : 1.75} aria-hidden="true" />
                  {item.id === "sessions" && attentionCount > 0 ? (
                    <span className="dm-tab-badge" aria-hidden="true">
                      {Math.min(attentionCount, 99)}
                    </span>
                  ) : null}
                </span>
                <span>{item.label}</span>
              </button>
            );
          })}
        </nav>
      ) : null}
    </div>
  );
}

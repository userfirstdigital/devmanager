import { CircleStop, Play, RefreshCw } from "lucide-react";

import {
  isLiveStatus,
  type SemanticEvent,
  type WebPortStatus,
  type WebProjectCommand,
  type WebSessionSummary,
} from "../../api/types";
import type { InterfaceDensity } from "../timeline/eventRenderers";
import { Timeline } from "../timeline/Timeline";

export function ServerSessionView({
  session,
  command,
  port,
  events,
  density,
  onStart,
  onStop,
  onRestart,
}: {
  session: WebSessionSummary;
  command: WebProjectCommand | null;
  port: WebPortStatus | null;
  events: SemanticEvent[];
  density: InterfaceDensity;
  onStart(): void;
  onStop(): void;
  onRestart(): void;
}) {
  const live = isLiveStatus(session.status);
  return (
    <div className="dm-session-body">
      <section className="dm-server-overview" aria-label="Server status">
        <div><span>Status</span><strong data-live={live || undefined}>{session.status}</strong></div>
        <div><span>Port</span><strong>{command?.port ?? "—"}</strong></div>
        <div><span>Process</span><strong>{port?.processName ?? (port?.inUse ? `PID ${port.pid ?? "?"}` : "—")}</strong></div>
      </section>
      <div className="dm-server-controls">
        {live ? (
          <>
            <button type="button" onClick={onRestart}><RefreshCw size={17} aria-hidden="true" />Restart</button>
            <button type="button" className="is-destructive" onClick={onStop}><CircleStop size={18} aria-hidden="true" />Stop</button>
          </>
        ) : (
          <button type="button" className="is-primary" onClick={onStart}><Play size={16} fill="currentColor" aria-hidden="true" />Start</button>
        )}
      </div>
      <Timeline
        events={events}
        density={density}
        emptyTitle="No server output yet"
        emptyDetail="Logs and lifecycle changes will appear here automatically."
      />
    </div>
  );
}

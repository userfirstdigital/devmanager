import { MoreHorizontal, PlugZap, RefreshCw, Unplug } from "lucide-react";
import type { ReactNode } from "react";

import type { SemanticEvent } from "../../api/types";
import type { InterfaceDensity } from "../timeline/eventRenderers";
import { Timeline } from "../timeline/Timeline";

export function CommandSessionView({
  events,
  density,
  connected,
  actionsDisabled,
  composer,
  onReconnect,
  onRestart,
  onDisconnect,
  disconnectLabel = "Disconnect",
}: {
  events: SemanticEvent[];
  density: InterfaceDensity;
  connected: boolean;
  actionsDisabled: boolean;
  composer: ReactNode;
  onReconnect?: () => void;
  onRestart?: () => void;
  onDisconnect?: () => void;
  disconnectLabel?: string;
}) {
  return (
    <div className="dm-session-body">
      {!connected && onReconnect && (
        <div className="dm-session-action-strip">
          <button type="button" className="dm-primary-inline-button" disabled={actionsDisabled} onClick={onReconnect}>
            <PlugZap size={17} aria-hidden="true" /> Connect
          </button>
        </div>
      )}
      {connected && (onRestart || onDisconnect) && (
        <div className="dm-session-action-strip dm-session-action-strip-end">
          <details className="dm-session-actions-menu">
            <summary aria-label="Session actions" aria-disabled={actionsDisabled || undefined}>
              <MoreHorizontal size={21} aria-hidden="true" />
            </summary>
            <div role="menu">
              {onRestart && (
                <button type="button" role="menuitem" disabled={actionsDisabled} onClick={onRestart}>
                  <RefreshCw size={17} aria-hidden="true" /> Restart
                </button>
              )}
              {onDisconnect && (
                <button
                  type="button"
                  role="menuitem"
                  disabled={actionsDisabled}
                  className="is-destructive"
                  onClick={onDisconnect}
                >
                  <Unplug size={17} aria-hidden="true" /> {disconnectLabel}
                </button>
              )}
            </div>
          </details>
        </div>
      )}
      <Timeline
        events={events}
        density={density}
        emptyTitle="Ready for a command"
        emptyDetail="Commands and their output will appear as a readable timeline."
      />
      {composer}
    </div>
  );
}

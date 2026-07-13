import { CircleStop, Play, Sparkles } from "lucide-react";
import type { ReactNode } from "react";

import type { SemanticAdapterHealth, SemanticEvent } from "../../api/types";
import type { InterfaceDensity } from "../timeline/eventRenderers";
import { Timeline } from "../timeline/Timeline";

export function AiSessionView({
  events,
  density,
  adapterHealth,
  running,
  actionsDisabled,
  composer,
  onInterrupt,
  onRestart,
}: {
  events: SemanticEvent[];
  density: InterfaceDensity;
  adapterHealth: SemanticAdapterHealth;
  running: boolean;
  actionsDisabled: boolean;
  composer: ReactNode;
  onInterrupt(): void;
  onRestart(): void;
}) {
  return (
    <div className="dm-session-body">
      {adapterHealth === "degraded" && (
        <div className="dm-native-notice" role="status">
          <Sparkles size={17} aria-hidden="true" />
          <span><strong>Native text mode</strong> · Rich activity cards are temporarily simplified.</span>
        </div>
      )}
      <div className="dm-session-action-strip">
        {running ? (
          <button type="button" className="dm-interrupt-button" disabled={actionsDisabled} onClick={onInterrupt}>
            <CircleStop size={17} aria-hidden="true" /> Interrupt
          </button>
        ) : (
          <button type="button" className="dm-primary-inline-button" disabled={actionsDisabled} onClick={onRestart}>
            <Play size={16} fill="currentColor" aria-hidden="true" /> Reopen session
          </button>
        )}
      </div>
      <Timeline
        events={events}
        density={density}
        emptyTitle="Start the conversation"
        emptyDetail="Messages and coding activity will stay readable here while you multitask."
      />
      {composer}
    </div>
  );
}

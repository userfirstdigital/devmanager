import { PlugZap } from "lucide-react";
import type { ReactNode } from "react";

import type { SemanticEvent } from "../../api/types";
import type { InterfaceDensity } from "../timeline/eventRenderers";
import { Timeline } from "../timeline/Timeline";

export function CommandSessionView({
  events,
  density,
  connected,
  composer,
  onReconnect,
}: {
  events: SemanticEvent[];
  density: InterfaceDensity;
  connected: boolean;
  composer: ReactNode;
  onReconnect?: () => void;
}) {
  return (
    <div className="dm-session-body">
      {!connected && onReconnect && (
        <div className="dm-session-action-strip">
          <button type="button" className="dm-primary-inline-button" onClick={onReconnect}>
            <PlugZap size={17} aria-hidden="true" /> Connect
          </button>
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

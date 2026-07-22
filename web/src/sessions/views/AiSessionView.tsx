import { Play, Sparkles } from "lucide-react";
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
  questionChoicesDisabled = false,
  composer,
  onRestart,
  onQuestionChoice,
}: {
  events: SemanticEvent[];
  density: InterfaceDensity;
  adapterHealth: SemanticAdapterHealth;
  running: boolean;
  actionsDisabled: boolean;
  questionChoicesDisabled?: boolean;
  composer: ReactNode;
  onRestart(): void;
  onQuestionChoice?(choice: string): void;
}) {
  return (
    <div className="dm-session-body">
      {adapterHealth === "degraded" && (
        <div className="dm-native-notice" role="status">
          <Sparkles size={15} aria-hidden="true" />
          <span>Live text remains available · activity detail is simplified for now.</span>
        </div>
      )}
      {!running ? (
        <div className="dm-session-action-strip">
          <button
            type="button"
            className="dm-primary-inline-button"
            disabled={actionsDisabled}
            onClick={onRestart}
          >
            <Play size={16} fill="currentColor" aria-hidden="true" /> Reopen
          </button>
        </div>
      ) : null}
      <Timeline
        events={events}
        density={density}
        includeFallbackOutput={adapterHealth !== "healthy"}
        emptyTitle="Start the conversation"
        emptyDetail="Messages and coding activity will stay readable here while you multitask."
        onQuestionChoice={onQuestionChoice}
        questionChoicesDisabled={questionChoicesDisabled || actionsDisabled}
      />
      {composer}
    </div>
  );
}

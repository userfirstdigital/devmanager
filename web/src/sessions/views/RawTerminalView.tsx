import { lazy, Suspense, useEffect } from "react";

import { MobileKeyRow } from "../../components/MobileKeyRow";
import { useStore } from "../../store";

const LazyTerminal = lazy(() =>
  import("../../components/Terminal").then((module) => ({ default: module.TerminalView })),
);

export function RawTerminalView({
  sessionId,
  interactionLabel,
}: {
  sessionId: string;
  interactionLabel?: string;
}) {
  const setRawTerminalSession = useStore((state) => state.setRawTerminalSession);

  useEffect(() => {
    setRawTerminalSession(sessionId);
    return () => setRawTerminalSession(null);
  }, [sessionId, setRawTerminalSession]);

  return (
    <div className="dm-raw-terminal" aria-label="Raw terminal view">
      {interactionLabel && (
        <div className="dm-provider-interaction-bar">
          <span>{interactionLabel}</span>
          <small>Provider interaction</small>
        </div>
      )}
      <Suspense fallback={<div className="dm-terminal-loading" role="status">Opening terminal…</div>}>
        <LazyTerminal sessionId={sessionId} />
        <MobileKeyRow sessionId={sessionId} />
      </Suspense>
    </div>
  );
}

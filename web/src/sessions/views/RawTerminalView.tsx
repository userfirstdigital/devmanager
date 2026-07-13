import { lazy, Suspense } from "react";

import { MobileKeyRow } from "../../components/MobileKeyRow";

const LazyTerminal = lazy(() =>
  import("../../components/Terminal").then((module) => ({ default: module.TerminalView })),
);

export function RawTerminalView({ sessionId }: { sessionId: string }) {
  return (
    <div className="dm-raw-terminal" aria-label="Raw terminal view">
      <Suspense fallback={<div className="dm-terminal-loading" role="status">Opening terminal…</div>}>
        <LazyTerminal sessionId={sessionId} />
        <MobileKeyRow sessionId={sessionId} />
      </Suspense>
    </div>
  );
}

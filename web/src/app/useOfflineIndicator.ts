import { useEffect, useState } from "react";

import type { WsStatus } from "../api/ws";

export const OFFLINE_INDICATOR_DELAY_MS = 7_000;

export function useOfflineIndicator(
  status: WsStatus,
  delayMs = OFFLINE_INDICATOR_DELAY_MS,
): boolean {
  const [visible, setVisible] = useState(false);

  useEffect(() => {
    setVisible(false);
    if (status.kind !== "closed") return;
    const timeout = globalThis.setTimeout(() => setVisible(true), delayMs);
    return () => globalThis.clearTimeout(timeout);
  }, [delayMs, status.kind]);

  return visible;
}

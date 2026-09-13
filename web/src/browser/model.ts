export type BrowserSecurityState = "secure" | "insecure" | "unknown";
export type BrowserInteractionMode = "observe" | "interact";
export type BrowserFrameKind = "full" | "tile";
export type BrowserRemoteInputKind = "pointer" | "touch" | "keyboard";

export type BrowserTabProjection = {
  tabId: string;
  title: string;
  url: string;
  kind: "page" | "popup";
  security: BrowserSecurityState;
  loading: boolean;
  error?: string;
};

export type BrowserProjection = {
  taskId: string;
  contextId: string;
  generation: number;
  boundsEpoch: number;
  focusEpoch: number;
  frameId: number;
  selectedTabId?: string;
  tabs: BrowserTabProjection[];
  progress?: string;
  interactionMode: BrowserInteractionMode;
  frameKind?: BrowserFrameKind;
  frameSrc?: string;
};

export type BrowserRemoteInput = {
  frameId: number;
  generation: number;
  boundsEpoch: number;
  focusEpoch: number;
  kind: BrowserRemoteInputKind;
  x: number;
  y: number;
  contentWidth: number;
  contentHeight: number;
  scale: number;
};

export type BrowserProjectionError =
  | "stale-generation"
  | "stale-frame"
  | "stale-bounds-epoch"
  | "stale-focus-epoch"
  | "not-local-dom";

export const MAX_BROWSER_PROJECTION_FPS = 8;
export const MAX_BROWSER_PROJECTION_BYTES_PER_SECOND = 512 * 1024;

export function pixelsAreLocalDom(): false {
  return false;
}

export function mapProjectedInput(
  projection: BrowserProjection,
  input: BrowserRemoteInput,
): { x: number; y: number } | BrowserProjectionError {
  if (input.generation === 0 || input.generation !== projection.generation) {
    return "stale-generation";
  }
  if (input.frameId === 0 || input.frameId !== projection.frameId) {
    return "stale-frame";
  }
  if (input.boundsEpoch === 0 || input.boundsEpoch !== projection.boundsEpoch) {
    return "stale-bounds-epoch";
  }
  if (input.focusEpoch === 0 || input.focusEpoch !== projection.focusEpoch) {
    return "stale-focus-epoch";
  }
  const x = Math.trunc((input.x * input.scale) / 96);
  const y = Math.trunc((input.y * input.scale) / 96);
  if (x < 0 || y < 0 || x >= input.contentWidth || y >= input.contentHeight) {
    return "stale-bounds-epoch";
  }
  return { x, y };
}

export function firstAnswerWins(
  pending: string | null,
  answered: string | null,
  requestId: string,
): "accepted" | "consumed" | "unknown" {
  if (answered === requestId) {
    return "consumed";
  }
  if (pending !== requestId) {
    return "unknown";
  }
  return "accepted";
}

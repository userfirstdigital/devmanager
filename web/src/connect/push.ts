const FORBIDDEN_KEYS = [
  "body",
  "prompt",
  "response",
  "transcript",
  "terminal",
  "browser",
  "diff",
  "file",
  "code",
] as const;

export interface ConnectPushPayload {
  hostId: string;
  taskId: string;
  attentionKind: "needsInput" | "completed" | "degraded";
  safeTitle?: string;
  timestampMs: number;
  route: string;
}

export function sanitizeConnectPush(
  value: unknown,
  allowSafeTitle: boolean,
): ConnectPushPayload | null {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    return null;
  }
  const record = value as Record<string, unknown>;
  for (const key of FORBIDDEN_KEYS) {
    if (key in record) return null;
  }
  if (
    typeof record.hostId !== "string" ||
    typeof record.taskId !== "string" ||
    typeof record.timestampMs !== "number" ||
    typeof record.route !== "string"
  ) {
    return null;
  }
  if (
    record.attentionKind !== "needsInput" &&
    record.attentionKind !== "completed" &&
    record.attentionKind !== "degraded"
  ) {
    return null;
  }
  if (
    record.route.includes("prompt=") ||
    record.route.includes("transcript") ||
    record.route.includes("diff")
  ) {
    return null;
  }
  const payload: ConnectPushPayload = {
    hostId: record.hostId,
    taskId: record.taskId,
    attentionKind: record.attentionKind,
    timestampMs: record.timestampMs,
    route: record.route,
  };
  if (allowSafeTitle && typeof record.safeTitle === "string") {
    if (
      record.safeTitle.includes("\n") ||
      record.safeTitle.toLowerCase().includes("diff --git")
    ) {
      return null;
    }
    payload.safeTitle = record.safeTitle;
  }
  return payload;
}

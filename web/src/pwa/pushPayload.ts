export interface PushPayload {
  title?: string;
  body?: string;
  route?: string;
  tag?: string;
  eventId?: string;
  runtimeInstanceId?: string;
  action?: "needsInput" | "completed" | "serverCrashed" | "sshDisconnected";
  badge?: number;
}

interface PushEventDataLike {
  json(): unknown;
}

export function parsePushPayload(value: unknown): PushPayload {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    return {};
  }

  const record = value as Record<string, unknown>;
  const payload: PushPayload = {};
  for (const field of [
    "title",
    "body",
    "route",
    "tag",
    "eventId",
    "runtimeInstanceId",
  ] as const) {
    if (typeof record[field] === "string") payload[field] = record[field];
  }
  if (
    record.action === "needsInput" ||
    record.action === "completed" ||
    record.action === "serverCrashed" ||
    record.action === "sshDisconnected"
  ) {
    payload.action = record.action;
  }
  if (
    typeof record.badge === "number" &&
    Number.isSafeInteger(record.badge) &&
    record.badge >= 0
  ) {
    payload.badge = record.badge;
  }
  return payload;
}

export function parsePushEventData(
  data: PushEventDataLike | null | undefined,
): PushPayload {
  if (!data) return {};
  try {
    return parsePushPayload(data.json());
  } catch {
    return {};
  }
}

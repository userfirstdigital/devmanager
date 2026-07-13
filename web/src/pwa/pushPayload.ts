export interface PushPayload {
  title?: string;
  body?: string;
  route?: string;
  tag?: string;
}

export function parsePushPayload(value: unknown): PushPayload {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    return {};
  }

  const record = value as Record<string, unknown>;
  const payload: PushPayload = {};
  for (const field of ["title", "body", "route", "tag"] as const) {
    if (typeof record[field] === "string") payload[field] = record[field];
  }
  return payload;
}

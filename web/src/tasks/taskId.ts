import type { StableSessionKey } from "../api/types";

/** Opaque Task identity used by product routes and Connect deep links. */
export type TaskId = string;

export type TaskResource = "chat" | "terminal" | "browser";

export const DEFAULT_TASK_RESOURCE: TaskResource = "chat";

const TASK_RESOURCES = new Set<TaskResource>(["chat", "terminal", "browser"]);

export function isTaskResource(value: string): value is TaskResource {
  return TASK_RESOURCES.has(value as TaskResource);
}

/**
 * Host wire identity still uses StableSessionKey. Until the web snapshot
 * carries TaskId directly, product routes treat that key as the TaskId.
 */
export function taskIdFromStableSessionKey(
  stableSessionKey: StableSessionKey,
): TaskId {
  return stableSessionKey;
}

export function taskIdToStableSessionKey(taskId: TaskId): StableSessionKey {
  return taskId;
}

export function parseTaskKeyParts(
  taskId: TaskId,
): { kind: "server" | "tab"; id: string } | null {
  const separator = taskId.indexOf(":");
  if (separator <= 0 || separator === taskId.length - 1) return null;
  const kind = taskId.slice(0, separator);
  if (kind !== "server" && kind !== "tab") return null;
  return { kind, id: taskId.slice(separator + 1) };
}

export function makeTaskId(kind: "server" | "tab", id: string): TaskId {
  return `${kind}:${id}`;
}

/**
 * Scoped host/task identity for multi-host phone projection.
 *
 * Qualification and stable-key pattern adapted from T3 Tools under the MIT
 * License. Copyright (c) 2026 T3 Tools Inc. Full notice: THIRD_PARTY_NOTICES.md
 * ("T3 Tools scoped projection / registry pattern").
 *
 * This module does not use T3's Effect/Atom runtime or bearer/DPoP transport.
 */

import { protocolUuid } from "./hostOutput";
import type { NativeUuid } from "./nativeProtocol";

/** Owner-scoped conversation identity. Domain UUIDs alone are never global. */
export interface ScopedHostTaskRef {
  hostPublicId: NativeUuid;
  taskId: NativeUuid;
}

/** Stable map/list key: `${hostPublicId}:${taskId}`. */
export function scopedHostTaskKey(ref: ScopedHostTaskRef): string {
  return `${ref.hostPublicId}:${ref.taskId}`;
}

export function scopeHostTask(
  hostPublicId: string,
  taskId: string,
): ScopedHostTaskRef | null {
  const host = protocolUuid(hostPublicId);
  const task = protocolUuid(taskId);
  if (!host || !task) return null;
  return { hostPublicId: host, taskId: task };
}

export function parseScopedHostTaskKey(key: string): ScopedHostTaskRef | null {
  const separatorIndex = key.indexOf(":");
  if (separatorIndex <= 0 || separatorIndex >= key.length - 1) {
    return null;
  }
  return scopeHostTask(
    key.slice(0, separatorIndex),
    key.slice(separatorIndex + 1),
  );
}

import { describe, expect, it } from "vitest";
import { decodeHostOutput, protocolUuid } from "./hostOutput";
import type { DecodedConnectEnvelope } from "./transport";

const id = "01234567-89ab-7000-8000-000000000055";
const bytes = [...id.replace(/-/g, "").matchAll(/../g)].map(([hex]) => parseInt(hex, 16));
function envelope(kind = 19): DecodedConnectEnvelope {
  return {
    protocolMajor: 1, protocolMinor: 0, connectionId: id, channelId: id, sessionId: id,
    channel: kind === 19 ? "durable" : kind === 20 ? "critical" : "ephemeral",
    sequence: 3, requestId: null, operationId: null, compression: "none", privacyClass: "local_only",
    payloadKind: kind, payloadVersion: 1, payloadBase64: "",
    limits: { max_physical_frame_bytes: 1024 * 1024, max_reassembled_message_bytes: 16 * 1024 * 1024,
      max_page_items: 1000, max_page_encoded_bytes: 512 * 1024, max_chunk_bytes: 256 * 1024, max_cumulative_bytes: 16 * 1024 * 1024 },
    payload: kind === 19 ? { required_capabilities: 2, message: { durable_event: {
      subscription_id: bytes, event: { id: bytes, task_id: bytes, sequence: 7, task_revision: 3, occurred_at_ms: 1,
        payload: { schema_version: 1, body: { task_reopened: {} } } },
    } } } : kind === 20 ? { required_capabilities: 2, message: { resync_required: {
      subscription_id: bytes, last_delivered_sequence: 7, newest_sequence: 10,
    } } } : kind === 21 ? { required_capabilities: 128, message: { stream: {
      subscription_id: bytes, stream: bytes, generation: 2, sequence: 4, payload_kind: 8, schema_version: 1, payload: bytes,
    } } } : { required_capabilities: 131104, message: { conversation_dirty: {
      subscription_id: bytes, task_id: bytes, high_water: 4,
    } } },
  };
}

describe("native host-output boundary", () => {
  it("normalizes only typed UUIDv7 fields and preserves content", () => {
    expect(protocolUuid(bytes)).toBe(id);
    expect(protocolUuid(id.toUpperCase())).toBe(id);
    expect(protocolUuid(new Array(16).fill(0))).toBeNull();
    expect(protocolUuid([...bytes.slice(0, 15), 256])).toBeNull();
    expect(decodeHostOutput(envelope(), 130)).toMatchObject({ message: { durable_event: {
      subscription_id: id, event: { id, task_id: id, sequence: 7, task_revision: 3 },
    } } });
    expect(decodeHostOutput(envelope(21), 130)).toMatchObject({ message: { stream: {
      subscription_id: id, stream: id, payload: bytes, generation: 2, sequence: 4,
    } } });
    expect(decodeHostOutput(envelope(22), 131104)).toEqual({ required_capabilities: 131104, message: { conversation_dirty: {
      subscription_id: id, task_id: id, high_water: 4,
    } } });
  });
  it.each([19, 20, 21, 22])("rejects forged wrapper metadata for kind %s", (kind) => {
    const base = envelope(kind);
    const negotiatedCapabilities = kind === 22 ? 131104 : 130;
    expect(() => decodeHostOutput(base, negotiatedCapabilities)).not.toThrow();
    for (const change of [
      { privacyClass: "managed_metadata" as const }, { payloadVersion: 2 },
      { requestId: id }, { operationId: id }, { channel: "unknown" },
    ]) expect(() => decodeHostOutput({ ...base, ...change } as DecodedConnectEnvelope, negotiatedCapabilities)).toThrow();
    expect(() => decodeHostOutput(base, 0)).toThrow();
    expect(() => decodeHostOutput(base, Number.MAX_SAFE_INTEGER + 1)).toThrow();
    expect(() => decodeHostOutput({ ...base, payload: { ...(base.payload as object), extra: 1 } }, negotiatedCapabilities)).toThrow();
  });
  it("keeps critical resync identity and rejects inverted cursors", () => {
    const base = envelope(20);
    expect(decodeHostOutput(base, 2)).toEqual({ required_capabilities: 2, message: { resync_required: {
      subscription_id: id, last_delivered_sequence: 7, newest_sequence: 10,
    } } });
    base.payload = { required_capabilities: 2, message: { resync_required: {
      subscription_id: bytes, last_delivered_sequence: 11, newest_sequence: 10,
    } } };
    expect(() => decodeHostOutput(base, 2)).toThrow();
  });
  it("rejects unknown streams, missing grants, unsafe sequences and invalid bytes", () => {
    const base = envelope(21);
    const payload = base.payload as { required_capabilities: number; message: { stream: Record<string, unknown> } };
    for (const change of [{ payload_kind: 1 }, { sequence: Number.MAX_SAFE_INTEGER + 1 },
      { payload: [256] }, { stream: "not-an-id" }, { schema_version: 0 }]) {
      expect(() => decodeHostOutput({ ...base, payload: { ...payload, message: {
        stream: { ...payload.message.stream, ...change },
      } } }, 130)).toThrow();
    }
    expect(() => decodeHostOutput({ ...base, payload: { ...payload, required_capabilities: 2 } }, 130)).toThrow();
    expect(() => decodeHostOutput({ ...base, privacyClass: "raw_content" }, 128)).not.toThrow();
    expect(() => decodeHostOutput({ ...envelope(), privacyClass: "raw_content" }, 2)).toThrow();
  });
});

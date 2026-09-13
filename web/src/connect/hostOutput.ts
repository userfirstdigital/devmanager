import type { DecodedConnectEnvelope } from "./transport";

export const HOST_DURABLE_OUTPUT = 19;
export const HOST_CRITICAL_OUTPUT = 20;
export const HOST_STREAM_OUTPUT = 21;
export const HOST_CONVERSATION_OUTPUT = 22;

export function isHostOutputKind(kind: number): boolean {
  return kind === HOST_DURABLE_OUTPUT || kind === HOST_CRITICAL_OUTPUT || kind === HOST_STREAM_OUTPUT || kind === HOST_CONVERSATION_OUTPUT;
}

/** Native UUIDs use MessagePack bin; JSON-shaped fixtures may use strings. */
export function protocolUuid(value: unknown): string | null {
  if (Array.isArray(value) && value.length === 16 && value.every(isByte)) {
    const hex = value.map((byte: number) => byte.toString(16).padStart(2, "0")).join("");
    value = `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
  }
  return typeof value === "string" && /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(value)
    ? value.toLowerCase() : null;
}

function isByte(value: unknown): value is number {
  return typeof value === "number" && Number.isInteger(value) && value >= 0 && value <= 255;
}
function record(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown> : null;
}
function exact(value: Record<string, unknown>, keys: string[]): boolean {
  return Object.keys(value).length === keys.length && keys.every((key) => Object.prototype.hasOwnProperty.call(value, key));
}
export function capabilityBits(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}
function unsigned(value: unknown): value is number {
  return capabilityBits(value);
}
function rejected(): never { throw new Error("Connect host output rejected"); }
function uuid(value: unknown): string { return protocolUuid(value) ?? rejected(); }

/** Validate the transport wrapper; a projection reducer still validates event semantics.
 * Normalize only typed ID fields, never arbitrary 16-byte content or event bodies.
 */
export function decodeHostOutput(envelope: DecodedConnectEnvelope, negotiatedCapabilities: number): unknown {
  const wrapper = record(envelope.payload);
  if (!wrapper || !exact(wrapper, ["required_capabilities", "message"]) ||
      !capabilityBits(wrapper.required_capabilities) || !capabilityBits(negotiatedCapabilities) ||
      (BigInt(wrapper.required_capabilities) & BigInt(negotiatedCapabilities)) !== BigInt(wrapper.required_capabilities) ||
      envelope.payloadVersion !== 1 || envelope.requestId !== null || envelope.operationId !== null ||
      envelope.privacyClass === "managed_metadata") rejected();
  const message = record(wrapper.message);
  if (!message || Object.keys(message).length !== 1) rejected();
  const required = BigInt(wrapper.required_capabilities);
  if (envelope.payloadKind === HOST_DURABLE_OUTPUT) {
    const body = record(message.durable_event);
    const event = body && record(body.event);
    if (envelope.channel !== "durable" || envelope.privacyClass !== "local_only" || (required & 2n) === 0n ||
        !body || !exact(body, ["subscription_id", "event"]) || !event ||
        !exact(event, ["id", "task_id", "sequence", "task_revision", "occurred_at_ms", "payload"]) ||
        !unsigned(event.sequence) || event.sequence === 0 ||
        (event.task_revision !== null && !unsigned(event.task_revision)) ||
        typeof event.occurred_at_ms !== "number" || !Number.isSafeInteger(event.occurred_at_ms) || !record(event.payload)) rejected();
    return { required_capabilities: wrapper.required_capabilities, message: { durable_event: {
      subscription_id: uuid(body.subscription_id),
      event: { ...event, id: uuid(event.id), task_id: event.task_id === null ? null : uuid(event.task_id) },
    } } };
  }
  if (envelope.payloadKind === HOST_CRITICAL_OUTPUT) {
    const body = record(message.resync_required);
    if (envelope.channel !== "critical" || envelope.privacyClass !== "local_only" || (required & 2n) === 0n ||
        !body || !exact(body, ["subscription_id", "last_delivered_sequence", "newest_sequence"]) ||
        !unsigned(body.last_delivered_sequence) || !unsigned(body.newest_sequence) || body.newest_sequence < body.last_delivered_sequence) rejected();
    return { required_capabilities: wrapper.required_capabilities, message: { resync_required: {
      ...body, subscription_id: uuid(body.subscription_id),
    } } };
  }
  if (envelope.payloadKind === HOST_STREAM_OUTPUT) {
    const body = record(message.stream);
    if (envelope.channel !== "ephemeral" || (required & 128n) === 0n || !body ||
        !exact(body, ["subscription_id", "stream", "generation", "sequence", "payload_kind", "schema_version", "payload"]) ||
        body.payload_kind !== 8 || !unsigned(body.generation) || !unsigned(body.sequence) ||
        !unsigned(body.schema_version) || body.schema_version === 0 || body.schema_version > 65535 ||
        !Array.isArray(body.payload) || body.payload.length > envelope.limits.max_reassembled_message_bytes || !body.payload.every(isByte)) rejected();
    return { required_capabilities: wrapper.required_capabilities, message: { stream: {
      ...body, subscription_id: uuid(body.subscription_id), stream: uuid(body.stream),
    } } };
  }
  if (envelope.payloadKind === HOST_CONVERSATION_OUTPUT) {
    const body = record(message.conversation_dirty);
    if (envelope.channel !== "ephemeral" || envelope.privacyClass !== "local_only" || required !== 131104n || !body ||
        !exact(body, ["subscription_id", "task_id", "high_water"]) || !unsigned(body.high_water) || body.high_water === 0) rejected();
    return { required_capabilities: wrapper.required_capabilities, message: { conversation_dirty: {
      subscription_id: uuid(body.subscription_id), task_id: uuid(body.task_id), high_water: body.high_water,
    } } };
  }
  return rejected();
}

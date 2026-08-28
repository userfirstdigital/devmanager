// This opt-in gate uses the actual generated Rust/WASM ABI, not a crypto mock.
// DEVMANAGER_CONNECT_WASM_DIR points at an isolated wasm-bindgen --target web build.
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { describe, expect, it } from "vitest";
import {
  bootstrapConnect,
  CONNECT_HOST_PUBLICATION_KEY,
  loadOrCreateConnectIdentity,
  type ConnectIdentityStorage,
  type PersistedConnectIdentity,
} from "../connect/identity";
import type { ConnectCryptoRuntime, ConnectWasmTransport } from "../connect/crypto";
import type { HostTrustRecord } from "../connect/hostTrust";
import type { ConnectEnvelopeJson, DecodedConnectEnvelope } from "../connect/transport";
import {
  NATIVE_BROWSER_CAPABILITIES,
  decodeCommandReceiptStatusQueryResult,
  decodeProviderInputStateQueryResult,
  decodeQueryReply,
  decodeTaskCockpitConversationResult,
} from "../connect/nativeProtocol";

const artifactDirectory = process.env.DEVMANAGER_CONNECT_WASM_DIR;

function memoryStorage(): ConnectIdentityStorage {
  let record: PersistedConnectIdentity | null = null;
  return {
    async load() { return record; },
    async putIfAbsent(next) { return record ??= next; },
    async clear() { record = null; },
  };
}

function id(tail: number): Uint8Array {
  const bytes = new Uint8Array(16);
  bytes.set([1, 35, 69, 103, 137, 171, 112, 0, 128]);
  bytes[15] = tail;
  return bytes;
}

class LoopbackSocket {
  readyState = 0;
  binaryType: BinaryType = "arraybuffer";
  onopen: ((event: unknown) => void) | null = null;
  onmessage: ((event: { data: unknown }) => void) | null = null;
  onclose: ((event: { code?: number; reason?: string }) => void) | null = null;
  onerror: ((event: unknown) => void) | null = null;
  constructor(private readonly receive: (bytes: Uint8Array) => void) {}
  send(bytes: Uint8Array) {
    const copy = bytes.slice();
    queueMicrotask(() => { if (this.readyState === 1) this.receive(copy); });
  }
  emit(bytes: Uint8Array) {
    const copy = bytes.slice();
    queueMicrotask(() => { if (this.readyState === 1) this.onmessage?.({ data: copy }); });
  }
  close() { this.readyState = 3; this.onclose?.({ code: 1000 }); }
  open() { this.readyState = 1; this.onopen?.({}); }
}

describe.skipIf(!artifactDirectory)("real Rust/WASM browser custody integration", () => {
  it("reuses wrapped identity across host restarts and completes authenticated request/reply", async () => {
    const moduleUrl = pathToFileURL(join(artifactDirectory!, "connect_crypto.js")).href;
    const runtime = await import(/* @vite-ignore */ moduleUrl) as ConnectCryptoRuntime & {
      default(input: { module_or_path: Uint8Array }): Promise<unknown>;
    };
    await runtime.default({ module_or_path: await readFile(join(artifactDirectory!, "connect_crypto_bg.wasm")) });
    const nativeFixtures = JSON.parse(await readFile(new URL("../../../tests/fixtures/connect/v1/native-payloads.json", import.meta.url), "utf8")) as Array<{
      name: string; payloadKind: number; channel: ConnectEnvelopeJson["channel"]; payloadBase64: string;
    }>;
    expect(nativeFixtures.map((fixture) => fixture.name)).toEqual(expect.arrayContaining([
      "host_conversation_output", "command_receipt_status_query", "command_receipt_status_result",
    ]));
    // These are native rmp_serde payloads, including real UUID bin16 fields.
    for (const fixture of nativeFixtures) {
      const payload = JSON.parse(runtime.decode_connect_payload_json(
        Uint8Array.from(atob(fixture.payloadBase64), (ch) => ch.charCodeAt(0)),
      ));
      if (fixture.name === "provider_input_state") {
        const requestId = "01234567-89ab-7000-8000-000000000041";
        const view = decodeProviderInputStateQueryResult(decodeQueryReply(payload, requestId), {
          hostPublicId: "01234567-89ab-7000-8000-000000000017",
          clientId: "01234567-89ab-7000-8000-000000000042", requestId,
        }, "01234567-89ab-7000-8000-000000000043");
        expect(view.providerSessionId).toBe("native-exact-conversation");
        expect(view.runtimeGeneration).toBe(7);
      } else if (fixture.name.startsWith("conversation_")) {
        const page = decodeTaskCockpitConversationResult(decodeQueryReply(payload,
          "01234567-89ab-7000-8000-000000000041"));
        expect(page.facts[0].payload).toEqual({ kind: "assistant_text", text: "Native conversation text" });
        expect(page.cursorRolledOver).toBe(fixture.name === "conversation_rollover");
        expect(page.nextSequence).toBe(fixture.name === "conversation_page" ? 1 : null);
      } else if (fixture.name === "command_receipt_status_query") {
        expect(payload.query.command_receipt_status.command).toMatchObject({
          client_id: Array.from(id(0x62)),
          task_id: Array.from(id(0x63)),
          command_id: Array.from(id(0x64)),
          command: "begin_close_task",
        });
      } else if (fixture.name === "command_receipt_status_result") {
        const receipt = decodeCommandReceiptStatusQueryResult(decodeQueryReply(payload,
          "01234567-89ab-7000-8000-000000000061"), "01234567-89ab-7000-8000-000000000064");
        expect(receipt).toMatchObject({ kind: "accepted", taskRevision: 4,
          operationId: "01234567-89ab-7000-8000-000000000065" });
      }
    }
    const browserStorage = memoryStorage();
    const hostCustody = await loadOrCreateConnectIdentity(1, { storage: memoryStorage() });
    let trustedHost: HostTrustRecord | null = null;
    let firstDeviceId: string | undefined;

    // Deliberately decrease host generation: a durable device is not a process.
    for (const generation of [7, 2]) {
      const privateBytes = await hostCustody.unwrapPrivateKey();
      const handshake = new runtime.WasmConnectHandshake(
        runtime.connect_noise_pattern(true), true, 1, privateBytes,
        hostCustody.publicKey, undefined, id(17), undefined, id(18), id(19),
        1, BigInt(Math.floor(Date.now() / 1000)), true,
      );
      privateBytes.fill(0);
      let channel: ConnectWasmTransport | null = null;
      let sendSequence = 0n;
      let lastEnvelope: ConnectEnvelopeJson | null = null;
      const outputs: DecodedConnectEnvelope[] = [];
      const received: unknown[] = [];
      const failures: unknown[] = [];
      const socket = new LoopbackSocket((bytes) => {
        try {
          if (!channel) {
            handshake.read_message(bytes);
            if (handshake.is_finished()) {
              channel = handshake.finish();
            } else {
              socket.emit(handshake.write_message());
            }
            return;
          }
          const envelope = JSON.parse(runtime.decode_connect_envelope_json(channel.open(bytes))) as ConnectEnvelopeJson;
          expect(envelope.connectionId).toBe("01234567-89ab-7000-8000-000000000012");
          expect(envelope.sessionId).toBe("01234567-89ab-7000-8000-000000000013");
          lastEnvelope = envelope;
          const payload = JSON.parse(runtime.decode_connect_payload_json(
            Uint8Array.from(atob(envelope.payloadBase64), (value) => value.charCodeAt(0)),
          )) as Record<string, unknown>;
          received.push(payload);
          const responsePayload = envelope.payloadKind === 1
            ? { ...payload, client_id: envelope.connectionId }
            : { request_id: envelope.requestId, outcome: { err: { unavailable: { reason: "fixture reply" } } } };
          const response = {
            ...envelope,
            channel: "critical",
            sequence: Number(++sendSequence),
            payloadKind: envelope.payloadKind === 1 ? 1 : 18,
            payloadBase64: btoa(String.fromCharCode(...runtime.encode_connect_payload_json(JSON.stringify(responsePayload)))),
          };
          const sealed = channel.seal(sendSequence, crypto.getRandomValues(new Uint8Array(16)),
            runtime.encode_connect_envelope_json(JSON.stringify(response)));
          socket.emit(sealed);
        } catch (error) { failures.push(error); }
      });
      const handle = await bootstrapConnect({
        host: { [CONNECT_HOST_PUBLICATION_KEY]: {
          transport: "connect", endpoint: "/api/connect", generation, protocolMajor: 1, protocolMinor: 0,
          hostPublicId: "01234567-89ab-7000-8000-000000000011",
          hostPublicKey: Array.from(hostCustody.publicKey, (byte) => byte.toString(16).padStart(2, "0")).join(""),
        } },
        hostTrustStorage: { async pin(record) { return trustedHost ??= record; } },
        storage: browserStorage,
        fetch: async () => new Response(null, { status: 200 }),
        location: { protocol: "http:", host: "localhost:43872" },
        transportOptions: { cryptoLoader: async () => runtime, socketFactory: () => socket,
          onEnvelope: (envelope) => outputs.push(envelope) },
      });
      expect(handle).not.toBeNull();
      try {
        firstDeviceId ??= handle!.identity.deviceId;
        expect(handle!.identity.deviceId).toBe(firstDeviceId);
        await handle!.transport.start();
        socket.open();
        const greeting = new Uint8Array(53);
        greeting.set(new TextEncoder().encode("DMCN1"));
        greeting.set(id(17), 5); greeting.set(id(18), 21); greeting.set(id(19), 37);
        socket.emit(greeting);
        await expect.poll(() => handle!.transport.state().kind).toBe("ready");
        // Exercise bootstrap's production default, not a test-only capability override.
        expect(received[0]).toMatchObject({ capabilities: NATIVE_BROWSER_CAPABILITIES });
        const requestId = "01234567-89ab-7000-8000-000000000099";
        const reply = await handle!.transport.request(5, {
          request_id: requestId, required_capabilities: 0,
          query: { kind: "fixture-query" },
        }, { requestId });
        expect(reply.payloadKind).toBe(18);
        expect(reply.requestId).toBe(requestId);
        expect(reply.payload).toEqual({ request_id: requestId,
          outcome: { err: { unavailable: { reason: "fixture reply" } } } });
        expect(received).toHaveLength(2);
        expect(failures).toEqual([]);
        for (const fixture of nativeFixtures.filter((entry) => entry.payloadKind >= 19)) {
          const output: ConnectEnvelopeJson = {
            ...lastEnvelope!, sequence: Number(++sendSequence), channel: fixture.channel,
            requestId: null, operationId: null, payloadKind: fixture.payloadKind,
            payloadBase64: fixture.payloadBase64, privacyClass: "local_only",
          };
          socket.emit((channel as ConnectWasmTransport | null)!.seal(sendSequence, crypto.getRandomValues(new Uint8Array(16)),
            runtime.encode_connect_envelope_json(JSON.stringify(output))));
        }
        await expect.poll(() => outputs.filter((entry) => entry.payloadKind >= 19).length).toBe(4);
        expect(outputs.find((entry) => entry.payloadKind === 19)?.payload).toMatchObject({ message: { durable_event: {
          subscription_id: "01234567-89ab-7000-8000-000000000071", event: { id: "01234567-89ab-7000-8000-000000000055", sequence: 3 },
        } } });
        expect(outputs.find((entry) => entry.payloadKind === 21)?.payload).toMatchObject({ message: { stream: {
          stream: "01234567-89ab-7000-8000-000000000074", payload: [0x62, 0x72],
        } } });
        expect(outputs.find((entry) => entry.payloadKind === 22)?.payload).toMatchObject({
          required_capabilities: 131104,
          message: { conversation_dirty: {
            subscription_id: "01234567-89ab-7000-8000-000000000075",
            task_id: "01234567-89ab-7000-8000-000000000043", high_water: 4,
          } },
        });
      } finally {
        handle?.stop();
        (channel as ConnectWasmTransport | null)?.free?.();
        handshake.free?.();
      }
    }
  });
});

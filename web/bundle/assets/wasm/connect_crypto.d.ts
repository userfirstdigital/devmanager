/* tslint:disable */
/* eslint-disable */

/**
 * A Rust-owned Noise XX/IK state machine. Private material stays in native
 * Rust/wasm memory and is never converted to a string or logged.
 */
export class WasmConnectHandshake {
    free(): void;
    [Symbol.dispose](): void;
    finish(): WasmConnectTransport;
    is_finished(): boolean;
    constructor(pattern: string, first_pairing: boolean, role_value: number, private_key: Uint8Array, local_public: Uint8Array, expected_remote: Uint8Array | null | undefined, host_public_id: Uint8Array, device_public_id: Uint8Array | null | undefined, route_id: Uint8Array, session_id: Uint8Array, purpose_value: number, opened_at_unix: bigint, direct_reachable: boolean);
    read_message(encoded: Uint8Array): void;
    write_message(): Uint8Array;
}

/**
 * Rust-owned ChaChaPoly/BLAKE2s transport and native sealed-frame codec.
 */
export class WasmConnectTransport {
    private constructor();
    free(): void;
    [Symbol.dispose](): void;
    open(encoded: Uint8Array): Uint8Array;
    seal(sequence: bigint, nonce: Uint8Array, plaintext: Uint8Array): Uint8Array;
}

export function connect_noise_pattern(first_pairing: boolean): string;

export function connect_protocol_major(): number;

/**
 * Decode only the bounded envelope metadata. Payload bytes remain base64 in
 * this diagnostic/dispatch ABI and are never logged or interpolated in an
 * error string.
 */
export function decode_connect_envelope_json(input: Uint8Array): string;

export function decode_connect_payload_json(input: Uint8Array): string;

/**
 * Encode a ConnectEnvelope from its stable, non-secret JSON ABI into the
 * native named-field MessagePack wire format.
 */
export function encode_connect_envelope_json(input: string): Uint8Array;

/**
 * Encode a typed Connect payload JSON document into the native named-field
 * MessagePack wire format.
 *
 * Binary fields (for example native query `resume_cursor`) must use the exact
 * marker object `{"$connectBinary":"<STANDARD padded base64>"}`. That marker
 * serializes as MessagePack BIN. Ordinary JSON arrays remain arrays; UUID and
 * other identity strings remain strings. Envelope `payloadBase64` is a
 * separate envelope-level ABI and is unchanged here.
 */
export function encode_connect_payload_json(input: string): Uint8Array;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly decode_connect_envelope_json: (a: number, b: number) => [number, number, number, number];
    readonly decode_connect_payload_json: (a: number, b: number) => [number, number, number, number];
    readonly encode_connect_envelope_json: (a: number, b: number) => [number, number, number, number];
    readonly encode_connect_payload_json: (a: number, b: number) => [number, number, number, number];
    readonly __wbg_wasmconnecthandshake_free: (a: number, b: number) => void;
    readonly __wbg_wasmconnecttransport_free: (a: number, b: number) => void;
    readonly connect_noise_pattern: (a: number) => [number, number];
    readonly connect_protocol_major: () => number;
    readonly wasmconnecthandshake_finish: (a: number) => [number, number, number];
    readonly wasmconnecthandshake_is_finished: (a: number) => number;
    readonly wasmconnecthandshake_new: (a: number, b: number, c: number, d: number, e: number, f: number, g: number, h: number, i: number, j: number, k: number, l: number, m: number, n: number, o: number, p: number, q: number, r: number, s: number, t: bigint, u: number) => [number, number, number];
    readonly wasmconnecthandshake_read_message: (a: number, b: number, c: number) => [number, number];
    readonly wasmconnecthandshake_write_message: (a: number) => [number, number, number, number];
    readonly wasmconnecttransport_open: (a: number, b: number, c: number) => [number, number, number, number];
    readonly wasmconnecttransport_seal: (a: number, b: bigint, c: number, d: number, e: number, f: number) => [number, number, number, number];
    readonly __wbindgen_exn_store: (a: number) => void;
    readonly __externref_table_alloc: () => number;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __externref_table_dealloc: (a: number) => void;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;

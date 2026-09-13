/* tslint:disable */
/* eslint-disable */
export const memory: WebAssembly.Memory;
export const decode_connect_envelope_json: (a: number, b: number) => [number, number, number, number];
export const decode_connect_payload_json: (a: number, b: number) => [number, number, number, number];
export const encode_connect_envelope_json: (a: number, b: number) => [number, number, number, number];
export const encode_connect_payload_json: (a: number, b: number) => [number, number, number, number];
export const __wbg_wasmconnecthandshake_free: (a: number, b: number) => void;
export const __wbg_wasmconnecttransport_free: (a: number, b: number) => void;
export const connect_noise_pattern: (a: number) => [number, number];
export const connect_protocol_major: () => number;
export const wasmconnecthandshake_finish: (a: number) => [number, number, number];
export const wasmconnecthandshake_is_finished: (a: number) => number;
export const wasmconnecthandshake_new: (a: number, b: number, c: number, d: number, e: number, f: number, g: number, h: number, i: number, j: number, k: number, l: number, m: number, n: number, o: number, p: number, q: number, r: number, s: number, t: bigint, u: number) => [number, number, number];
export const wasmconnecthandshake_read_message: (a: number, b: number, c: number) => [number, number];
export const wasmconnecthandshake_write_message: (a: number) => [number, number, number, number];
export const wasmconnecttransport_open: (a: number, b: number, c: number) => [number, number, number, number];
export const wasmconnecttransport_seal: (a: number, b: bigint, c: number, d: number, e: number, f: number) => [number, number, number, number];
export const __wbindgen_exn_store: (a: number) => void;
export const __externref_table_alloc: () => number;
export const __wbindgen_externrefs: WebAssembly.Table;
export const __wbindgen_free: (a: number, b: number, c: number) => void;
export const __wbindgen_malloc: (a: number, b: number) => number;
export const __externref_table_dealloc: (a: number) => void;
export const __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
export const __wbindgen_start: () => void;

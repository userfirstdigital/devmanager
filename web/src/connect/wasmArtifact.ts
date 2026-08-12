/**
 * Non-secret contract for the generated Connect Rust/WASM leaf.
 *
 * The implementation is intentionally generated outside the TypeScript
 * source tree.  Vite copies these files from `src/connect/wasm` to the
 * relative `assets/wasm` package directory when they are present.  A clean
 * checkout therefore remains usable for ordinary web work while the runtime
 * reports a typed HOLD until the reviewed artifact is explicitly produced.
 */

export const CONNECT_CRYPTO_WASM_ARTIFACT_SCHEMA_VERSION = 1 as const;
export const CONNECT_CRYPTO_WASM_PROTOCOL_MAJOR = 1 as const;
export const CONNECT_CRYPTO_WASM_MODULE_PATH = "./wasm/connect_crypto.js" as const;
export const CONNECT_CRYPTO_WASM_ARTIFACT_DIRECTORY = "assets/wasm" as const;
export const CONNECT_CRYPTO_WASM_MANIFEST_NAME =
  "connect_crypto.manifest.json" as const;

export const CONNECT_CRYPTO_WASM_REQUIRED_FILES = [
  "connect_crypto.js",
  "connect_crypto_bg.wasm",
] as const;

export const CONNECT_CRYPTO_WASM_OPTIONAL_FILES = [
  "connect_crypto.d.ts",
] as const;

export const CONNECT_CRYPTO_WASM_EXPORTS = [
  "WasmConnectHandshake",
  "connect_protocol_major",
  "connect_noise_pattern",
  "encode_connect_envelope_json",
  "decode_connect_envelope_json",
  "encode_connect_payload_json",
  "decode_connect_payload_json",
] as const;

export type ConnectCryptoWasmArtifactFile = {
  path: string;
  bytes: number;
  sha256: string;
};

export type ConnectCryptoWasmArtifactManifest = {
  schemaVersion: typeof CONNECT_CRYPTO_WASM_ARTIFACT_SCHEMA_VERSION;
  artifact: "connect-crypto";
  protocolMajor: typeof CONNECT_CRYPTO_WASM_PROTOCOL_MAJOR;
  target: "wasm32-unknown-unknown";
  rustToolchain: "1.94.0";
  wasmBindgenVersion: "0.2.114";
  modulePath: typeof CONNECT_CRYPTO_WASM_MODULE_PATH;
  files: ConnectCryptoWasmArtifactFile[];
};

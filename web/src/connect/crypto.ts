/**
 * Rust/WASM Connect crypto boundary.
 *
 * This module intentionally contains no cryptographic primitive and no
 * fallback implementation. The generated wasm-bindgen module is loaded only
 * after its protocol identity is checked; any load/identity failure remains a
 * typed browser HOLD.
 */

export const CONNECT_BROWSER_E2E_HOLD = "browser-e2e-transport-held" as const;

export class ConnectCryptoHoldError extends Error {
  readonly code = CONNECT_BROWSER_E2E_HOLD;

  constructor(message = "Connect Rust/WASM crypto is unavailable") {
    super(message);
    this.name = "ConnectCryptoHoldError";
  }
}

export interface ConnectWasmHandshake {
  write_message(): Uint8Array;
  read_message(encoded: Uint8Array): void;
  is_finished(): boolean;
  finish(): ConnectWasmTransport;
}

export interface ConnectWasmTransport {
  seal(sequence: bigint, nonce: Uint8Array, plaintext: Uint8Array): Uint8Array;
  open(encoded: Uint8Array): Uint8Array;
}

export interface ConnectCryptoRuntime {
  WasmConnectHandshake: new (
    pattern: string,
    firstPairing: boolean,
    role: number,
    privateKey: Uint8Array,
    localPublic: Uint8Array,
    expectedRemote: Uint8Array | undefined,
    hostPublicId: Uint8Array,
    devicePublicId: Uint8Array | undefined,
    routeId: Uint8Array,
    sessionId: Uint8Array,
    purpose: number,
    openedAtUnix: bigint,
    directReachable: boolean,
  ) => ConnectWasmHandshake;
  connect_protocol_major(): number;
  connect_noise_pattern(firstPairing: boolean): string;
  encode_connect_envelope_json(input: string): Uint8Array;
  decode_connect_envelope_json(input: Uint8Array): string;
  encode_connect_payload_json(input: string): Uint8Array;
  decode_connect_payload_json(input: Uint8Array): string;
}

export type ConnectCryptoLoader = () => Promise<ConnectCryptoRuntime>;

// Keep the optional generated module out of Vite's static asset graph. A
// source checkout may not contain it; the loader must preserve the typed HOLD
// rather than turning a missing optional module into a build-time import.
const wasmModulePath = [".", "wasm", "connect_crypto.js"].join("/");

/** Default loader for the wasm-bindgen output produced by the native-next build. */
export const loadConnectCrypto: ConnectCryptoLoader = async () => {
  try {
    // The generated module is intentionally resolved at runtime so source
    // checkouts do not carry a checked-in wasm binary or generated artifact.
    const module = (await import(
      /* @vite-ignore */ wasmModulePath
    )) as ConnectCryptoRuntime & {
      default?: (input?: unknown) => Promise<unknown>;
    };
    if (typeof module.default === "function") await module.default();
    const runtime = module as ConnectCryptoRuntime;
    if (
      runtime.connect_protocol_major() !== 1 ||
      runtime.connect_noise_pattern(true) !==
        "Noise_XX_25519_ChaChaPoly_BLAKE2s" ||
      runtime.connect_noise_pattern(false) !==
        "Noise_IK_25519_ChaChaPoly_BLAKE2s" ||
      typeof runtime.WasmConnectHandshake !== "function" ||
      typeof runtime.encode_connect_envelope_json !== "function"
    ) {
      throw new ConnectCryptoHoldError("Connect WASM protocol identity rejected");
    }
    return runtime;
  } catch (error) {
    if (error instanceof ConnectCryptoHoldError) throw error;
    // Do not include import errors or module data: generated errors can carry
    // paths, URLs, or accidental build metadata.
    throw new ConnectCryptoHoldError();
  }
};

export async function resolveConnectCrypto(
  loader: ConnectCryptoLoader = loadConnectCrypto,
): Promise<ConnectCryptoRuntime> {
  try {
    return await loader();
  } catch (error) {
    if (error instanceof ConnectCryptoHoldError) throw error;
    throw new ConnectCryptoHoldError();
  }
}

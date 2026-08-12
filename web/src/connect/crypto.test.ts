import { describe, expect, it, vi } from "vitest";

import {
  CONNECT_BROWSER_E2E_HOLD,
  ConnectCryptoHoldError,
  resolveConnectCrypto,
  type ConnectCryptoRuntime,
} from "./crypto";

function runtimeFixture(): ConnectCryptoRuntime {
  return {
    WasmConnectHandshake: vi.fn() as unknown as ConnectCryptoRuntime["WasmConnectHandshake"],
    connect_protocol_major: () => 1,
    connect_noise_pattern: (firstPairing) =>
      firstPairing
        ? "Noise_XX_25519_ChaChaPoly_BLAKE2s"
        : "Noise_IK_25519_ChaChaPoly_BLAKE2s",
    encode_connect_envelope_json: () => new Uint8Array([1]),
    decode_connect_envelope_json: () => "{}",
    encode_connect_payload_json: () => new Uint8Array([1]),
    decode_connect_payload_json: () => "{}",
  };
}

describe("Connect Rust/WASM loader", () => {
  it("keeps missing or failed WASM as a typed, redacted HOLD", async () => {
    const secret = "private-key-material";
    const loader = vi.fn(async () => {
      throw new Error(secret);
    });

    const promise = resolveConnectCrypto(loader);
    await expect(promise).rejects.toBeInstanceOf(ConnectCryptoHoldError);
    await expect(promise).rejects.toMatchObject({
      code: CONNECT_BROWSER_E2E_HOLD,
    });
    await promise.catch((error: unknown) => {
      expect(String(error)).not.toContain(secret);
    });
  });

  it("accepts only the exact server protocol identity", async () => {
    const runtime = runtimeFixture();
    await expect(resolveConnectCrypto(async () => runtime)).resolves.toBe(runtime);
  });
});

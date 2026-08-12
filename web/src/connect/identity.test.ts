import { describe, expect, it } from "vitest";

import {
  CONNECT_HOST_PUBLICATION_KEY,
  CONNECT_IDENTITY_HOLD,
  ConnectBrowserIdentityHoldError,
  bootstrapConnect,
  readConnectHostPublication,
} from "./identity";

const marker = {
  transport: "connect" as const,
  endpoint: "/api/connect",
  generation: 7,
  protocolMajor: 1,
  protocolMinor: 0,
};

describe("Connect browser identity bootstrap", () => {
  it("accepts only the bounded current host publication marker", () => {
    expect(
      readConnectHostPublication({ [CONNECT_HOST_PUBLICATION_KEY]: marker }),
    ).toEqual(marker);
    expect(
      readConnectHostPublication({
        [CONNECT_HOST_PUBLICATION_KEY]: { ...marker, endpoint: "/api/ws" },
      }),
    ).toBeNull();
    expect(
      readConnectHostPublication({
        [CONNECT_HOST_PUBLICATION_KEY]: { ...marker, generation: 0 },
      }),
    ).toBeNull();
  });

  it("does not install a transport without a host publication", async () => {
    const host: Record<string, unknown> = {};
    expect(await bootstrapConnect({ host })).toBeNull();
    expect(host.__DEVMANAGER_CONNECT__).toBeUndefined();
  });

  it("holds before identity creation when the pair cookie is absent", async () => {
    const host: Record<string, unknown> = {
      [CONNECT_HOST_PUBLICATION_KEY]: marker,
    };
    const fetcher = async () => new Response(null, { status: 401 });
    await expect(bootstrapConnect({ host, fetch: fetcher })).rejects.toMatchObject({
      code: CONNECT_IDENTITY_HOLD,
    });
    expect(host.__DEVMANAGER_CONNECT__).toBeUndefined();
  });

  it("uses a typed hold rather than a plaintext fallback when WebCrypto cannot create X25519", async () => {
    const host: Record<string, unknown> = {
      [CONNECT_HOST_PUBLICATION_KEY]: marker,
    };
    const fetcher = async () => new Response(JSON.stringify({ ok: true }), { status: 200 });
    const crypto = {
      subtle: {
        generateKey: async () => {
          throw new Error("X25519 unavailable");
        },
      },
      getRandomValues<T extends ArrayBufferView | null>(array: T): T {
        return array;
      },
    } as unknown as Crypto;
    await expect(
      bootstrapConnect({ host, fetch: fetcher, crypto }),
    ).rejects.toBeInstanceOf(ConnectBrowserIdentityHoldError);
    expect(host.__DEVMANAGER_CONNECT__).toMatchObject({
      transport: "connect",
      endpoint: "/api/connect",
      generation: 7,
    });
  });
});

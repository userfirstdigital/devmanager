import { describe, expect, it } from "vitest";

import {
  CONNECT_STORE_CONFIGURATION_KEY,
  readConnectStoreConfiguration,
  selectStoreClientOptions,
  type ConnectStoreConfiguration,
} from "./storeAdapter";

function fakeConnectConfiguration(
  overrides: Partial<ConnectStoreConfiguration> = {},
): ConnectStoreConfiguration {
  return {
    transport: "connect",
    ...overrides,
  };
}

describe("Connect store adapter", () => {
  it("keeps the existing legacy client when no Connect configuration is published", () => {
    expect(selectStoreClientOptions(null)).toEqual({ transport: "legacy" });
  });

  it("selects Connect and preserves its typed transport adapters", () => {
    const connectTransport = {
      start: async () => {},
      stop: () => {},
      state: () => ({ kind: "idle" as const }),
      subscribe: () => () => {},
      subscribeEnvelope: () => () => {},
    } as unknown as ConnectStoreConfiguration["connectTransport"];
    const connectRequest = () => null;
    const connectMessage = () => null;
    const config = fakeConnectConfiguration({
      connectTransport,
      connectRequest,
      connectMessage,
      relayUrl: "wss://relay.example/connect",
    });

    expect(selectStoreClientOptions(config)).toEqual(config);
  });

  it("does not downgrade an explicitly configured Connect deployment when WASM is missing", () => {
    const config = fakeConnectConfiguration();

    expect(selectStoreClientOptions(config)).toEqual({ transport: "connect" });
  });

  it("reads only the typed Connect marker from the runtime host", () => {
    const config = fakeConnectConfiguration();
    const host = {
      [CONNECT_STORE_CONFIGURATION_KEY]: config,
      unrelated: { transport: "connect" },
    };

    expect(readConnectStoreConfiguration(host)).toEqual(config);
    expect(
      readConnectStoreConfiguration({
        [CONNECT_STORE_CONFIGURATION_KEY]: { transport: "legacy" },
      }),
    ).toBeNull();
    expect(readConnectStoreConfiguration({})).toBeNull();
  });
});

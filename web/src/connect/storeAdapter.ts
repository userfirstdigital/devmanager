import type {
  RemoteAction,
  RemoteActionResult,
  ResumeContext,
  WsOutbound,
} from "../api/types";
import type { WsClientOptions } from "../api/ws";
import type {
  ConnectBrowserTransport,
  ConnectPayloadRequest,
  DecodedConnectEnvelope,
} from "./transport";

/**
 * Runtime handoff used by the web shell when a paired Connect session has
 * already been constructed by the host/browser identity layer.
 *
 * The store deliberately receives an owned transport object rather than key
 * bytes. Browser private identity stays inside the Connect crypto boundary;
 * this adapter only supplies the projection callbacks needed to feed the
 * existing workspace store.
 */
export type ConnectStoreConfiguration = Omit<
  WsClientOptions,
  "transport"
> & {
  transport: "connect";
};

/** Stable, non-secret runtime configuration hook for the embedded web shell. */
export const CONNECT_STORE_CONFIGURATION_KEY =
  "__DEVMANAGER_CONNECT__" as const;

type RuntimeHost = Record<string, unknown>;

function isRuntimeHost(value: unknown): value is RuntimeHost {
  return typeof value === "object" && value !== null;
}

function isFunction(value: unknown): value is (...args: never[]) => unknown {
  return typeof value === "function";
}

function isConnectTransport(value: unknown): value is ConnectBrowserTransport {
  if (!isRuntimeHost(value)) return false;
  return (
    isFunction(value.start) &&
    isFunction(value.stop) &&
    isFunction(value.state) &&
    isFunction(value.subscribe) &&
    isFunction(value.subscribeEnvelope)
  );
}

function copyOptionalFunction<T extends (...args: never[]) => unknown>(
  source: RuntimeHost,
  key: string,
): T | undefined {
  const value = source[key];
  return isFunction(value) ? (value as T) : undefined;
}

/**
 * Read the host-published Connect marker without treating arbitrary runtime
 * data as a transport configuration. A malformed Connect marker remains a
 * Connect selection with no transport so WsClient reports its typed HOLD;
 * it must never silently return to plaintext `/api/ws`.
 */
export function readConnectStoreConfiguration(
  host: unknown = globalThis,
): ConnectStoreConfiguration | null {
  if (!isRuntimeHost(host)) return null;
  const candidate = host[CONNECT_STORE_CONFIGURATION_KEY];
  if (!isRuntimeHost(candidate) || candidate.transport !== "connect") {
    return null;
  }

  const config: ConnectStoreConfiguration = { transport: "connect" };
  if (isConnectTransport(candidate.connectTransport)) {
    config.connectTransport = candidate.connectTransport;
  }
  if (candidate.preferDirect === true || candidate.preferDirect === false) {
    config.preferDirect = candidate.preferDirect;
  }
  if (
    candidate.directAvailable === true ||
    candidate.directAvailable === false
  ) {
    config.directAvailable = candidate.directAvailable;
  }
  if (candidate.relayUrl === null || typeof candidate.relayUrl === "string") {
    config.relayUrl = candidate.relayUrl;
  }

  const connectRequest = copyOptionalFunction<
    NonNullable<ConnectStoreConfiguration["connectRequest"]>
  >(candidate, "connectRequest");
  if (connectRequest) config.connectRequest = connectRequest;
  const connectResponse = copyOptionalFunction<
    NonNullable<ConnectStoreConfiguration["connectResponse"]>
  >(candidate, "connectResponse");
  if (connectResponse) config.connectResponse = connectResponse;
  const connectResume = copyOptionalFunction<
    NonNullable<ConnectStoreConfiguration["connectResume"]>
  >(candidate, "connectResume");
  if (connectResume) config.connectResume = connectResume;
  const connectMessage = copyOptionalFunction<
    NonNullable<ConnectStoreConfiguration["connectMessage"]>
  >(candidate, "connectMessage");
  if (connectMessage) config.connectMessage = connectMessage;

  return config;
}

/**
 * Produce the exact WsClient option set for the store. Legacy remains an
 * explicit deployment choice only when no Connect marker is present.
 */
export function selectStoreClientOptions(
  config: ConnectStoreConfiguration | null,
): WsClientOptions {
  return config ?? { transport: "legacy" };
}

/**
 * Narrow callback aliases keep the adapter contract visible to callers that
 * build the Connect configuration without importing the whole WsClient API.
 */
export type ConnectStoreRequestAdapter = NonNullable<
  ConnectStoreConfiguration["connectRequest"]
>;
export type ConnectStoreResponseAdapter = NonNullable<
  ConnectStoreConfiguration["connectResponse"]
>;
export type ConnectStoreResumeAdapter = NonNullable<
  ConnectStoreConfiguration["connectResume"]
>;
export type ConnectStoreMessageAdapter = NonNullable<
  ConnectStoreConfiguration["connectMessage"]
>;

// Keep the public adapter module tied to the projection types. These aliases
// are intentionally compile-time only and make accidental `any` callbacks
// fail at the configuration boundary.
export type {
  ConnectPayloadRequest,
  DecodedConnectEnvelope,
  RemoteAction,
  RemoteActionResult,
  ResumeContext,
  WsOutbound,
};

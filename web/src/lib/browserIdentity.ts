const BROWSER_INSTALL_ID_KEY = "devmanager.browserInstallId";

type LocationLike = {
  protocol: string;
  host: string;
};

function createBrowserInstallId(): string {
  if (typeof globalThis.crypto?.randomUUID === "function") {
    return globalThis.crypto.randomUUID();
  }
  return `browser-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

export function getBrowserInstallId(): string {
  const existing = globalThis.localStorage?.getItem(BROWSER_INSTALL_ID_KEY)?.trim();
  if (existing) {
    return existing;
  }
  const created = createBrowserInstallId();
  globalThis.localStorage?.setItem(BROWSER_INSTALL_ID_KEY, created);
  return created;
}

export function buildPairingUrl(token: string): string {
  const params = new URLSearchParams({ t: token });
  params.set("browserInstallId", getBrowserInstallId());
  return `/pair?${params.toString()}`;
}

export function buildWebSocketUrl(
  locationLike: LocationLike = window.location,
): string {
  const scheme = locationLike.protocol === "https:" ? "wss" : "ws";
  const params = new URLSearchParams();
  params.set("browserInstallId", getBrowserInstallId());
  return `${scheme}://${locationLike.host}/api/ws?${params.toString()}`;
}

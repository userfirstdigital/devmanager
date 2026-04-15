import type { RunCommand, SessionRuntimeState } from "../api/types";

type BrowserLocationLike = string | URL | Pick<Location, "href">;
type WindowOpenLike = (
  url?: string | URL,
  target?: string,
  features?: string,
) => unknown;

function asUrl(location: BrowserLocationLike): URL {
  if (location instanceof URL) {
    return new URL(location.toString());
  }
  if (typeof location === "string") {
    return new URL(location);
  }
  return new URL(location.href);
}

export function canOpenRemoteSite(
  command: Pick<RunCommand, "port">,
  session: Pick<SessionRuntimeState, "status"> | null,
): boolean {
  return command.port != null && session?.status === "Running";
}

export function buildRemoteSiteUrl(
  location: BrowserLocationLike,
  port: number,
): string {
  const url = asUrl(location);
  url.port = String(port);
  url.pathname = "/";
  url.search = "";
  url.hash = "";
  return url.toString();
}

export function openRemoteSiteInNewTab(
  openWindow: WindowOpenLike,
  location: BrowserLocationLike,
  port: number,
): string {
  const url = buildRemoteSiteUrl(location, port);
  openWindow(url, "_blank", "noopener,noreferrer");
  return url;
}

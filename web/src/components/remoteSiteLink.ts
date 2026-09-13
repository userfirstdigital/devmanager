import type {
  RunCommand,
  SessionRuntimeState,
  WebPortAuthority,
} from "../api/types";

type BrowserLocationLike = string | URL | Pick<Location, "href">;
type RemoteSiteSession = {
  status: SessionRuntimeState["status"];
  session_id?: string;
  sessionId?: string;
};
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
  session: RemoteSiteSession | null,
  authority:
    | Pick<
        WebPortAuthority,
        | "kind"
        | "controlReason"
        | "fresh"
        | "listeners"
        | "reapIncomplete"
        | "error"
        | "diagnostic"
        | "sessionId"
      >
    | undefined,
): boolean {
  return (
    command.port != null &&
    session?.status === "Running" &&
    authority?.fresh === true &&
    authority.reapIncomplete === false &&
    authority.error === null &&
    authority.diagnostic == null &&
    authority.listeners.length > 0 &&
    authority.listeners.every(
      (listener) =>
        listener.pid > 0 &&
        listener.creationTime100ns > 0 &&
        listener.executableProven,
    ) &&
    ((authority.kind === "managed" &&
      authority.controlReason === "exactManagedFence" &&
      authority.sessionId != null &&
      authority.sessionId === (session.session_id ?? session.sessionId)) ||
      (authority.kind === "provenExternal" &&
        authority.controlReason === "provenExternalNoControl"))
  );
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

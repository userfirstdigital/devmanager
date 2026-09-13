import {
  canPerform,
  deriveComposerMode,
  deriveConnectUiGate,
  type CapabilityGrant,
  type ConnectAction,
  type ConnectConnectionKind,
} from "./permissions";

export function GuestActionNotice({
  grant,
  action,
  statusKind = "open",
}: {
  grant: CapabilityGrant | null;
  action: ConnectAction;
  statusKind?: ConnectConnectionKind;
}) {
  if (canPerform(grant, action) && statusKind === "open") return null;
  const gate = deriveConnectUiGate({ grant, action, statusKind });
  if (gate.kind === "allowed") return null;
  const mode = deriveComposerMode(grant);
  const message =
    gate.kind === "disabled"
      ? gate.reason === "unauthorized"
        ? "Pairing required before this action can run."
        : "Reconnecting · this action is paused."
      : gate.reason === "watcher"
        ? "View only · you cannot perform this action."
        : "This action is not permitted.";
  return (
    <p
      data-testid="guest-action-disabled"
      data-gate={gate.kind}
      data-reason={gate.reason}
      data-composer-mode={mode}
    >
      {message}
    </p>
  );
}

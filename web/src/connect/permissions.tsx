import {
  canPerform,
  deriveComposerMode,
  type CapabilityGrant,
  type ConnectAction,
} from "./permissions";

export function GuestActionNotice({
  grant,
  action,
}: {
  grant: CapabilityGrant | null;
  action: ConnectAction;
}) {
  if (canPerform(grant, action)) return null;
  const mode = deriveComposerMode(grant);
  if (mode === "hidden") return null;
  return <p data-testid="guest-action-disabled">This action is not in the grant.</p>;
}

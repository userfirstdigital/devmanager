import { shouldShowCollaborationUi, type TaskInvite } from "./invites";

export function CollaborationPanel({ invites }: { invites: readonly TaskInvite[] }) {
  if (!shouldShowCollaborationUi(invites)) {
    return null;
  }
  return (
    <section data-testid="collaboration-panel">
      <h2>Task guests</h2>
      <ul>
        {invites.map((invite) => (
          <li key={invite.inviteId}>
            {invite.nickname} ({invite.role})
          </li>
        ))}
      </ul>
    </section>
  );
}

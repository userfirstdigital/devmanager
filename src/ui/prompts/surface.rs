//! Production Prompt Library surface model.
//!
//! This is the small seam the native shell mounts: it owns selection and
//! action construction, while `PromptLibrarySession` remains the bounded
//! projection and `PromptMutationExecutor` remains the authenticated write
//! path. It deliberately contains no GPUI callbacks and never sends input to
//! a provider.

use crate::domain::command::{Command, CommandEnvelope};
use crate::domain::id::{
    AgentSessionId, ClientId, CommandId, PromptChainId, PromptChainLinkId, PromptHistoryId,
    PromptVersionId, TaskId,
};
use crate::prompts::organization::{OrgPrompt, OrgPromptVersion};
use crate::prompts::projection::PromptChainLinkRecord;
use crate::ui::task_cockpit::composer::{ComposerInsertionMode, PutPromptVersionInComposer};

use super::chain_editor::{ChainEditorProjection, ChainLinkView};
use super::mutation::{prompt_mutation_command, PromptMutationError};
use super::picker::{open_organization_picker, PromptPicker, PromptPickerSource};
use super::{PromptLibraryAction, PromptLibrarySession, PromptLibraryUiError};

/// Maximum number of chain rows the native surface exposes at once. The
/// session applies the larger projection cap; this cap bounds controls and
/// accessibility work in the mounted view.
pub const MAX_SURFACE_CHAIN_ROWS: usize = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptTaskAuthority {
    task_id: TaskId,
    agent_session_id: AgentSessionId,
}

impl PromptTaskAuthority {
    pub const fn new(task_id: TaskId, agent_session_id: AgentSessionId) -> Self {
        Self {
            task_id,
            agent_session_id,
        }
    }

    pub const fn task_id(self) -> TaskId {
        self.task_id
    }

    pub const fn agent_session_id(self) -> AgentSessionId {
        self.agent_session_id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptSurfaceSelection {
    PersonalVersion(PromptVersionId),
    History(PromptHistoryId),
    OrganizationVersion(crate::org::OrgPromptVersionId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptSurfaceError {
    NoSelection,
    VersionNotFound,
    HistoryNotFound,
    OrganizationSelectionCannotSend,
    ChainNotFound,
    InvalidChainGap,
    Mutation(PromptMutationError),
}

impl From<PromptLibraryUiError> for PromptSurfaceError {
    fn from(error: PromptLibraryUiError) -> Self {
        match error {
            PromptLibraryUiError::NotFound => Self::VersionNotFound,
            PromptLibraryUiError::AdjacentLinksRequired => Self::InvalidChainGap,
            other => {
                let _ = other;
                Self::InvalidChainGap
            }
        }
    }
}

/// One mounted Prompt Library surface, bound to the currently selected task
/// and agent session. IDs are captured at activation and copied into every
/// composer action; callers must not substitute the currently focused tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptLibrarySurface {
    authority: PromptTaskAuthority,
    selection: Option<PromptSurfaceSelection>,
    chain_id: Option<PromptChainId>,
}

impl PromptLibrarySurface {
    pub fn new(authority: PromptTaskAuthority) -> Self {
        Self {
            authority,
            selection: None,
            chain_id: None,
        }
    }

    pub const fn authority(&self) -> PromptTaskAuthority {
        self.authority
    }

    pub const fn selection(&self) -> Option<PromptSurfaceSelection> {
        self.selection
    }

    pub const fn chain_id(&self) -> Option<PromptChainId> {
        self.chain_id
    }

    pub fn open_personal_picker(
        &mut self,
        session: &PromptLibrarySession,
    ) -> Result<PromptPicker, PromptSurfaceError> {
        let picker = super::picker::open_picker(
            PromptPickerSource::Saved,
            &session.saved,
            &session.history,
            &session.links,
            &session.versions,
            &session.query,
        );
        Ok(picker)
    }

    pub fn open_organization_picker(
        &mut self,
        prompts: &[OrgPrompt],
        versions: &[OrgPromptVersion],
        query: &str,
    ) -> PromptPicker {
        open_organization_picker(prompts, versions, query)
    }

    pub fn select_personal_version(
        &mut self,
        session: &PromptLibrarySession,
        version_id: PromptVersionId,
    ) -> Result<PromptLibraryAction, PromptSurfaceError> {
        if !session
            .versions
            .iter()
            .any(|version| version.id == version_id)
        {
            return Err(PromptSurfaceError::VersionNotFound);
        }
        self.selection = Some(PromptSurfaceSelection::PersonalVersion(version_id));
        Ok(PromptLibraryAction::SelectVersion { version_id })
    }

    pub fn select_history(
        &mut self,
        session: &PromptLibrarySession,
        history_id: PromptHistoryId,
    ) -> Result<(), PromptSurfaceError> {
        if !session.history.iter().any(|row| row.id == history_id) {
            return Err(PromptSurfaceError::HistoryNotFound);
        }
        self.selection = Some(PromptSurfaceSelection::History(history_id));
        Ok(())
    }

    pub fn select_organization_version(&mut self, version_id: crate::org::OrgPromptVersionId) {
        self.selection = Some(PromptSurfaceSelection::OrganizationVersion(version_id));
    }

    pub fn put_selected_in_composer(
        &self,
        session: &PromptLibrarySession,
        insertion: ComposerInsertionMode,
        chain_link_id: Option<PromptChainLinkId>,
    ) -> Result<PutPromptVersionInComposer, PromptSurfaceError> {
        let Some(PromptSurfaceSelection::PersonalVersion(version_id)) = self.selection else {
            return if matches!(
                self.selection,
                Some(PromptSurfaceSelection::OrganizationVersion(_))
            ) {
                Err(PromptSurfaceError::OrganizationSelectionCannotSend)
            } else {
                Err(PromptSurfaceError::NoSelection)
            };
        };
        if !session
            .versions
            .iter()
            .any(|version| version.id == version_id)
        {
            return Err(PromptSurfaceError::VersionNotFound);
        }
        Ok(super::put_in_composer_action(
            self.authority.task_id(),
            self.authority.agent_session_id(),
            version_id,
            insertion,
            chain_link_id,
        ))
    }

    pub fn select_chain(&mut self, chain_id: PromptChainId) {
        self.chain_id = Some(chain_id);
    }

    pub fn chain_projection(
        &self,
        session: &PromptLibrarySession,
    ) -> Result<ChainEditorProjection, PromptSurfaceError> {
        let chain_id = self.chain_id.ok_or(PromptSurfaceError::ChainNotFound)?;
        session.chain_projection(chain_id).map_err(Into::into)
    }

    /// Return the visible chain rows with explicit, manually invoked controls.
    /// There is intentionally no run/advance action.
    pub fn chain_rows(
        &self,
        session: &PromptLibrarySession,
    ) -> Result<Vec<PromptChainSurfaceRow>, PromptSurfaceError> {
        let projection = self.chain_projection(session)?;
        let revision = projection.chain.revision;
        let mut rows = projection
            .links
            .iter()
            .enumerate()
            .map(|(index, view)| {
                let after_next = projection
                    .links
                    .get(index.saturating_add(2))
                    .map(|next| next.link.id());
                PromptChainSurfaceRow::from_view(
                    view,
                    projection.chain.id,
                    revision,
                    after_next,
                    session
                        .saved
                        .iter()
                        .find(|prompt| prompt.id == view.link.prompt_id())
                        .map(|prompt| prompt.current_version_id),
                    self.authority,
                )
            })
            .collect::<Vec<_>>();
        rows.truncate(MAX_SURFACE_CHAIN_ROWS);
        Ok(rows)
    }

    pub fn insert_selected_between(
        &self,
        session: &PromptLibrarySession,
        after_link_id: PromptChainLinkId,
        before_link_id: PromptChainLinkId,
    ) -> Result<PromptLibraryAction, PromptSurfaceError> {
        let Some(PromptSurfaceSelection::PersonalVersion(version_id)) = self.selection else {
            return Err(PromptSurfaceError::NoSelection);
        };
        let version = session
            .versions
            .iter()
            .find(|version| version.id == version_id)
            .ok_or(PromptSurfaceError::VersionNotFound)?;
        let chain_id = self.chain_id.ok_or(PromptSurfaceError::ChainNotFound)?;
        let link = PromptChainLinkRecord::try_new(
            PromptChainLinkId::new(),
            chain_id,
            1,
            version.prompt_id,
            version.id,
            Some(after_link_id),
            Some(before_link_id),
            false,
        )
        .map_err(|_| PromptSurfaceError::InvalidChainGap)?;
        let chain = session
            .chains
            .iter()
            .find(|chain| chain.id == chain_id)
            .ok_or(PromptSurfaceError::ChainNotFound)?;
        Ok(PromptLibraryAction::InsertChainLinkBetween {
            chain_id,
            after_link_id,
            before_link_id,
            link,
            expected_revision: chain.revision,
        })
    }

    /// Map a visible chain control to the authenticated durable command bus.
    /// The native mount sends this `Command` in a normal `CommandEnvelope`;
    /// this helper never applies the mutation locally.
    pub fn durable_command(
        &self,
        action: &PromptLibraryAction,
    ) -> Result<Command, PromptSurfaceError> {
        prompt_mutation_command(action).map_err(PromptSurfaceError::Mutation)
    }

    /// Build the exact authenticated envelope used by the native mount. The
    /// selected task is always carried on the envelope, even for a prompt
    /// chain mutation whose own revision fence lives in the command payload.
    pub fn durable_envelope(
        &self,
        client_id: ClientId,
        command_id: CommandId,
        issued_at_ms: i64,
        action: &PromptLibraryAction,
    ) -> Result<CommandEnvelope, PromptSurfaceError> {
        Ok(CommandEnvelope {
            command_id,
            client_id,
            task_id: Some(self.authority.task_id()),
            issued_at_ms,
            expected_task_revision: None,
            command: self.durable_command(action)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptChainSurfaceRow {
    pub link: PromptChainLinkRecord,
    pub put_in_composer: PutPromptVersionInComposer,
    pub move_up: Option<PromptLibraryAction>,
    pub move_down: Option<PromptLibraryAction>,
    pub remove: PromptLibraryAction,
    pub update_to_current: Option<PromptLibraryAction>,
}

impl PromptChainSurfaceRow {
    fn from_view(
        view: &ChainLinkView,
        chain_id: PromptChainId,
        revision: u64,
        before_after_next: Option<PromptChainLinkId>,
        current_version_id: Option<PromptVersionId>,
        authority: PromptTaskAuthority,
    ) -> Self {
        let move_up =
            view.link
                .previous_link_id()
                .map(|before| PromptLibraryAction::ReorderChainLink {
                    chain_id,
                    link_id: view.link.id(),
                    before_link_id: Some(before),
                    expected_revision: revision,
                });
        let move_down = view
            .link
            .next_link_id()
            .map(|_| PromptLibraryAction::ReorderChainLink {
                chain_id,
                link_id: view.link.id(),
                before_link_id: before_after_next,
                expected_revision: revision,
            });
        Self {
            link: view.link.clone(),
            put_in_composer: super::put_in_composer_action(
                authority.task_id(),
                authority.agent_session_id(),
                view.link.prompt_version_id(),
                ComposerInsertionMode::ReplaceDraft,
                Some(view.link.id()),
            ),
            move_up,
            move_down,
            remove: PromptLibraryAction::RemoveChainLink {
                chain_id,
                link_id: view.link.id(),
                expected_revision: revision,
            },
            update_to_current: view.link.update_available().then_some(
                PromptLibraryAction::UpdateLinkToCurrent {
                    chain_id,
                    link_id: view.link.id(),
                    current_version_id: current_version_id
                        .unwrap_or_else(|| view.link.prompt_version_id()),
                    expected_revision: revision,
                },
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::prompts::fixtures::{agent_session_id, chain_id, lifecycle_fixture, task_id};
    use crate::ui::prompts::{PromptLibraryLoadState, PromptLibrarySession};
    use crate::ui::shell::{
        ColorScheme, DataFixtureKind, Density, LayoutWidth, PromptLibraryViewport, ScalePercent,
    };

    fn session() -> PromptLibrarySession {
        let fixture = lifecycle_fixture();
        let mut session = PromptLibrarySession::new(PromptLibraryViewport {
            scheme: ColorScheme::Dark,
            density: Density::Comfortable,
            scale: ScalePercent::OneHundred,
            width: LayoutWidth::Wide,
            data: DataFixtureKind::Populated,
        });
        session.saved = fixture.prompts;
        session.versions = fixture.versions;
        session.chains = fixture.chains;
        session.links = fixture.links;
        session.history = fixture.history;
        session.load = PromptLibraryLoadState::Ready;
        session
    }

    #[test]
    fn surface_keeps_exact_task_authority_and_manual_chain_controls() {
        let session = session();
        let authority = PromptTaskAuthority::new(task_id(8), agent_session_id(9));
        let mut surface = PromptLibrarySurface::new(authority);
        surface
            .select_personal_version(&session, crate::ui::prompts::fixtures::version_id(12))
            .expect("version");
        let put = surface
            .put_selected_in_composer(&session, ComposerInsertionMode::ReplaceDraft, None)
            .expect("put");
        assert_eq!(put.task_id, task_id(8));
        assert_eq!(put.agent_session_id, agent_session_id(9));
        surface.select_chain(chain_id(30));
        let rows = surface.chain_rows(&session).expect("rows");
        assert_eq!(rows.len(), 5);
        assert!(rows
            .iter()
            .all(|row| row.remove.clone() != PromptLibraryAction::ClearHistory));
        assert!(rows.iter().any(|row| row.move_up.is_some()));
    }
}

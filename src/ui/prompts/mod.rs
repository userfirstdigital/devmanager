//! Prompt Library list/detail, editor, diff, chain, history, and picker.

pub mod chain_editor;
pub mod editor;
pub mod fixtures;
pub mod history;
pub mod library;
pub mod mutation;
pub mod picker;
pub mod version_diff;

use super::shell::{AccessibleName, LibrarySection, PromptLibraryChrome, PromptLibraryViewport};
use super::task_cockpit::composer::{
    apply_put_prompt_version, ComposerDraft, ComposerInsertionMode, ExactPromptPayload,
    ProviderCommandSuggestion, PutPromptVersionInComposer,
};
use crate::domain::id::{
    AgentSessionId, PromptChainId, PromptChainLinkId, PromptHistoryId, PromptId, PromptVersionId,
    TaskId,
};
use crate::prompts::diff::diff_versions;
use crate::prompts::model::{PromptChain, PromptVersion, SavedPrompt};
use crate::prompts::projection::PromptChainLinkRecord;

use self::chain_editor::{
    apply_chain_action, ChainEditorProjection, ChainGap, ChainLinkView, ManualSuggestedNext,
};
use self::editor::{PromptEditor, PromptEditorAction};
use self::history::{
    apply_history_policy, HistoryClearResult, PromptHistoryPolicy, RecentHistoryRecord,
};
use self::library::{filter_saved_prompts, virtualize, LibraryListState, SavedPromptRow};
use self::picker::{open_picker, PromptPicker, PromptPickerHit, PromptPickerSource};
use self::version_diff::VersionDiffView;
pub use super::shell::PromptLibraryUiError;

pub const LIBRARY_VISIBLE_ROWS: usize = 80;
pub const CHAIN_VISIBLE_LINKS: usize = 80;
pub const MAX_VIRTUALIZED_PROMPTS: usize = 5_000;
pub const MAX_VIRTUALIZED_LINKS: usize = 2_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptLibraryLoadState {
    Empty,
    Loading,
    Ready,
    Error { message: String },
    StaleRevision { expected: u64, actual: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptLibraryKey {
    ArrowUp,
    ArrowDown,
    Enter,
    Escape,
    Tab,
    Slash,
    LibraryShortcut,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptLibraryAction {
    SelectSection(LibrarySection),
    Search(String),
    FocusNext,
    FocusPrevious,
    CreatePrompt {
        prompt: SavedPrompt,
        version: PromptVersion,
    },
    EditPrompt(PromptEditorAction),
    ArchivePrompt {
        prompt_id: PromptId,
        expected_revision: u64,
        archived_at_ms: i64,
    },
    RestorePrompt {
        prompt_id: PromptId,
        expected_revision: u64,
    },
    SelectVersion {
        version_id: PromptVersionId,
    },
    DiffVersions {
        old_version_id: PromptVersionId,
        new_version_id: PromptVersionId,
    },
    InsertChainLinkBetween {
        chain_id: PromptChainId,
        after_link_id: PromptChainLinkId,
        before_link_id: PromptChainLinkId,
        link: PromptChainLinkRecord,
    },
    ReorderChainLink {
        chain_id: PromptChainId,
        link_id: PromptChainLinkId,
        before_link_id: Option<PromptChainLinkId>,
    },
    RemoveChainLink {
        chain_id: PromptChainId,
        link_id: PromptChainLinkId,
    },
    UpdateLinkToCurrent {
        chain_id: PromptChainId,
        link_id: PromptChainLinkId,
        current_version_id: PromptVersionId,
    },
    PutInComposer(PutPromptVersionInComposer),
    ShowSuggestedNext {
        link_id: PromptChainLinkId,
    },
    ClearSuggestedNext,
    SaveHistoryAsPrompt {
        history_id: PromptHistoryId,
        prompt: SavedPrompt,
        version: PromptVersion,
    },
    ClearHistory,
    OpenPicker {
        source: PromptPickerSource,
    },
    ClosePicker,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptLibrarySession {
    pub chrome: PromptLibraryChrome,
    pub load: PromptLibraryLoadState,
    pub query: String,
    pub library_revision: u64,
    pub expected_revision: Option<u64>,
    pub saved: Vec<SavedPrompt>,
    pub versions: Vec<PromptVersion>,
    pub chains: Vec<PromptChain>,
    pub links: Vec<PromptChainLinkRecord>,
    pub history: Vec<RecentHistoryRecord>,
    pub history_policy: PromptHistoryPolicy,
    pub editor: PromptEditor,
    pub picker: Option<PromptPicker>,
    pub draft: ComposerDraft,
    pub provider_commands: Vec<ProviderCommandSuggestion>,
    pub suggested_next: Option<ManualSuggestedNext>,
    pub focused_index: usize,
    pub list_offset: usize,
    pub provider_commands_in_library: bool,
}

impl PromptLibrarySession {
    pub fn new(viewport: PromptLibraryViewport) -> Self {
        Self {
            chrome: PromptLibraryChrome::new(viewport),
            load: PromptLibraryLoadState::Empty,
            query: String::new(),
            library_revision: 0,
            expected_revision: None,
            saved: Vec::new(),
            versions: Vec::new(),
            chains: Vec::new(),
            links: Vec::new(),
            history: Vec::new(),
            history_policy: PromptHistoryPolicy::default(),
            editor: PromptEditor::default(),
            picker: None,
            draft: ComposerDraft::default(),
            provider_commands: Vec::new(),
            suggested_next: None,
            focused_index: 0,
            list_offset: 0,
            provider_commands_in_library: false,
        }
    }

    pub fn apply(&mut self, action: PromptLibraryAction) -> Result<(), PromptLibraryUiError> {
        match action {
            PromptLibraryAction::SelectSection(section) => {
                self.chrome.select_section(section);
                self.focused_index = 0;
                self.list_offset = 0;
                self.picker = None;
            }
            PromptLibraryAction::Search(query) => {
                if query.chars().count() > 512 {
                    return Err(PromptLibraryUiError::SearchTooLong);
                }
                self.query = query;
                self.focused_index = 0;
                self.list_offset = 0;
            }
            PromptLibraryAction::FocusNext => self.move_focus(1),
            PromptLibraryAction::FocusPrevious => self.move_focus(-1),
            PromptLibraryAction::CreatePrompt { prompt, version } => {
                self.upsert_prompt(prompt, version);
            }
            PromptLibraryAction::EditPrompt(edit) => {
                self.editor.apply(edit)?;
                if let Some((prompt, version)) = self.editor.take_saved_version() {
                    self.upsert_prompt(prompt, version);
                }
            }
            PromptLibraryAction::ArchivePrompt {
                prompt_id,
                expected_revision,
                archived_at_ms,
            } => {
                let prompt = self
                    .saved
                    .iter_mut()
                    .find(|prompt| prompt.id == prompt_id)
                    .ok_or(PromptLibraryUiError::NotFound)?;
                if prompt.revision != expected_revision {
                    self.load = PromptLibraryLoadState::StaleRevision {
                        expected: expected_revision,
                        actual: prompt.revision,
                    };
                    return Err(PromptLibraryUiError::StaleRevision);
                }
                prompt.archived_at_ms = Some(archived_at_ms);
                prompt.revision = prompt.revision.saturating_add(1);
                self.library_revision = self.library_revision.saturating_add(1);
            }
            PromptLibraryAction::RestorePrompt {
                prompt_id,
                expected_revision,
            } => {
                let prompt = self
                    .saved
                    .iter_mut()
                    .find(|prompt| prompt.id == prompt_id)
                    .ok_or(PromptLibraryUiError::NotFound)?;
                if prompt.revision != expected_revision {
                    self.load = PromptLibraryLoadState::StaleRevision {
                        expected: expected_revision,
                        actual: prompt.revision,
                    };
                    return Err(PromptLibraryUiError::StaleRevision);
                }
                prompt.archived_at_ms = None;
                prompt.revision = prompt.revision.saturating_add(1);
                self.library_revision = self.library_revision.saturating_add(1);
            }
            PromptLibraryAction::SelectVersion { version_id } => {
                let version = self
                    .versions
                    .iter()
                    .find(|version| version.id == version_id)
                    .cloned()
                    .ok_or(PromptLibraryUiError::NotFound)?;
                self.editor.select_version(version);
            }
            PromptLibraryAction::DiffVersions {
                old_version_id,
                new_version_id,
            } => {
                let old = self.version(old_version_id)?;
                let new = self.version(new_version_id)?;
                self.editor.diff = Some(VersionDiffView::from_versions(&old, &new));
            }
            PromptLibraryAction::InsertChainLinkBetween {
                chain_id,
                after_link_id,
                before_link_id,
                link,
            } => {
                apply_chain_action::insert_between(
                    &mut self.links,
                    chain_id,
                    after_link_id,
                    before_link_id,
                    link,
                )?;
                self.bump_chain_revision(chain_id)?;
            }
            PromptLibraryAction::ReorderChainLink {
                chain_id,
                link_id,
                before_link_id,
            } => {
                apply_chain_action::reorder(&mut self.links, chain_id, link_id, before_link_id)?;
                self.bump_chain_revision(chain_id)?;
            }
            PromptLibraryAction::RemoveChainLink { chain_id, link_id } => {
                apply_chain_action::remove(&mut self.links, chain_id, link_id)?;
                self.bump_chain_revision(chain_id)?;
            }
            PromptLibraryAction::UpdateLinkToCurrent {
                chain_id,
                link_id,
                current_version_id,
            } => {
                apply_chain_action::update_pinned(
                    &mut self.links,
                    chain_id,
                    link_id,
                    current_version_id,
                )?;
                self.bump_chain_revision(chain_id)?;
            }
            PromptLibraryAction::PutInComposer(action) => {
                let payload = self.exact_payload(action.prompt_version_id)?;
                apply_put_prompt_version(&mut self.draft, &action, &payload)?;
                if let Some(link_id) = action.chain_link_id {
                    self.suggested_next = self.manual_suggested_next(link_id);
                }
            }
            PromptLibraryAction::ShowSuggestedNext { link_id } => {
                self.suggested_next = self.manual_suggested_next(link_id);
            }
            PromptLibraryAction::ClearSuggestedNext => {
                self.suggested_next = None;
            }
            PromptLibraryAction::SaveHistoryAsPrompt {
                history_id,
                prompt,
                version,
            } => {
                if !self.history.iter().any(|row| row.id == history_id) {
                    return Err(PromptLibraryUiError::NotFound);
                }
                self.upsert_prompt(prompt, version);
            }
            PromptLibraryAction::ClearHistory => {
                self.history.clear();
            }
            PromptLibraryAction::OpenPicker { source } => {
                self.picker = Some(open_picker(
                    source,
                    &self.saved,
                    &self.history,
                    &self.links,
                    &self.versions,
                    &self.query,
                ));
            }
            PromptLibraryAction::ClosePicker => self.picker = None,
        }
        if self.saved.is_empty()
            && self.history.is_empty()
            && self.chains.is_empty()
            && !matches!(
                self.load,
                PromptLibraryLoadState::Error { .. } | PromptLibraryLoadState::Loading
            )
        {
            self.load = PromptLibraryLoadState::Empty;
        } else if !matches!(
            self.load,
            PromptLibraryLoadState::Error { .. }
                | PromptLibraryLoadState::StaleRevision { .. }
                | PromptLibraryLoadState::Loading
        ) {
            self.load = PromptLibraryLoadState::Ready;
        }
        Ok(())
    }

    pub fn handle_key(&mut self, key: PromptLibraryKey) -> Result<(), PromptLibraryUiError> {
        match key {
            PromptLibraryKey::ArrowDown => self.apply(PromptLibraryAction::FocusNext),
            PromptLibraryKey::ArrowUp => self.apply(PromptLibraryAction::FocusPrevious),
            PromptLibraryKey::Tab => {
                let idx = Self::section_index(self.chrome.active_section);
                let next = LibrarySection::ALL[(idx + 1) % LibrarySection::ALL.len()];
                self.apply(PromptLibraryAction::SelectSection(next))
            }
            PromptLibraryKey::Slash => {
                self.apply(PromptLibraryAction::OpenPicker {
                    source: PromptPickerSource::ProviderCommands,
                })?;
                if let Some(picker) = &mut self.picker {
                    picker.hits.clear();
                    picker.notice = Some(
                        "slash searches provider-native commands, not the Prompt Library".into(),
                    );
                }
                Ok(())
            }
            PromptLibraryKey::LibraryShortcut => self.apply(PromptLibraryAction::OpenPicker {
                source: PromptPickerSource::Saved,
            }),
            PromptLibraryKey::Escape => {
                self.picker = None;
                Ok(())
            }
            PromptLibraryKey::Enter => Ok(()),
        }
    }

    pub fn list_state(&self) -> LibraryListState {
        match self.chrome.active_section {
            LibrarySection::SavedPrompts => {
                let rows = filter_saved_prompts(&self.saved, &self.query);
                let window = virtualize(&rows, self.list_offset, LIBRARY_VISIBLE_ROWS);
                LibraryListState {
                    section: LibrarySection::SavedPrompts,
                    total: rows.len(),
                    offset: window.offset,
                    visible: window.visible,
                    focused_index: self.focused_index.min(rows.len().saturating_sub(1)),
                    includes_provider_commands: self.provider_commands_in_library,
                }
            }
            LibrarySection::RecentHistory => LibraryListState {
                section: LibrarySection::RecentHistory,
                total: self.visible_history().len(),
                offset: self.list_offset,
                visible: Vec::new(),
                focused_index: self.focused_index,
                includes_provider_commands: false,
            },
            LibrarySection::Chains => LibraryListState {
                section: LibrarySection::Chains,
                total: self.links.len(),
                offset: self.list_offset,
                visible: Vec::new(),
                focused_index: self.focused_index,
                includes_provider_commands: false,
            },
        }
    }

    pub fn chain_projection(
        &self,
        chain_id: PromptChainId,
    ) -> Result<ChainEditorProjection, PromptLibraryUiError> {
        let chain = self
            .chains
            .iter()
            .find(|chain| chain.id == chain_id)
            .cloned()
            .ok_or(PromptLibraryUiError::NotFound)?;
        let mut links: Vec<_> = self
            .links
            .iter()
            .filter(|link| link.chain_id() == chain_id)
            .cloned()
            .collect();
        links.sort_by_key(|link| link.position());
        if links.len() > MAX_VIRTUALIZED_LINKS {
            return Err(PromptLibraryUiError::CapExceeded);
        }
        let views: Vec<ChainLinkView> = links
            .iter()
            .map(|link| {
                let title = self
                    .saved
                    .iter()
                    .find(|prompt| prompt.id == link.prompt_id())
                    .map(|prompt| prompt.title.clone())
                    .unwrap_or_else(|| "Untitled prompt".into());
                ChainLinkView {
                    link: link.clone(),
                    title,
                    numbered_position: link.position(),
                    connector_visible: link.next_link_id().is_some(),
                    put_in_composer_label: "Put in composer",
                    insert_here_label: "Insert prompt here",
                    update_available: link.update_available(),
                }
            })
            .collect();
        let mut gaps = Vec::with_capacity(views.len().saturating_add(1));
        gaps.push(ChainGap {
            before_link_id: views.first().map(|view| view.link.id()),
            after_link_id: None,
            label: "Insert prompt here",
        });
        for window in views.windows(2) {
            gaps.push(ChainGap {
                after_link_id: Some(window[0].link.id()),
                before_link_id: Some(window[1].link.id()),
                label: "Insert prompt here",
            });
        }
        if let Some(last) = views.last() {
            gaps.push(ChainGap {
                after_link_id: Some(last.link.id()),
                before_link_id: None,
                label: "Insert prompt here",
            });
        }
        let window = virtualize(&views, self.list_offset, CHAIN_VISIBLE_LINKS);
        Ok(ChainEditorProjection {
            chain,
            links: window.visible,
            gaps,
            total_links: views.len(),
            suggested_next: self.suggested_next.clone(),
            auto_advance: false,
            has_run_button: false,
            has_graph_canvas: false,
        })
    }

    pub fn visible_history(&self) -> Vec<RecentHistoryRecord> {
        apply_history_policy(&self.history, &self.history_policy, &self.query)
    }

    pub fn clear_history(&mut self) -> HistoryClearResult {
        let removed = self.history.len();
        self.history.clear();
        HistoryClearResult {
            removed_history_rows: removed,
            removed_task_facts: 0,
            removed_saved_prompts: 0,
        }
    }

    pub fn version_diff(
        &self,
        old_version_id: PromptVersionId,
        new_version_id: PromptVersionId,
    ) -> Result<VersionDiffView, PromptLibraryUiError> {
        Ok(VersionDiffView::from_versions(
            &self.version(old_version_id)?,
            &self.version(new_version_id)?,
        ))
    }

    pub fn focus_accessible_name(&self) -> AccessibleName {
        AccessibleName {
            name: format!(
                "{} item {}",
                self.chrome.active_section.label(),
                self.focused_index.saturating_add(1)
            ),
            role: "option",
            status: Some(match &self.load {
                PromptLibraryLoadState::Ready => "ready".into(),
                PromptLibraryLoadState::Empty => "empty".into(),
                PromptLibraryLoadState::Loading => "loading".into(),
                PromptLibraryLoadState::Error { message } => message.clone(),
                PromptLibraryLoadState::StaleRevision { .. } => "stale revision".into(),
            }),
        }
    }

    pub fn exact_payload(
        &self,
        version_id: PromptVersionId,
    ) -> Result<ExactPromptPayload, PromptLibraryUiError> {
        let version = self.version(version_id)?;
        Ok(ExactPromptPayload::from_version(&version))
    }

    fn version(&self, version_id: PromptVersionId) -> Result<PromptVersion, PromptLibraryUiError> {
        self.versions
            .iter()
            .find(|version| version.id == version_id)
            .cloned()
            .ok_or(PromptLibraryUiError::NotFound)
    }

    fn upsert_prompt(&mut self, prompt: SavedPrompt, version: PromptVersion) {
        if let Some(existing) = self
            .saved
            .iter_mut()
            .find(|candidate| candidate.id == prompt.id)
        {
            *existing = prompt;
        } else {
            self.saved.push(prompt);
        }
        if !self
            .versions
            .iter()
            .any(|existing| existing.id == version.id)
        {
            self.versions.push(version);
        }
        self.library_revision = self.library_revision.saturating_add(1);
        self.load = PromptLibraryLoadState::Ready;
    }

    fn bump_chain_revision(&mut self, chain_id: PromptChainId) -> Result<(), PromptLibraryUiError> {
        let chain = self
            .chains
            .iter_mut()
            .find(|chain| chain.id == chain_id)
            .ok_or(PromptLibraryUiError::NotFound)?;
        chain.revision = chain.revision.saturating_add(1);
        self.library_revision = self.library_revision.saturating_add(1);
        Ok(())
    }

    fn manual_suggested_next(&self, link_id: PromptChainLinkId) -> Option<ManualSuggestedNext> {
        let current = self.links.iter().find(|link| link.id() == link_id)?;
        let next_id = current.next_link_id()?;
        let next = self.links.iter().find(|link| link.id() == next_id)?;
        let title = self
            .saved
            .iter()
            .find(|prompt| prompt.id == next.prompt_id())
            .map(|prompt| prompt.title.clone())
            .unwrap_or_else(|| "Next prompt".into());
        Some(ManualSuggestedNext {
            link_id: next.id(),
            prompt_id: next.prompt_id(),
            prompt_version_id: next.prompt_version_id(),
            title,
            automatic: false,
        })
    }

    fn move_focus(&mut self, delta: isize) {
        let len = match self.chrome.active_section {
            LibrarySection::SavedPrompts => filter_saved_prompts(&self.saved, &self.query).len(),
            LibrarySection::RecentHistory => self.visible_history().len(),
            LibrarySection::Chains => self.links.len(),
        };
        if len == 0 {
            self.focused_index = 0;
            return;
        }
        let next = (self.focused_index as isize + delta).clamp(0, (len - 1) as isize) as usize;
        self.focused_index = next;
        if next < self.list_offset {
            self.list_offset = next;
        } else if next >= self.list_offset.saturating_add(LIBRARY_VISIBLE_ROWS) {
            self.list_offset = next.saturating_sub(LIBRARY_VISIBLE_ROWS.saturating_sub(1));
        }
    }

    fn section_index(section: LibrarySection) -> usize {
        LibrarySection::ALL
            .iter()
            .position(|candidate| *candidate == section)
            .unwrap_or(0)
    }
}

pub fn put_in_composer_action(
    task_id: TaskId,
    agent_session_id: AgentSessionId,
    prompt_version_id: PromptVersionId,
    insertion: ComposerInsertionMode,
    chain_link_id: Option<PromptChainLinkId>,
) -> PutPromptVersionInComposer {
    PutPromptVersionInComposer {
        task_id,
        agent_session_id,
        prompt_version_id,
        insertion,
        chain_link_id,
        sends_provider_input: false,
        advances_chain: false,
    }
}

pub fn diff_is_pure(old: &PromptVersion, new: &PromptVersion) -> bool {
    let diff = diff_versions(&old.body, &new.body);
    diff.old_body() == old.body.as_str() && diff.new_body() == new.body.as_str()
}

pub fn saved_rows(prompts: &[SavedPrompt]) -> Vec<SavedPromptRow> {
    prompts
        .iter()
        .map(|prompt| SavedPromptRow {
            id: prompt.id,
            title: prompt.title.clone(),
            tags: prompt.tags.clone(),
            current_version_id: prompt.current_version_id,
            revision: prompt.revision,
            archived: prompt.archived_at_ms.is_some(),
        })
        .collect()
}

pub fn picker_hits_exclude_provider_library(hits: &[PromptPickerHit]) -> bool {
    hits.iter()
        .all(|hit| hit.source != PromptPickerSource::ProviderCommands)
}

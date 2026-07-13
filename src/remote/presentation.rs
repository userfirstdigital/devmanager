use crate::models::{SessionTab, TabType};
use crate::state::{SessionKind, SessionRuntimeState, SessionStatus};
use crate::terminal::session::TerminalModeSnapshot;
use alacritty_terminal::vte::{Parser, Perform};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::Arc;

const DEFAULT_CANONICAL_EVENTS: usize = 50_000;
const DEFAULT_CANONICAL_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_VERBOSE_EVENTS: usize = 5_000;
const DEFAULT_VERBOSE_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_STORE_SESSIONS: usize = 256;
const DEFAULT_STORE_BYTES: usize = 128 * 1024 * 1024;
pub(crate) const MAX_SEMANTIC_EVENT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StableSessionKey(String);

impl StableSessionKey {
    pub fn from_server(command_id: impl AsRef<str>) -> Self {
        Self(format!("server:{}", command_id.as_ref()))
    }

    pub fn from_tab(tab_id: impl AsRef<str>) -> Self {
        Self(format!("tab:{}", tab_id.as_ref()))
    }

    pub fn resolve(runtime: &SessionRuntimeState, tabs: &[SessionTab]) -> Option<Self> {
        match runtime.session_kind {
            SessionKind::Server | SessionKind::Shell => runtime
                .command_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .map(Self::from_server)
                .or_else(|| Self::resolve_from_tabs(runtime, tabs)),
            SessionKind::Claude | SessionKind::Codex | SessionKind::Ssh => runtime
                .tab_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .map(Self::from_tab)
                .or_else(|| Self::resolve_from_tabs(runtime, tabs)),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn resolve_from_tabs(runtime: &SessionRuntimeState, tabs: &[SessionTab]) -> Option<Self> {
        let tab = tabs.iter().find(|tab| {
            tab.pty_session_id.as_deref() == Some(runtime.session_id.as_str())
                || (matches!(tab.tab_type, TabType::Server)
                    && runtime
                        .command_id
                        .as_deref()
                        .is_some_and(|command_id| tab.command_id.as_deref() == Some(command_id)))
        })?;
        match tab.tab_type {
            TabType::Server => tab
                .command_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .map(Self::from_server),
            TabType::Claude | TabType::Codex | TabType::Ssh => {
                (!tab.id.is_empty()).then(|| Self::from_tab(&tab.id))
            }
        }
    }
}

impl fmt::Display for StableSessionKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SemanticSource {
    Claude,
    Codex,
    Shell,
    Server,
    Ssh,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SemanticStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SemanticToolState {
    Pending,
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum SemanticEventKind {
    UserMessage {
        text: String,
    },
    AssistantMessage {
        message_id: String,
        text: String,
        streaming: bool,
    },
    Reasoning {
        item_id: String,
        summary: String,
    },
    Tool {
        tool_id: String,
        name: String,
        state: SemanticToolState,
        summary: String,
    },
    Diff {
        item_id: String,
        unified_diff: String,
    },
    Command {
        command_id: String,
        text: String,
        exit_code: Option<i32>,
    },
    Output {
        stream: SemanticStream,
        text: String,
    },
    Question {
        question_id: String,
        prompt: String,
        choices: Vec<String>,
    },
    Status {
        state: String,
        detail: Option<String>,
    },
    Error {
        message: String,
    },
    TerminalMode {
        raw_required: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticEvent {
    pub stable_session_key: StableSessionKey,
    pub sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces_sequence: Option<u64>,
    pub occurred_at_epoch_ms: u64,
    pub source: SemanticSource,
    #[serde(flatten)]
    pub kind: SemanticEventKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticRetention {
    Canonical,
    Verbose,
}

#[derive(Debug, Clone)]
pub struct SemanticEventDraft {
    pub stable_session_key: StableSessionKey,
    pub occurred_at_epoch_ms: u64,
    pub source: SemanticSource,
    pub kind: SemanticEventKind,
    pub retention: SemanticRetention,
    pub deduplication_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalLimits {
    pub canonical_events: usize,
    pub canonical_bytes: usize,
    pub verbose_events: usize,
    pub verbose_bytes: usize,
}

impl Default for JournalLimits {
    fn default() -> Self {
        Self {
            canonical_events: DEFAULT_CANONICAL_EVENTS,
            canonical_bytes: DEFAULT_CANONICAL_BYTES,
            verbose_events: DEFAULT_VERBOSE_EVENTS,
            verbose_bytes: DEFAULT_VERBOSE_BYTES,
        }
    }
}

#[derive(Debug, Clone)]
struct StoredSemanticEvent {
    event: Arc<SemanticEvent>,
    encoded_bytes: usize,
    deduplication_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticReplay {
    pub oldest_sequence: u64,
    pub through_sequence: u64,
    pub cursor_rolled_over: bool,
    pub events: Vec<Arc<SemanticEvent>>,
}

/// Pointer-only capture made while the journal mutex is held. Sorting and
/// serialization happen after the caller releases that mutex.
#[derive(Debug, Clone)]
pub struct SemanticReplayCapture {
    pub oldest_sequence: u64,
    pub through_sequence: u64,
    pub cursor_rolled_over: bool,
    events: Vec<Arc<SemanticEvent>>,
}

impl SemanticReplayCapture {
    pub fn events(&self) -> impl Iterator<Item = &Arc<SemanticEvent>> {
        self.events.iter()
    }

    pub fn into_replay(mut self) -> SemanticReplay {
        self.events.sort_unstable_by_key(|event| event.sequence);
        SemanticReplay {
            oldest_sequence: self.oldest_sequence,
            through_sequence: self.through_sequence,
            cursor_rolled_over: self.cursor_rolled_over,
            events: self.events,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JournalCursorMetadata {
    pub oldest_sequence: u64,
    pub latest_sequence: u64,
}

#[derive(Debug)]
pub struct SemanticJournal {
    limits: JournalLimits,
    next_sequence: u64,
    canonical: VecDeque<StoredSemanticEvent>,
    verbose: VecDeque<StoredSemanticEvent>,
    verbose_truncation_marker: Option<StoredSemanticEvent>,
    canonical_bytes: usize,
    verbose_bytes: usize,
    highest_evicted_sequence: u64,
    deduplication_sequences: HashMap<String, u64>,
    #[cfg(test)]
    replay_invocations: std::cell::Cell<u64>,
}

impl Default for SemanticJournal {
    fn default() -> Self {
        Self::with_limits(JournalLimits::default())
    }
}

impl SemanticJournal {
    pub fn with_limits(limits: JournalLimits) -> Self {
        Self {
            limits,
            next_sequence: 1,
            canonical: VecDeque::new(),
            verbose: VecDeque::new(),
            verbose_truncation_marker: None,
            canonical_bytes: 0,
            verbose_bytes: 0,
            highest_evicted_sequence: 0,
            deduplication_sequences: HashMap::new(),
            #[cfg(test)]
            replay_invocations: std::cell::Cell::new(0),
        }
    }

    pub fn push(&mut self, draft: SemanticEventDraft) -> SemanticEvent {
        let replaced_sequence = draft
            .deduplication_key
            .as_ref()
            .and_then(|key| self.deduplication_sequences.get(key))
            .copied();
        let sequence = self.allocate_sequence();
        if let Some(replaced_sequence) = replaced_sequence {
            if !self.preserve_replacement_link(replaced_sequence) {
                self.remove_sequence(replaced_sequence);
            }
        }
        self.insert_draft(draft, sequence, replaced_sequence)
    }

    pub fn cursor_metadata(&self) -> JournalCursorMetadata {
        let oldest_sequence = self
            .canonical
            .front()
            .into_iter()
            .chain(self.verbose.front())
            .chain(self.verbose_truncation_marker.as_ref())
            .map(|stored| stored.event.sequence)
            .min()
            .unwrap_or(0);
        JournalCursorMetadata {
            oldest_sequence,
            latest_sequence: self.next_sequence - 1,
        }
    }

    pub fn capture_replay_after(&self, cursor: u64) -> SemanticReplayCapture {
        #[cfg(test)]
        self.replay_invocations
            .set(self.replay_invocations.get().saturating_add(1));
        let events = self
            .canonical
            .iter()
            .chain(self.verbose.iter())
            .chain(self.verbose_truncation_marker.iter())
            .filter(|stored| stored.event.sequence > cursor)
            .map(|stored| Arc::clone(&stored.event))
            .collect::<Vec<_>>();
        let cursor_metadata = self.cursor_metadata();
        let cursor_rolled_over = cursor != 0 && cursor <= self.highest_evicted_sequence;
        SemanticReplayCapture {
            oldest_sequence: cursor_metadata.oldest_sequence,
            through_sequence: cursor_metadata.latest_sequence,
            cursor_rolled_over,
            events,
        }
    }

    pub fn replay_after(&self, cursor: u64) -> SemanticReplay {
        self.capture_replay_after(cursor).into_replay()
    }

    #[cfg(test)]
    fn replay_invocations(&self) -> u64 {
        self.replay_invocations.get()
    }

    fn allocate_sequence(&mut self) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence = sequence
            .checked_add(1)
            .expect("semantic journal sequence exhausted");
        sequence
    }

    fn insert_draft(
        &mut self,
        draft: SemanticEventDraft,
        sequence: u64,
        replaces_sequence: Option<u64>,
    ) -> SemanticEvent {
        let mut event = SemanticEvent {
            stable_session_key: draft.stable_session_key,
            sequence,
            replaces_sequence,
            occurred_at_epoch_ms: draft.occurred_at_epoch_ms,
            source: draft.source,
            kind: draft.kind,
        };
        let mut encoded_bytes = serde_json::to_vec(&event).map_or(0, |encoded| encoded.len());
        if encoded_bytes > MAX_SEMANTIC_EVENT_BYTES {
            event.kind = SemanticEventKind::Status {
                state: "semanticEventTruncated".to_string(),
                detail: Some(format!(
                    "An oversized semantic event ({encoded_bytes} bytes) was omitted; use the raw terminal stream."
                )),
            };
            encoded_bytes = serde_json::to_vec(&event).map_or(0, |encoded| encoded.len());
        }
        let event = Arc::new(event);
        let stored = StoredSemanticEvent {
            encoded_bytes,
            event: Arc::clone(&event),
            deduplication_key: draft.deduplication_key.clone(),
        };
        if let Some(key) = draft.deduplication_key {
            self.deduplication_sequences.insert(key, sequence);
        }
        match draft.retention {
            SemanticRetention::Canonical => {
                self.canonical_bytes = self.canonical_bytes.saturating_add(stored.encoded_bytes);
                self.canonical.push_back(stored);
                self.enforce_canonical_limits();
            }
            SemanticRetention::Verbose => {
                self.verbose_bytes = self.verbose_bytes.saturating_add(stored.encoded_bytes);
                self.verbose.push_back(stored);
                if self.enforce_verbose_limits() {
                    self.upsert_verbose_truncation_marker(&event.stable_session_key);
                }
            }
        }
        (*event).clone()
    }

    fn remove_sequence(&mut self, sequence: u64) {
        if let Some(index) = self
            .canonical
            .iter()
            .position(|stored| stored.event.sequence == sequence)
        {
            if let Some(stored) = self.canonical.remove(index) {
                self.canonical_bytes = self.canonical_bytes.saturating_sub(stored.encoded_bytes);
                self.remove_deduplication_for(&stored);
            }
            return;
        }
        if let Some(index) = self
            .verbose
            .iter()
            .position(|stored| stored.event.sequence == sequence)
        {
            if let Some(stored) = self.verbose.remove(index) {
                self.verbose_bytes = self.verbose_bytes.saturating_sub(stored.encoded_bytes);
                self.remove_deduplication_for(&stored);
            }
        }
    }

    fn preserve_replacement_link(&mut self, sequence: u64) -> bool {
        let mut deduplication_key = None;
        let retained = if let Some(stored) = self
            .canonical
            .iter_mut()
            .find(|stored| stored.event.sequence == sequence)
        {
            if stored.event.replaces_sequence.is_some() {
                deduplication_key = stored.deduplication_key.take();
                true
            } else {
                false
            }
        } else if let Some(stored) = self
            .verbose
            .iter_mut()
            .find(|stored| stored.event.sequence == sequence)
        {
            if stored.event.replaces_sequence.is_some() {
                deduplication_key = stored.deduplication_key.take();
                true
            } else {
                false
            }
        } else {
            false
        };
        if let Some(key) = deduplication_key {
            if self.deduplication_sequences.get(&key).copied() == Some(sequence) {
                self.deduplication_sequences.remove(&key);
            }
        }
        retained
    }

    fn retained_bytes(&self) -> usize {
        self.canonical_bytes
            .saturating_add(self.verbose_bytes)
            .saturating_add(
                self.verbose_truncation_marker
                    .as_ref()
                    .map_or(0, |stored| stored.encoded_bytes),
            )
    }

    fn trim_oldest_event(&mut self) -> bool {
        enum Queue {
            Canonical,
            Verbose,
            Marker,
        }

        let oldest = [
            self.canonical
                .front()
                .map(|stored| (stored.event.sequence, Queue::Canonical)),
            self.verbose
                .front()
                .map(|stored| (stored.event.sequence, Queue::Verbose)),
            self.verbose_truncation_marker
                .as_ref()
                .map(|stored| (stored.event.sequence, Queue::Marker)),
        ]
        .into_iter()
        .flatten()
        .min_by_key(|(sequence, _)| *sequence);

        let Some((_, queue)) = oldest else {
            return false;
        };
        let stored = match queue {
            Queue::Canonical => self.canonical.pop_front().map(|stored| {
                self.canonical_bytes = self.canonical_bytes.saturating_sub(stored.encoded_bytes);
                stored
            }),
            Queue::Verbose => self.verbose.pop_front().map(|stored| {
                self.verbose_bytes = self.verbose_bytes.saturating_sub(stored.encoded_bytes);
                stored
            }),
            Queue::Marker => self.verbose_truncation_marker.take(),
        };
        if let Some(stored) = stored {
            self.highest_evicted_sequence =
                self.highest_evicted_sequence.max(stored.event.sequence);
            self.remove_deduplication_for(&stored);
            true
        } else {
            false
        }
    }

    fn enforce_canonical_limits(&mut self) {
        while self.canonical.len() > self.limits.canonical_events
            || self.canonical_bytes > self.limits.canonical_bytes
        {
            let Some(stored) = self.canonical.pop_front() else {
                break;
            };
            self.canonical_bytes = self.canonical_bytes.saturating_sub(stored.encoded_bytes);
            self.highest_evicted_sequence =
                self.highest_evicted_sequence.max(stored.event.sequence);
            self.remove_deduplication_for(&stored);
        }
    }

    fn enforce_verbose_limits(&mut self) -> bool {
        let mut truncated = false;
        while self.verbose.len() > self.limits.verbose_events
            || self.verbose_bytes > self.limits.verbose_bytes
        {
            let Some(stored) = self.verbose.pop_front() else {
                break;
            };
            self.verbose_bytes = self.verbose_bytes.saturating_sub(stored.encoded_bytes);
            self.highest_evicted_sequence =
                self.highest_evicted_sequence.max(stored.event.sequence);
            self.remove_deduplication_for(&stored);
            truncated = true;
        }
        truncated
    }

    fn upsert_verbose_truncation_marker(&mut self, key: &StableSessionKey) {
        let event = SemanticEvent {
            stable_session_key: key.clone(),
            sequence: self.allocate_sequence(),
            replaces_sequence: None,
            occurred_at_epoch_ms: 0,
            source: SemanticSource::System,
            kind: SemanticEventKind::Status {
                state: "verboseOutputTruncated".to_string(),
                detail: Some(
                    "Earlier verbose output was discarded by the rolling limit.".to_string(),
                ),
            },
        };
        self.verbose_truncation_marker = Some(StoredSemanticEvent {
            encoded_bytes: serde_json::to_vec(&event).map_or(0, |encoded| encoded.len()),
            event: Arc::new(event),
            deduplication_key: None,
        });
    }

    fn remove_deduplication_for(&mut self, stored: &StoredSemanticEvent) {
        if let Some(key) = stored.deduplication_key.as_ref() {
            if self.deduplication_sequences.get(key).copied() == Some(stored.event.sequence) {
                self.deduplication_sequences.remove(key);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SemanticAttention {
    #[default]
    None,
    Unread,
    NeedsInput,
    Failed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SemanticAdapterHealth {
    #[default]
    Healthy,
    Degraded,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticSessionMetadata {
    pub last_activity_epoch_ms: Option<u64>,
    pub attention: SemanticAttention,
    pub attention_count: u64,
    pub adapter_health: SemanticAdapterHealth,
    pub raw_required: bool,
    pub oldest_sequence: u64,
    pub latest_sequence: u64,
}

#[derive(Debug, Clone)]
struct SessionBinding {
    key: StableSessionKey,
    source: SemanticSource,
    status: Option<SessionStatus>,
    raw_required: Option<bool>,
    evicted_through_sequence: u64,
}

#[derive(Debug)]
struct StoredSessionJournal {
    journal: SemanticJournal,
    metadata: SemanticSessionMetadata,
    active: bool,
}

pub struct SemanticJournalStore {
    limits: JournalLimits,
    max_sessions: usize,
    max_total_bytes: usize,
    sessions: HashMap<StableSessionKey, StoredSessionJournal>,
    session_bindings: HashMap<String, SessionBinding>,
    projectors: HashMap<String, PlainTextProjector>,
}

impl Default for SemanticJournalStore {
    fn default() -> Self {
        Self::with_limits(JournalLimits::default())
    }
}

impl SemanticJournalStore {
    pub fn with_limits(limits: JournalLimits) -> Self {
        Self::with_store_limits(limits, DEFAULT_STORE_SESSIONS, DEFAULT_STORE_BYTES)
    }

    pub fn with_store_limits(
        limits: JournalLimits,
        max_sessions: usize,
        max_total_bytes: usize,
    ) -> Self {
        Self {
            limits,
            max_sessions: max_sessions.max(1),
            max_total_bytes: max_total_bytes.max(1),
            sessions: HashMap::new(),
            session_bindings: HashMap::new(),
            projectors: HashMap::new(),
        }
    }

    pub fn observe_runtime(
        &mut self,
        runtime: &SessionRuntimeState,
        tabs: &[SessionTab],
        occurred_at_epoch_ms: u64,
    ) -> bool {
        let Some(key) = StableSessionKey::resolve(runtime, tabs) else {
            return false;
        };
        let source = semantic_source(runtime.session_kind);
        let previous_binding = self.session_bindings.get(&runtime.session_id).cloned();
        let previous_for_key = previous_binding
            .as_ref()
            .filter(|binding| binding.key == key);
        let previous_status = previous_for_key.and_then(|binding| binding.status);
        self.session_bindings.insert(
            runtime.session_id.clone(),
            SessionBinding {
                key: key.clone(),
                source,
                status: Some(runtime.status),
                raw_required: previous_for_key.and_then(|binding| binding.raw_required),
                evicted_through_sequence: previous_for_key
                    .map(|binding| binding.evicted_through_sequence)
                    .unwrap_or(0),
            },
        );

        let is_new = !self.sessions.contains_key(&key);
        let session = self.ensure_session(&key, runtime.session_kind.is_ai());
        session.active = runtime.status.is_live();
        let runtime_attention = semantic_attention(runtime);
        // An explicit provider question remains the highest-priority live
        // attention state until the user replies. Runtime readiness refreshes
        // must not silently downgrade it to a generic unread marker.
        let attention = if session.metadata.attention == SemanticAttention::NeedsInput
            && runtime_attention != SemanticAttention::Failed
        {
            SemanticAttention::NeedsInput
        } else {
            runtime_attention
        };
        let attention_count = match attention {
            SemanticAttention::None => 0,
            SemanticAttention::NeedsInput => session.metadata.attention_count.max(1),
            SemanticAttention::Unread | SemanticAttention::Failed => {
                runtime.notification_count.max(1)
            }
        };
        let metadata_changed = session.metadata.attention != attention
            || session.metadata.attention_count != attention_count;
        session.metadata.attention = attention;
        session.metadata.attention_count = attention_count;

        let status_changed = previous_status != Some(runtime.status);
        if status_changed {
            let event = SemanticEventDraft {
                stable_session_key: key.clone(),
                occurred_at_epoch_ms,
                source: SemanticSource::System,
                kind: SemanticEventKind::Status {
                    state: semantic_status(runtime.status).to_string(),
                    detail: runtime.exit.as_ref().map(|exit| exit.summary.clone()),
                },
                retention: SemanticRetention::Canonical,
                deduplication_key: None,
            };
            self.record(event);
        }
        if is_new || status_changed || metadata_changed {
            if let Some(session) = self.sessions.get_mut(&key) {
                session.metadata.last_activity_epoch_ms = Some(occurred_at_epoch_ms);
            }
            self.enforce_store_limits();
            true
        } else {
            self.enforce_store_limits();
            false
        }
    }

    pub fn observe_output(
        &mut self,
        session_id: &str,
        bytes: &[u8],
        occurred_at_epoch_ms: u64,
    ) -> bool {
        let Some(binding) = self.session_bindings.get(session_id).cloned() else {
            return false;
        };
        let text = self
            .projectors
            .entry(session_id.to_string())
            .or_default()
            .push(bytes);
        if text.is_empty() {
            return false;
        }
        self.record(SemanticEventDraft {
            stable_session_key: binding.key,
            occurred_at_epoch_ms,
            source: binding.source,
            kind: SemanticEventKind::Output {
                stream: SemanticStream::Stdout,
                text,
            },
            retention: SemanticRetention::Verbose,
            deduplication_key: None,
        });
        true
    }

    pub fn observe_terminal_mode(
        &mut self,
        session_id: &str,
        raw_required: bool,
        occurred_at_epoch_ms: u64,
    ) -> bool {
        let Some(binding) = self.session_bindings.get_mut(session_id) else {
            return false;
        };
        if binding.raw_required == Some(raw_required) {
            return false;
        }
        binding.raw_required = Some(raw_required);
        let key = binding.key.clone();
        let session = self.ensure_session(&key, false);
        session.metadata.raw_required = raw_required;
        self.record(SemanticEventDraft {
            stable_session_key: key,
            occurred_at_epoch_ms,
            source: SemanticSource::System,
            kind: SemanticEventKind::TerminalMode { raw_required },
            retention: SemanticRetention::Canonical,
            deduplication_key: None,
        });
        true
    }

    pub fn observe_native_terminal_mode(
        &mut self,
        session_id: &str,
        mode: TerminalModeSnapshot,
        occurred_at_epoch_ms: u64,
    ) -> bool {
        let Some(source) = self
            .session_bindings
            .get(session_id)
            .map(|binding| binding.source)
        else {
            return false;
        };
        let raw_required = if matches!(source, SemanticSource::Claude | SemanticSource::Codex) {
            mode.mouse_reporting()
        } else {
            mode.alternate_screen || mode.mouse_reporting()
        };
        self.observe_terminal_mode(session_id, raw_required, occurred_at_epoch_ms)
    }

    pub fn record(&mut self, draft: SemanticEventDraft) -> SemanticEvent {
        let key = draft.stable_session_key.clone();
        let occurred_at_epoch_ms = draft.occurred_at_epoch_ms;
        let is_question = matches!(&draft.kind, SemanticEventKind::Question { .. });
        let is_user_message = matches!(&draft.kind, SemanticEventKind::UserMessage { .. });
        let event = {
            let session = self.ensure_session(&key, false);
            let event = session.journal.push(draft);
            let cursor = session.journal.cursor_metadata();
            session.metadata.last_activity_epoch_ms = Some(occurred_at_epoch_ms);
            session.metadata.oldest_sequence = cursor.oldest_sequence;
            session.metadata.latest_sequence = cursor.latest_sequence;
            if is_question {
                let count = if session.metadata.attention == SemanticAttention::NeedsInput
                    && event.replaces_sequence.is_none()
                {
                    session.metadata.attention_count.saturating_add(1)
                } else {
                    session.metadata.attention_count.max(1)
                };
                session.metadata.attention = SemanticAttention::NeedsInput;
                session.metadata.attention_count = count;
            } else if is_user_message
                && session.metadata.attention == SemanticAttention::NeedsInput
            {
                session.metadata.attention = SemanticAttention::None;
                session.metadata.attention_count = 0;
            }
            event
        };
        self.enforce_store_limits();
        event
    }

    pub fn metadata(&self, key: &StableSessionKey) -> Option<SemanticSessionMetadata> {
        self.sessions
            .get(key)
            .map(|session| session.metadata.clone())
    }

    pub fn metadata_snapshot(&self) -> HashMap<StableSessionKey, SemanticSessionMetadata> {
        self.sessions
            .iter()
            .map(|(key, session)| (key.clone(), session.metadata.clone()))
            .collect()
    }

    pub fn replay_after(&self, key: &StableSessionKey, cursor: u64) -> Option<SemanticReplay> {
        self.sessions
            .get(key)
            .map(|session| session.journal.replay_after(cursor))
    }

    pub fn capture_replay_after(
        &self,
        key: &StableSessionKey,
        cursor: u64,
    ) -> Option<SemanticReplayCapture> {
        self.sessions
            .get(key)
            .map(|session| session.journal.capture_replay_after(cursor))
    }

    pub fn stable_key_for_session(&self, session_id: &str) -> Option<StableSessionKey> {
        self.session_bindings
            .get(session_id)
            .map(|binding| binding.key.clone())
    }

    pub fn status_for_session(&self, session_id: &str) -> Option<SessionStatus> {
        self.session_bindings
            .get(session_id)
            .and_then(|binding| binding.status)
    }

    #[cfg(test)]
    pub(crate) fn set_next_sequence_for_test(
        &mut self,
        key: &StableSessionKey,
        next_sequence: u64,
    ) {
        self.ensure_session(key, false).journal.next_sequence = next_sequence;
    }

    pub fn set_attention(
        &mut self,
        key: &StableSessionKey,
        attention: SemanticAttention,
        count: u64,
    ) -> bool {
        let session = self.ensure_session(key, false);
        let count = if attention == SemanticAttention::None {
            0
        } else {
            count.max(1)
        };
        if session.metadata.attention == attention && session.metadata.attention_count == count {
            return false;
        }
        session.metadata.attention = attention;
        session.metadata.attention_count = count;
        self.enforce_store_limits();
        true
    }

    pub fn set_adapter_health(
        &mut self,
        key: &StableSessionKey,
        health: SemanticAdapterHealth,
    ) -> bool {
        let session = self.ensure_session(key, false);
        if session.metadata.adapter_health == health {
            return false;
        }
        session.metadata.adapter_health = health;
        self.enforce_store_limits();
        true
    }

    pub fn remove_session_binding(&mut self, session_id: &str) -> Option<StableSessionKey> {
        self.projectors.remove(session_id);
        let key = self
            .session_bindings
            .remove(session_id)
            .map(|binding| binding.key)?;
        let still_active = self.session_bindings.values().any(|binding| {
            binding.key == key && binding.status.is_some_and(SessionStatus::is_live)
        });
        if let Some(session) = self.sessions.get_mut(&key) {
            session.active = still_active;
        }
        self.enforce_store_limits();
        Some(key)
    }

    pub fn retained_session_count(&self) -> usize {
        self.sessions.len()
    }

    pub fn retained_bytes(&self) -> usize {
        self.sessions
            .values()
            .map(|session| session.journal.retained_bytes())
            .sum()
    }

    fn ensure_session(
        &mut self,
        key: &StableSessionKey,
        degraded_adapter: bool,
    ) -> &mut StoredSessionJournal {
        let matching_bindings = self
            .session_bindings
            .values()
            .filter(|binding| binding.key == *key);
        let (active, raw_required, evicted_through_sequence) = matching_bindings.fold(
            (false, false, 0_u64),
            |(active, raw_required, evicted_through_sequence), binding| {
                (
                    active || binding.status.is_some_and(SessionStatus::is_live),
                    raw_required || binding.raw_required.unwrap_or(false),
                    evicted_through_sequence.max(binding.evicted_through_sequence),
                )
            },
        );
        let limits = self.limits;
        self.sessions.entry(key.clone()).or_insert_with(|| {
            let mut journal = SemanticJournal::with_limits(limits);
            journal.next_sequence = evicted_through_sequence.saturating_add(1);
            journal.highest_evicted_sequence = evicted_through_sequence;
            StoredSessionJournal {
                journal,
                metadata: SemanticSessionMetadata {
                    adapter_health: if degraded_adapter {
                        SemanticAdapterHealth::Degraded
                    } else {
                        SemanticAdapterHealth::Healthy
                    },
                    raw_required,
                    latest_sequence: evicted_through_sequence,
                    ..SemanticSessionMetadata::default()
                },
                active,
            }
        })
    }

    fn enforce_store_limits(&mut self) {
        loop {
            let over_sessions = self.sessions.len() > self.max_sessions;
            let over_bytes = self.retained_bytes() > self.max_total_bytes;
            if !over_sessions && !over_bytes {
                break;
            }

            let oldest_inactive = self
                .sessions
                .iter()
                .filter(|(_, session)| !session.active)
                .min_by_key(|(_, session)| session.metadata.last_activity_epoch_ms.unwrap_or(0))
                .map(|(key, _)| key.clone());
            if let Some(key) = oldest_inactive {
                self.remove_stored_session(&key);
                continue;
            }

            if over_sessions {
                let oldest_active = self
                    .sessions
                    .iter()
                    .min_by_key(|(_, session)| session.metadata.last_activity_epoch_ms.unwrap_or(0))
                    .map(|(key, _)| key.clone());
                if let Some(key) = oldest_active {
                    self.remove_stored_session(&key);
                    continue;
                }
            }

            let oldest_active = self
                .sessions
                .iter()
                .min_by_key(|(_, session)| session.metadata.last_activity_epoch_ms.unwrap_or(0))
                .map(|(key, _)| key.clone());
            let Some(key) = oldest_active else {
                break;
            };
            let trimmed = self
                .sessions
                .get_mut(&key)
                .is_some_and(|session| session.journal.trim_oldest_event());
            if !trimmed {
                self.remove_stored_session(&key);
            } else if let Some(session) = self.sessions.get_mut(&key) {
                let cursor = session.journal.cursor_metadata();
                session.metadata.oldest_sequence = cursor.oldest_sequence;
                session.metadata.latest_sequence = cursor.latest_sequence;
            }
        }
    }

    fn remove_stored_session(&mut self, key: &StableSessionKey) {
        let Some(removed) = self.sessions.remove(key) else {
            return;
        };
        if removed.active {
            let latest_sequence = removed.journal.cursor_metadata().latest_sequence;
            for binding in self
                .session_bindings
                .values_mut()
                .filter(|binding| &binding.key == key)
            {
                binding.evicted_through_sequence =
                    binding.evicted_through_sequence.max(latest_sequence);
            }
            return;
        }
        let removed_session_ids = self
            .session_bindings
            .iter()
            .filter(|(_, binding)| &binding.key == key)
            .map(|(session_id, _)| session_id.clone())
            .collect::<Vec<_>>();
        for session_id in removed_session_ids {
            self.session_bindings.remove(&session_id);
            self.projectors.remove(&session_id);
        }
    }
}

fn semantic_source(kind: SessionKind) -> SemanticSource {
    match kind {
        SessionKind::Shell => SemanticSource::Shell,
        SessionKind::Server => SemanticSource::Server,
        SessionKind::Claude => SemanticSource::Claude,
        SessionKind::Codex => SemanticSource::Codex,
        SessionKind::Ssh => SemanticSource::Ssh,
    }
}

fn semantic_attention(runtime: &SessionRuntimeState) -> SemanticAttention {
    match runtime.status {
        SessionStatus::Crashed | SessionStatus::Failed => SemanticAttention::Failed,
        _ if runtime.unseen_ready || runtime.notification_count > 0 => SemanticAttention::Unread,
        _ => SemanticAttention::None,
    }
}

fn semantic_status(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Stopped => "stopped",
        SessionStatus::Starting => "starting",
        SessionStatus::Running => "running",
        SessionStatus::Stopping => "stopping",
        SessionStatus::Crashed => "crashed",
        SessionStatus::Exited => "exited",
        SessionStatus::Failed => "failed",
    }
}

pub struct PlainTextProjector {
    parser: Parser,
    pending_carriage_return: bool,
}

impl Default for PlainTextProjector {
    fn default() -> Self {
        Self {
            parser: Parser::new(),
            pending_carriage_return: false,
        }
    }
}

impl PlainTextProjector {
    pub fn push(&mut self, bytes: &[u8]) -> String {
        let mut collector = PlainTextCollector {
            output: String::new(),
            pending_carriage_return: &mut self.pending_carriage_return,
        };
        self.parser.advance(&mut collector, bytes);
        collector.output
    }
}

struct PlainTextCollector<'a> {
    output: String,
    pending_carriage_return: &'a mut bool,
}

impl PlainTextCollector<'_> {
    fn flush_carriage_return(&mut self) {
        if std::mem::take(self.pending_carriage_return) {
            self.output.push('\n');
        }
    }
}

impl Perform for PlainTextCollector<'_> {
    fn print(&mut self, character: char) {
        self.flush_carriage_return();
        self.output.push(character);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => *self.pending_carriage_return = true,
            b'\n' => {
                *self.pending_carriage_return = false;
                self.output.push('\n');
            }
            b'\t' => {
                self.flush_carriage_return();
                self.output.push('\t');
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{SessionTab, TabType};
    use crate::state::{SessionKind, SessionRuntimeState};
    use crate::terminal::session::{TerminalBackend, TerminalModeSnapshot};
    use std::path::PathBuf;

    fn output_draft(
        key: StableSessionKey,
        text: &str,
        retention: SemanticRetention,
        deduplication_key: Option<&str>,
    ) -> SemanticEventDraft {
        SemanticEventDraft {
            stable_session_key: key,
            occurred_at_epoch_ms: 7,
            source: SemanticSource::Server,
            kind: SemanticEventKind::Output {
                stream: SemanticStream::Stdout,
                text: text.to_string(),
            },
            retention,
            deduplication_key: deduplication_key.map(str::to_string),
        }
    }

    #[test]
    fn stable_keys_never_use_pty_ids() {
        assert_eq!(
            StableSessionKey::from_server("cmd-1").to_string(),
            "server:cmd-1"
        );
        assert_eq!(StableSessionKey::from_tab("tab-1").to_string(), "tab:tab-1");

        let mut runtime = SessionRuntimeState::new(
            "pty-ephemeral",
            PathBuf::new(),
            Default::default(),
            TerminalBackend::default(),
        );
        runtime.session_kind = SessionKind::Claude;
        runtime.tab_id = Some("tab-stable".to_string());
        let tab = SessionTab {
            id: "tab-stable".to_string(),
            tab_type: TabType::Claude,
            pty_session_id: Some("pty-ephemeral".to_string()),
            ..SessionTab::default()
        };

        assert_eq!(
            StableSessionKey::resolve(&runtime, &[tab]).map(|key| key.to_string()),
            Some("tab:tab-stable".to_string())
        );
    }

    #[test]
    fn journal_replays_strictly_after_cursor_and_reports_rollover() {
        let key = StableSessionKey::from_server("cmd-1");
        let mut journal = SemanticJournal::with_limits(JournalLimits {
            canonical_events: 3,
            canonical_bytes: 1024,
            verbose_events: 2,
            verbose_bytes: 128,
        });
        for text in ["one", "two", "three", "four"] {
            journal.push(output_draft(
                key.clone(),
                text,
                SemanticRetention::Canonical,
                None,
            ));
        }

        let replay = journal.replay_after(1);
        assert_eq!(replay.oldest_sequence, 2);
        assert!(replay.cursor_rolled_over);
        assert_eq!(
            replay
                .events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
    }

    #[test]
    fn replay_capture_uses_arc_refs_and_survives_later_retention_eviction() {
        let key = StableSessionKey::from_server("cmd-arc-snapshot");
        let mut journal = SemanticJournal::with_limits(JournalLimits {
            canonical_events: 2,
            canonical_bytes: 16 * 1024,
            verbose_events: 1,
            verbose_bytes: 16 * 1024,
        });
        let first = journal.push(output_draft(
            key.clone(),
            "first-retained-payload",
            SemanticRetention::Canonical,
            None,
        ));
        journal.push(output_draft(
            key.clone(),
            "second",
            SemanticRetention::Canonical,
            None,
        ));

        let capture = journal.capture_replay_after(0);
        let captured_first = capture
            .events()
            .find(|event| event.sequence == first.sequence)
            .expect("first captured event")
            .clone();
        assert_eq!(capture.through_sequence, 2);

        journal.push(output_draft(
            key,
            "third-evicts-first",
            SemanticRetention::Canonical,
            None,
        ));
        let snapshot = capture.into_replay();

        assert_eq!(snapshot.through_sequence, 2);
        assert_eq!(
            snapshot
                .events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(std::sync::Arc::ptr_eq(&captured_first, &snapshot.events[0]));
        assert!(matches!(
            &snapshot.events[0].kind,
            SemanticEventKind::Output { text, .. } if text == "first-retained-payload"
        ));
    }

    #[test]
    fn fifty_five_thousand_event_capture_is_pointer_only_and_snapshot_stable() {
        const EVENT_COUNT: usize = 55_000;
        let key = StableSessionKey::from_server("large-pointer-snapshot");
        let mut journal = SemanticJournal::with_limits(JournalLimits {
            canonical_events: EVENT_COUNT,
            canonical_bytes: 32 * 1024 * 1024,
            verbose_events: 1,
            verbose_bytes: 1024,
        });
        for sequence in 0..EVENT_COUNT {
            journal.push(output_draft(
                key.clone(),
                if sequence == 0 { "first" } else { "x" },
                SemanticRetention::Canonical,
                None,
            ));
        }
        let stored_first = journal
            .canonical
            .front()
            .expect("first retained event")
            .event
            .clone();

        let capture = journal.capture_replay_after(0);

        assert_eq!(capture.events().count(), EVENT_COUNT);
        assert_eq!(capture.through_sequence, EVENT_COUNT as u64);
        let replay = capture.into_replay();
        assert_eq!(replay.events.len(), EVENT_COUNT);
        assert!(Arc::ptr_eq(&stored_first, &replay.events[0]));
    }

    #[test]
    fn replay_capture_orders_gapped_canonical_verbose_and_marker_sequences() {
        let key = StableSessionKey::from_server("cmd-gapped-snapshot");
        let mut journal = SemanticJournal::with_limits(JournalLimits {
            canonical_events: 8,
            canonical_bytes: 16 * 1024,
            verbose_events: 1,
            verbose_bytes: 16 * 1024,
        });
        journal.push(output_draft(
            key.clone(),
            "canonical-one",
            SemanticRetention::Canonical,
            None,
        ));
        journal.push(output_draft(
            key.clone(),
            "verbose-evicted",
            SemanticRetention::Verbose,
            None,
        ));
        journal.push(output_draft(
            key.clone(),
            "canonical-three",
            SemanticRetention::Canonical,
            None,
        ));
        journal.push(output_draft(
            key,
            "verbose-retained",
            SemanticRetention::Verbose,
            None,
        ));

        let snapshot = journal.capture_replay_after(1).into_replay();

        assert!(snapshot.cursor_rolled_over);
        assert_eq!(snapshot.through_sequence, 5);
        assert_eq!(
            snapshot
                .events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
    }

    #[test]
    fn oversized_event_is_replaced_before_journal_insertion() {
        let key = StableSessionKey::from_server("cmd-oversized");
        let mut journal = SemanticJournal::with_limits(JournalLimits {
            canonical_events: 8,
            canonical_bytes: MAX_SEMANTIC_EVENT_BYTES * 2,
            verbose_events: 8,
            verbose_bytes: MAX_SEMANTIC_EVENT_BYTES * 2,
        });

        let published = journal.push(output_draft(
            key,
            &"x".repeat(MAX_SEMANTIC_EVENT_BYTES * 2),
            SemanticRetention::Canonical,
            None,
        ));
        let snapshot = journal.capture_replay_after(0).into_replay();

        assert!(matches!(
            &published.kind,
            SemanticEventKind::Status { state, detail: Some(detail) }
                if state == "semanticEventTruncated" && detail.contains("raw terminal")
        ));
        assert!(serde_json::to_vec(&published).unwrap().len() <= MAX_SEMANTIC_EVENT_BYTES);
        assert_eq!(snapshot.events.len(), 1);
        assert!(std::sync::Arc::ptr_eq(
            &journal.canonical.front().expect("stored surrogate").event,
            &snapshot.events[0]
        ));
    }

    #[test]
    fn verbose_rollover_preserves_canonical_events_and_adds_a_marker() {
        let key = StableSessionKey::from_server("cmd-1");
        let mut journal = SemanticJournal::with_limits(JournalLimits {
            canonical_events: 4,
            canonical_bytes: 1024,
            verbose_events: 1,
            verbose_bytes: 1024,
        });
        journal.push(output_draft(
            key.clone(),
            "canonical",
            SemanticRetention::Canonical,
            None,
        ));
        journal.push(output_draft(
            key.clone(),
            "verbose-one",
            SemanticRetention::Verbose,
            None,
        ));
        journal.push(output_draft(
            key,
            "verbose-two",
            SemanticRetention::Verbose,
            None,
        ));

        let replay = journal.replay_after(0);
        let output = replay
            .events
            .iter()
            .filter_map(|event| match &event.kind {
                SemanticEventKind::Output { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(output.contains(&"canonical"));
        assert!(output.contains(&"verbose-two"));
        assert!(replay.events.iter().any(|event| matches!(
            &event.kind,
            SemanticEventKind::Status { state, .. } if state == "verboseOutputTruncated"
        )));
    }

    #[test]
    fn verbose_truncation_marker_does_not_consume_canonical_capacity() {
        let key = StableSessionKey::from_server("cmd-1");
        let mut journal = SemanticJournal::with_limits(JournalLimits {
            canonical_events: 2,
            canonical_bytes: 1024,
            verbose_events: 1,
            verbose_bytes: 1024,
        });
        journal.push(output_draft(
            key.clone(),
            "canonical-one",
            SemanticRetention::Canonical,
            None,
        ));
        journal.push(output_draft(
            key.clone(),
            "canonical-two",
            SemanticRetention::Canonical,
            None,
        ));
        journal.push(output_draft(
            key.clone(),
            "verbose-one",
            SemanticRetention::Verbose,
            None,
        ));
        journal.push(output_draft(
            key,
            "verbose-two",
            SemanticRetention::Verbose,
            None,
        ));

        let replay = journal.replay_after(0);
        let output = replay
            .events
            .iter()
            .filter_map(|event| match &event.kind {
                SemanticEventKind::Output { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            output,
            vec!["canonical-one", "canonical-two", "verbose-two"]
        );
        assert!(replay.events.iter().any(|event| matches!(
            &event.kind,
            SemanticEventKind::Status { state, .. } if state == "verboseOutputTruncated"
        )));
    }

    #[test]
    fn replay_reports_rollover_for_a_verbose_gap_after_retained_canonical_history() {
        let key = StableSessionKey::from_server("cmd-1");
        let mut journal = SemanticJournal::with_limits(JournalLimits {
            canonical_events: 4,
            canonical_bytes: 1024,
            verbose_events: 1,
            verbose_bytes: 1024,
        });
        journal.push(output_draft(
            key.clone(),
            "canonical",
            SemanticRetention::Canonical,
            None,
        ));
        journal.push(output_draft(
            key.clone(),
            "verbose-one",
            SemanticRetention::Verbose,
            None,
        ));
        journal.push(output_draft(
            key,
            "verbose-two",
            SemanticRetention::Verbose,
            None,
        ));

        let replay = journal.replay_after(1);
        assert!(replay.cursor_rolled_over);
    }

    #[test]
    fn deduplication_replacement_gets_a_fresh_sequence_and_preserves_eviction_order() {
        let key = StableSessionKey::from_tab("tab-1");
        let mut journal = SemanticJournal::with_limits(JournalLimits {
            canonical_events: 2,
            canonical_bytes: 1024,
            verbose_events: 2,
            verbose_bytes: 1024,
        });
        let first = journal.push(output_draft(
            key.clone(),
            "partial",
            SemanticRetention::Canonical,
            Some("message-1"),
        ));
        let middle = journal.push(output_draft(
            key.clone(),
            "middle",
            SemanticRetention::Canonical,
            None,
        ));
        let replacement = journal.push(output_draft(
            key.clone(),
            "complete",
            SemanticRetention::Canonical,
            Some("message-1"),
        ));

        assert!(replacement.sequence > middle.sequence);
        assert!(replacement.sequence > first.sequence);
        assert_eq!(
            journal
                .replay_after(first.sequence)
                .events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![middle.sequence, replacement.sequence]
        );

        let last = journal.push(output_draft(
            key,
            "last",
            SemanticRetention::Canonical,
            None,
        ));
        assert_eq!(
            journal
                .replay_after(0)
                .events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![replacement.sequence, last.sequence]
        );
    }

    #[test]
    fn incremental_replay_identifies_the_sequence_replaced_by_deduplication() {
        let key = StableSessionKey::from_tab("tab-incremental");
        let mut journal = SemanticJournal::default();
        let partial = journal.push(output_draft(
            key.clone(),
            "partial response",
            SemanticRetention::Canonical,
            Some("assistant-message-1"),
        ));

        let replacement = journal.push(output_draft(
            key,
            "complete response",
            SemanticRetention::Canonical,
            Some("assistant-message-1"),
        ));
        let incremental = journal.replay_after(partial.sequence);

        assert_eq!(incremental.events.len(), 1);
        assert_eq!(incremental.events[0].sequence, replacement.sequence);
        assert_eq!(
            incremental.events[0].replaces_sequence,
            Some(partial.sequence)
        );
        let json = serde_json::to_value(incremental.events[0].as_ref()).unwrap();
        assert_eq!(json["replacesSequence"], partial.sequence);
    }

    #[test]
    fn chained_deduplication_replay_deletes_every_observable_predecessor() {
        let key = StableSessionKey::from_tab("tab-chain");
        let mut journal = SemanticJournal::default();
        let first = journal.push(output_draft(
            key.clone(),
            "first partial",
            SemanticRetention::Canonical,
            Some("assistant-message-chain"),
        ));
        let second = journal.push(output_draft(
            key.clone(),
            "second partial",
            SemanticRetention::Canonical,
            Some("assistant-message-chain"),
        ));
        let final_event = journal.push(output_draft(
            key,
            "final response",
            SemanticRetention::Canonical,
            Some("assistant-message-chain"),
        ));

        let replay = journal.replay_after(first.sequence);
        assert_eq!(
            replay
                .events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![second.sequence, final_event.sequence]
        );

        let mut browser_sequences = std::collections::BTreeSet::from([first.sequence]);
        for event in replay.events {
            if let Some(replaced) = event.replaces_sequence {
                browser_sequences.remove(&replaced);
            }
            browser_sequences.insert(event.sequence);
        }
        assert_eq!(browser_sequences, [final_event.sequence].into());
    }

    #[test]
    fn cursor_metadata_tracks_retained_bounds_without_replaying_payloads() {
        let key = StableSessionKey::from_server("cmd-1");
        let mut journal = SemanticJournal::with_limits(JournalLimits {
            canonical_events: 2,
            canonical_bytes: 1024,
            verbose_events: 1,
            verbose_bytes: 1024,
        });
        journal.push(output_draft(
            key.clone(),
            "canonical",
            SemanticRetention::Canonical,
            None,
        ));
        journal.push(output_draft(
            key.clone(),
            "verbose-one",
            SemanticRetention::Verbose,
            None,
        ));
        journal.push(output_draft(
            key,
            "verbose-two",
            SemanticRetention::Verbose,
            None,
        ));

        let cursor = journal.cursor_metadata();
        fn assert_copy<T: Copy>(_: T) {}
        assert_copy(cursor);
        assert_eq!(cursor.oldest_sequence, 1);
        assert_eq!(cursor.latest_sequence, 4);
    }

    #[test]
    fn store_metadata_updates_do_not_replay_retained_payloads() {
        let key = StableSessionKey::from_server("cmd-1");
        let mut store = SemanticJournalStore::default();
        let retained_payload = "retained-payload".repeat(1_000);
        store.record(output_draft(
            key.clone(),
            &retained_payload,
            SemanticRetention::Canonical,
            None,
        ));
        let replay_calls = {
            let journal = &store.sessions.get(&key).expect("journal").journal;
            assert_eq!(journal.replay_after(0).events.len(), 1);
            journal.replay_invocations()
        };

        store.record(output_draft(
            key.clone(),
            "next-event",
            SemanticRetention::Canonical,
            None,
        ));

        assert_eq!(
            store
                .sessions
                .get(&key)
                .expect("journal")
                .journal
                .replay_invocations(),
            replay_calls
        );
    }

    #[test]
    fn sequence_exhaustion_fails_closed_before_inserting_an_event() {
        let key = StableSessionKey::from_server("cmd-1");
        let mut journal = SemanticJournal::default();
        journal.next_sequence = u64::MAX;

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            journal.push(output_draft(
                key,
                "must-not-be-inserted",
                SemanticRetention::Canonical,
                None,
            ));
        }));

        assert!(result.is_err());
        assert!(journal.replay_after(0).events.is_empty());
    }

    #[test]
    fn ansi_projector_handles_sequences_split_across_chunks() {
        let mut projector = PlainTextProjector::default();
        assert_eq!(projector.push(b"ok\x1b[3"), "ok");
        assert_eq!(projector.push(b"1mred\x1b[0m\rnext\n"), "red\nnext\n");
    }

    #[test]
    fn ansi_projector_ignores_osc_terminated_by_bel() {
        let mut projector = PlainTextProjector::default();
        assert_eq!(
            projector.push(b"before\x1b]0;title\x07after"),
            "beforeafter"
        );
    }

    #[test]
    fn ansi_projector_handles_split_osc_string_terminator() {
        let mut projector = PlainTextProjector::default();
        assert_eq!(projector.push(b"before\x1b]0;title\x1b"), "before");
        assert_eq!(projector.push(b"\\after"), "after");
    }

    #[test]
    fn ansi_projector_handles_utf8_split_across_chunks() {
        let mut projector = PlainTextProjector::default();
        assert_eq!(projector.push(&[b'b', 0xc3]), "b");
        assert_eq!(projector.push(&[0xa9, b'!']), "é!");
    }

    #[test]
    fn ansi_projector_does_not_leak_malformed_or_incomplete_tails() {
        let mut projector = PlainTextProjector::default();
        assert_eq!(projector.push(b"ok\x1b["), "ok");
        assert_eq!(projector.push(b"999999999999999999999999"), "");
    }

    #[test]
    fn journal_store_tracks_runtime_output_and_session_metadata() {
        let mut runtime = SessionRuntimeState::new(
            "pty-ephemeral",
            PathBuf::new(),
            Default::default(),
            TerminalBackend::default(),
        );
        runtime.session_kind = SessionKind::Claude;
        runtime.tab_id = Some("tab-stable".to_string());
        runtime.notification_count = 2;
        let tabs = vec![SessionTab {
            id: "tab-stable".to_string(),
            tab_type: TabType::Claude,
            pty_session_id: Some("pty-ephemeral".to_string()),
            ..SessionTab::default()
        }];
        let key = StableSessionKey::from_tab("tab-stable");
        let mut store = SemanticJournalStore::default();

        assert!(store.observe_runtime(&runtime, &tabs, 100));
        assert!(store.observe_output("pty-ephemeral", b"\x1b[31mhello\x1b[0m", 101));

        let metadata = store.metadata(&key).expect("session metadata");
        assert_eq!(metadata.last_activity_epoch_ms, Some(101));
        assert_eq!(metadata.attention, SemanticAttention::Unread);
        assert_eq!(metadata.attention_count, 2);
        assert_eq!(metadata.adapter_health, SemanticAdapterHealth::Degraded);
        let replay = store.replay_after(&key, 0).expect("session replay");
        assert!(replay.events.iter().any(|event| matches!(
            &event.kind,
            SemanticEventKind::Output { text, .. } if text == "hello"
        )));
    }

    #[test]
    fn needs_input_attention_survives_runtime_refresh_and_clears_on_user_reply() {
        let mut store = SemanticJournalStore::default();
        let key = StableSessionKey::from_tab("ai-tab");
        let tab = SessionTab {
            id: "ai-tab".to_string(),
            tab_type: TabType::Claude,
            pty_session_id: Some("ai-pty".to_string()),
            ..SessionTab::default()
        };
        let mut runtime = SessionRuntimeState::new(
            "ai-pty",
            PathBuf::new(),
            Default::default(),
            TerminalBackend::default(),
        );
        runtime.status = crate::state::SessionStatus::Running;
        runtime.session_kind = SessionKind::Claude;
        runtime.tab_id = Some("ai-tab".to_string());
        store.observe_runtime(&runtime, std::slice::from_ref(&tab), 1);

        store.record(SemanticEventDraft {
            stable_session_key: key.clone(),
            occurred_at_epoch_ms: 2,
            source: SemanticSource::Claude,
            kind: SemanticEventKind::Question {
                question_id: "question-1".to_string(),
                prompt: "Choose".to_string(),
                choices: vec!["A".to_string(), "B".to_string()],
            },
            retention: SemanticRetention::Canonical,
            deduplication_key: Some("question-1".to_string()),
        });
        assert_eq!(
            store.metadata(&key).unwrap().attention,
            SemanticAttention::NeedsInput
        );
        assert_eq!(store.status_for_session("ai-pty"), Some(crate::state::SessionStatus::Running));

        store.observe_runtime(&runtime, &[tab], 3);
        assert_eq!(
            store.metadata(&key).unwrap().attention,
            SemanticAttention::NeedsInput
        );

        store.record(SemanticEventDraft {
            stable_session_key: key.clone(),
            occurred_at_epoch_ms: 4,
            source: SemanticSource::Claude,
            kind: SemanticEventKind::UserMessage {
                text: "A".to_string(),
            },
            retention: SemanticRetention::Canonical,
            deduplication_key: None,
        });
        assert_eq!(
            store.metadata(&key).unwrap().attention,
            SemanticAttention::None
        );
        assert_eq!(store.metadata(&key).unwrap().attention_count, 0);
    }

    #[test]
    fn removed_session_journals_and_projectors_obey_global_store_caps() {
        let mut store = SemanticJournalStore::with_store_limits(
            JournalLimits {
                canonical_events: 64,
                canonical_bytes: 64 * 1024,
                verbose_events: 64,
                verbose_bytes: 64 * 1024,
            },
            3,
            2 * 1024,
        );

        for index in 0..24 {
            let session_id = format!("pty-{index}");
            let command_id = format!("command-{index}");
            let mut runtime = SessionRuntimeState::new(
                session_id.clone(),
                PathBuf::new(),
                Default::default(),
                TerminalBackend::default(),
            );
            runtime.session_kind = SessionKind::Server;
            runtime.command_id = Some(command_id);
            runtime.status = SessionStatus::Running;
            assert!(store.observe_runtime(&runtime, &[], index * 10));
            assert!(store.observe_output(
                &session_id,
                format!("payload-{index}-{}", "x".repeat(256)).as_bytes(),
                index * 10 + 1,
            ));
            assert_eq!(
                store.remove_session_binding(&session_id),
                Some(StableSessionKey::from_server(format!("command-{index}")))
            );
        }

        assert!(store.retained_session_count() <= 3);
        assert!(store.retained_bytes() <= 2 * 1024);
        assert!(store.session_bindings.is_empty());
        assert!(store.projectors.is_empty());
        assert!(store
            .metadata(&StableSessionKey::from_server("command-23"))
            .is_some());
    }

    #[test]
    fn active_history_is_trimmed_with_rollover_instead_of_growing_past_global_bytes() {
        let mut store = SemanticJournalStore::with_store_limits(
            JournalLimits {
                canonical_events: 128,
                canonical_bytes: 128 * 1024,
                verbose_events: 128,
                verbose_bytes: 128 * 1024,
            },
            8,
            1024,
        );
        let session_id = "active-pty";
        let key = StableSessionKey::from_server("active-command");
        let mut runtime = SessionRuntimeState::new(
            session_id,
            PathBuf::new(),
            Default::default(),
            TerminalBackend::default(),
        );
        runtime.session_kind = SessionKind::Server;
        runtime.command_id = Some("active-command".to_string());
        runtime.status = SessionStatus::Running;
        assert!(store.observe_runtime(&runtime, &[], 1));

        let first = store.record(output_draft(
            key.clone(),
            &format!("first-{}", "x".repeat(300)),
            SemanticRetention::Canonical,
            None,
        ));
        for index in 0..20 {
            store.record(output_draft(
                key.clone(),
                &format!("event-{index}-{}", "x".repeat(300)),
                SemanticRetention::Canonical,
                None,
            ));
        }

        assert_eq!(store.retained_session_count(), 1);
        assert!(store.retained_bytes() <= 1024);
        assert!(store.session_bindings.contains_key(session_id));
        let replay = store
            .capture_replay_after(&key, first.sequence)
            .expect("active journal retained")
            .into_replay();
        assert!(replay.cursor_rolled_over);
        assert!(replay.through_sequence > first.sequence);
    }

    #[test]
    fn active_session_cap_eviction_preserves_binding_and_sequence_rollover() {
        let mut store =
            SemanticJournalStore::with_store_limits(JournalLimits::default(), 1, 1024 * 1024);
        let key_a = StableSessionKey::from_server("command-a");
        let key_b = StableSessionKey::from_server("command-b");

        let mut runtime_a = SessionRuntimeState::new(
            "pty-a",
            PathBuf::new(),
            Default::default(),
            TerminalBackend::default(),
        );
        runtime_a.session_kind = SessionKind::Server;
        runtime_a.command_id = Some("command-a".to_string());
        runtime_a.status = SessionStatus::Running;
        assert!(store.observe_runtime(&runtime_a, &[], 10));
        let previous_latest = store
            .metadata(&key_a)
            .expect("first active journal")
            .latest_sequence;

        let mut runtime_b = SessionRuntimeState::new(
            "pty-b",
            PathBuf::new(),
            Default::default(),
            TerminalBackend::default(),
        );
        runtime_b.session_kind = SessionKind::Server;
        runtime_b.command_id = Some("command-b".to_string());
        runtime_b.status = SessionStatus::Running;
        assert!(store.observe_runtime(&runtime_b, &[], 20));

        assert_eq!(store.retained_session_count(), 1);
        assert_eq!(store.stable_key_for_session("pty-a"), Some(key_a.clone()));
        assert_eq!(store.stable_key_for_session("pty-b"), Some(key_b));
        assert!(store.observe_output("pty-a", b"after eviction\n", 30));

        let replay = store
            .capture_replay_after(&key_a, previous_latest)
            .expect("active journal recreated")
            .into_replay();
        assert!(replay.cursor_rolled_over);
        assert!(replay.through_sequence > previous_latest);
        assert!(replay
            .events
            .iter()
            .all(|event| event.sequence > previous_latest));
    }

    #[test]
    fn journal_store_emits_terminal_mode_only_when_raw_requirement_changes() {
        let mut runtime = SessionRuntimeState::new(
            "server-runtime",
            PathBuf::new(),
            Default::default(),
            TerminalBackend::default(),
        );
        runtime.session_kind = SessionKind::Server;
        runtime.command_id = Some("command-stable".to_string());
        let key = StableSessionKey::from_server("command-stable");
        let mut store = SemanticJournalStore::default();
        assert!(store.observe_runtime(&runtime, &[], 100));

        assert!(store.observe_terminal_mode("server-runtime", false, 101));
        assert!(!store.observe_terminal_mode("server-runtime", false, 102));
        assert!(store.observe_terminal_mode("server-runtime", true, 103));

        let replay = store.replay_after(&key, 0).expect("session replay");
        let modes = replay
            .events
            .iter()
            .filter_map(|event| match event.kind {
                SemanticEventKind::TerminalMode { raw_required } => Some(raw_required),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(modes, vec![false, true]);
    }

    #[test]
    fn native_terminal_mode_keeps_ai_alternate_screens_semantic_but_shells_raw() {
        let mut ai_runtime = SessionRuntimeState::new(
            "ai-runtime",
            PathBuf::new(),
            Default::default(),
            TerminalBackend::default(),
        );
        ai_runtime.session_kind = SessionKind::Claude;
        ai_runtime.tab_id = Some("ai-tab".to_string());
        let mut shell_runtime = SessionRuntimeState::new(
            "shell-runtime",
            PathBuf::new(),
            Default::default(),
            TerminalBackend::default(),
        );
        shell_runtime.session_kind = SessionKind::Shell;
        shell_runtime.command_id = Some("shell-command".to_string());
        let mut store = SemanticJournalStore::default();
        assert!(store.observe_runtime(&ai_runtime, &[], 100));
        assert!(store.observe_runtime(&shell_runtime, &[], 100));
        let alternate_screen = TerminalModeSnapshot {
            alternate_screen: true,
            ..TerminalModeSnapshot::default()
        };

        assert!(store.observe_native_terminal_mode("ai-runtime", alternate_screen, 101));
        assert!(store.observe_native_terminal_mode("shell-runtime", alternate_screen, 101));
        assert!(
            !store
                .metadata(&StableSessionKey::from_tab("ai-tab"))
                .expect("AI metadata")
                .raw_required
        );
        assert!(
            store
                .metadata(&StableSessionKey::from_server("shell-command"))
                .expect("shell metadata")
                .raw_required
        );

        assert!(store.observe_native_terminal_mode(
            "ai-runtime",
            TerminalModeSnapshot {
                mouse_report_click: true,
                ..alternate_screen
            },
            102,
        ));
        assert!(
            store
                .metadata(&StableSessionKey::from_tab("ai-tab"))
                .expect("AI metadata")
                .raw_required
        );
    }
}

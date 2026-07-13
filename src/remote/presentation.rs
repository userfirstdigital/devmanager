use crate::models::{SessionTab, TabType};
use crate::state::{SessionKind, SessionRuntimeState, SessionStatus};
use alacritty_terminal::vte::{Parser, Perform};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fmt;

const DEFAULT_CANONICAL_EVENTS: usize = 50_000;
const DEFAULT_CANONICAL_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_VERBOSE_EVENTS: usize = 5_000;
const DEFAULT_VERBOSE_BYTES: usize = 8 * 1024 * 1024;
const VERBOSE_TRUNCATION_DEDUP_KEY: &str = "semantic:verbose-truncation";

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
    event: SemanticEvent,
    encoded_bytes: usize,
    deduplication_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticReplay {
    pub oldest_sequence: u64,
    pub latest_sequence: u64,
    pub cursor_rolled_over: bool,
    pub events: Vec<SemanticEvent>,
}

#[derive(Debug)]
pub struct SemanticJournal {
    limits: JournalLimits,
    next_sequence: u64,
    canonical: VecDeque<StoredSemanticEvent>,
    verbose: VecDeque<StoredSemanticEvent>,
    canonical_bytes: usize,
    verbose_bytes: usize,
    highest_evicted_sequence: u64,
    deduplication_sequences: HashMap<String, u64>,
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
            canonical_bytes: 0,
            verbose_bytes: 0,
            highest_evicted_sequence: 0,
            deduplication_sequences: HashMap::new(),
        }
    }

    pub fn push(&mut self, draft: SemanticEventDraft) -> SemanticEvent {
        if let Some(sequence) = draft
            .deduplication_key
            .as_ref()
            .and_then(|key| self.deduplication_sequences.get(key))
            .copied()
        {
            self.remove_sequence(sequence);
            return self.insert_draft(draft, sequence);
        }

        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.insert_draft(draft, sequence)
    }

    pub fn replay_after(&self, cursor: u64) -> SemanticReplay {
        let mut available = self
            .canonical
            .iter()
            .chain(self.verbose.iter())
            .map(|stored| stored.event.clone())
            .collect::<Vec<_>>();
        available.sort_by_key(|event| event.sequence);
        let oldest_sequence = available.first().map(|event| event.sequence).unwrap_or(0);
        let latest_sequence = self.next_sequence.saturating_sub(1);
        let cursor_rolled_over = cursor != 0 && cursor <= self.highest_evicted_sequence;
        let events = available
            .into_iter()
            .filter(|event| event.sequence > cursor)
            .collect();
        SemanticReplay {
            oldest_sequence,
            latest_sequence,
            cursor_rolled_over,
            events,
        }
    }

    fn insert_draft(&mut self, draft: SemanticEventDraft, sequence: u64) -> SemanticEvent {
        let event = SemanticEvent {
            stable_session_key: draft.stable_session_key,
            sequence,
            occurred_at_epoch_ms: draft.occurred_at_epoch_ms,
            source: draft.source,
            kind: draft.kind,
        };
        let stored = StoredSemanticEvent {
            encoded_bytes: serde_json::to_vec(&event).map_or(0, |encoded| encoded.len()),
            event: event.clone(),
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
        event
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
        let marker = SemanticEventDraft {
            stable_session_key: key.clone(),
            occurred_at_epoch_ms: 0,
            source: SemanticSource::System,
            kind: SemanticEventKind::Status {
                state: "verboseOutputTruncated".to_string(),
                detail: Some(
                    "Earlier verbose output was discarded by the rolling limit.".to_string(),
                ),
            },
            retention: SemanticRetention::Canonical,
            deduplication_key: Some(VERBOSE_TRUNCATION_DEDUP_KEY.to_string()),
        };
        self.push(marker);
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
}

#[derive(Debug)]
struct StoredSessionJournal {
    journal: SemanticJournal,
    metadata: SemanticSessionMetadata,
}

pub struct SemanticJournalStore {
    limits: JournalLimits,
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
        Self {
            limits,
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
        let previous_status = previous_binding.as_ref().and_then(|binding| binding.status);
        self.session_bindings.insert(
            runtime.session_id.clone(),
            SessionBinding {
                key: key.clone(),
                source,
                status: Some(runtime.status),
                raw_required: previous_binding.and_then(|binding| binding.raw_required),
            },
        );

        let is_new = !self.sessions.contains_key(&key);
        let session = self.ensure_session(&key, runtime.session_kind.is_ai());
        let attention = semantic_attention(runtime);
        let attention_count = if attention == SemanticAttention::None {
            0
        } else {
            runtime.notification_count.max(1)
        };
        let metadata_changed = session.metadata.attention != attention
            || session.metadata.attention_count != attention_count;
        session.metadata.attention = attention;
        session.metadata.attention_count = attention_count;

        let status_changed = previous_status != Some(runtime.status);
        if status_changed {
            let event = SemanticEventDraft {
                stable_session_key: key,
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
            if let Some(session) = self.sessions.get_mut(
                &self
                    .session_bindings
                    .get(&runtime.session_id)
                    .expect("binding was inserted")
                    .key,
            ) {
                session.metadata.last_activity_epoch_ms = Some(occurred_at_epoch_ms);
            }
            true
        } else {
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

    pub fn record(&mut self, draft: SemanticEventDraft) -> SemanticEvent {
        let key = draft.stable_session_key.clone();
        let occurred_at_epoch_ms = draft.occurred_at_epoch_ms;
        let session = self.ensure_session(&key, false);
        let event = session.journal.push(draft);
        let replay = session.journal.replay_after(0);
        session.metadata.last_activity_epoch_ms = Some(occurred_at_epoch_ms);
        session.metadata.oldest_sequence = replay.oldest_sequence;
        session.metadata.latest_sequence = replay.latest_sequence;
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

    pub fn stable_key_for_session(&self, session_id: &str) -> Option<StableSessionKey> {
        self.session_bindings
            .get(session_id)
            .map(|binding| binding.key.clone())
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
        true
    }

    pub fn remove_session_binding(&mut self, session_id: &str) -> Option<StableSessionKey> {
        self.projectors.remove(session_id);
        self.session_bindings
            .remove(session_id)
            .map(|binding| binding.key)
    }

    fn ensure_session(
        &mut self,
        key: &StableSessionKey,
        degraded_adapter: bool,
    ) -> &mut StoredSessionJournal {
        self.sessions
            .entry(key.clone())
            .or_insert_with(|| StoredSessionJournal {
                journal: SemanticJournal::with_limits(self.limits),
                metadata: SemanticSessionMetadata {
                    adapter_health: if degraded_adapter {
                        SemanticAdapterHealth::Degraded
                    } else {
                        SemanticAdapterHealth::Healthy
                    },
                    ..SemanticSessionMetadata::default()
                },
            })
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
    use crate::terminal::session::TerminalBackend;
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
    fn deduplication_replaces_an_existing_event_without_advancing_sequence() {
        let key = StableSessionKey::from_tab("tab-1");
        let mut journal = SemanticJournal::default();
        let first = journal.push(output_draft(
            key.clone(),
            "partial",
            SemanticRetention::Canonical,
            Some("message-1"),
        ));
        let replacement = journal.push(output_draft(
            key,
            "complete",
            SemanticRetention::Canonical,
            Some("message-1"),
        ));

        assert_eq!(replacement.sequence, first.sequence);
        assert_eq!(journal.replay_after(0).events.len(), 1);
    }

    #[test]
    fn ansi_projector_handles_sequences_split_across_chunks() {
        let mut projector = PlainTextProjector::default();
        assert_eq!(projector.push(b"ok\x1b[3"), "ok");
        assert_eq!(projector.push(b"1mred\x1b[0m\rnext\n"), "red\nnext\n");
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
}

use super::{
    redact_browser_text, BrowserAnnotation, BrowserAttachmentRevision, BrowserWorkspaceKey,
    BrowserWorkspaceSnapshot,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

pub const MAX_BROWSER_ATTACHMENT_PREAMBLE_BYTES: usize = 2_048;
const MAX_BROWSER_ATTACHMENT_TOMBSTONES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserPromptInput<'a> {
    Text(&'a str),
    RawBytes(&'a [u8]),
    Paste(&'a str),
}

pub fn browser_input_opens_prompt_boundary(input: BrowserPromptInput<'_>) -> bool {
    match input {
        BrowserPromptInput::Text(text) => prompt_text_opens_boundary(text),
        BrowserPromptInput::RawBytes(bytes) => std::str::from_utf8(bytes)
            .ok()
            .is_some_and(prompt_text_opens_boundary),
        BrowserPromptInput::Paste(text) => !text.is_empty(),
    }
}

fn prompt_text_opens_boundary(text: &str) -> bool {
    !text.is_empty()
        && text.chars().all(|character| {
            matches!(character, ' ' | '\r' | '\n')
                || (!character.is_control() && !character.is_whitespace())
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserAttachmentError {
    StaleBinding,
    StaleReservation,
}

impl fmt::Display for BrowserAttachmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleBinding => formatter.write_str("browser attachment session was replaced"),
            Self::StaleReservation => {
                formatter.write_str("browser attachment reservation is no longer active")
            }
        }
    }
}

impl std::error::Error for BrowserAttachmentError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserAttachmentSessionBinding {
    pub session_id: String,
    pub workspace_key: BrowserWorkspaceKey,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserAttachmentReservation {
    session_id: String,
    workspace_key: BrowserWorkspaceKey,
    binding_generation: u64,
    annotation_ids: Vec<String>,
    preamble: String,
    reservation_id: u64,
}

impl BrowserAttachmentReservation {
    pub fn workspace_key(&self) -> &BrowserWorkspaceKey {
        &self.workspace_key
    }

    pub fn annotation_ids(&self) -> &[String] {
        &self.annotation_ids
    }

    pub fn preamble(&self) -> &str {
        &self.preamble
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserAttachmentProjection {
    pub workspace_key: BrowserWorkspaceKey,
    pub revision: BrowserAttachmentRevision,
    pub pending_annotation_ids: Vec<String>,
    pub pending_annotations: Vec<BrowserAnnotation>,
}

#[derive(Debug, Clone)]
struct ActiveReservation {
    reservation_id: u64,
    session_id: String,
    binding_generation: u64,
    annotation_ids: Vec<String>,
}

#[derive(Debug, Clone)]
struct AttachmentTombstone {
    annotation_id: String,
}

#[derive(Debug, Default)]
struct AttachmentWorkspaceState {
    revision: BrowserAttachmentRevision,
    pending_order: Vec<String>,
    annotations: HashMap<String, BrowserAnnotation>,
    tombstones: VecDeque<AttachmentTombstone>,
    active_reservation: Option<ActiveReservation>,
}

impl AttachmentWorkspaceState {
    fn has_tombstone(&self, annotation_id: &str) -> bool {
        self.tombstones
            .iter()
            .any(|tombstone| tombstone.annotation_id == annotation_id)
    }

    fn advance_revision(&mut self) -> BrowserAttachmentRevision {
        self.revision.0 = self.revision.0.saturating_add(1);
        self.revision
    }

    fn add_tombstone(&mut self, annotation_id: String) {
        if self.has_tombstone(&annotation_id) {
            return;
        }
        while self.tombstones.len() >= MAX_BROWSER_ATTACHMENT_TOMBSTONES {
            self.tombstones.pop_front();
        }
        self.tombstones
            .push_back(AttachmentTombstone { annotation_id });
    }
}

#[derive(Debug, Default)]
struct BrowserAttachmentBrokerState {
    next_generation: u64,
    next_reservation_id: u64,
    bindings: HashMap<String, BrowserAttachmentSessionBinding>,
    workspaces: HashMap<BrowserWorkspaceKey, AttachmentWorkspaceState>,
    dirty_workspaces: HashSet<BrowserWorkspaceKey>,
}

#[derive(Debug, Clone, Default)]
pub struct BrowserAttachmentBroker {
    inner: Arc<Mutex<BrowserAttachmentBrokerState>>,
}

impl BrowserAttachmentBroker {
    fn lock(&self) -> MutexGuard<'_, BrowserAttachmentBrokerState> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn observe_workspace(
        &self,
        workspace_key: BrowserWorkspaceKey,
        snapshot: &BrowserWorkspaceSnapshot,
    ) -> BrowserAttachmentProjection {
        let mut broker = self.lock();
        let workspace = broker.workspaces.entry(workspace_key.clone()).or_default();
        let before = projection_for(&workspace_key, workspace);

        for annotation in &snapshot.annotations {
            workspace
                .annotations
                .insert(annotation.id.clone(), annotation.clone());
        }

        // The broker owns the live pending set. A host/AppState snapshot may
        // have been produced before an attachment commit or detach, so its
        // absence is never evidence that a live ID should be removed.  Only
        // explicit broker transactions remove IDs; snapshots can contribute
        // genuinely new IDs and advance the persisted revision.
        let incoming_revision = snapshot.pending_annotation_revision;
        let previous_revision = workspace.revision;
        if incoming_revision > workspace.revision {
            workspace.revision = incoming_revision;
        }

        let mut added_pending_annotation = false;
        for annotation_id in &snapshot.pending_annotation_ids {
            if workspace.has_tombstone(annotation_id)
                || !workspace.annotations.contains_key(annotation_id)
                || workspace
                    .pending_order
                    .iter()
                    .any(|pending| pending == annotation_id)
            {
                continue;
            }
            workspace.pending_order.push(annotation_id.clone());
            added_pending_annotation = true;
        }

        // An equal or stale snapshot can still contain an annotation created
        // concurrently. Preserve monotonicity while making that addition
        // visible to consumers that use the attachment revision as a cursor.
        if added_pending_annotation && incoming_revision <= previous_revision {
            workspace.advance_revision();
        }

        let after = projection_for(&workspace_key, workspace);
        if after != before {
            broker.dirty_workspaces.insert(workspace_key);
        }
        after
    }

    pub fn bind_session(
        &self,
        session_id: impl Into<String>,
        workspace_key: BrowserWorkspaceKey,
    ) -> BrowserAttachmentSessionBinding {
        let session_id = session_id.into();
        let mut broker = self.lock();
        broker.next_generation = broker.next_generation.saturating_add(1);
        let generation = broker.next_generation;
        // One PTY session owns a browser workspace at a time.  Rebinding the
        // same session to another workspace *or* a new session to this
        // workspace must invalidate every prior generation before a stale
        // reservation can commit.
        let replaced_bindings = broker
            .bindings
            .values()
            .filter(|binding| {
                binding.session_id == session_id || binding.workspace_key == workspace_key
            })
            .cloned()
            .collect::<Vec<_>>();
        for previous in replaced_bindings {
            broker.bindings.remove(&previous.session_id);
            if let Some(workspace) = broker.workspaces.get_mut(&previous.workspace_key) {
                if workspace.active_reservation.as_ref().is_some_and(|active| {
                    active.session_id == previous.session_id
                        && active.binding_generation == previous.generation
                }) {
                    workspace.active_reservation = None;
                }
            }
        }
        let binding = BrowserAttachmentSessionBinding {
            session_id: session_id.clone(),
            workspace_key,
            generation,
        };
        broker.bindings.insert(session_id, binding.clone());
        binding
    }

    pub fn binding(&self, session_id: &str) -> Option<BrowserAttachmentSessionBinding> {
        self.lock().bindings.get(session_id).cloned()
    }

    pub fn unbind_if_matches(&self, binding: &BrowserAttachmentSessionBinding) -> bool {
        let mut broker = self.lock();
        let matches = broker
            .bindings
            .get(&binding.session_id)
            .is_some_and(|current| current == binding);
        if !matches {
            return false;
        }
        broker.bindings.remove(&binding.session_id);
        if let Some(workspace) = broker.workspaces.get_mut(&binding.workspace_key) {
            if workspace.active_reservation.as_ref().is_some_and(|active| {
                active.session_id == binding.session_id
                    && active.binding_generation == binding.generation
            }) {
                workspace.active_reservation = None;
            }
        }
        true
    }

    pub fn reserve_for_input(
        &self,
        session_id: &str,
        input: BrowserPromptInput<'_>,
    ) -> Option<BrowserAttachmentReservation> {
        if !browser_input_opens_prompt_boundary(input) {
            return None;
        }
        let mut broker = self.lock();
        let binding = broker.bindings.get(session_id)?.clone();
        let (annotation_ids, annotations) = {
            let workspace = broker.workspaces.get(&binding.workspace_key)?;
            if workspace.active_reservation.is_some() || workspace.pending_order.is_empty() {
                return None;
            }
            let annotation_ids = workspace.pending_order.clone();
            let annotations = annotation_ids
                .iter()
                .filter_map(|id| workspace.annotations.get(id).cloned())
                .collect::<Vec<_>>();
            (annotation_ids, annotations)
        };
        broker.next_reservation_id = broker.next_reservation_id.saturating_add(1);
        let reservation_id = broker.next_reservation_id;
        broker
            .workspaces
            .get_mut(&binding.workspace_key)?
            .active_reservation = Some(ActiveReservation {
            reservation_id,
            session_id: binding.session_id.clone(),
            binding_generation: binding.generation,
            annotation_ids: annotation_ids.clone(),
        });
        Some(BrowserAttachmentReservation {
            session_id: binding.session_id,
            workspace_key: binding.workspace_key,
            binding_generation: binding.generation,
            annotation_ids,
            preamble: build_attachment_preamble(&annotations),
            reservation_id,
        })
    }

    pub fn commit(
        &self,
        reservation: BrowserAttachmentReservation,
    ) -> Result<BrowserAttachmentProjection, BrowserAttachmentError> {
        let mut broker = self.lock();
        if !broker
            .bindings
            .get(&reservation.session_id)
            .is_some_and(|binding| {
                binding.workspace_key == reservation.workspace_key
                    && binding.generation == reservation.binding_generation
            })
        {
            return Err(BrowserAttachmentError::StaleBinding);
        }
        let workspace = broker
            .workspaces
            .get_mut(&reservation.workspace_key)
            .ok_or(BrowserAttachmentError::StaleReservation)?;
        if !workspace.active_reservation.as_ref().is_some_and(|active| {
            active.reservation_id == reservation.reservation_id
                && active.session_id == reservation.session_id
                && active.binding_generation == reservation.binding_generation
        }) {
            return Err(BrowserAttachmentError::StaleReservation);
        }
        let exact_annotation_ids = workspace
            .active_reservation
            .as_ref()
            .map(|active| active.annotation_ids.clone())
            .unwrap_or_default();
        workspace.active_reservation = None;
        let previous_len = workspace.pending_order.len();
        workspace.pending_order.retain(|pending| {
            !exact_annotation_ids
                .iter()
                .any(|reserved| reserved == pending)
        });
        if previous_len != workspace.pending_order.len() {
            workspace.advance_revision();
            for annotation_id in exact_annotation_ids {
                workspace.add_tombstone(annotation_id);
            }
        }
        let projection = projection_for(&reservation.workspace_key, workspace);
        broker
            .dirty_workspaces
            .insert(reservation.workspace_key.clone());
        Ok(projection)
    }

    pub fn rollback(
        &self,
        reservation: BrowserAttachmentReservation,
    ) -> Result<(), BrowserAttachmentError> {
        let mut broker = self.lock();
        if !broker
            .bindings
            .get(&reservation.session_id)
            .is_some_and(|binding| {
                binding.workspace_key == reservation.workspace_key
                    && binding.generation == reservation.binding_generation
            })
        {
            return Err(BrowserAttachmentError::StaleBinding);
        }
        let workspace = broker
            .workspaces
            .get_mut(&reservation.workspace_key)
            .ok_or(BrowserAttachmentError::StaleReservation)?;
        if !workspace.active_reservation.as_ref().is_some_and(|active| {
            active.reservation_id == reservation.reservation_id
                && active.session_id == reservation.session_id
                && active.binding_generation == reservation.binding_generation
        }) {
            return Err(BrowserAttachmentError::StaleReservation);
        }
        workspace.active_reservation = None;
        Ok(())
    }

    pub fn detach(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        annotation_id: &str,
    ) -> BrowserAttachmentProjection {
        let mut broker = self.lock();
        let (projection, changed) = {
            let workspace = broker.workspaces.entry(workspace_key.clone()).or_default();
            let previous_len = workspace.pending_order.len();
            workspace
                .pending_order
                .retain(|pending| pending != annotation_id);
            let changed = previous_len != workspace.pending_order.len();
            if changed {
                workspace.advance_revision();
                workspace.add_tombstone(annotation_id.to_string());
            }
            (projection_for(workspace_key, workspace), changed)
        };
        if changed {
            broker.dirty_workspaces.insert(workspace_key.clone());
        }
        projection
    }

    pub fn projection(&self, workspace_key: &BrowserWorkspaceKey) -> BrowserAttachmentProjection {
        let broker = self.lock();
        broker
            .workspaces
            .get(workspace_key)
            .map(|workspace| projection_for(workspace_key, workspace))
            .unwrap_or_else(|| BrowserAttachmentProjection {
                workspace_key: workspace_key.clone(),
                revision: BrowserAttachmentRevision::default(),
                pending_annotation_ids: Vec::new(),
                pending_annotations: Vec::new(),
            })
    }

    pub fn overlay_snapshot(
        &self,
        workspace_key: &BrowserWorkspaceKey,
        snapshot: &mut BrowserWorkspaceSnapshot,
    ) -> bool {
        let projection = self.projection(workspace_key);
        let mut changed = snapshot.pending_annotation_revision != projection.revision
            || snapshot.pending_annotation_ids != projection.pending_annotation_ids;
        snapshot.pending_annotation_revision = projection.revision;
        snapshot.pending_annotation_ids = projection.pending_annotation_ids;
        for annotation in projection.pending_annotations {
            if snapshot
                .annotations
                .iter()
                .any(|existing| existing.id == annotation.id)
            {
                continue;
            }
            snapshot.annotations.push(annotation);
            changed = true;
        }
        changed
    }

    pub fn drain_dirty_projections(&self) -> Vec<BrowserAttachmentProjection> {
        let mut broker = self.lock();
        let mut keys = broker.dirty_workspaces.drain().collect::<Vec<_>>();
        keys.sort_by(|left, right| {
            (&left.project_id, &left.ai_tab_id).cmp(&(&right.project_id, &right.ai_tab_id))
        });
        keys.into_iter()
            .map(|workspace_key| {
                broker
                    .workspaces
                    .get(&workspace_key)
                    .map(|workspace| projection_for(&workspace_key, workspace))
                    .unwrap_or(BrowserAttachmentProjection {
                        workspace_key,
                        revision: BrowserAttachmentRevision::default(),
                        pending_annotation_ids: Vec::new(),
                        pending_annotations: Vec::new(),
                    })
            })
            .collect()
    }

    pub fn retire_workspace(&self, workspace_key: &BrowserWorkspaceKey) {
        let mut broker = self.lock();
        broker.workspaces.remove(workspace_key);
        broker.dirty_workspaces.remove(workspace_key);
        broker
            .bindings
            .retain(|_, binding| &binding.workspace_key != workspace_key);
    }
}

fn projection_for(
    workspace_key: &BrowserWorkspaceKey,
    workspace: &AttachmentWorkspaceState,
) -> BrowserAttachmentProjection {
    BrowserAttachmentProjection {
        workspace_key: workspace_key.clone(),
        revision: workspace.revision,
        pending_annotation_ids: workspace.pending_order.clone(),
        pending_annotations: workspace
            .pending_order
            .iter()
            .filter_map(|id| workspace.annotations.get(id).cloned())
            .collect(),
    }
}

fn build_attachment_preamble(annotations: &[BrowserAnnotation]) -> String {
    let mut preamble = String::from(
        "[DevManager browser annotations attached; call browser_annotations get for full details. ",
    );
    for (index, annotation) in annotations.iter().enumerate() {
        if index > 0 {
            preamble.push_str("; ");
        }
        preamble.push_str("id=");
        preamble.push_str(&compact_redacted(&annotation.id, 96));
        preamble.push_str(" comment=");
        preamble.push_str(&compact_redacted(&annotation.comment, 180));
        preamble.push_str(" url=");
        preamble.push_str(&compact_redacted(&annotation.url, 240));
    }
    const SUFFIX: &str = "] ";
    truncate_utf8_bytes(
        &mut preamble,
        MAX_BROWSER_ATTACHMENT_PREAMBLE_BYTES.saturating_sub(SUFFIX.len()),
    );
    preamble.push_str(SUFFIX);
    preamble
}

fn compact_redacted(value: &str, max_chars: usize) -> String {
    let mut compact = String::new();
    let mut previous_space = false;
    for character in redact_browser_text(value).chars() {
        if compact.chars().count() >= max_chars {
            break;
        }
        if character.is_control() || character.is_whitespace() {
            if !previous_space && !compact.is_empty() {
                compact.push(' ');
            }
            previous_space = true;
        } else {
            compact.push(character);
            previous_space = false;
        }
    }
    compact.trim().to_string()
}

fn truncate_utf8_bytes(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value.truncate(end);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::{
        BrowserAnnotation, BrowserAnnotationKind, BrowserAttachmentRevision, BrowserBounds,
        BrowserLocator, BrowserResourceId, BrowserRevision, BrowserViewport, BrowserWorkspaceKey,
        BrowserWorkspaceSnapshot, REDACTED_VALUE,
    };
    use std::collections::BTreeMap;

    fn key(project: &str, tab: &str) -> BrowserWorkspaceKey {
        BrowserWorkspaceKey::new(project, tab).unwrap()
    }

    fn annotation(id: &str, comment: &str, url: &str) -> BrowserAnnotation {
        BrowserAnnotation {
            id: id.to_string(),
            kind: BrowserAnnotationKind::Element,
            tab_id: "page".to_string(),
            anchor_revision: BrowserRevision(7),
            comment: comment.to_string(),
            url: url.to_string(),
            locator: BrowserLocator::default(),
            bounds: BrowserBounds {
                x: 1,
                y: 2,
                width: 30,
                height: 40,
            },
            viewport: BrowserViewport::default(),
            screenshot_resource: BrowserResourceId(format!("shot-{id}")),
            computed_styles: BTreeMap::new(),
            resolved: false,
        }
    }

    fn snapshot_with(annotation: BrowserAnnotation) -> BrowserWorkspaceSnapshot {
        let mut snapshot = BrowserWorkspaceSnapshot::default();
        snapshot.save_annotation(annotation).unwrap();
        snapshot
    }

    #[test]
    fn prompt_boundary_classifier_accepts_only_user_prompt_content() {
        for input in [
            BrowserPromptInput::Text("hello"),
            BrowserPromptInput::Text("é"),
            BrowserPromptInput::Text(" "),
            BrowserPromptInput::Text("\r"),
            BrowserPromptInput::Text("\n"),
            BrowserPromptInput::RawBytes("hello é".as_bytes()),
            BrowserPromptInput::Paste("\t"),
        ] {
            assert!(browser_input_opens_prompt_boundary(input), "{input:?}");
        }

        for input in [
            BrowserPromptInput::Text(""),
            BrowserPromptInput::Text("\t"),
            BrowserPromptInput::Text("\u{1b}[A"),
            BrowserPromptInput::Text("\u{7f}"),
            BrowserPromptInput::Text("\u{80}"),
            BrowserPromptInput::Text("\u{a0}"),
            BrowserPromptInput::Text("\u{2028}"),
            BrowserPromptInput::RawBytes(&[0xff]),
            BrowserPromptInput::RawBytes(b"\x1b[M !!"),
            BrowserPromptInput::RawBytes(b""),
            BrowserPromptInput::Paste(""),
        ] {
            assert!(!browser_input_opens_prompt_boundary(input), "{input:?}");
        }
    }

    #[test]
    fn reserved_preamble_is_compact_bounded_control_free_and_redacted() {
        let workspace_key = key("project", "conversation");
        let mut snapshot = BrowserWorkspaceSnapshot::default();
        snapshot
            .save_annotation(annotation(
                "ann-1",
                &format!("review this\n{} password=hunter2", "x".repeat(4_000)),
                "https://example.test/path?token=top-secret&safe=yes",
            ))
            .unwrap();
        for index in 2..20 {
            snapshot
                .save_annotation(annotation(
                    &format!("ann-{index}"),
                    &"long annotation context ".repeat(20),
                    &format!("https://example.test/{index}?token=also-secret"),
                ))
                .unwrap();
        }
        let broker = BrowserAttachmentBroker::default();
        broker.observe_workspace(workspace_key.clone(), &snapshot);
        broker.bind_session("session", workspace_key);

        let reservation = broker
            .reserve_for_input("session", BrowserPromptInput::Text("go"))
            .unwrap();

        assert!(reservation.preamble.len() <= MAX_BROWSER_ATTACHMENT_PREAMBLE_BYTES);
        assert!(!reservation.preamble.contains('\n'));
        assert!(!reservation.preamble.contains('\r'));
        assert!(!reservation.preamble.chars().any(char::is_control));
        assert!(reservation.preamble.contains("ann-1"));
        assert!(reservation.preamble.contains("browser_annotations"));
        assert!(reservation.preamble.ends_with(' '));
        assert!(reservation.preamble.contains(REDACTED_VALUE));
        assert!(!reservation.preamble.contains("hunter2"));
        assert!(!reservation.preamble.contains("top-secret"));
        assert!(!reservation.preamble.contains("cssSelectors"));
    }

    #[test]
    fn reservation_commit_is_exact_and_concurrent_additions_survive() {
        let workspace_key = key("project", "conversation");
        let mut snapshot = snapshot_with(annotation("ann-1", "first", "https://one.test"));
        let broker = BrowserAttachmentBroker::default();
        broker.observe_workspace(workspace_key.clone(), &snapshot);
        broker.bind_session("session", workspace_key.clone());

        let mut reservation = broker
            .reserve_for_input("session", BrowserPromptInput::Text("prompt"))
            .unwrap();
        assert_eq!(reservation.annotation_ids, vec!["ann-1"]);
        assert!(broker
            .reserve_for_input("session", BrowserPromptInput::Paste("second"))
            .is_none());

        snapshot
            .save_annotation(annotation("ann-2", "second", "https://two.test"))
            .unwrap();
        broker.observe_workspace(workspace_key.clone(), &snapshot);
        // The broker's active record is the immutable source of the exact
        // claim, even if an in-module caller corrupts its transport token.
        reservation.annotation_ids = vec!["ann-2".to_string()];
        let projection = broker.commit(reservation).unwrap();

        assert_eq!(projection.pending_annotation_ids, vec!["ann-2"]);
        assert_eq!(projection.revision, BrowserAttachmentRevision(3));
    }

    #[test]
    fn rollback_retries_the_same_pending_annotation() {
        let workspace_key = key("project", "conversation");
        let snapshot = snapshot_with(annotation("ann-1", "first", "https://one.test"));
        let broker = BrowserAttachmentBroker::default();
        broker.observe_workspace(workspace_key.clone(), &snapshot);
        broker.bind_session("session", workspace_key);

        let first = broker
            .reserve_for_input("session", BrowserPromptInput::Text("prompt"))
            .unwrap();
        broker.rollback(first).unwrap();
        let retry = broker
            .reserve_for_input("session", BrowserPromptInput::Text("prompt"))
            .unwrap();

        assert_eq!(retry.annotation_ids, vec!["ann-1"]);
    }

    #[test]
    fn stale_snapshots_cannot_resurrect_delivered_or_detached_ids() {
        let workspace_key = key("project", "conversation");
        let snapshot = snapshot_with(annotation("ann-1", "first", "https://one.test"));
        let broker = BrowserAttachmentBroker::default();
        broker.observe_workspace(workspace_key.clone(), &snapshot);
        broker.bind_session("session", workspace_key.clone());

        let reservation = broker
            .reserve_for_input("session", BrowserPromptInput::Text("prompt"))
            .unwrap();
        broker.commit(reservation).unwrap();
        broker.observe_workspace(workspace_key.clone(), &snapshot);
        assert!(broker
            .projection(&workspace_key)
            .pending_annotation_ids
            .is_empty());

        let mut second = snapshot.clone();
        second
            .save_annotation(annotation("ann-2", "second", "https://two.test"))
            .unwrap();
        broker.observe_workspace(workspace_key.clone(), &second);
        broker.detach(&workspace_key, "ann-2");
        broker.observe_workspace(workspace_key.clone(), &second);
        assert!(broker
            .projection(&workspace_key)
            .pending_annotation_ids
            .is_empty());
    }

    #[test]
    fn snapshot_observation_unions_concurrent_additions_and_keeps_revision_monotonic() {
        let workspace_key = key("project", "conversation");
        let broker = BrowserAttachmentBroker::default();
        broker.observe_workspace(
            workspace_key.clone(),
            &snapshot_with(annotation("ann-1", "first", "https://one.test")),
        );

        let mut independently_newer =
            snapshot_with(annotation("ann-2", "second", "https://two.test"));
        independently_newer.pending_annotation_revision = BrowserAttachmentRevision(2);
        let projection = broker.observe_workspace(workspace_key.clone(), &independently_newer);
        assert_eq!(projection.pending_annotation_ids, vec!["ann-1", "ann-2"]);
        assert_eq!(projection.revision, BrowserAttachmentRevision(2));

        let independently_older = snapshot_with(annotation("ann-3", "third", "https://three.test"));
        let projection = broker.observe_workspace(workspace_key, &independently_older);
        assert_eq!(
            projection.pending_annotation_ids,
            vec!["ann-1", "ann-2", "ann-3"]
        );
        assert_eq!(projection.revision, BrowserAttachmentRevision(3));
    }

    #[test]
    fn bindings_fence_replaced_sessions_and_isolate_workspaces() {
        let first_key = key("project", "one");
        let second_key = key("project", "two");
        let broker = BrowserAttachmentBroker::default();
        broker.observe_workspace(
            first_key.clone(),
            &snapshot_with(annotation("ann-1", "first", "https://one.test")),
        );
        broker.observe_workspace(
            second_key.clone(),
            &snapshot_with(annotation("ann-2", "second", "https://two.test")),
        );
        let old_binding = broker.bind_session("session", first_key.clone());
        let old_reservation = broker
            .reserve_for_input("session", BrowserPromptInput::Text("prompt"))
            .unwrap();
        let replacement = broker.bind_session("session", second_key.clone());

        assert!(replacement.generation > old_binding.generation);
        assert_eq!(
            broker.commit(old_reservation),
            Err(BrowserAttachmentError::StaleBinding)
        );
        let reservation = broker
            .reserve_for_input("session", BrowserPromptInput::Text("prompt"))
            .unwrap();
        assert_eq!(reservation.workspace_key, second_key);
        assert_eq!(reservation.annotation_ids, vec!["ann-2"]);
        assert_eq!(
            broker.projection(&first_key).pending_annotation_ids,
            vec!["ann-1"]
        );
    }

    #[test]
    fn a_new_session_for_the_same_workspace_fences_the_old_session() {
        let workspace_key = key("project", "conversation");
        let broker = BrowserAttachmentBroker::default();
        broker.observe_workspace(
            workspace_key.clone(),
            &snapshot_with(annotation("ann-1", "first", "https://one.test")),
        );
        broker.bind_session("old-session", workspace_key.clone());
        let old_reservation = broker
            .reserve_for_input("old-session", BrowserPromptInput::Text("prompt"))
            .unwrap();

        broker.bind_session("new-session", workspace_key);

        assert_eq!(
            broker.commit(old_reservation),
            Err(BrowserAttachmentError::StaleBinding)
        );
        assert!(broker
            .reserve_for_input("new-session", BrowserPromptInput::Text("prompt"))
            .is_some());
        assert!(broker
            .reserve_for_input("old-session", BrowserPromptInput::Text("prompt"))
            .is_none());
    }

    #[test]
    fn pending_revision_is_serde_defaulted_and_does_not_advance_page_revision() {
        let mut snapshot: BrowserWorkspaceSnapshot = serde_json::from_str("{}").unwrap();
        let page_revision = BrowserRevision(41);
        snapshot.revision = page_revision;

        snapshot
            .save_annotation(annotation("ann-1", "first", "https://one.test"))
            .unwrap();
        assert_eq!(snapshot.revision, page_revision);
        assert_eq!(
            snapshot.pending_annotation_revision,
            BrowserAttachmentRevision(1)
        );
        assert!(snapshot.remove_pending_annotation("ann-1"));
        assert_eq!(
            snapshot.pending_annotation_revision,
            BrowserAttachmentRevision(2)
        );
        assert_eq!(snapshot.revision, page_revision);
    }
}

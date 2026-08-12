//! Transport-neutral browser projection consumed by desktop and web viewers.
//!
//! Screenshot pixels are never a local DOM. Frames emit only while a viewer is
//! subscribed, under an explicit FPS/byte budget. Metadata is immediate.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::domain::id::{BrowserTabId, ClientId, TaskId};
use crate::protocol::{
    BrowserBoundsEpoch, BrowserFocusEpoch, BrowserFrameKind, BrowserInteractionMode,
    BrowserProjectionEnvelope, BrowserProjectionFrame, BrowserProjectionMeta,
    BrowserRemoteInput, BrowserRuntimeGeneration, BrowserSecurityState, BrowserTabProjection,
    StreamPayloadKind, MAX_BROWSER_PROJECTION_BYTES_PER_SECOND, MAX_BROWSER_PROJECTION_FPS,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserProjectionError {
    StaleGeneration,
    StaleFrame,
    StaleBoundsEpoch,
    StaleFocusEpoch,
    NotSubscribed,
    BudgetExceeded,
    ApprovalConsumed,
    InvalidRequest,
}

impl std::fmt::Display for BrowserProjectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleGeneration => write!(f, "browser projection generation is stale"),
            Self::StaleFrame => write!(f, "browser projection frame is stale"),
            Self::StaleBoundsEpoch => write!(f, "browser projection bounds epoch is stale"),
            Self::StaleFocusEpoch => write!(f, "browser projection focus epoch is stale"),
            Self::NotSubscribed => write!(f, "browser projection has no frame subscriber"),
            Self::BudgetExceeded => write!(f, "browser projection budget exceeded"),
            Self::ApprovalConsumed => write!(f, "browser approval already answered"),
            Self::InvalidRequest => write!(f, "browser projection request is invalid"),
        }
    }
}

impl std::error::Error for BrowserProjectionError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserProjectionEvent {
    Metadata(BrowserProjectionMeta),
    Frame(BrowserProjectionFrame),
    Approval {
        request_id: String,
        allowed: bool,
    },
    Resync(BrowserProjectionMeta),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserProjectionSession {
    meta: BrowserProjectionMeta,
    subscribers: BTreeSet<ClientId>,
    last_frame_bytes: usize,
    frames_this_second: u32,
    bytes_this_second: u64,
    budget_window: Duration,
    pending_approval: Option<String>,
    approval_answer: Option<bool>,
}

impl BrowserProjectionSession {
    pub fn new(meta: BrowserProjectionMeta) -> Result<Self, BrowserProjectionError> {
        meta.validate()
            .map_err(|_| BrowserProjectionError::InvalidRequest)?;
        Ok(Self {
            meta,
            subscribers: BTreeSet::new(),
            last_frame_bytes: 0,
            frames_this_second: 0,
            bytes_this_second: 0,
            budget_window: Duration::from_secs(1),
            pending_approval: None,
            approval_answer: None,
        })
    }

    pub fn max_fps() -> u32 {
        MAX_BROWSER_PROJECTION_FPS
    }

    pub fn max_bytes_per_second() -> u64 {
        MAX_BROWSER_PROJECTION_BYTES_PER_SECOND
    }

    pub fn frame_payload_kind() -> StreamPayloadKind {
        StreamPayloadKind::BROWSER_FRAME
    }

    pub fn pixels_are_local_dom() -> bool {
        false
    }

    pub fn meta(&self) -> &BrowserProjectionMeta {
        &self.meta
    }

    pub fn subscribe(&mut self, client_id: ClientId) {
        self.subscribers.insert(client_id);
    }

    pub fn unsubscribe(&mut self, client_id: ClientId) {
        self.subscribers.remove(&client_id);
    }

    pub fn has_subscriber(&self) -> bool {
        !self.subscribers.is_empty()
    }

    pub fn emit_metadata(
        &mut self,
        generation: u64,
        progress: Option<String>,
    ) -> Result<BrowserProjectionEvent, BrowserProjectionError> {
        self.require_generation(generation)?;
        self.meta.progress = progress;
        self.meta
            .validate()
            .map_err(|_| BrowserProjectionError::InvalidRequest)?;
        Ok(BrowserProjectionEvent::Metadata(self.meta.clone()))
    }

    pub fn replace_tabs(
        &mut self,
        generation: u64,
        tabs: Vec<BrowserTabProjection>,
        selected_tab_id: Option<BrowserTabId>,
    ) -> Result<BrowserProjectionEvent, BrowserProjectionError> {
        self.require_generation(generation)?;
        self.meta.tabs = tabs;
        self.meta.selected_tab_id = selected_tab_id;
        self.meta
            .validate()
            .map_err(|_| BrowserProjectionError::InvalidRequest)?;
        Ok(BrowserProjectionEvent::Metadata(self.meta.clone()))
    }

    pub fn maybe_emit_frame(
        &mut self,
        generation: u64,
        kind: BrowserFrameKind,
        bytes: Vec<u8>,
    ) -> Result<BrowserProjectionEvent, BrowserProjectionError> {
        self.require_generation(generation)?;
        if self.subscribers.is_empty() {
            return Err(BrowserProjectionError::NotSubscribed);
        }
        if kind == BrowserFrameKind::Full
            && self.last_frame_bytes == bytes.len()
            && self.frames_this_second > 0
        {
            return Err(BrowserProjectionError::BudgetExceeded);
        }
        if self.frames_this_second >= MAX_BROWSER_PROJECTION_FPS
            || self
                .bytes_this_second
                .saturating_add(bytes.len() as u64)
                > MAX_BROWSER_PROJECTION_BYTES_PER_SECOND
        {
            return Err(BrowserProjectionError::BudgetExceeded);
        }
        let next_frame = self
            .meta
            .frame_id
            .checked_add(1)
            .ok_or(BrowserProjectionError::InvalidRequest)?;
        let frame = BrowserProjectionFrame::new(
            next_frame,
            kind,
            self.meta.generation,
            self.meta.bounds_epoch,
            bytes,
        )
        .map_err(|_| BrowserProjectionError::InvalidRequest)?;
        self.meta.frame_id = next_frame;
        self.frames_this_second = self.frames_this_second.saturating_add(1);
        self.bytes_this_second = self
            .bytes_this_second
            .saturating_add(frame.bytes.len() as u64);
        self.last_frame_bytes = frame.bytes.len();
        let _ = self.budget_window;
        Ok(BrowserProjectionEvent::Frame(frame))
    }

    pub fn map_input(
        &self,
        input: &BrowserRemoteInput,
    ) -> Result<(i32, i32), BrowserProjectionError> {
        input
            .validate()
            .map_err(|_| BrowserProjectionError::InvalidRequest)?;
        self.require_generation(input.generation.value())?;
        if input.frame_id != self.meta.frame_id {
            return Err(BrowserProjectionError::StaleFrame);
        }
        if input.bounds_epoch != self.meta.bounds_epoch {
            return Err(BrowserProjectionError::StaleBoundsEpoch);
        }
        if input.focus_epoch != self.meta.focus_epoch {
            return Err(BrowserProjectionError::StaleFocusEpoch);
        }
        if self.meta.interaction_mode != BrowserInteractionMode::Interact {
            return Err(BrowserProjectionError::InvalidRequest);
        }
        input
            .mapped_point()
            .map_err(|_| BrowserProjectionError::InvalidRequest)
    }

    pub fn offer_approval(&mut self, request_id: impl Into<String>) -> Result<(), BrowserProjectionError> {
        let request_id = request_id.into();
        if request_id.is_empty() {
            return Err(BrowserProjectionError::InvalidRequest);
        }
        self.pending_approval = Some(request_id);
        self.approval_answer = None;
        Ok(())
    }

    pub fn decide_approval(
        &mut self,
        request_id: &str,
        allowed: bool,
    ) -> Result<BrowserProjectionEvent, BrowserProjectionError> {
        if self.pending_approval.as_deref() != Some(request_id) {
            return Err(BrowserProjectionError::InvalidRequest);
        }
        if self.approval_answer.is_some() {
            return Err(BrowserProjectionError::ApprovalConsumed);
        }
        self.approval_answer = Some(allowed);
        Ok(BrowserProjectionEvent::Approval {
            request_id: request_id.to_string(),
            allowed,
        })
    }

    pub fn resync(
        &mut self,
        generation: u64,
    ) -> Result<BrowserProjectionEvent, BrowserProjectionError> {
        self.require_generation(generation)?;
        self.frames_this_second = 0;
        self.bytes_this_second = 0;
        Ok(BrowserProjectionEvent::Resync(self.meta.clone()))
    }

    pub fn envelope_for(
        &self,
        payload: &[u8],
    ) -> Result<BrowserProjectionEnvelope, BrowserProjectionError> {
        BrowserProjectionEnvelope::new(
            self.meta.generation.value(),
            self.meta.bounds_epoch.value(),
            self.meta.focus_epoch.value(),
            self.meta.frame_id,
            StreamPayloadKind::BROWSER_FRAME.get(),
            payload,
        )
        .map_err(|_| BrowserProjectionError::InvalidRequest)
    }

    pub fn tab_security(url: &str) -> BrowserSecurityState {
        if url.starts_with("https://") {
            BrowserSecurityState::Secure
        } else if url.starts_with("http://") {
            BrowserSecurityState::Insecure
        } else {
            BrowserSecurityState::Unknown
        }
    }

    fn require_generation(&self, generation: u64) -> Result<(), BrowserProjectionError> {
        if generation == 0 || generation != self.meta.generation.value() {
            return Err(BrowserProjectionError::StaleGeneration);
        }
        Ok(())
    }
}

pub fn projection_meta(
    task_id: TaskId,
    context_id: crate::domain::id::BrowserContextId,
    generation: u64,
    tabs: Vec<BrowserTabProjection>,
    selected_tab_id: Option<BrowserTabId>,
) -> Result<BrowserProjectionMeta, BrowserProjectionError> {
    let meta = BrowserProjectionMeta {
        task_id,
        context_id,
        generation: BrowserRuntimeGeneration::new(generation)
            .map_err(|_| BrowserProjectionError::StaleGeneration)?,
        bounds_epoch: BrowserBoundsEpoch::initial(),
        focus_epoch: BrowserFocusEpoch::initial(),
        frame_id: 1,
        selected_tab_id,
        tabs,
        progress: None,
        interaction_mode: BrowserInteractionMode::Observe,
    };
    meta.validate()
        .map_err(|_| BrowserProjectionError::InvalidRequest)?;
    Ok(meta)
}

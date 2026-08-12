//! Task-owned context dock: native chrome plus an exact host surface slot.
//!
//! The existing terminal remains a sibling. This dock never injects Web chrome
//! and fails closed on a stale generation.

use crate::browser::surface::{
    BrowserDockError, BrowserDockFocusTarget, BrowserDockGesture, BrowserDockSurface,
    BrowserPointerDisposition,
};
use crate::domain::id::{BrowserTabId, TaskId};
use crate::protocol::{BrowserAttachRequest, BrowserPhysicalBounds, BrowserProjectionMeta};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserContextDockError {
    StaleGeneration,
    TerminalPreserved,
    Surface(BrowserDockError),
}

impl std::fmt::Display for BrowserContextDockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleGeneration => write!(f, "context dock generation is stale"),
            Self::TerminalPreserved => write!(f, "native terminal sibling must remain"),
            Self::Surface(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for BrowserContextDockError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextDockFocus {
    Terminal,
    BrowserChrome,
    BrowserPage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextDockLayout {
    pub terminal_width: u32,
    pub browser_width: u32,
}

impl ContextDockLayout {
    pub fn split(total: u32, browser_percent: u32) -> Result<Self, BrowserContextDockError> {
        if total == 0 || !(25..=75).contains(&browser_percent) {
            return Err(BrowserContextDockError::Surface(
                BrowserDockError::InvalidRequest,
            ));
        }
        let browser_width = total.saturating_mul(browser_percent) / 100;
        Ok(Self {
            terminal_width: total.saturating_sub(browser_width),
            browser_width,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserContextDock {
    surface: BrowserDockSurface,
    projection: BrowserProjectionMeta,
    layout: ContextDockLayout,
    terminal_present: bool,
}

impl BrowserContextDock {
    pub fn open(
        surface: BrowserDockSurface,
        projection: BrowserProjectionMeta,
        layout: ContextDockLayout,
    ) -> Result<Self, BrowserContextDockError> {
        if surface.task_id() != projection.task_id {
            return Err(BrowserContextDockError::Surface(
                BrowserDockError::CrossTask,
            ));
        }
        if surface.generation() != projection.generation.value() {
            return Err(BrowserContextDockError::StaleGeneration);
        }
        Ok(Self {
            surface,
            projection,
            layout,
            terminal_present: true,
        })
    }

    pub fn uses_web_chrome() -> bool {
        BrowserDockSurface::uses_web_chrome()
    }

    pub fn terminal_present(&self) -> bool {
        self.terminal_present
    }

    pub fn task_id(&self) -> TaskId {
        self.surface.task_id()
    }

    pub fn surface(&self) -> &BrowserDockSurface {
        &self.surface
    }

    pub fn projection(&self) -> &BrowserProjectionMeta {
        &self.projection
    }

    pub fn layout(&self) -> ContextDockLayout {
        self.layout
    }

    pub fn focus(&self) -> ContextDockFocus {
        match self.surface.focus_target() {
            BrowserDockFocusTarget::Terminal => ContextDockFocus::Terminal,
            BrowserDockFocusTarget::BrowserChrome => ContextDockFocus::BrowserChrome,
            BrowserDockFocusTarget::BrowserPage => ContextDockFocus::BrowserPage,
        }
    }

    pub fn attach(&mut self, request: BrowserAttachRequest) -> Result<(), BrowserContextDockError> {
        self.surface
            .attach(request)
            .map_err(BrowserContextDockError::Surface)
    }

    pub fn detach(&mut self) -> Result<(), BrowserContextDockError> {
        self.surface
            .detach(false)
            .map_err(BrowserContextDockError::Surface)
    }

    pub fn resize(
        &mut self,
        generation: u64,
        layout: ContextDockLayout,
        bounds: BrowserPhysicalBounds,
    ) -> Result<u64, BrowserContextDockError> {
        if !self.terminal_present {
            return Err(BrowserContextDockError::TerminalPreserved);
        }
        self.surface
            .hide_for_layout()
            .map_err(BrowserContextDockError::Surface)?;
        let epoch = self
            .surface
            .apply_bounds(generation, bounds, self.surface.bounds_epoch())
            .map_err(BrowserContextDockError::Surface)?;
        self.layout = layout;
        Ok(epoch)
    }

    pub fn switch_task(
        &mut self,
        incoming: BrowserAttachRequest,
        projection: BrowserProjectionMeta,
    ) -> Result<(), BrowserContextDockError> {
        self.surface
            .switch_task(incoming)
            .map_err(BrowserContextDockError::Surface)?;
        if self.surface.generation() != projection.generation.value() {
            return Err(BrowserContextDockError::StaleGeneration);
        }
        self.projection = projection;
        Ok(())
    }

    pub fn select_tab(&mut self, tab_id: BrowserTabId) -> Result<(), BrowserContextDockError> {
        self.surface
            .select_tab(tab_id)
            .map_err(BrowserContextDockError::Surface)?;
        self.projection.selected_tab_id = Some(tab_id);
        Ok(())
    }

    pub fn focus_terminal(&mut self) -> Result<(), BrowserContextDockError> {
        self.surface
            .focus_terminal()
            .map_err(BrowserContextDockError::Surface)
    }

    pub fn classify(&self, gesture: BrowserDockGesture) -> BrowserPointerDisposition {
        self.surface.classify_gesture(gesture)
    }

    pub fn admit_page_input(
        &self,
        generation: u64,
        gesture: BrowserDockGesture,
    ) -> Result<(), BrowserContextDockError> {
        self.surface
            .admit_page_input(
                generation,
                self.surface.bounds_epoch(),
                self.surface.focus_epoch(),
                gesture,
            )
            .map_err(BrowserContextDockError::Surface)
    }

    pub fn arm_page_input(&mut self) -> Result<(), BrowserContextDockError> {
        self.surface
            .arm_page_input_after_gesture()
            .map_err(BrowserContextDockError::Surface)
    }
}

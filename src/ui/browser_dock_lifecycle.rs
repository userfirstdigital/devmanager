//! Pure planner for attaching the sole BrowserWebViewHost into the Browser dock.
//!
//! NativeShell executes the resulting plan through TaskCockpitShell +
//! BrowserWebViewHost. Missing identity or gateway fails closed with a visible
//! diagnostic — never with fake pixels.

use crate::browser::{BrowserBounds, BrowserNativeIdentity};
use crate::domain::id::{AgentSessionId, BrowserContextId, ResourceId, TaskId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserDockIdentity {
    pub task_id: TaskId,
    pub agent_session_id: AgentSessionId,
    pub context_id: BrowserContextId,
    pub resource_id: ResourceId,
    pub process_session_id: String,
}

impl BrowserDockIdentity {
    pub fn to_native_identity(&self) -> BrowserNativeIdentity {
        BrowserNativeIdentity::new(
            self.task_id,
            self.agent_session_id,
            self.context_id,
            self.resource_id,
        )
    }

    pub fn validate(&self) -> Result<(), BrowserDockLifecycleError> {
        if self.process_session_id.trim().is_empty() {
            return Err(BrowserDockLifecycleError::GatewayMissing);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserDockLifecycleError {
    IdentityIncomplete,
    GatewayMissing,
    ParentHwndMissing,
    BoundsMissing,
    HostUnavailable,
}

impl BrowserDockLifecycleError {
    pub fn message(self) -> &'static str {
        match self {
            Self::IdentityIncomplete => {
                "Browser identity is incomplete — task/agent/context/resource required."
            }
            Self::GatewayMissing => {
                "Browser gateway binding is absent — cannot attach real page pixels."
            }
            Self::ParentHwndMissing => "Browser parent HWND is unavailable.",
            Self::BoundsMissing => "Browser page bounds are not ready.",
            Self::HostUnavailable => "BrowserWebViewHost is unavailable.",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserDockSurfaceState {
    pub dock_expanded: bool,
    pub browser_tab_active: bool,
    pub host_available: bool,
    pub attached: bool,
    pub parent_hwnd: Option<u64>,
    pub bounds: Option<BrowserBounds>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserDockPlan {
    ShowDiagnostic(BrowserDockLifecycleError),
    Park,
    BindAndAttach {
        identity: BrowserDockIdentity,
        parent_hwnd: u64,
        bounds: BrowserBounds,
    },
    Resize {
        bounds: BrowserBounds,
    },
    Focus {
        focused: bool,
    },
    Idle,
}

pub fn plan_browser_dock(
    identity: Option<&BrowserDockIdentity>,
    surface: BrowserDockSurfaceState,
) -> BrowserDockPlan {
    if !surface.dock_expanded || !surface.browser_tab_active {
        return if surface.attached {
            BrowserDockPlan::Park
        } else {
            BrowserDockPlan::Idle
        };
    }
    if !surface.host_available {
        return BrowserDockPlan::ShowDiagnostic(BrowserDockLifecycleError::HostUnavailable);
    }
    let Some(identity) = identity else {
        return BrowserDockPlan::ShowDiagnostic(BrowserDockLifecycleError::IdentityIncomplete);
    };
    if let Err(error) = identity.validate() {
        return BrowserDockPlan::ShowDiagnostic(error);
    }
    let Some(parent_hwnd) = surface.parent_hwnd.filter(|hwnd| *hwnd != 0) else {
        return BrowserDockPlan::ShowDiagnostic(BrowserDockLifecycleError::ParentHwndMissing);
    };
    let Some(bounds) = surface
        .bounds
        .filter(|bounds| bounds.width > 0 && bounds.height > 0)
    else {
        return BrowserDockPlan::ShowDiagnostic(BrowserDockLifecycleError::BoundsMissing);
    };
    if surface.attached {
        BrowserDockPlan::Resize { bounds }
    } else {
        BrowserDockPlan::BindAndAttach {
            identity: identity.clone(),
            parent_hwnd,
            bounds,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> BrowserDockIdentity {
        BrowserDockIdentity {
            task_id: TaskId::new(),
            agent_session_id: AgentSessionId::new(),
            context_id: BrowserContextId::new(),
            resource_id: ResourceId::new(),
            process_session_id: "process-session".into(),
        }
    }

    fn bounds() -> BrowserBounds {
        BrowserBounds {
            x: 10,
            y: 20,
            width: 400,
            height: 300,
        }
    }

    #[test]
    fn parks_when_dock_collapses_or_tab_leaves_browser() {
        let surface = BrowserDockSurfaceState {
            dock_expanded: false,
            browser_tab_active: true,
            host_available: true,
            attached: true,
            parent_hwnd: Some(1),
            bounds: Some(bounds()),
        };
        assert_eq!(
            plan_browser_dock(Some(&identity()), surface),
            BrowserDockPlan::Park
        );
        let surface = BrowserDockSurfaceState {
            dock_expanded: true,
            browser_tab_active: false,
            ..surface
        };
        assert_eq!(
            plan_browser_dock(Some(&identity()), surface),
            BrowserDockPlan::Park
        );
    }

    #[test]
    fn fails_visibly_without_identity_gateway_hwnd_or_bounds() {
        let base = BrowserDockSurfaceState {
            dock_expanded: true,
            browser_tab_active: true,
            host_available: true,
            attached: false,
            parent_hwnd: Some(42),
            bounds: Some(bounds()),
        };
        assert!(matches!(
            plan_browser_dock(None, base),
            BrowserDockPlan::ShowDiagnostic(BrowserDockLifecycleError::IdentityIncomplete)
        ));
        let mut incomplete = identity();
        incomplete.process_session_id.clear();
        assert!(matches!(
            plan_browser_dock(Some(&incomplete), base),
            BrowserDockPlan::ShowDiagnostic(BrowserDockLifecycleError::GatewayMissing)
        ));
        assert!(matches!(
            plan_browser_dock(
                Some(&identity()),
                BrowserDockSurfaceState {
                    parent_hwnd: None,
                    ..base
                }
            ),
            BrowserDockPlan::ShowDiagnostic(BrowserDockLifecycleError::ParentHwndMissing)
        ));
    }

    #[test]
    fn attach_then_resize_when_identity_and_surface_are_ready() {
        let identity = identity();
        let surface = BrowserDockSurfaceState {
            dock_expanded: true,
            browser_tab_active: true,
            host_available: true,
            attached: false,
            parent_hwnd: Some(99),
            bounds: Some(bounds()),
        };
        match plan_browser_dock(Some(&identity), surface) {
            BrowserDockPlan::BindAndAttach {
                parent_hwnd,
                bounds,
                ..
            } => {
                assert_eq!(parent_hwnd, 99);
                assert_eq!(bounds, self::bounds());
            }
            other => panic!("expected bind/attach, got {other:?}"),
        }
        let attached = BrowserDockSurfaceState {
            attached: true,
            ..surface
        };
        assert!(matches!(
            plan_browser_dock(Some(&identity), attached),
            BrowserDockPlan::Resize { .. }
        ));
    }
}

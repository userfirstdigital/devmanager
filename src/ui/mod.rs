//! Native Task Cockpit and related projection surfaces.
//!
//! Phase 5 shell modules land incrementally. Services panel projection is
//! available for Task 6.7 while the remaining cockpit panels arrive.

pub mod task_cockpit;

pub use task_cockpit::{
    project_services_panel, ServiceActionAffordance, ServicePanelAction, ServicePanelRow,
    ServicePanelTone, ServicesPanelProjection,
};

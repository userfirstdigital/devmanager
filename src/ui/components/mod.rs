//! The first reusable native component vocabulary slice.

pub mod badge;
pub mod button;
pub mod empty_state;
pub mod error_boundary;
pub mod icon_button;
pub mod interaction;
pub mod status_light;
pub mod text_field;

pub use badge::Badge;
pub use button::{Button, ButtonVariant};
pub use empty_state::{EmptyState, RecoveryAction};
pub use error_boundary::ErrorBoundary;
pub use error_boundary::{SafeErrorCode, SafeErrorProjection};
pub use icon_button::{IconButton, IconId, TooltipContract};
pub use interaction::{
    AccessibilityMetadata, AccessibleRole, ActionEvent, ActionRequest, ActivationSource,
    ComponentError, ControlPresentation, FocusRing, InteractionState, InteractionStateModel,
    InteractionTransition, KeyboardKey, VisualState,
};
pub use status_light::{StatusLight, StatusPresentation};
pub use text_field::{TextField, TextFieldError, TextFieldKey, TextFieldLimits};

//! Prompt Library chrome: rail item, distinct sections, viewport tokens, hooks.

use crate::prompts::projection::{PromptNamespace, PromptPrivacyClass};

/// Navigation-rail destination for the personal Prompt Library.
pub const PROMPT_LIBRARY_RAIL_ID: &str = "prompt_library";
pub const PROMPT_LIBRARY_RAIL_LABEL: &str = "Prompt Library";
pub const PROMPT_LIBRARY_SHORTCUT: &str = "Ctrl+Shift+P";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibrarySection {
    SavedPrompts,
    RecentHistory,
    Chains,
}

impl LibrarySection {
    pub const ALL: [Self; 3] = [Self::SavedPrompts, Self::RecentHistory, Self::Chains];

    pub fn label(self) -> &'static str {
        match self {
            Self::SavedPrompts => "Saved Prompts",
            Self::RecentHistory => "Recent History",
            Self::Chains => "Chains",
        }
    }

    pub fn role(self) -> &'static str {
        "tab"
    }

    pub fn admits_provider_commands(self) -> bool {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorScheme {
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Density {
    Compact,
    Comfortable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalePercent {
    OneHundred,
    OneTwentyFive,
    OneFifty,
    TwoHundred,
}

impl ScalePercent {
    pub fn percent(self) -> u16 {
        match self {
            Self::OneHundred => 100,
            Self::OneTwentyFive => 125,
            Self::OneFifty => 150,
            Self::TwoHundred => 200,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutWidth {
    Narrow,
    Wide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataFixtureKind {
    Empty,
    Error,
    LargeData,
    Populated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptLibraryViewport {
    pub scheme: ColorScheme,
    pub density: Density,
    pub scale: ScalePercent,
    pub width: LayoutWidth,
    pub data: DataFixtureKind,
}

impl PromptLibraryViewport {
    pub fn token_set(self) -> ViewportTokens {
        ViewportTokens {
            scheme: match self.scheme {
                ColorScheme::Light => "phase5.light",
                ColorScheme::Dark => "phase5.dark",
            },
            density: match self.density {
                Density::Compact => "phase5.compact",
                Density::Comfortable => "phase5.comfortable",
            },
            scale_percent: self.scale.percent(),
            list_width_px: match self.width {
                LayoutWidth::Narrow => 280,
                LayoutWidth::Wide => 420,
            },
            detail_min_px: match self.width {
                LayoutWidth::Narrow => 320,
                LayoutWidth::Wide => 640,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewportTokens {
    pub scheme: &'static str,
    pub density: &'static str,
    pub scale_percent: u16,
    pub list_width_px: u16,
    pub detail_min_px: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessibleName {
    pub name: String,
    pub role: &'static str,
    pub status: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptLibraryUiError {
    NotFound,
    StaleRevision,
    SearchTooLong,
    AdjacentLinksRequired,
    CapExceeded,
    PayloadMismatch,
    ProviderCommandNotSavableAutomatically,
}

impl std::fmt::Display for PromptLibraryUiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "prompt library item not found"),
            Self::StaleRevision => write!(f, "prompt library revision is stale"),
            Self::SearchTooLong => write!(f, "prompt library search exceeds 512 characters"),
            Self::AdjacentLinksRequired => {
                write!(f, "insert-between requires two adjacent chain links")
            }
            Self::CapExceeded => write!(f, "prompt library virtualization cap exceeded"),
            Self::PayloadMismatch => write!(f, "composer payload is not the exact prompt version"),
            Self::ProviderCommandNotSavableAutomatically => {
                write!(
                    f,
                    "saving a provider command requires explicit Save as prompt"
                )
            }
        }
    }
}

impl std::error::Error for PromptLibraryUiError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncHookStatus {
    LocalAuthoritative,
    PendingPublish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrganizationCatalogHook {
    Unavailable,
    ReadOnlyPreview,
}

/// Personal library stays host-local. Sync/org are typed hooks only — no
/// encryption, merge, or Connect persistence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncOrgHooks {
    pub namespace: PromptNamespace,
    pub privacy: PromptPrivacyClass,
    pub sync: SyncHookStatus,
    pub organization: OrganizationCatalogHook,
    pub encrypts_prompt_bodies: bool,
}

impl Default for SyncOrgHooks {
    fn default() -> Self {
        Self {
            namespace: PromptNamespace::Personal,
            privacy: PromptPrivacyClass::LocalOnly,
            sync: SyncHookStatus::LocalAuthoritative,
            organization: OrganizationCatalogHook::Unavailable,
            encrypts_prompt_bodies: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptLibraryChrome {
    pub rail_id: &'static str,
    pub rail_label: &'static str,
    pub shortcut: &'static str,
    pub active_section: LibrarySection,
    pub sections: [LibrarySection; 3],
    pub viewport: PromptLibraryViewport,
    pub hooks: SyncOrgHooks,
}

impl PromptLibraryChrome {
    pub fn new(viewport: PromptLibraryViewport) -> Self {
        Self {
            rail_id: PROMPT_LIBRARY_RAIL_ID,
            rail_label: PROMPT_LIBRARY_RAIL_LABEL,
            shortcut: PROMPT_LIBRARY_SHORTCUT,
            active_section: LibrarySection::SavedPrompts,
            sections: LibrarySection::ALL,
            viewport,
            hooks: SyncOrgHooks::default(),
        }
    }

    pub fn select_section(&mut self, section: LibrarySection) {
        self.active_section = section;
    }

    pub fn rail_accessible_name(&self) -> AccessibleName {
        AccessibleName {
            name: self.rail_label.to_string(),
            role: "button",
            status: Some(format!("section {}", self.active_section.label())),
        }
    }

    pub fn section_accessible_name(&self, section: LibrarySection) -> AccessibleName {
        let selected = if self.active_section == section {
            "selected"
        } else {
            "not selected"
        };
        AccessibleName {
            name: section.label().to_string(),
            role: section.role(),
            status: Some(selected.to_string()),
        }
    }
}

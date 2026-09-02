use std::path::PathBuf;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

use crate::domain::canonical;
use crate::domain::id::{ResourceId, TaskId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceValidationError {
    InvalidTerminalGeometry,
    InvalidTerminalLaunch,
    InvalidTerminalTitle,
    EmptyRecipe,
    OwnerBinding,
    KindRecipeMismatch,
    InvalidRegistrationLifecycle,
}

impl std::fmt::Display for ResourceValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTerminalGeometry => {
                write!(f, "terminal cols and rows must be greater than zero")
            }
            Self::InvalidTerminalLaunch => {
                write!(
                    f,
                    "terminal launch requires an absolute cwd and a non-empty program"
                )
            }
            Self::InvalidTerminalTitle => {
                write!(
                    f,
                    "terminal title requires a launch, and must be trimmed, non-empty, and at most {MAX_TERMINAL_TITLE_CHARS} characters"
                )
            }
            Self::EmptyRecipe => write!(f, "resource recipe must be non-empty"),
            Self::OwnerBinding => {
                write!(f, "Task owner requires task_id; Host owner requires None")
            }
            Self::KindRecipeMismatch => {
                write!(f, "resource_kind must match recipe variant")
            }
            Self::InvalidRegistrationLifecycle => {
                write!(f, "registered resources must start Active")
            }
        }
    }
}

impl std::error::Error for ResourceValidationError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerKind {
    Task,
    Host,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Terminal,
    BrowserContext,
    Service,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceLifecycle {
    Active,
    Releasing,
    Released,
}

pub const MAX_TERMINAL_TITLE_CHARS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalLaunch {
    pub cwd: PathBuf,
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl TerminalLaunch {
    pub fn validate(&self) -> Result<(), ResourceValidationError> {
        if !self.cwd.is_absolute() || self.program.as_os_str().is_empty() {
            return Err(ResourceValidationError::InvalidTerminalLaunch);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceRecipe {
    Terminal {
        cols: u16,
        rows: u16,
        /// `None`: provider-owned terminal (the pre-V16 shape). `Some`: plain shell.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        launch: Option<TerminalLaunch>,
        /// Only a plain shell may be titled. `projector::pack` is `rmp_serde::to_vec`
        /// (compact), which serialises a struct variant POSITIONALLY, so a skipped
        /// field shortens the array and `{ launch: None, title: Some(_) }` would pack
        /// the title into launch's slot and fail to decode. `canonicalize`/`validate`
        /// refuse that combination so it can never reach the store.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    Browser {
        start_url: String,
    },
    Service {
        command: String,
    },
}

impl ResourceRecipe {
    pub fn terminal(cols: u16, rows: u16) -> Self {
        Self::Terminal {
            cols,
            rows,
            launch: None,
            title: None,
        }
    }

    pub fn is_plain_shell(&self) -> bool {
        matches!(
            self,
            Self::Terminal {
                launch: Some(_),
                ..
            }
        )
    }

    fn canonical_title(title: Option<String>) -> Result<Option<String>, ResourceValidationError> {
        match title {
            None => Ok(None),
            Some(raw) => {
                let trimmed = raw.trim();
                if trimmed.is_empty() || trimmed.chars().count() > MAX_TERMINAL_TITLE_CHARS {
                    return Err(ResourceValidationError::InvalidTerminalTitle);
                }
                Ok(Some(trimmed.to_string()))
            }
        }
    }

    pub fn browser(start_url: impl Into<String>) -> Result<Self, ResourceValidationError> {
        let start_url = canonical::canonicalize(start_url.into())
            .ok_or(ResourceValidationError::EmptyRecipe)?;
        Ok(Self::Browser { start_url })
    }

    pub fn service(command: impl Into<String>) -> Result<Self, ResourceValidationError> {
        let command =
            canonical::canonicalize(command.into()).ok_or(ResourceValidationError::EmptyRecipe)?;
        Ok(Self::Service { command })
    }

    pub fn canonicalize(self) -> Result<Self, ResourceValidationError> {
        match self {
            Self::Terminal {
                cols,
                rows,
                launch,
                title,
            } => {
                if cols == 0 || rows == 0 {
                    return Err(ResourceValidationError::InvalidTerminalGeometry);
                }
                if let Some(launch) = launch.as_ref() {
                    launch.validate()?;
                }
                let title = Self::canonical_title(title)?;
                if launch.is_none() && title.is_some() {
                    return Err(ResourceValidationError::InvalidTerminalTitle);
                }
                Ok(Self::Terminal {
                    cols,
                    rows,
                    launch,
                    title,
                })
            }
            Self::Browser { start_url } => Self::browser(start_url),
            Self::Service { command } => Self::service(command),
        }
    }

    pub fn validate(&self) -> Result<(), ResourceValidationError> {
        match self {
            Self::Terminal {
                cols,
                rows,
                launch,
                title,
            } => {
                if *cols == 0 || *rows == 0 {
                    return Err(ResourceValidationError::InvalidTerminalGeometry);
                }
                if let Some(launch) = launch.as_ref() {
                    launch.validate()?;
                }
                if let Some(title) = title.as_ref() {
                    if launch.is_none()
                        || title.trim() != title
                        || title.is_empty()
                        || title.chars().count() > MAX_TERMINAL_TITLE_CHARS
                    {
                        return Err(ResourceValidationError::InvalidTerminalTitle);
                    }
                }
                Ok(())
            }
            Self::Browser { start_url } => {
                if canonical::is_canonical(start_url) {
                    Ok(())
                } else {
                    Err(ResourceValidationError::EmptyRecipe)
                }
            }
            Self::Service { command } => {
                if canonical::is_canonical(command) {
                    Ok(())
                } else {
                    Err(ResourceValidationError::EmptyRecipe)
                }
            }
        }
    }
}

impl<'de> Deserialize<'de> for ResourceRecipe {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case", deny_unknown_fields)]
        enum ResourceRecipeWire {
            Terminal {
                cols: u16,
                rows: u16,
                #[serde(default)]
                launch: Option<TerminalLaunch>,
                #[serde(default)]
                title: Option<String>,
            },
            Browser {
                start_url: String,
            },
            Service {
                command: String,
            },
        }

        match ResourceRecipeWire::deserialize(deserializer)? {
            ResourceRecipeWire::Terminal {
                cols,
                rows,
                launch,
                title,
            } => Self::Terminal {
                cols,
                rows,
                launch,
                title,
            }
            .canonicalize()
            .map_err(de::Error::custom),
            ResourceRecipeWire::Browser { start_url } => {
                Self::browser(start_url).map_err(de::Error::custom)
            }
            ResourceRecipeWire::Service { command } => {
                Self::service(command).map_err(de::Error::custom)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResourceFacts {
    pub id: ResourceId,
    pub task_id: Option<TaskId>,
    pub owner_kind: OwnerKind,
    pub resource_kind: ResourceKind,
    pub recipe: ResourceRecipe,
    pub lifecycle: ResourceLifecycle,
    pub runtime_generation: u64,
    pub updated_at_ms: i64,
}

impl ResourceFacts {
    pub fn new(
        task_id: Option<TaskId>,
        owner_kind: OwnerKind,
        resource_kind: ResourceKind,
        recipe: ResourceRecipe,
        updated_at_ms: i64,
    ) -> Result<Self, ResourceValidationError> {
        let recipe = recipe.canonicalize()?;
        let facts = Self {
            id: ResourceId::new(),
            task_id,
            owner_kind,
            resource_kind,
            recipe,
            lifecycle: ResourceLifecycle::Active,
            runtime_generation: 0,
            updated_at_ms,
        };
        facts.validate()?;
        Ok(facts)
    }

    pub fn validate(&self) -> Result<(), ResourceValidationError> {
        match (self.owner_kind, self.task_id) {
            (OwnerKind::Task, Some(_)) | (OwnerKind::Host, None) => {}
            _ => return Err(ResourceValidationError::OwnerBinding),
        }
        match (&self.resource_kind, &self.recipe) {
            (ResourceKind::Terminal, ResourceRecipe::Terminal { .. })
            | (ResourceKind::BrowserContext, ResourceRecipe::Browser { .. })
            | (ResourceKind::Service, ResourceRecipe::Service { .. }) => self.recipe.validate()?,
            _ => return Err(ResourceValidationError::KindRecipeMismatch),
        }
        Ok(())
    }

    pub fn validate_for_registration(&self) -> Result<(), ResourceValidationError> {
        self.validate()?;
        if self.owner_kind != OwnerKind::Task {
            return Err(ResourceValidationError::OwnerBinding);
        }
        if self.lifecycle != ResourceLifecycle::Active {
            return Err(ResourceValidationError::InvalidRegistrationLifecycle);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for ResourceFacts {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ResourceFactsWire {
            id: ResourceId,
            task_id: Option<TaskId>,
            owner_kind: OwnerKind,
            resource_kind: ResourceKind,
            recipe: ResourceRecipe,
            lifecycle: ResourceLifecycle,
            runtime_generation: u64,
            updated_at_ms: i64,
        }

        let wire = ResourceFactsWire::deserialize(deserializer)?;
        let facts = Self {
            id: wire.id,
            task_id: wire.task_id,
            owner_kind: wire.owner_kind,
            resource_kind: wire.resource_kind,
            recipe: wire.recipe,
            lifecycle: wire.lifecycle,
            runtime_generation: wire.runtime_generation,
            updated_at_ms: wire.updated_at_ms,
        };
        facts.validate().map_err(de::Error::custom)?;
        Ok(facts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_terminal_recipe_decodes_with_no_launch_or_title() {
        // The NAMED-MAP (IPC) encoding of the pre-V16 shape:
        // {"terminal": {"cols": 120, "rows": 40}}. The durable form is positional --
        // see provider_terminal_encoding_is_byte_stable_without_new_fields -- and the
        // decoder must keep accepting both.
        let legacy = rmp_serde::to_vec(&serde_json::json!({
            "terminal": { "cols": 120, "rows": 40 }
        }))
        .expect("legacy encode");
        let decoded: ResourceRecipe = rmp_serde::from_slice(&legacy).expect("legacy decode");
        assert_eq!(decoded, ResourceRecipe::terminal(120, 40));
        assert!(!decoded.is_plain_shell());
    }

    #[test]
    fn plain_shell_recipe_round_trips_and_is_detected() {
        let recipe = ResourceRecipe::Terminal {
            cols: 100,
            rows: 30,
            launch: Some(TerminalLaunch {
                cwd: std::path::PathBuf::from(r"C:\Code\demo"),
                program: std::path::PathBuf::from(r"C:\Program Files\PowerShell\7\pwsh.exe"),
                args: vec!["-NoLogo".to_string()],
            }),
            title: Some("build".to_string()),
        };
        let bytes = rmp_serde::to_vec(&recipe).expect("encode");
        let decoded: ResourceRecipe = rmp_serde::from_slice(&bytes).expect("decode");
        assert_eq!(decoded, recipe);
        assert!(decoded.is_plain_shell());
    }

    #[test]
    fn provider_terminal_encoding_is_byte_stable_without_new_fields() {
        // `projector::pack` is `rmp_serde::to_vec` (compact): a struct variant encodes as a
        // one-entry map from the variant name to an ARRAY of its serialized fields, not a
        // map of field names. The pre-V16 two-field shape is therefore
        // 0x81 "terminal" 0x92 120 40, and `skip_serializing_if` must keep it exactly.
        #[derive(Serialize)]
        #[serde(rename_all = "snake_case")]
        enum LegacyRecipe {
            Terminal { cols: u16, rows: u16 },
        }
        let before = rmp_serde::to_vec(&LegacyRecipe::Terminal {
            cols: 120,
            rows: 40,
        })
        .expect("legacy encode");
        assert_eq!(
            before,
            vec![0x81, 0xa8, b't', b'e', b'r', b'm', b'i', b'n', b'a', b'l', 0x92, 0x78, 0x28,],
            "the pre-V16 on-disk encoding"
        );
        let now = rmp_serde::to_vec(&ResourceRecipe::terminal(120, 40)).expect("encode");
        assert_eq!(
            before, now,
            "absent launch/title must not change the encoding"
        );
        assert_eq!(
            rmp_serde::from_slice::<ResourceRecipe>(&before).expect("decode on-disk bytes"),
            ResourceRecipe::terminal(120, 40),
            "the on-disk bytes must still decode to the provider shape"
        );
    }

    #[test]
    fn terminal_launch_rejects_relative_cwd_and_empty_program() {
        let relative = ResourceRecipe::Terminal {
            cols: 80,
            rows: 24,
            launch: Some(TerminalLaunch {
                cwd: std::path::PathBuf::from("relative"),
                program: std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe"),
                args: Vec::new(),
            }),
            title: None,
        };
        assert_eq!(
            relative.validate(),
            Err(ResourceValidationError::InvalidTerminalLaunch)
        );
        let empty_program = ResourceRecipe::Terminal {
            cols: 80,
            rows: 24,
            launch: Some(TerminalLaunch {
                cwd: std::path::PathBuf::from(r"C:\Code"),
                program: std::path::PathBuf::new(),
                args: Vec::new(),
            }),
            title: None,
        };
        assert_eq!(
            empty_program.validate(),
            Err(ResourceValidationError::InvalidTerminalLaunch)
        );
    }

    fn sample_launch() -> TerminalLaunch {
        TerminalLaunch {
            cwd: std::path::PathBuf::from(r"C:\Code"),
            program: std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe"),
            args: vec!["-NoProfile".to_string()],
        }
    }

    #[test]
    fn terminal_title_is_trimmed_and_bounded() {
        // Only a plain shell may be titled, so the trim/bound cases all carry a launch.
        let recipe = ResourceRecipe::Terminal {
            cols: 80,
            rows: 24,
            launch: Some(sample_launch()),
            title: Some("  build  ".to_string()),
        }
        .canonicalize()
        .expect("canonical");
        assert_eq!(
            recipe,
            ResourceRecipe::Terminal {
                cols: 80,
                rows: 24,
                launch: Some(sample_launch()),
                title: Some("build".to_string())
            }
        );
        let too_long = ResourceRecipe::Terminal {
            cols: 80,
            rows: 24,
            launch: Some(sample_launch()),
            title: Some("x".repeat(65)),
        };
        assert_eq!(
            too_long.validate(),
            Err(ResourceValidationError::InvalidTerminalTitle)
        );
    }

    #[test]
    fn provider_terminal_may_not_be_titled() {
        // A provider-owned terminal is labelled by its provider. It also cannot be
        // titled on the wire: the compact codec is positional, so skipping `launch`
        // would slide the title into launch's slot.
        let titled_provider_terminal = ResourceRecipe::Terminal {
            cols: 80,
            rows: 24,
            launch: None,
            title: Some("build".to_string()),
        };
        assert_eq!(
            titled_provider_terminal.validate(),
            Err(ResourceValidationError::InvalidTerminalTitle)
        );
        assert_eq!(
            titled_provider_terminal.canonicalize(),
            Err(ResourceValidationError::InvalidTerminalTitle)
        );
    }

    #[test]
    fn every_representable_terminal_codec_shape_round_trips() {
        // `projector::pack` is `rmp_serde::to_vec` (compact), which serialises a struct
        // variant POSITIONALLY: {"terminal": [cols, rows, launch?, title?]}. A skipped
        // field shortens the array, so a present field may only follow present ones --
        // which is why (None, Some) is refused rather than encoded.
        //
        // The expected count is read from the msgpack array header at byte 10, i.e.
        // after the 0x81 map header and the 8-byte "terminal" key: 0x9N for N fields.
        let cases: Vec<(Option<TerminalLaunch>, Option<String>, u8)> = vec![
            (None, None, 2),
            (Some(sample_launch()), None, 3),
            (Some(sample_launch()), Some("build".to_string()), 4),
        ];
        for (launch, title, packed_fields) in cases {
            let recipe = ResourceRecipe::Terminal {
                cols: 120,
                rows: 40,
                launch,
                title,
            }
            .canonicalize()
            .expect("canonical");
            let bytes = rmp_serde::to_vec(&recipe).expect("encode");
            assert_eq!(
                bytes[10],
                0x90 | packed_fields,
                "packed field count for {recipe:?}"
            );
            let decoded: ResourceRecipe = rmp_serde::from_slice(&bytes).expect("decode");
            assert_eq!(decoded, recipe, "round trip for {recipe:?}");
        }

        // The fourth combination is unrepresentable, and canonicalize is the gate that
        // keeps it out of the store.
        assert_eq!(
            ResourceRecipe::Terminal {
                cols: 120,
                rows: 40,
                launch: None,
                title: Some("build".to_string()),
            }
            .canonicalize(),
            Err(ResourceValidationError::InvalidTerminalTitle)
        );
    }
}

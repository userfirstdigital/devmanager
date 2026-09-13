//! Session-local planning preferences. These never edit a provider's global config.

use serde::{Deserialize, Serialize};

use super::{ProviderDriverKind, ProviderSettingsError};

pub const DEFAULT_PLAN_INSTRUCTION: &str = "Before starting multi-step work, create a task list with one entry per step using your task tool, mark each entry in progress when you start it and completed when it is done, and add entries when you discover new steps.";
const TASK_TOOLS: &[&str] = &["TaskCreate", "TaskUpdate", "TaskList"];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlanProgressSettings {
    pub enabled: bool,
    pub instruction: String,
}

impl Default for PlanProgressSettings {
    fn default() -> Self {
        // Old documents retain their launch identity and behavior. Newly added
        // Claude instances explicitly opt in in builtin_default.
        Self {
            enabled: false,
            instruction: DEFAULT_PLAN_INSTRUCTION.into(),
        }
    }
}

impl PlanProgressSettings {
    pub fn validate(&self) -> Result<(), ProviderSettingsError> {
        if self.instruction.len() > crate::providers::MAX_PROVIDER_ARGUMENT_BYTES
            || self.instruction.contains('\0')
            || (self.enabled && self.instruction.trim().is_empty())
        {
            return Err(ProviderSettingsError::Corrupt("task-list instruction must contain 1–2048 bytes when enabled and cannot contain NUL".into()));
        }
        Ok(())
    }

    pub fn launch_args(&self, driver: ProviderDriverKind, original: &[String]) -> Vec<String> {
        let mut args = original.to_vec();
        if !self.enabled || driver != ProviderDriverKind::Claude {
            return args;
        }
        append_option(
            &mut args,
            "--append-system-prompt",
            &self.instruction,
            "\n\n",
            "",
        );
        for tool in TASK_TOOLS {
            append_option(&mut args, "--tools", tool, ",", "default");
        }
        args
    }
}

/// Extend an existing option (including the `--key=value` form), preserving a
/// user's restricted tool list. Only an absent tool option seeds `default`.
fn append_option(args: &mut Vec<String>, option: &str, value: &str, separator: &str, seed: &str) {
    let equals = format!("{option}=");
    if let Some(index) = args
        .iter()
        .rposition(|arg| arg == option || arg.starts_with(&equals))
    {
        let target = if args[index] == option {
            // The provider itself diagnoses malformed CLI arguments. Do not
            // turn a following option into the missing value.
            if index + 1 == args.len() || args[index + 1].starts_with("--") {
                return;
            }
            &mut args[index + 1]
        } else {
            &mut args[index]
        };
        let content = target.strip_prefix(&equals).unwrap_or(target);
        if option == "--tools" && content.split([',', ' ']).any(|tool| tool == value) {
            return;
        }
        if !content.is_empty() {
            target.push_str(separator);
        }
        target.push_str(value);
    } else {
        args.push(option.into());
        args.push(if seed.is_empty() {
            value.into()
        } else {
            format!("{seed}{separator}{value}")
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planning_adds_tools_without_removing_defaults_or_user_restrictions() {
        let settings = PlanProgressSettings {
            enabled: true,
            ..Default::default()
        };
        let args = settings.launch_args(ProviderDriverKind::Claude, &[]);
        assert_eq!(
            args,
            [
                "--append-system-prompt",
                DEFAULT_PLAN_INSTRUCTION,
                "--tools",
                "default,TaskCreate,TaskUpdate,TaskList"
            ]
        );
        for original in [
            vec!["--tools".into(), "Read,Write".into()],
            vec!["--tools=Read,Write".into()],
        ] {
            let args = settings.launch_args(ProviderDriverKind::Claude, &original);
            assert!(args
                .iter()
                .any(|arg| arg.ends_with("Read,Write,TaskCreate,TaskUpdate,TaskList")));
            assert!(!args.iter().any(|arg| arg.contains("default")));
        }
        let args = settings.launch_args(
            ProviderDriverKind::Claude,
            &["--append-system-prompt=Keep it brief".into()],
        );
        assert_eq!(
            args[0],
            format!("--append-system-prompt=Keep it brief\n\n{DEFAULT_PLAN_INSTRUCTION}")
        );
    }

    #[test]
    fn disabled_and_unverified_drivers_preserve_the_exact_launch_arguments() {
        let original = vec!["--model".into(), "chosen".into()];
        let mut settings = PlanProgressSettings::default();
        assert_eq!(
            settings.launch_args(ProviderDriverKind::Claude, &original),
            original
        );
        settings.enabled = true;
        for driver in [ProviderDriverKind::Codex, ProviderDriverKind::Cursor] {
            assert_eq!(settings.launch_args(driver, &original), original);
        }
    }
}

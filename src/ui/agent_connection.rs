use crate::domain::{
    AgentConnectionRow, AgentConnectionSnapshot, AgentPresence, ConfigSidebarProviderKind,
};
use crate::providers::ProviderKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InboxAgentAction {
    pub provider: ProviderKind,
    pub label: &'static str,
}

pub fn snapshot_connected(snapshot: Option<&AgentConnectionSnapshot>) -> bool {
    snapshot.is_some_and(AgentConnectionSnapshot::connected)
}

pub fn placeholder_task_title(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::ClaudeCode => "New Claude task",
        ProviderKind::Codex => "New Codex task",
        ProviderKind::Cursor => "New Cursor task",
    }
}

pub fn inbox_agent_actions(snapshot: &AgentConnectionSnapshot) -> Vec<InboxAgentAction> {
    snapshot
        .agents
        .iter()
        .filter(|row| row.presence == AgentPresence::SignedIn)
        .filter_map(|row| match row.provider {
            ConfigSidebarProviderKind::Claude => Some(InboxAgentAction {
                provider: ProviderKind::ClaudeCode,
                label: "+Claude",
            }),
            ConfigSidebarProviderKind::Codex => Some(InboxAgentAction {
                provider: ProviderKind::Codex,
                label: "+Codex",
            }),
        })
        .collect()
}

pub fn settings_row_copy(provider: ConfigSidebarProviderKind, presence: AgentPresence) -> String {
    let name = match provider {
        ConfigSidebarProviderKind::Claude => "Claude Code",
        ConfigSidebarProviderKind::Codex => "Codex",
    };
    match presence {
        AgentPresence::Checking => format!("Checking {name}…"),
        AgentPresence::SignedIn => format!("{name} is signed in."),
        AgentPresence::NotSignedIn => {
            format!("Sign in with {name}, then Refresh.")
        }
        AgentPresence::NotFound => {
            format!("{name} was not found on this machine. Install it, then Refresh.")
        }
        AgentPresence::CheckFailed => {
            format!("Could not check {name}. Retry.")
        }
    }
}

pub fn connect_canvas_copy(snapshot: Option<&AgentConnectionSnapshot>) -> (&'static str, String) {
    if snapshot_connected(snapshot) {
        (
            "Add a project",
            "Use + in the project list to add one.".into(),
        )
    } else {
        (
            "Connect an agent",
            "Sign in with Claude Code or Codex on this machine, then Refresh. You can add a project after one of them is connected.".into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::domain::{
        AgentConnectionRow, AgentConnectionSnapshot, AgentPresence, ConfigSidebarProviderKind,
    };
    use crate::providers::ProviderKind;

    use super::{inbox_agent_actions, placeholder_task_title, settings_row_copy, InboxAgentAction};

    #[test]
    fn placeholder_titles_are_non_empty_and_provider_specific() {
        assert_eq!(
            placeholder_task_title(ProviderKind::ClaudeCode),
            "New Claude task"
        );
        assert_eq!(placeholder_task_title(ProviderKind::Codex), "New Codex task");
    }

    #[test]
    fn inbox_actions_list_only_signed_in_claude_and_codex() {
        let snapshot = AgentConnectionSnapshot {
            agents: vec![
                AgentConnectionRow {
                    provider: ConfigSidebarProviderKind::Claude,
                    presence: AgentPresence::SignedIn,
                },
                AgentConnectionRow {
                    provider: ConfigSidebarProviderKind::Codex,
                    presence: AgentPresence::NotSignedIn,
                },
            ],
        };
        assert_eq!(
            inbox_agent_actions(&snapshot),
            vec![InboxAgentAction {
                provider: ProviderKind::ClaudeCode,
                label: "+Claude",
            }]
        );
    }

    #[test]
    fn settings_copy_does_not_claim_signed_out_on_check_failed() {
        let copy = settings_row_copy(ConfigSidebarProviderKind::Claude, AgentPresence::CheckFailed);
        assert!(copy.to_ascii_lowercase().contains("could not check"));
        assert!(!copy.to_ascii_lowercase().contains("signed out"));
    }
}

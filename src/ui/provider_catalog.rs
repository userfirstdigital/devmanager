//! Rust-owned provider slash-command catalog.
//!
//! Source of truth: `provider_catalog_seeds.rs` (reviewed Rust consts).
//! `build.rs` mirrors those consts into TypeScript. Scrape/silent-empty
//! fallbacks are forbidden.

use crate::providers::ProviderKind;
use crate::ui::provider_catalog_seeds::{
    CatalogSeed, CATALOG_REVIEWED_AT, CLAUDE_SEEDS, CODEX_SEEDS,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCommandSuggestion {
    pub label: String,
    pub command: String,
    pub provider_kind: String,
}

pub fn suggest_provider_commands<'a>(
    prefix: &str,
    catalog: &'a [ProviderCommandSuggestion],
) -> Vec<&'a ProviderCommandSuggestion> {
    let needle = prefix.trim().to_ascii_lowercase();
    catalog
        .iter()
        .filter(|suggestion| {
            needle.is_empty()
                || suggestion.command.to_ascii_lowercase().starts_with(&needle)
                || suggestion.label.to_ascii_lowercase().contains(&needle)
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCatalogEntry {
    pub command: String,
    pub description: String,
    pub aliases: Vec<String>,
    pub opens_provider_menu: bool,
}

pub fn catalog_reviewed_at() -> &'static str {
    CATALOG_REVIEWED_AT
}

fn seeds_for(provider: &ProviderKind) -> &'static [CatalogSeed] {
    match provider {
        ProviderKind::ClaudeCode => CLAUDE_SEEDS,
        ProviderKind::Codex => CODEX_SEEDS,
        ProviderKind::Cursor => &[],
    }
}

fn entries_for(provider: &ProviderKind) -> Vec<ProviderCatalogEntry> {
    seeds_for(provider)
        .iter()
        .map(|entry| ProviderCatalogEntry {
            command: entry.command.to_string(),
            description: entry.description.to_string(),
            aliases: entry
                .aliases
                .iter()
                .map(|alias| (*alias).to_string())
                .collect(),
            opens_provider_menu: entry.opens_provider_menu,
        })
        .collect()
}

pub fn provider_command_catalog(provider_kind: &ProviderKind) -> Vec<ProviderCommandSuggestion> {
    let provider_name = match provider_kind {
        ProviderKind::ClaudeCode => "claude",
        ProviderKind::Codex => "codex",
        ProviderKind::Cursor => return Vec::new(),
    };
    let mut suggestions = Vec::new();
    for entry in entries_for(provider_kind) {
        suggestions.push(ProviderCommandSuggestion {
            label: entry.description.clone(),
            command: entry.command.clone(),
            provider_kind: provider_name.to_string(),
        });
        for alias in entry.aliases {
            let alias = if alias.starts_with('/') {
                alias
            } else {
                format!("/{alias}")
            };
            suggestions.push(ProviderCommandSuggestion {
                label: format!("{} (alias for {})", entry.description, entry.command),
                command: alias,
                provider_kind: provider_name.to_string(),
            });
        }
    }
    suggestions
}

pub fn provider_command_opens_terminal(provider_kind: &ProviderKind, draft: &str) -> bool {
    let submitted = draft.trim();
    if !submitted.starts_with('/') || submitted.chars().any(char::is_whitespace) {
        return false;
    }
    let catalog = entries_for(provider_kind);
    let Some(entry) = catalog.iter().find(|entry| {
        entry.command == submitted
            || entry
                .aliases
                .iter()
                .any(|alias| alias == submitted || format!("/{alias}") == submitted)
    }) else {
        return false;
    };
    entry.opens_provider_menu
}

/// Count reviewed seed rows for one provider. Cursor stays empty intentionally.
pub fn catalog_entry_count(provider_kind: &ProviderKind) -> usize {
    seeds_for(provider_kind).len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_catalog_owns_non_empty_claude_and_codex_tables() {
        assert!(catalog_entry_count(&ProviderKind::ClaudeCode) >= 90);
        assert!(catalog_entry_count(&ProviderKind::Codex) >= 40);
        assert!(!catalog_reviewed_at().is_empty());
    }

    #[test]
    fn opens_terminal_reads_interaction_without_scraping_typescript() {
        assert!(provider_command_opens_terminal(
            &ProviderKind::ClaudeCode,
            "/diff"
        ));
        assert!(!provider_command_opens_terminal(
            &ProviderKind::ClaudeCode,
            "/clear"
        ));
    }

    #[test]
    fn suggestions_include_aliases_with_stable_command_identity() {
        let catalog = provider_command_catalog(&ProviderKind::ClaudeCode);
        assert!(catalog.iter().any(|row| row.command == "/clear"));
        assert!(catalog
            .iter()
            .any(|row| row.command == "/reset" && row.label.contains("alias for /clear")));
    }

    #[test]
    fn cursor_has_no_silent_fallback_catalog() {
        assert!(provider_command_catalog(&ProviderKind::Cursor).is_empty());
        assert_eq!(catalog_entry_count(&ProviderKind::Cursor), 0);
    }
}

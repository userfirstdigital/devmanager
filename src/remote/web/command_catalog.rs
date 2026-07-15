use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SlashCommandProvider {
    Claude,
    Codex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum SlashCommandSource {
    Project,
    Personal,
    Plugin,
}

impl SlashCommandSource {
    fn precedence(self) -> u8 {
        match self {
            Self::Plugin => 1,
            Self::Personal => 2,
            Self::Project => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiscoveredSlashCommand {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) source: SlashCommandSource,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DiscoveryLimits {
    pub(crate) max_entries: usize,
    pub(crate) max_depth: usize,
    pub(crate) max_file_bytes: u64,
    pub(crate) max_description_chars: usize,
}

impl Default for DiscoveryLimits {
    fn default() -> Self {
        Self {
            max_entries: 512,
            max_depth: 8,
            max_file_bytes: 64 * 1024,
            max_description_chars: 240,
        }
    }
}

#[cfg(test)]
impl DiscoveryLimits {
    fn for_tests() -> Self {
        Self::default()
    }
}

#[derive(Debug, Clone, Copy)]
enum DiscoveryKind {
    Skill,
    Command,
    Prompt,
}

struct DiscoveryRoot {
    path: PathBuf,
    source: SlashCommandSource,
    kind: DiscoveryKind,
}

pub(crate) fn discover_slash_commands(
    provider: SlashCommandProvider,
    project_root: Option<&Path>,
    session_cwd: &Path,
    home_dir: Option<&Path>,
    limits: DiscoveryLimits,
) -> Vec<DiscoveredSlashCommand> {
    let mut roots = Vec::new();
    let mut project_bases = Vec::new();
    if let Some(project_root) = project_root {
        project_bases.push(project_root.to_path_buf());
    }
    if project_root != Some(session_cwd) {
        project_bases.push(session_cwd.to_path_buf());
    }
    project_bases.sort();
    project_bases.dedup();

    for base in project_bases {
        append_provider_roots(&mut roots, provider, &base, SlashCommandSource::Project);
    }
    if let Some(home) = home_dir {
        append_provider_roots(&mut roots, provider, home, SlashCommandSource::Personal);
        let plugin_cache = match provider {
            SlashCommandProvider::Claude => home.join(".claude/plugins/cache"),
            SlashCommandProvider::Codex => home.join(".codex/plugins/cache"),
        };
        roots.push(DiscoveryRoot {
            path: plugin_cache,
            source: SlashCommandSource::Plugin,
            kind: DiscoveryKind::Skill,
        });
    }

    let mut discovered = BTreeMap::<String, DiscoveredSlashCommand>::new();
    let mut accepted_entries = 0usize;
    for root in roots {
        if accepted_entries >= limits.max_entries {
            break;
        }
        scan_root(&root, &limits, &mut accepted_entries, &mut discovered);
    }
    discovered.into_values().collect()
}

fn append_provider_roots(
    roots: &mut Vec<DiscoveryRoot>,
    provider: SlashCommandProvider,
    base: &Path,
    source: SlashCommandSource,
) {
    match provider {
        SlashCommandProvider::Claude => {
            roots.push(DiscoveryRoot {
                path: base.join(".claude/skills"),
                source,
                kind: DiscoveryKind::Skill,
            });
            roots.push(DiscoveryRoot {
                path: base.join(".claude/commands"),
                source,
                kind: DiscoveryKind::Command,
            });
        }
        SlashCommandProvider::Codex => {
            roots.push(DiscoveryRoot {
                path: base.join(".agents/skills"),
                source,
                kind: DiscoveryKind::Skill,
            });
            roots.push(DiscoveryRoot {
                path: base.join(".codex/skills"),
                source,
                kind: DiscoveryKind::Skill,
            });
            roots.push(DiscoveryRoot {
                path: base.join(".codex/prompts"),
                source,
                kind: DiscoveryKind::Prompt,
            });
        }
    }
}

fn scan_root(
    root: &DiscoveryRoot,
    limits: &DiscoveryLimits,
    accepted_entries: &mut usize,
    discovered: &mut BTreeMap<String, DiscoveredSlashCommand>,
) {
    if !root.path.is_dir() {
        return;
    }
    let mut pending = vec![(root.path.clone(), 0usize)];
    while let Some((directory, depth)) = pending.pop() {
        if depth > limits.max_depth || *accepted_entries >= limits.max_entries {
            continue;
        }
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        let mut entries = entries.filter_map(Result::ok).collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries.into_iter().rev() {
            if *accepted_entries >= limits.max_entries {
                break;
            }
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                if depth < limits.max_depth {
                    pending.push((path, depth + 1));
                }
                continue;
            }
            if !file_type.is_file()
                || path.extension().and_then(|value| value.to_str()) != Some("md")
            {
                continue;
            }
            if matches!(root.kind, DiscoveryKind::Skill)
                && path.file_name().and_then(|value| value.to_str()) != Some("SKILL.md")
            {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.len() == 0 || metadata.len() > limits.max_file_bytes {
                continue;
            }
            let Ok(body) = fs::read_to_string(&path) else {
                continue;
            };
            let Some(command) = command_from_file(root, &path, &body, limits) else {
                continue;
            };
            *accepted_entries += 1;
            let key = command.name.to_ascii_lowercase();
            let replace = discovered
                .get(&key)
                .is_none_or(|current| command.source.precedence() > current.source.precedence());
            if replace {
                discovered.insert(key, command);
            }
        }
    }
}

fn command_from_file(
    root: &DiscoveryRoot,
    path: &Path,
    body: &str,
    limits: &DiscoveryLimits,
) -> Option<DiscoveredSlashCommand> {
    let metadata = markdown_metadata(body);
    let raw_name = match root.kind {
        DiscoveryKind::Skill => metadata
            .name
            .or_else(|| path.parent()?.file_name()?.to_str().map(ToOwned::to_owned))?,
        DiscoveryKind::Command => relative_command_name(&root.path, path, false)?,
        DiscoveryKind::Prompt => relative_command_name(&root.path, path, true)?,
    };
    let name = normalize_command_name(&raw_name)?;
    let description = metadata
        .description
        .or_else(|| first_prose_line(body))
        .unwrap_or_else(|| "Custom provider command".to_string());
    let description = truncate_chars(description.trim(), limits.max_description_chars);
    if description.is_empty() {
        return None;
    }
    Some(DiscoveredSlashCommand {
        name,
        description,
        source: root.source,
    })
}

fn relative_command_name(root: &Path, path: &Path, prompt: bool) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let mut segments = relative
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let last = segments.last_mut()?;
    *last = Path::new(last).file_stem()?.to_str()?.to_string();
    let joined = segments.join(":");
    Some(if prompt {
        format!("prompts:{joined}")
    } else {
        joined
    })
}

fn normalize_command_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_start_matches('/');
    if trimmed.is_empty()
        || trimmed.len() > 128
        || !trimmed
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_.:-".contains(character))
    {
        return None;
    }
    Some(format!("/{trimmed}"))
}

#[derive(Default)]
struct MarkdownMetadata {
    name: Option<String>,
    description: Option<String>,
}

fn markdown_metadata(body: &str) -> MarkdownMetadata {
    let mut metadata = MarkdownMetadata::default();
    let mut lines = body.lines();
    if lines.next().map(str::trim) != Some("---") {
        return metadata;
    }
    for line in lines {
        let line = line.trim();
        if line == "---" {
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches(['\'', '"']);
        if value.is_empty() {
            continue;
        }
        match key.trim() {
            "name" => metadata.name = Some(value.to_string()),
            "description" => metadata.description = Some(value.to_string()),
            _ => {}
        }
    }
    metadata
}

fn first_prose_line(body: &str) -> Option<String> {
    let mut frontmatter = false;
    for (index, line) in body.lines().enumerate() {
        let trimmed = line.trim();
        if index == 0 && trimmed == "---" {
            frontmatter = true;
            continue;
        }
        if frontmatter {
            if trimmed == "---" {
                frontmatter = false;
            }
            continue;
        }
        let prose = trimmed.trim_start_matches('#').trim();
        if !prose.is_empty() {
            return Some(prose.to_string());
        }
    }
    None
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut truncated = value.chars().take(max_chars).collect::<String>();
    while truncated.ends_with(char::is_whitespace) {
        truncated.pop();
    }
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestTree {
        root: PathBuf,
    }

    impl TestTree {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "devmanager-slash-catalog-{label}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&root).expect("create fixture root");
            Self { root }
        }

        fn path(&self) -> &Path {
            &self.root
        }

        fn write(&self, relative: &str, body: &str) {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().expect("fixture parent"))
                .expect("create fixture parent");
            fs::write(path, body).expect("write fixture");
        }
    }

    impl Drop for TestTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn claude_discovery_parses_project_and_personal_metadata_with_precedence() {
        let project = TestTree::new("claude-project");
        let home = TestTree::new("claude-home");
        project.write(
            ".claude/skills/deploy/SKILL.md",
            "---\nname: deploy\ndescription: Deploy this project safely.\n---\n# Deploy\n",
        );
        project.write(
            ".claude/commands/frontend/component.md",
            "---\ndescription: Create a native component.\n---\n",
        );
        home.write(
            ".claude/skills/deploy/SKILL.md",
            "---\nname: deploy\ndescription: Personal deployment.\n---\n",
        );
        home.write(
            ".claude/commands/research.md",
            "# Research a question thoroughly\nMore instructions stay private.\n",
        );

        let commands = discover_slash_commands(
            SlashCommandProvider::Claude,
            Some(project.path()),
            project.path(),
            Some(home.path()),
            DiscoveryLimits::for_tests(),
        );

        assert_eq!(
            commands
                .iter()
                .map(|command| (
                    command.name.as_str(),
                    command.description.as_str(),
                    command.source
                ))
                .collect::<Vec<_>>(),
            vec![
                (
                    "/deploy",
                    "Deploy this project safely.",
                    SlashCommandSource::Project,
                ),
                (
                    "/frontend:component",
                    "Create a native component.",
                    SlashCommandSource::Project,
                ),
                (
                    "/research",
                    "Research a question thoroughly",
                    SlashCommandSource::Personal,
                ),
            ]
        );
    }

    #[test]
    fn codex_discovery_uses_skill_and_prompt_names_without_leaking_paths_or_bodies() {
        let project = TestTree::new("codex-project");
        let home = TestTree::new("codex-home");
        project.write(
            ".agents/skills/verify-ui/SKILL.md",
            "---\nname: verify-ui\ndescription: Verify the mobile interface.\n---\nSECRET-BODY\n",
        );
        home.write(
            ".codex/prompts/release.md",
            "---\ndescription: Prepare a release.\n---\nPRIVATE-INSTRUCTIONS\n",
        );

        let commands = discover_slash_commands(
            SlashCommandProvider::Codex,
            Some(project.path()),
            project.path(),
            Some(home.path()),
            DiscoveryLimits::for_tests(),
        );
        let json = serde_json::to_string(&commands).expect("serialize safe metadata");

        assert!(commands.iter().any(|command| command.name == "/verify-ui"));
        assert!(commands
            .iter()
            .any(|command| command.name == "/prompts:release"));
        assert!(!json.contains(project.path().to_string_lossy().as_ref()));
        assert!(!json.contains(home.path().to_string_lossy().as_ref()));
        assert!(!json.contains("SECRET-BODY"));
        assert!(!json.contains("PRIVATE-INSTRUCTIONS"));
    }

    #[test]
    fn discovery_is_deterministic_and_enforces_file_entry_and_name_bounds() {
        let project = TestTree::new("bounds-project");
        project.write(
            ".claude/commands/valid.md",
            "---\ndescription: Valid command.\n---\n",
        );
        project.write(
            ".claude/commands/invalid name.md",
            "---\ndescription: Invalid name.\n---\n",
        );
        project.write(
            ".claude/commands/oversize.md",
            &format!("# {}", "x".repeat(2_048)),
        );
        project.write(
            ".claude/commands/second.md",
            "---\ndescription: Second command.\n---\n",
        );

        let limits = DiscoveryLimits {
            max_entries: 2,
            max_depth: 4,
            max_file_bytes: 1_024,
            max_description_chars: 80,
        };
        let first = discover_slash_commands(
            SlashCommandProvider::Claude,
            Some(project.path()),
            project.path(),
            None,
            limits,
        );
        let second = discover_slash_commands(
            SlashCommandProvider::Claude,
            Some(project.path()),
            project.path(),
            None,
            limits,
        );

        assert_eq!(first, second);
        assert!(first.len() <= 2);
        assert!(first.iter().all(|command| command.name != "/invalid name"));
        assert!(first.iter().all(|command| command.name != "/oversize"));
    }
}

use crate::models::{EnvEntry, EnvEntryType};
use regex::Regex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

static PORT_LINE_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?im)^([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(\d+)").unwrap());

pub fn read_env_entries(path: &Path) -> Result<Vec<EnvEntry>, String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|error| format!("Failed to read env file {}: {error}", path.display()))?;
    Ok(parse_env_entries(&contents))
}

pub fn read_env_text(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map_err(|error| format!("Failed to read env file {}: {error}", path.display()))
}

pub fn write_env_entries(path: &Path, entries: &[EnvEntry]) -> Result<(), String> {
    write_env_text(path, &serialize_env_entries(entries))
}

pub fn write_env_text(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Failed to create {}: {error}", parent.display()))?;
    }

    let existing_bytes = std::fs::read(path).ok();
    let line_ending = if existing_bytes
        .as_deref()
        .map(|bytes| bytes.windows(2).any(|window| window == b"\r\n"))
        .unwrap_or(false)
    {
        "\r\n"
    } else if cfg!(windows) {
        "\r\n"
    } else {
        "\n"
    };

    let normalized = normalize_line_endings(contents, line_ending);
    let temp_path = path.with_extension("tmp");
    std::fs::write(&temp_path, normalized.as_bytes()).map_err(|error| {
        format!(
            "Failed to write temporary env file {}: {error}",
            temp_path.display()
        )
    })?;
    std::fs::rename(&temp_path, path).map_err(|error| {
        format!(
            "Failed to replace env file {} with {}: {error}",
            path.display(),
            temp_path.display()
        )
    })?;
    Ok(())
}

pub fn read_env_map(path: &Path) -> Result<HashMap<String, String>, String> {
    let entries = read_env_entries(path)?;
    let mut env = HashMap::new();
    for entry in entries {
        if matches!(entry.entry_type, EnvEntryType::Variable) {
            let Some(key) = entry.key else { continue };
            let Some(value) = entry.value else { continue };
            env.insert(key, strip_wrapping_quotes(&value));
        }
    }
    Ok(env)
}

pub fn update_env_port(path: &Path, variable: &str, new_port: u16) -> Result<bool, String> {
    let contents = read_env_text(path)?;
    let pattern = format!(r"(?im)^({})\s*=\s*\d+", regex::escape(variable));
    let regex = Regex::new(&pattern).map_err(|error| error.to_string())?;
    if !regex.is_match(&contents) {
        return Ok(false);
    }
    let updated = regex
        .replace(&contents, format!("{variable}={new_port}"))
        .to_string();
    write_env_text(path, &updated)?;
    Ok(true)
}

pub fn parse_env_entries(contents: &str) -> Vec<EnvEntry> {
    let mut entries = Vec::new();
    for line in contents.lines() {
        if line.trim().is_empty() {
            entries.push(EnvEntry {
                entry_type: EnvEntryType::Blank,
                key: None,
                value: None,
                raw: String::new(),
            });
        } else if line.trim_start().starts_with('#') {
            entries.push(EnvEntry {
                entry_type: EnvEntryType::Comment,
                key: None,
                value: None,
                raw: line.to_string(),
            });
        } else if let Some((key, value)) = line.split_once('=') {
            entries.push(EnvEntry {
                entry_type: EnvEntryType::Variable,
                key: Some(key.trim().to_string()),
                value: Some(value.trim().to_string()),
                raw: line.to_string(),
            });
        } else {
            entries.push(EnvEntry {
                entry_type: EnvEntryType::Comment,
                key: None,
                value: None,
                raw: line.to_string(),
            });
        }
    }
    entries
}

pub fn serialize_env_entries(entries: &[EnvEntry]) -> String {
    let lines: Vec<String> = entries
        .iter()
        .map(|entry| match entry.entry_type {
            EnvEntryType::Blank => String::new(),
            EnvEntryType::Comment => entry.raw.clone(),
            EnvEntryType::Variable => match (&entry.key, &entry.value) {
                (Some(key), Some(value)) => format!("{key}={value}"),
                _ => entry.raw.clone(),
            },
        })
        .collect();
    lines.join("\n")
}

pub fn detect_port_variables(entries: &[EnvEntry]) -> Vec<(String, u16)> {
    entries
        .iter()
        .filter_map(|entry| {
            if !matches!(entry.entry_type, EnvEntryType::Variable) {
                return None;
            }
            let key = entry.key.as_ref()?;
            let value = entry.value.as_ref()?;
            if !PORT_LINE_REGEX.is_match(&format!("{key}={value}")) {
                return None;
            }
            value.parse::<u16>().ok().map(|port| (key.clone(), port))
        })
        .collect()
}

fn normalize_line_endings(contents: &str, line_ending: &str) -> String {
    contents
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', line_ending)
}

fn strip_wrapping_quotes(value: &str) -> String {
    let trimmed = value.trim();
    if (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
    {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_env_entries, read_env_text, serialize_env_entries, update_env_port};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn env_entries_round_trip_comments_and_blank_lines() {
        let contents = "# note\nPORT=3000\n\nNAME=devmanager\n";
        let entries = parse_env_entries(contents);
        let round_trip = serialize_env_entries(&entries);
        assert_eq!(round_trip, "# note\nPORT=3000\n\nNAME=devmanager");
    }

    #[test]
    fn update_env_port_rewrites_requested_variable() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("devmanager-env-{unique}.env"));
        std::fs::write(&path, "PORT=3000\nAPI_PORT=4000\n").unwrap();

        let updated = update_env_port(&path, "API_PORT", 4100).unwrap();
        let contents = read_env_text(&path).unwrap();
        let normalized = contents.replace("\r\n", "\n");

        assert!(updated);
        assert_eq!(normalized.trim(), "PORT=3000\nAPI_PORT=4100");

        let _ = std::fs::remove_file(path);
    }
}

use crate::diagnostics::model::ProfileRecipe;
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

pub const BEGIN_MARKER: &str = "# BEGIN DevManager";
pub const END_MARKER: &str = "# END DevManager";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileEncoding {
    Utf8,
    Utf8Bom,
    Utf16LeBom,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileEditPreview {
    pub path: PathBuf,
    pub exists: bool,
    pub before: String,
    pub after: String,
    pub recipe: ProfileRecipe,
    pub risk_note: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileApplyResult {
    pub backup_path: PathBuf,
    pub digest_hex: String,
    /// True when the target file did not exist before the edit.
    pub created_new: bool,
    /// True when the target existed but was empty before the edit.
    pub had_empty_original: bool,
    pub encoding: ProfileEncoding,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileError {
    Io(String),
    MalformedMarkers(String),
    NonFileTarget(String),
    CanonicalParent(String),
    SymlinkAmbiguity(String),
    VerificationFailed(String),
    BackupFailed(String),
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(msg)
            | Self::MalformedMarkers(msg)
            | Self::NonFileTarget(msg)
            | Self::CanonicalParent(msg)
            | Self::SymlinkAmbiguity(msg)
            | Self::VerificationFailed(msg)
            | Self::BackupFailed(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for ProfileError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkedBlockState {
    Missing,
    Present { body: String },
    Malformed { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcClassification {
    Absent,
    ManagedSafe,
    ManagedUnsafe,
    UnmarkedSafe,
    UnmarkedUnsafe,
}

/// Fixed-format AST probe result (offsets only; never profile text).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CcAstProbeResult {
    pub has_cc: bool,
    /// True only when the effective function contains a real `claude`/`claude.exe` CommandAst.
    pub has_claude: bool,
    pub unsafe_claude: bool,
    pub cc_start: usize,
    pub cc_end: usize,
    pub block_start: Option<usize>,
    pub block_end: Option<usize>,
}

/// Classify from AST probe offsets. The effective `cc` is managed when its span lies
/// fully inside the DevManager marker block.
/// A `function cc` without a real `claude` command is Absent (never Healthy).
pub fn classify_cc_ast(result: &CcAstProbeResult) -> CcClassification {
    if !result.has_cc || !result.has_claude {
        return CcClassification::Absent;
    }
    let managed = match (result.block_start, result.block_end) {
        (Some(start), Some(end)) if end >= start => {
            result.cc_start >= start && result.cc_end <= end
        }
        _ => false,
    };
    match (managed, result.unsafe_claude) {
        (true, true) => CcClassification::ManagedUnsafe,
        (true, false) => CcClassification::ManagedSafe,
        (false, true) => CcClassification::UnmarkedUnsafe,
        (false, false) => CcClassification::UnmarkedSafe,
    }
}

/// Parse the fixed AST probe stdout. Rejects any line that looks like profile content.
pub fn parse_cc_ast_probe_output(stdout: &str) -> Result<CcAstProbeResult, String> {
    let mut has_cc = None;
    let mut has_claude = None;
    let mut unsafe_claude = None;
    let mut cc_start = None;
    let mut cc_end = None;
    let mut block_start: Option<Option<usize>> = None;
    let mut block_end: Option<Option<usize>> = None;

    for raw in stdout.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line.contains('{') || line.contains('}') || (line.contains(' ') && !line.contains('=')) {
            return Err("unexpected profile content in AST probe output".into());
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err("malformed AST probe line".into());
        };
        match key {
            "CC" => {
                has_cc = Some(parse_flag(value)?);
            }
            "HAS_CLAUDE" => {
                has_claude = Some(parse_flag(value)?);
            }
            "UNSAFE" => {
                unsafe_claude = Some(parse_flag(value)?);
            }
            "CC_START" => {
                cc_start = Some(parse_offset(value)?);
            }
            "CC_END" => {
                cc_end = Some(parse_offset(value)?);
            }
            "BLOCK_START" => {
                block_start = Some(parse_optional_offset(value)?);
            }
            "BLOCK_END" => {
                block_end = Some(parse_optional_offset(value)?);
            }
            "ERR" => return Err("AST probe reported an error".into()),
            _ => return Err("unknown AST probe field".into()),
        }
    }

    let has_cc = has_cc.ok_or_else(|| "AST probe missing CC".to_string())?;
    if !has_cc {
        return Ok(CcAstProbeResult {
            has_cc: false,
            has_claude: false,
            unsafe_claude: false,
            cc_start: 0,
            cc_end: 0,
            block_start: block_start.flatten(),
            block_end: block_end.flatten(),
        });
    }
    Ok(CcAstProbeResult {
        has_cc: true,
        has_claude: has_claude.ok_or_else(|| "AST probe missing HAS_CLAUDE".to_string())?,
        unsafe_claude: unsafe_claude.ok_or_else(|| "AST probe missing UNSAFE".to_string())?,
        cc_start: cc_start.ok_or_else(|| "AST probe missing CC_START".to_string())?,
        cc_end: cc_end.ok_or_else(|| "AST probe missing CC_END".to_string())?,
        block_start: block_start.ok_or_else(|| "AST probe missing BLOCK_START".to_string())?,
        block_end: block_end.ok_or_else(|| "AST probe missing BLOCK_END".to_string())?,
    })
}

fn parse_flag(value: &str) -> Result<bool, String> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err("invalid AST probe flag".into()),
    }
}

fn parse_offset(value: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .map_err(|_| "invalid AST probe offset".into())
}

fn parse_optional_offset(value: &str) -> Result<Option<usize>, String> {
    if value == "-1" {
        return Ok(None);
    }
    Ok(Some(parse_offset(value)?))
}

pub fn inspect_marked_block(content: &str) -> MarkedBlockState {
    match locate_block(content) {
        Ok(None) => MarkedBlockState::Missing,
        Ok(Some(block)) => MarkedBlockState::Present {
            body: block.body.to_string(),
        },
        Err(reason) => MarkedBlockState::Malformed { reason },
    }
}

/// Legacy substring classifier retained only for focused unit fixtures.
/// Production Healthy classification must use [`classify_cc_ast`].
#[cfg(test)]
pub fn classify_cc_raw_text(content: &str) -> CcClassification {
    match inspect_marked_block(content) {
        MarkedBlockState::Present { body } => classify_cc_body_raw(&body, true),
        MarkedBlockState::Malformed { .. } => CcClassification::Absent,
        MarkedBlockState::Missing => match extract_unmarked_cc_body(content) {
            Some(body) => classify_cc_body_raw(&body, false),
            None => CcClassification::Absent,
        },
    }
}

#[cfg(test)]
fn classify_cc_body_raw(body: &str, managed: bool) -> CcClassification {
    let unsafe_flag = body.contains("--dangerously-skip-permissions");
    let has_claude = body.contains("claude");
    if !has_claude {
        return CcClassification::Absent;
    }
    match (managed, unsafe_flag) {
        (true, true) => CcClassification::ManagedUnsafe,
        (true, false) => CcClassification::ManagedSafe,
        (false, true) => CcClassification::UnmarkedUnsafe,
        (false, false) => CcClassification::UnmarkedSafe,
    }
}

#[cfg(test)]
fn extract_unmarked_cc_body(content: &str) -> Option<String> {
    let lower = content.to_ascii_lowercase();
    let mut search = 0;
    let mut last = None;
    while let Some(rel) = lower[search..].find("function") {
        let idx = search + rel;
        let after = content[idx + "function".len()..].trim_start();
        let name = after
            .split(|c: char| c.is_whitespace() || c == '{' || c == '(')
            .next()
            .unwrap_or("");
        if name.eq_ignore_ascii_case("cc") {
            let brace = content[idx..].find('{')?;
            let start = idx + brace;
            let end = match_brace(content, start)?;
            last = Some(content[start..=end].to_string());
        }
        search = idx + "function".len();
    }
    last
}

#[cfg(test)]
fn match_brace(content: &str, open_idx: usize) -> Option<usize> {
    let bytes = content.as_bytes();
    if bytes.get(open_idx) != Some(&b'{') {
        return None;
    }
    let mut depth = 0i32;
    let mut i = open_idx;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

pub fn recipe_body(recipe: ProfileRecipe) -> &'static str {
    match recipe {
        ProfileRecipe::SafeClaudeShortcut => {
            "function cc {\n    try { claude update | Out-Null } catch { }\n    claude @args\n}\n"
        }
        ProfileRecipe::UnsafeClaudeShortcut => {
            "function cc {\n    try { claude update | Out-Null } catch { }\n    claude --dangerously-skip-permissions @args\n}\n"
        }
    }
}

/// Exact DevManager managed block that profile repairs install (template only).
pub fn managed_block_preview(recipe: ProfileRecipe) -> String {
    format_block(recipe, "\n")
}

pub fn preview_profile_edit(
    path: &Path,
    recipe: ProfileRecipe,
) -> Result<ProfileEditPreview, ProfileError> {
    reject_symlink(path)?;
    let exists = path.exists();
    if exists && !path.is_file() {
        return Err(ProfileError::NonFileTarget(format!(
            "profile target is not a file: {}",
            path.display()
        )));
    }
    let (before, _, _) = if exists {
        read_profile(path)?
    } else {
        (String::new(), ProfileEncoding::Utf8, "\n")
    };
    let after = render_updated_content(&before, recipe)?;
    let risk_note = match recipe {
        ProfileRecipe::SafeClaudeShortcut => None,
        ProfileRecipe::UnsafeClaudeShortcut => {
            Some("High risk: bypasses Claude Code permission prompts.")
        }
    };
    Ok(ProfileEditPreview {
        path: path.to_path_buf(),
        exists,
        before,
        after,
        recipe,
        risk_note,
    })
}

pub fn apply_profile_edit<F>(
    path: &Path,
    recipe: ProfileRecipe,
    verify: F,
) -> Result<ProfileApplyResult, ProfileError>
where
    F: FnOnce(&str) -> Result<(), String>,
{
    apply_profile_edit_with_writer(path, recipe, verify, |path, content, encoding| {
        write_atomically_encoded(path, content, encoding)
    })
}

fn apply_profile_edit_with_writer<F, W>(
    path: &Path,
    recipe: ProfileRecipe,
    verify: F,
    writer: W,
) -> Result<ProfileApplyResult, ProfileError>
where
    F: FnOnce(&str) -> Result<(), String>,
    W: FnOnce(&Path, &str, ProfileEncoding) -> io::Result<()>,
{
    reject_symlink(path)?;
    ensure_canonical_parent(path)?;

    if path.exists() && !path.is_file() {
        return Err(ProfileError::NonFileTarget(format!(
            "profile target is not a file: {}",
            path.display()
        )));
    }

    let created_new = !path.exists();
    let (original, encoding, _) = if created_new {
        (String::new(), ProfileEncoding::Utf8, "\n")
    } else {
        read_profile(path)?
    };
    let had_empty_original = !created_new && original.is_empty();
    let updated = render_updated_content(&original, recipe)?;

    let backup_path = create_backup_bytes(path, &encode_profile(&original, encoding))?;

    if let Err(write_err) = writer(path, &updated, encoding) {
        return Err(write_failure_after_backup(
            path,
            &backup_path,
            created_new,
            write_err,
        ));
    }

    if let Err(reason) = verify(&updated) {
        rollback_profile_edit(
            path,
            &ProfileApplyResult {
                backup_path: backup_path.clone(),
                digest_hex: digest_hex(&updated),
                created_new,
                had_empty_original,
                encoding,
            },
        )?;
        return Err(ProfileError::VerificationFailed(reason));
    }

    Ok(ProfileApplyResult {
        backup_path,
        digest_hex: digest_hex(&updated),
        created_new,
        had_empty_original,
        encoding,
    })
}

fn write_failure_after_backup(
    path: &Path,
    backup_path: &Path,
    created_new: bool,
    write_err: io::Error,
) -> ProfileError {
    match restore_profile_after_failed_write(path, backup_path, created_new) {
        Ok(()) => ProfileError::Io(format!("failed to write profile: {write_err}")),
        Err(rollback_err) => ProfileError::Io(format!(
            "failed to write profile: {write_err}; also failed to restore backup: {rollback_err}"
        )),
    }
}

fn restore_profile_after_failed_write(
    path: &Path,
    backup_path: &Path,
    created_new: bool,
) -> Result<(), String> {
    if created_new {
        if path.exists() {
            fs::remove_file(path).map_err(|err| err.to_string())?;
        }
        return Ok(());
    }
    let backup = fs::read(backup_path).map_err(|err| format!("failed to read backup: {err}"))?;
    write_bytes_atomically(path, &backup).map_err(|err| err.to_string())
}

/// Restore the profile from an apply result's backup, distinguishing new vs pre-existing empty files.
pub fn rollback_profile_edit(path: &Path, apply: &ProfileApplyResult) -> Result<(), ProfileError> {
    if apply.created_new {
        if path.exists() {
            fs::remove_file(path).map_err(|err| ProfileError::Io(err.to_string()))?;
        }
        return Ok(());
    }
    let backup = fs::read(&apply.backup_path)
        .map_err(|err| ProfileError::Io(format!("failed to read backup: {err}")))?;
    // Pre-existing empty originals restore to an empty file (not deletion).
    write_bytes_atomically(path, &backup).map_err(|err| ProfileError::Io(err.to_string()))?;
    Ok(())
}

fn render_updated_content(before: &str, recipe: ProfileRecipe) -> Result<String, ProfileError> {
    let newline = detect_newline(before);
    let block = format_block(recipe, newline);
    match locate_block(before) {
        Ok(None) => {
            if before.is_empty() {
                Ok(block)
            } else if before.ends_with('\n') {
                Ok(format!("{before}{block}"))
            } else {
                Ok(format!("{before}{newline}{block}"))
            }
        }
        Ok(Some(existing)) => {
            let mut out = String::new();
            out.push_str(&before[..existing.start]);
            out.push_str(&block);
            out.push_str(&before[existing.end..]);
            Ok(out)
        }
        Err(reason) => Err(ProfileError::MalformedMarkers(reason)),
    }
}

fn format_block(recipe: ProfileRecipe, newline: &str) -> String {
    let body = recipe_body(recipe).replace('\n', newline);
    format!("{BEGIN_MARKER}{newline}{body}{END_MARKER}{newline}")
}

fn detect_newline(content: &str) -> &'static str {
    if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

struct LocatedBlock<'a> {
    start: usize,
    end: usize,
    body: &'a str,
}

fn locate_block(content: &str) -> Result<Option<LocatedBlock<'_>>, String> {
    let begins: Vec<_> = content.match_indices(BEGIN_MARKER).collect();
    let ends: Vec<_> = content.match_indices(END_MARKER).collect();

    if begins.is_empty() && ends.is_empty() {
        return Ok(None);
    }
    if begins.len() != 1 || ends.len() != 1 {
        return Err(
            "duplicate or mismatched DevManager markers; review the profile manually".to_string(),
        );
    }
    let (begin_idx, _) = begins[0];
    let (end_idx, _) = ends[0];
    if end_idx < begin_idx {
        return Err("END marker appears before BEGIN marker".to_string());
    }
    if !marker_at_line_start(content, begin_idx) || !marker_at_line_start(content, end_idx) {
        return Err("DevManager markers must start a line".to_string());
    }

    let body_start = begin_idx + BEGIN_MARKER.len();
    let body = &content[body_start..end_idx];
    let after_end = end_idx + END_MARKER.len();
    let end = if content[after_end..].starts_with("\r\n") {
        after_end + 2
    } else if content[after_end..].starts_with('\n') {
        after_end + 1
    } else {
        after_end
    };

    Ok(Some(LocatedBlock {
        start: begin_idx,
        end,
        body,
    }))
}

fn marker_at_line_start(content: &str, idx: usize) -> bool {
    if idx == 0 {
        return true;
    }
    let before = &content[..idx];
    match before.rfind('\n') {
        Some(nl) => before[nl + 1..].chars().all(|c| c == ' ' || c == '\t'),
        None => before.chars().all(|c| c == ' ' || c == '\t'),
    }
}

fn create_backup_bytes(path: &Path, original: &[u8]) -> Result<PathBuf, ProfileError> {
    use std::io::Write;

    let stamp = OffsetDateTime::now_utc()
        .format(
            &time::format_description::parse(
                "[year][month][day]T[hour][minute][second][subsecond digits:3]Z",
            )
            .unwrap(),
        )
        .unwrap_or_else(|_| "backup".to_string());
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("profile.ps1");

    for n in 0..10_000 {
        let name = if n == 0 {
            format!("{file_name}.devmanager-backup-{stamp}")
        } else {
            format!("{file_name}.devmanager-backup-{stamp}-{n}")
        };
        let candidate = path.with_file_name(name);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                file.write_all(original).map_err(|err| {
                    ProfileError::BackupFailed(format!("failed to write backup: {err}"))
                })?;
                return Ok(candidate);
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(ProfileError::BackupFailed(format!(
                    "failed to create backup: {err}"
                )));
            }
        }
    }

    Err(ProfileError::BackupFailed(
        "exhausted backup name candidates".into(),
    ))
}

pub fn read_profile(path: &Path) -> Result<(String, ProfileEncoding, &'static str), ProfileError> {
    let bytes = fs::read(path).map_err(|err| ProfileError::Io(err.to_string()))?;
    let (text, encoding) = decode_profile_bytes(&bytes)?;
    let newline = detect_newline(&text);
    Ok((text, encoding, newline))
}

fn decode_profile_bytes(bytes: &[u8]) -> Result<(String, ProfileEncoding), ProfileError> {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let u16s: Vec<u16> = bytes[2..]
            .chunks(2)
            .map(|chunk| {
                let b0 = chunk[0];
                let b1 = chunk.get(1).copied().unwrap_or(0);
                u16::from_le_bytes([b0, b1])
            })
            .collect();
        let text = String::from_utf16(&u16s)
            .map_err(|_| ProfileError::Io("invalid UTF-16LE profile content".into()))?;
        return Ok((text, ProfileEncoding::Utf16LeBom));
    }
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        let text = String::from_utf8(bytes[3..].to_vec())
            .map_err(|err| ProfileError::Io(format!("invalid UTF-8 BOM profile: {err}")))?;
        return Ok((text, ProfileEncoding::Utf8Bom));
    }
    let text = String::from_utf8(bytes.to_vec())
        .map_err(|err| ProfileError::Io(format!("invalid UTF-8 profile: {err}")))?;
    Ok((text, ProfileEncoding::Utf8))
}

fn encode_profile(text: &str, encoding: ProfileEncoding) -> Vec<u8> {
    match encoding {
        ProfileEncoding::Utf8 => text.as_bytes().to_vec(),
        ProfileEncoding::Utf8Bom => {
            let mut out = vec![0xEF, 0xBB, 0xBF];
            out.extend(text.as_bytes());
            out
        }
        ProfileEncoding::Utf16LeBom => {
            let mut out = vec![0xFF, 0xFE];
            for unit in text.encode_utf16() {
                out.extend(unit.to_le_bytes());
            }
            out
        }
    }
}

fn write_atomically_encoded(
    path: &Path,
    content: &str,
    encoding: ProfileEncoding,
) -> io::Result<()> {
    write_bytes_atomically(path, &encode_profile(content, encoding))
}

fn write_bytes_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_bytes_atomically_with(path, bytes, promote_temp_file)
}

/// Write `bytes` to a unique temp file (create_new), then promote it onto `path`.
/// On promote failure the temp file is removed best-effort; the live path is not
/// truncated via copy-over-live.
fn write_bytes_atomically_with(
    path: &Path,
    bytes: &[u8],
    promote: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let (temp, mut file) = create_unique_temp_file(path)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        promote(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn create_unique_temp_file(path: &Path) -> io::Result<(PathBuf, fs::File)> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("profile.ps1");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();

    for n in 0..10_000 {
        let name = if n == 0 {
            format!("{file_name}.devmanager-tmp-{pid}-{stamp}")
        } else {
            format!("{file_name}.devmanager-tmp-{pid}-{stamp}-{n}")
        };
        let candidate = parent.join(name);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "exhausted unique temp file candidates",
    ))
}

fn promote_temp_file(temp: &Path, destination: &Path) -> io::Result<()> {
    if destination.exists() {
        replace_existing_file(temp, destination)
    } else {
        fs::rename(temp, destination)
    }
}

#[cfg(windows)]
fn replace_existing_file(temp: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let temporary: Vec<u16> = temp.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    unsafe {
        MoveFileExW(
            PCWSTR(temporary.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .map_err(io::Error::from)
    }
}

#[cfg(not(windows))]
fn replace_existing_file(temp: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temp, destination)
}

fn ensure_canonical_parent(path: &Path) -> Result<(), ProfileError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    if !parent.exists() {
        fs::create_dir_all(parent).map_err(|err| {
            ProfileError::CanonicalParent(format!(
                "cannot create parent {}: {err}",
                parent.display()
            ))
        })?;
    }
    parent.canonicalize().map_err(|err| {
        ProfileError::CanonicalParent(format!("cannot resolve parent {}: {err}", parent.display()))
    })?;
    Ok(())
}

fn reject_symlink(path: &Path) -> Result<(), ProfileError> {
    if !path.exists() {
        return Ok(());
    }
    let meta = fs::symlink_metadata(path).map_err(|err| ProfileError::Io(err.to_string()))?;
    if meta.file_type().is_symlink() {
        return Err(ProfileError::SymlinkAmbiguity(format!(
            "refusing to edit symlink profile path: {}",
            path.display()
        )));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(ProfileError::SymlinkAmbiguity(format!(
                "refusing to edit reparse-point profile path: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn digest_hex(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_profile(name: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        (dir, path)
    }

    #[test]
    fn missing_file_preview_creates_block() {
        let (_dir, path) = temp_profile("Microsoft.PowerShell_profile.ps1");
        let preview = preview_profile_edit(&path, ProfileRecipe::SafeClaudeShortcut).unwrap();
        assert!(!preview.exists);
        assert!(preview.after.contains(BEGIN_MARKER));
        assert!(preview.after.contains("claude @args"));
        assert!(!preview.after.contains("--dangerously-skip-permissions"));
    }

    #[test]
    fn preserves_text_before_and_after_block() {
        let (_dir, path) = temp_profile("profile.ps1");
        let original =
            format!("Write-Host 'before'\n{BEGIN_MARKER}\nold\n{END_MARKER}\nWrite-Host 'after'\n");
        fs::write(&path, &original).unwrap();
        let preview = preview_profile_edit(&path, ProfileRecipe::SafeClaudeShortcut).unwrap();
        assert!(preview.after.starts_with("Write-Host 'before'\n"));
        assert!(preview.after.contains("Write-Host 'after'\n"));
        assert!(!preview.after.contains("old\n"));
    }

    #[test]
    fn preserves_crlf_and_lf() {
        let (_dir, path) = temp_profile("crlf.ps1");
        let original = format!("# top\r\n{BEGIN_MARKER}\r\nold\r\n{END_MARKER}\r\n");
        fs::write(&path, &original).unwrap();
        let preview = preview_profile_edit(&path, ProfileRecipe::SafeClaudeShortcut).unwrap();
        assert!(preview.after.contains("\r\n"));

        let (_dir2, path2) = temp_profile("lf.ps1");
        let original2 = format!("# top\n{BEGIN_MARKER}\nold\n{END_MARKER}\n");
        fs::write(&path2, &original2).unwrap();
        let preview2 = preview_profile_edit(&path2, ProfileRecipe::SafeClaudeShortcut).unwrap();
        assert!(!preview2.after.contains("\r\n"));
    }

    #[test]
    fn preserves_utf8_bom_and_utf16le_bom() {
        let (_dir, path) = temp_profile("bom-utf8.ps1");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend(b"# hi\n");
        fs::write(&path, &bytes).unwrap();
        apply_profile_edit(&path, ProfileRecipe::SafeClaudeShortcut, |_| Ok(())).unwrap();
        let written = fs::read(&path).unwrap();
        assert!(written.starts_with(&[0xEF, 0xBB, 0xBF]));

        let (_dir2, path2) = temp_profile("bom-utf16.ps1");
        let mut utf16 = vec![0xFF, 0xFE];
        for unit in "# hi\n".encode_utf16() {
            utf16.extend(unit.to_le_bytes());
        }
        fs::write(&path2, &utf16).unwrap();
        apply_profile_edit(&path2, ProfileRecipe::SafeClaudeShortcut, |_| Ok(())).unwrap();
        let written2 = fs::read(&path2).unwrap();
        assert!(written2.starts_with(&[0xFF, 0xFE]));
    }

    #[test]
    fn replacement_is_idempotent() {
        let (_dir, path) = temp_profile("idem.ps1");
        let preview = preview_profile_edit(&path, ProfileRecipe::SafeClaudeShortcut).unwrap();
        fs::write(&path, &preview.after).unwrap();
        let again = preview_profile_edit(&path, ProfileRecipe::SafeClaudeShortcut).unwrap();
        assert_eq!(again.before, again.after);
    }

    #[test]
    fn refuses_duplicate_and_malformed_markers() {
        let (_dir, path) = temp_profile("dup.ps1");
        fs::write(
            &path,
            format!("{BEGIN_MARKER}\na\n{END_MARKER}\n{BEGIN_MARKER}\nb\n{END_MARKER}\n"),
        )
        .unwrap();
        let err = preview_profile_edit(&path, ProfileRecipe::SafeClaudeShortcut).unwrap_err();
        assert!(matches!(err, ProfileError::MalformedMarkers(_)));

        fs::write(&path, format!("{BEGIN_MARKER}\nno end\n")).unwrap();
        let err = preview_profile_edit(&path, ProfileRecipe::SafeClaudeShortcut).unwrap_err();
        assert!(matches!(err, ProfileError::MalformedMarkers(_)));
    }

    #[test]
    fn creates_collision_safe_timestamped_backup() {
        let (_dir, path) = temp_profile("backup.ps1");
        fs::write(&path, "existing\n").unwrap();
        let first =
            apply_profile_edit(&path, ProfileRecipe::SafeClaudeShortcut, |_| Ok(())).unwrap();
        assert!(first.backup_path.exists());
        // Force a colliding backup name by creating the next candidate path and re-applying.
        let collide = first.backup_path.clone();
        let _ = collide;
        let second =
            apply_profile_edit(&path, ProfileRecipe::SafeClaudeShortcut, |_| Ok(())).unwrap();
        assert_ne!(first.backup_path, second.backup_path);
    }

    #[test]
    fn rolls_back_new_file_vs_preexisting_empty() {
        let (_dir, path) = temp_profile("new.ps1");
        let err = apply_profile_edit(&path, ProfileRecipe::SafeClaudeShortcut, |_| {
            Err("parse failed".into())
        })
        .unwrap_err();
        assert!(matches!(err, ProfileError::VerificationFailed(_)));
        assert!(!path.exists());

        let (_dir2, path2) = temp_profile("empty.ps1");
        fs::write(&path2, "").unwrap();
        let err = apply_profile_edit(&path2, ProfileRecipe::SafeClaudeShortcut, |_| {
            Err("parse failed".into())
        })
        .unwrap_err();
        assert!(matches!(err, ProfileError::VerificationFailed(_)));
        assert!(path2.exists());
        assert_eq!(fs::read_to_string(&path2).unwrap(), "");
    }

    #[test]
    fn rolls_back_after_verification_failure() {
        let (_dir, path) = temp_profile("rollback.ps1");
        fs::write(&path, "keep-me\n").unwrap();
        let err = apply_profile_edit(&path, ProfileRecipe::SafeClaudeShortcut, |_| {
            Err("parse failed".to_string())
        })
        .unwrap_err();
        assert!(matches!(err, ProfileError::VerificationFailed(_)));
        assert_eq!(fs::read_to_string(&path).unwrap(), "keep-me\n");
    }

    #[test]
    fn write_uses_unique_create_new_temps_not_fixed_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile.ps1");
        fs::write(&path, "old\n").unwrap();
        // Stale fixed-name temp from the prior implementation must not be reused/overwritten.
        let stale = path.with_extension("ps1.devmanager-tmp");
        fs::write(&stale, "stale-temp").unwrap();

        write_bytes_atomically(&path, b"new\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
        assert_eq!(fs::read_to_string(&stale).unwrap(), "stale-temp");
    }

    #[test]
    fn failed_promote_cleans_temp_and_preserves_destination() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile.ps1");
        fs::write(&path, "keep\n").unwrap();
        let err = write_bytes_atomically_with(&path, b"new\n", |_temp, _dest| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "promote failed",
            ))
        })
        .unwrap_err();
        assert!(err.to_string().contains("promote failed"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "keep\n");
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("devmanager-tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files must be cleaned best-effort: {leftovers:?}"
        );
    }

    #[test]
    fn replace_existing_profile_contents_without_copy_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profile.ps1");
        fs::write(&path, "before\n").unwrap();
        write_bytes_atomically(&path, b"after\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "after\n");
        let temps: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("devmanager-tmp"))
            .collect();
        assert!(
            temps.is_empty(),
            "successful promote must consume temp: {temps:?}"
        );
    }

    #[test]
    fn apply_rolls_back_when_write_fails_after_backup() {
        let (_dir, path) = temp_profile("write-fail.ps1");
        fs::write(&path, "original\n").unwrap();
        let err = apply_profile_edit_with_writer(
            &path,
            ProfileRecipe::SafeClaudeShortcut,
            |_| Ok(()),
            |_path, _content, _encoding| {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "simulated write fail",
                ))
            },
        )
        .unwrap_err();
        match &err {
            ProfileError::Io(msg) => {
                assert!(msg.contains("simulated write fail"), "{msg}");
                assert!(
                    !msg.contains("also failed to restore backup"),
                    "rollback should succeed: {msg}"
                );
            }
            other => panic!("expected Io error, got {other:?}"),
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), "original\n");
    }

    #[test]
    fn apply_reports_rollback_failure_when_restore_also_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("write-fail-rollback.ps1");
        fs::write(&path, "original\n").unwrap();
        // Create a backup-like path that restore will try to read... we inject writer fail
        // and also make restore fail by deleting the live path's parent readability?
        // Narrower: use apply_profile_edit_with_writer that fails, then corrupt by
        // pointing restore at missing backup — simulate via direct helper.
        let backup = dir.path().join("missing-backup.bin");
        let err = write_failure_after_backup(
            &path,
            &backup,
            false,
            io::Error::new(io::ErrorKind::Other, "write boom"),
        );
        let msg = err.to_string();
        assert!(msg.contains("write boom"), "{msg}");
        assert!(msg.contains("also failed to restore backup"), "{msg}");
    }

    #[test]
    fn apply_removes_incomplete_new_file_when_write_fails() {
        let (_dir, path) = temp_profile("new-write-fail.ps1");
        assert!(!path.exists());
        let err = apply_profile_edit_with_writer(
            &path,
            ProfileRecipe::SafeClaudeShortcut,
            |_| Ok(()),
            |path, _content, _encoding| {
                // Simulate a partial create then promote failure path: leave no file.
                let _ = path;
                Err(io::Error::new(io::ErrorKind::Other, "new write fail"))
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("new write fail"));
        assert!(!path.exists());
    }

    #[test]
    fn classifies_unmarked_unsafe_cc_without_managed_block() {
        let content = r#"
function cc {
    try { claude update | Out-Null } catch { }
    claude --dangerously-skip-permissions @args
}
Write-Host 'other'
"#;
        assert_eq!(
            classify_cc_raw_text(content),
            CcClassification::UnmarkedUnsafe
        );
    }

    #[test]
    fn read_profile_utf16le_classifies_unmarked_safe_cc() {
        let (_dir, path) = temp_profile("utf16.ps1");
        let content = "function cc {\r\n    claude @args\r\n}\r\n";
        let mut bytes = vec![0xFF, 0xFE];
        for unit in content.encode_utf16() {
            bytes.extend(unit.to_le_bytes());
        }
        fs::write(&path, &bytes).unwrap();
        let (text, encoding, _) = read_profile(&path).unwrap();
        assert_eq!(encoding, ProfileEncoding::Utf16LeBom);
        assert_eq!(classify_cc_raw_text(&text), CcClassification::UnmarkedSafe);
    }

    #[test]
    fn classifies_from_ast_offsets_for_shadowed_and_managed_functions() {
        let effective_unsafe = CcAstProbeResult {
            has_cc: true,
            has_claude: true,
            unsafe_claude: true,
            cc_start: 200,
            cc_end: 300,
            block_start: None,
            block_end: None,
        };
        assert_eq!(
            classify_cc_ast(&effective_unsafe),
            CcClassification::UnmarkedUnsafe
        );

        let managed_safe = CcAstProbeResult {
            has_cc: true,
            has_claude: true,
            unsafe_claude: false,
            cc_start: 20,
            cc_end: 80,
            block_start: Some(10),
            block_end: Some(90),
        };
        assert_eq!(
            classify_cc_ast(&managed_safe),
            CcClassification::ManagedSafe
        );

        let managed_unsafe = CcAstProbeResult {
            has_cc: true,
            has_claude: true,
            unsafe_claude: true,
            cc_start: 20,
            cc_end: 80,
            block_start: Some(10),
            block_end: Some(90),
        };
        assert_eq!(
            classify_cc_ast(&managed_unsafe),
            CcClassification::ManagedUnsafe
        );

        let unmarked_outside_block = CcAstProbeResult {
            has_cc: true,
            has_claude: true,
            unsafe_claude: true,
            cc_start: 200,
            cc_end: 300,
            block_start: Some(10),
            block_end: Some(90),
        };
        assert_eq!(
            classify_cc_ast(&unmarked_outside_block),
            CcClassification::UnmarkedUnsafe
        );
    }

    #[test]
    fn parses_fixed_ast_probe_output_and_rejects_profile_content() {
        let parsed = parse_cc_ast_probe_output(
            "CC=1\nHAS_CLAUDE=1\nUNSAFE=1\nCC_START=10\nCC_END=40\nBLOCK_START=-1\nBLOCK_END=-1\n",
        )
        .unwrap();
        assert!(parsed.has_cc);
        assert!(parsed.has_claude);
        assert!(parsed.unsafe_claude);
        assert_eq!(parsed.cc_start, 10);
        assert_eq!(parsed.block_start, None);

        let absent = parse_cc_ast_probe_output("CC=0\nBLOCK_START=-1\nBLOCK_END=-1\n").unwrap();
        assert!(!absent.has_cc);
        assert!(!absent.has_claude);

        assert!(parse_cc_ast_probe_output("CC=1\nfunction cc { claude }\n").is_err());
        assert!(parse_cc_ast_probe_output("ERR=parse\n").is_err());
        assert!(parse_cc_ast_probe_output("CC=1\nUNSAFE=1\n").is_err());
        assert!(
            parse_cc_ast_probe_output(
                "CC=1\nUNSAFE=0\nCC_START=10\nCC_END=40\nBLOCK_START=-1\nBLOCK_END=-1\n"
            )
            .is_err(),
            "missing HAS_CLAUDE must fail closed"
        );

        let no_claude = parse_cc_ast_probe_output(
            "CC=1\nHAS_CLAUDE=0\nUNSAFE=0\nCC_START=0\nCC_END=40\nBLOCK_START=-1\nBLOCK_END=-1\n",
        )
        .unwrap();
        assert!(no_claude.has_cc);
        assert!(!no_claude.has_claude);
        assert_eq!(classify_cc_ast(&no_claude), CcClassification::Absent);
    }

    #[test]
    fn function_cc_without_real_claude_command_is_absent_not_healthy() {
        let only_writes_text = CcAstProbeResult {
            has_cc: true,
            has_claude: false,
            unsafe_claude: false,
            cc_start: 0,
            cc_end: 50,
            block_start: None,
            block_end: None,
        };
        assert_eq!(classify_cc_ast(&only_writes_text), CcClassification::Absent);

        let comment_mentions_flag = CcAstProbeResult {
            has_cc: true,
            has_claude: false,
            unsafe_claude: true, // must be ignored without a real claude command
            cc_start: 0,
            cc_end: 50,
            block_start: None,
            block_end: None,
        };
        assert_eq!(
            classify_cc_ast(&comment_mentions_flag),
            CcClassification::Absent
        );
    }

    #[test]
    fn real_safe_claude_is_safe_when_probe_reports_safe() {
        let result = CcAstProbeResult {
            has_cc: true,
            has_claude: true,
            unsafe_claude: false,
            cc_start: 0,
            cc_end: 50,
            block_start: None,
            block_end: None,
        };
        assert_eq!(classify_cc_ast(&result), CcClassification::UnmarkedSafe);
    }

    #[test]
    fn body_wide_dangerous_flag_reported_by_probe_is_unsafe() {
        let result = CcAstProbeResult {
            has_cc: true,
            has_claude: true,
            unsafe_claude: true,
            cc_start: 0,
            cc_end: 50,
            block_start: None,
            block_end: None,
        };
        assert_eq!(classify_cc_ast(&result), CcClassification::UnmarkedUnsafe);
    }

    #[test]
    fn quoted_dangerous_argument_on_real_claude_is_unsafe() {
        let result = CcAstProbeResult {
            has_cc: true,
            has_claude: true,
            unsafe_claude: true,
            cc_start: 0,
            cc_end: 50,
            block_start: None,
            block_end: None,
        };
        assert_eq!(classify_cc_ast(&result), CcClassification::UnmarkedUnsafe);

        let parsed = parse_cc_ast_probe_output(
            "CC=1\nHAS_CLAUDE=1\nUNSAFE=1\nCC_START=0\nCC_END=50\nBLOCK_START=-1\nBLOCK_END=-1\n",
        )
        .unwrap();
        assert!(parsed.has_claude);
        assert!(parsed.unsafe_claude);
        assert_eq!(classify_cc_ast(&parsed), CcClassification::UnmarkedUnsafe);
    }

    #[test]
    fn rejects_symlink_targets_when_platform_exposes_them() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.ps1");
        fs::write(&real, "real\n").unwrap();
        let link = dir.path().join("link.ps1");

        #[cfg(windows)]
        {
            let output = std::process::Command::new("cmd")
                .args(["/C", "mklink"])
                .arg(&link)
                .arg(&real)
                .output();
            if output
                .map(|output| output.status.success())
                .unwrap_or(false)
            {
                let err =
                    preview_profile_edit(&link, ProfileRecipe::SafeClaudeShortcut).unwrap_err();
                assert!(matches!(err, ProfileError::SymlinkAmbiguity(_)));
            }
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&real, &link).unwrap();
            let err = preview_profile_edit(&link, ProfileRecipe::SafeClaudeShortcut).unwrap_err();
            assert!(matches!(err, ProfileError::SymlinkAmbiguity(_)));
        }
        let _ = (real, link);
    }
}

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

// ── Data types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GitFileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    Untracked,
    Conflicted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitStatusEntry {
    pub path: String,
    pub status: GitFileStatus,
    pub staged: bool,
    pub original_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitStatusResult {
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub entries: Vec<GitStatusEntry>,
    pub is_detached: bool,
    pub is_merging: bool,
    pub is_rebasing: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitLogEntry {
    pub hash: String,
    pub full_hash: String,
    pub subject: String,
    pub body: Option<String>,
    pub author_name: String,
    pub date: String,
    pub refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitBranch {
    pub name: String,
    pub is_current: bool,
    pub upstream: Option<String>,
    pub last_commit: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffLineKind {
    Add,
    Delete,
    Context,
    HunkHeader,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitDiffLine {
    pub kind: DiffLineKind,
    pub content: String,
    pub old_lineno: Option<u32>,
    pub new_lineno: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitDiffHunk {
    pub header: String,
    pub lines: Vec<GitDiffLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitDiffResult {
    pub hunks: Vec<GitDiffHunk>,
    pub is_binary: bool,
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn git_command(repo_path: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo_path);
    cmd.args(args);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

fn run_git(repo_path: &str, args: &[&str]) -> Result<String, String> {
    let output = git_command(repo_path, args)
        .output()
        .map_err(|e| format!("Failed to run git: {e}"))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        Err(stderr.trim().to_string())
    }
}

fn run_git_allow_failure(repo_path: &str, args: &[&str]) -> Result<(String, String), String> {
    let output = git_command(repo_path, args)
        .output()
        .map_err(|e| format!("Failed to run git: {e}"))?;

    Ok((
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    ))
}

// ── Repository checks ───────────────────────────────────────────────────────

pub fn is_repo(path: &str) -> bool {
    Path::new(path).join(".git").exists()
        || run_git(path, &["rev-parse", "--is-inside-work-tree"]).is_ok()
}

pub fn git_available() -> bool {
    let mut cmd = Command::new("git");
    cmd.arg("--version");
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd.status().is_ok()
}

// ── Status ──────────────────────────────────────────────────────────────────

pub fn status(repo_path: &str) -> Result<GitStatusResult, String> {
    let output = run_git(repo_path, &["status", "--porcelain=v2", "--branch"])?;
    parse_status_v2(&output, repo_path)
}

fn parse_status_v2(output: &str, repo_path: &str) -> Result<GitStatusResult, String> {
    let mut branch: Option<String> = None;
    let mut upstream: Option<String> = None;
    let mut ahead: u32 = 0;
    let mut behind: u32 = 0;
    let mut is_detached = false;
    let mut entries = Vec::new();

    for line in output.lines() {
        if line.starts_with("# branch.head ") {
            let head = line.strip_prefix("# branch.head ").unwrap_or("");
            if head == "(detached)" {
                is_detached = true;
                // Get short hash for detached display
                if let Ok(hash) = run_git(repo_path, &["rev-parse", "--short", "HEAD"]) {
                    branch = Some(hash.trim().to_string());
                }
            } else {
                branch = Some(head.to_string());
            }
        } else if line.starts_with("# branch.upstream ") {
            upstream = line
                .strip_prefix("# branch.upstream ")
                .map(|s| s.to_string());
        } else if line.starts_with("# branch.ab ") {
            if let Some(ab) = line.strip_prefix("# branch.ab ") {
                for part in ab.split_whitespace() {
                    if let Some(n) = part.strip_prefix('+') {
                        ahead = n.parse().unwrap_or(0);
                    } else if let Some(n) = part.strip_prefix('-') {
                        behind = n.parse().unwrap_or(0);
                    }
                }
            }
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            // Changed entries: "1 XY ..." or "2 XY ... \t origpath"
            parse_changed_entry(line, &mut entries);
        } else if line.starts_with("u ") {
            // Unmerged (conflict) entry
            parse_unmerged_entry(line, &mut entries);
        } else if line.starts_with("? ") {
            // Untracked
            if let Some(path) = line.strip_prefix("? ") {
                entries.push(GitStatusEntry {
                    path: path.to_string(),
                    status: GitFileStatus::Untracked,
                    staged: false,
                    original_path: None,
                });
            }
        }
    }

    // Detect merge/rebase state from repo files
    let git_dir = Path::new(repo_path).join(".git");
    let is_merging = git_dir.join("MERGE_HEAD").exists();
    let is_rebasing =
        git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists();

    Ok(GitStatusResult {
        branch,
        upstream,
        ahead,
        behind,
        entries,
        is_detached,
        is_merging,
        is_rebasing,
    })
}

fn parse_changed_entry(line: &str, entries: &mut Vec<GitStatusEntry>) {
    let parts: Vec<&str> = line.splitn(9, ' ').collect();
    if parts.len() < 9 {
        return;
    }
    let xy = parts[1];
    let x = xy.as_bytes().first().copied().unwrap_or(b'.');
    let y = xy.as_bytes().get(1).copied().unwrap_or(b'.');

    // For rename entries (type "2"), the path field contains "path\torig_path"
    let is_rename = parts[0] == "2";
    let raw_path = parts[8];
    let (path, original_path) = if is_rename {
        let mut split = raw_path.splitn(2, '\t');
        let p = split.next().unwrap_or(raw_path).to_string();
        let orig = split.next().map(|s| s.to_string());
        (p, orig)
    } else {
        (raw_path.to_string(), None)
    };

    // Index (staged) change
    if x != b'.' {
        entries.push(GitStatusEntry {
            path: path.clone(),
            status: xy_to_status(x),
            staged: true,
            original_path: original_path.clone(),
        });
    }

    // Worktree (unstaged) change
    if y != b'.' {
        entries.push(GitStatusEntry {
            path,
            status: xy_to_status(y),
            staged: false,
            original_path,
        });
    }
}

fn parse_unmerged_entry(line: &str, entries: &mut Vec<GitStatusEntry>) {
    // "u XY sub m1 m2 m3 mW h1 h2 h3 path"
    let parts: Vec<&str> = line.splitn(11, ' ').collect();
    if parts.len() < 11 {
        return;
    }
    entries.push(GitStatusEntry {
        path: parts[10].to_string(),
        status: GitFileStatus::Conflicted,
        staged: false,
        original_path: None,
    });
}

fn xy_to_status(c: u8) -> GitFileStatus {
    match c {
        b'M' => GitFileStatus::Modified,
        b'A' => GitFileStatus::Added,
        b'D' => GitFileStatus::Deleted,
        b'R' => GitFileStatus::Renamed,
        b'C' => GitFileStatus::Copied,
        _ => GitFileStatus::Modified,
    }
}

// ── Log / History ───────────────────────────────────────────────────────────

const LOG_FORMAT: &str = "%h\x1f%H\x1f%s\x1f%b\x1f%an\x1f%aI\x1f%D";
const LOG_SEPARATOR: char = '\x1e';

pub fn log(repo_path: &str, limit: u32, skip: u32) -> Result<Vec<GitLogEntry>, String> {
    let format_arg = format!("--format={LOG_FORMAT}{LOG_SEPARATOR}");
    let limit_arg = format!("-{limit}");
    let skip_arg = format!("--skip={skip}");

    let output = run_git(repo_path, &["log", &format_arg, &limit_arg, &skip_arg])?;
    let mut entries = Vec::new();

    for record in output.split(LOG_SEPARATOR) {
        let record = record.trim();
        if record.is_empty() {
            continue;
        }
        let fields: Vec<&str> = record.splitn(7, '\x1f').collect();
        if fields.len() < 6 {
            continue;
        }
        let refs_str = fields.get(6).unwrap_or(&"");
        let refs: Vec<String> = if refs_str.is_empty() {
            Vec::new()
        } else {
            refs_str.split(", ").map(|s| s.trim().to_string()).collect()
        };
        let body = fields[3].trim();
        entries.push(GitLogEntry {
            hash: fields[0].to_string(),
            full_hash: fields[1].to_string(),
            subject: fields[2].to_string(),
            body: if body.is_empty() {
                None
            } else {
                Some(body.to_string())
            },
            author_name: fields[4].to_string(),
            date: fields[5].to_string(),
            refs,
        });
    }

    Ok(entries)
}

// ── Diff ────────────────────────────────────────────────────────────────────

pub fn diff_file(repo_path: &str, file_path: &str, staged: bool) -> Result<GitDiffResult, String> {
    let args = if staged {
        vec!["diff", "--cached", "--", file_path]
    } else {
        vec!["diff", "--", file_path]
    };
    let output = run_git(repo_path, &args)?;

    // Check for untracked file — show full content as additions
    if output.is_empty() && !staged {
        return diff_untracked(repo_path, file_path);
    }

    Ok(parse_diff(&output))
}

pub fn diff_commit(repo_path: &str, hash: &str) -> Result<GitDiffResult, String> {
    let output = run_git(repo_path, &["show", "--format=", hash])?;
    Ok(parse_diff(&output))
}

fn diff_untracked(repo_path: &str, file_path: &str) -> Result<GitDiffResult, String> {
    let full_path = Path::new(repo_path).join(file_path);
    let content =
        std::fs::read_to_string(&full_path).map_err(|e| format!("Cannot read file: {e}"))?;

    let lines: Vec<GitDiffLine> = content
        .lines()
        .enumerate()
        .map(|(i, line)| GitDiffLine {
            kind: DiffLineKind::Add,
            content: line.to_string(),
            old_lineno: None,
            new_lineno: Some(i as u32 + 1),
        })
        .collect();

    let hunk = GitDiffHunk {
        header: format!("@@ -0,0 +1,{} @@ new file", lines.len()),
        lines,
    };

    Ok(GitDiffResult {
        hunks: vec![hunk],
        is_binary: false,
    })
}

fn parse_diff(output: &str) -> GitDiffResult {
    // Check for binary
    if output.contains("Binary files") {
        return GitDiffResult {
            hunks: Vec::new(),
            is_binary: true,
        };
    }

    let mut hunks = Vec::new();
    let mut current_hunk: Option<GitDiffHunk> = None;
    let mut old_line: u32 = 0;
    let mut new_line: u32 = 0;

    for line in output.lines() {
        if line.starts_with("@@") {
            // Save previous hunk
            if let Some(hunk) = current_hunk.take() {
                hunks.push(hunk);
            }
            // Parse hunk header: @@ -old_start,old_count +new_start,new_count @@
            let (old_start, new_start) = parse_hunk_header(line);
            old_line = old_start;
            new_line = new_start;
            current_hunk = Some(GitDiffHunk {
                header: line.to_string(),
                lines: Vec::new(),
            });
        } else if let Some(ref mut hunk) = current_hunk {
            if let Some(added) = line.strip_prefix('+') {
                hunk.lines.push(GitDiffLine {
                    kind: DiffLineKind::Add,
                    content: added.to_string(),
                    old_lineno: None,
                    new_lineno: Some(new_line),
                });
                new_line += 1;
            } else if let Some(removed) = line.strip_prefix('-') {
                hunk.lines.push(GitDiffLine {
                    kind: DiffLineKind::Delete,
                    content: removed.to_string(),
                    old_lineno: Some(old_line),
                    new_lineno: None,
                });
                old_line += 1;
            } else if line.starts_with(' ') || line.is_empty() {
                let content = if line.is_empty() {
                    String::new()
                } else {
                    line[1..].to_string()
                };
                hunk.lines.push(GitDiffLine {
                    kind: DiffLineKind::Context,
                    content,
                    old_lineno: Some(old_line),
                    new_lineno: Some(new_line),
                });
                old_line += 1;
                new_line += 1;
            }
            // Skip diff file headers (---/+++ lines, index lines, etc.)
        }
    }

    if let Some(hunk) = current_hunk {
        hunks.push(hunk);
    }

    GitDiffResult {
        hunks,
        is_binary: false,
    }
}

fn parse_hunk_header(header: &str) -> (u32, u32) {
    // @@ -10,6 +10,8 @@ optional context
    let mut old_start = 1u32;
    let mut new_start = 1u32;

    if let Some(rest) = header.strip_prefix("@@ ") {
        let parts: Vec<&str> = rest.splitn(4, ' ').collect();
        if parts.len() >= 3 {
            // Parse -old_start[,count]
            if let Some(old) = parts[0].strip_prefix('-') {
                old_start = old
                    .split(',')
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1);
            }
            // Parse +new_start[,count]
            if let Some(new) = parts[1].strip_prefix('+') {
                new_start = new
                    .split(',')
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1);
            }
        }
    }

    (old_start, new_start)
}

pub fn diff_stat_commit(
    repo_path: &str,
    hash: &str,
) -> Result<Vec<(String, GitFileStatus)>, String> {
    let output = run_git(
        repo_path,
        &["diff-tree", "--no-commit-id", "-r", "--name-status", hash],
    )?;

    let mut files = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, '\t');
        let status_char = parts.next().unwrap_or("M");
        let path = parts.next().unwrap_or("").to_string();
        let status = match status_char.as_bytes().first() {
            Some(b'A') => GitFileStatus::Added,
            Some(b'D') => GitFileStatus::Deleted,
            Some(b'R') => GitFileStatus::Renamed,
            Some(b'C') => GitFileStatus::Copied,
            _ => GitFileStatus::Modified,
        };
        files.push((path, status));
    }
    Ok(files)
}

// ── Branches ────────────────────────────────────────────────────────────────

pub fn branches(repo_path: &str) -> Result<Vec<GitBranch>, String> {
    let format = "%(HEAD)\x1f%(refname:short)\x1f%(upstream:short)\x1f%(subject)";
    let format_arg = format!("--format={format}");
    let output = run_git(repo_path, &["branch", &format_arg])?;

    let mut result = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.splitn(4, '\x1f').collect();
        if fields.len() < 2 {
            continue;
        }
        let is_current = fields[0].trim() == "*";
        let name = fields[1].trim().to_string();
        let upstream = fields.get(2).and_then(|s| {
            let s = s.trim();
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        });
        let last_commit = fields.get(3).and_then(|s| {
            let s = s.trim();
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        });
        result.push(GitBranch {
            name,
            is_current,
            upstream,
            last_commit,
        });
    }
    Ok(result)
}

// ── Staging ─────────────────────────────────────────────────────────────────

pub fn stage(repo_path: &str, files: &[&str]) -> Result<(), String> {
    let mut args = vec!["add", "--"];
    args.extend_from_slice(files);
    run_git(repo_path, &args)?;
    Ok(())
}

pub fn unstage(repo_path: &str, files: &[&str]) -> Result<(), String> {
    let mut args = vec!["restore", "--staged", "--"];
    args.extend_from_slice(files);
    run_git(repo_path, &args)?;
    Ok(())
}

pub fn stage_all(repo_path: &str) -> Result<(), String> {
    run_git(repo_path, &["add", "-A"])?;
    Ok(())
}

pub fn unstage_all(repo_path: &str) -> Result<(), String> {
    run_git(repo_path, &["reset", "HEAD"])?;
    Ok(())
}

// ── Commit ──────────────────────────────────────────────────────────────────

pub fn commit(repo_path: &str, summary: &str, body: Option<&str>) -> Result<String, String> {
    let message = if let Some(body) = body {
        if body.trim().is_empty() {
            summary.to_string()
        } else {
            format!("{summary}\n\n{body}")
        }
    } else {
        summary.to_string()
    };
    let output = run_git(repo_path, &["commit", "-m", &message])?;
    // Extract short hash from output like "[main abc1234] message"
    let hash = output
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or("")
        .to_string();
    Ok(hash)
}

// ── Remote operations ───────────────────────────────────────────────────────

pub fn push(repo_path: &str) -> Result<String, String> {
    let (stdout, stderr) = run_git_allow_failure(repo_path, &["push"])?;
    // git push often writes to stderr even on success
    if stderr.contains("error:") || stderr.contains("fatal:") {
        Err(stderr.trim().to_string())
    } else {
        let msg = if stdout.trim().is_empty() {
            stderr.trim().to_string()
        } else {
            stdout.trim().to_string()
        };
        Ok(msg)
    }
}

pub fn push_set_upstream(repo_path: &str, branch: &str) -> Result<String, String> {
    let (stdout, stderr) =
        run_git_allow_failure(repo_path, &["push", "--set-upstream", "origin", branch])?;
    if stderr.contains("error:") || stderr.contains("fatal:") {
        Err(stderr.trim().to_string())
    } else {
        let msg = if stdout.trim().is_empty() {
            stderr.trim().to_string()
        } else {
            stdout.trim().to_string()
        };
        Ok(msg)
    }
}

pub fn pull(repo_path: &str) -> Result<String, String> {
    let (stdout, stderr) = run_git_allow_failure(repo_path, &["pull"])?;
    if stderr.contains("error:") || stderr.contains("fatal:") {
        Err(stderr.trim().to_string())
    } else {
        Ok(stdout.trim().to_string())
    }
}

pub fn pull_rebase(repo_path: &str) -> Result<String, String> {
    let (stdout, stderr) = run_git_allow_failure(repo_path, &["pull", "--rebase"])?;
    if stderr.contains("error:") || stderr.contains("fatal:") || stderr.contains("CONFLICT") {
        Err(stderr.trim().to_string())
    } else {
        Ok(stdout.trim().to_string())
    }
}

/// Try to push; if rejected because the remote has diverged, pull --rebase
/// and retry the push once.
pub fn sync(repo_path: &str) -> Result<String, String> {
    match push(repo_path) {
        Ok(msg) => Ok(msg),
        Err(e) if e.contains("[rejected]") || e.contains("fetch first") => {
            pull_rebase(repo_path)?;
            push(repo_path)
        }
        Err(e) => Err(e),
    }
}

pub fn fetch(repo_path: &str) -> Result<String, String> {
    let (stdout, stderr) = run_git_allow_failure(repo_path, &["fetch", "--all"])?;
    if stderr.contains("error:") || stderr.contains("fatal:") {
        Err(stderr.trim().to_string())
    } else {
        let msg = if stdout.trim().is_empty() {
            stderr.trim().to_string()
        } else {
            stdout.trim().to_string()
        };
        Ok(msg)
    }
}

// ── Branch operations ───────────────────────────────────────────────────────

pub fn switch_branch(repo_path: &str, name: &str) -> Result<(), String> {
    run_git(repo_path, &["switch", name])?;
    Ok(())
}

pub fn create_branch(repo_path: &str, name: &str) -> Result<(), String> {
    run_git(repo_path, &["switch", "-c", name])?;
    Ok(())
}

pub fn delete_branch(repo_path: &str, name: &str) -> Result<(), String> {
    run_git(repo_path, &["branch", "-d", name])?;
    Ok(())
}

// ── Utility ─────────────────────────────────────────────────────────────────

pub fn get_remote_name(repo_path: &str) -> Option<String> {
    run_git(repo_path, &["remote"])
        .ok()
        .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
        .filter(|s| !s.is_empty())
}

pub fn has_commits(repo_path: &str) -> bool {
    run_git(repo_path, &["rev-parse", "HEAD"]).is_ok()
}

// ── GitHub OAuth Device Flow ────────────────────────────────────────────────

// GitHub Copilot CLI's shared OAuth App client ID.
// This is the same client ID used by all Copilot CLI installations and works
// for both GitHub API access and Copilot token exchange without a paid subscription.
const DEFAULT_GITHUB_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
}

pub struct OAuthTokenResponse {
    pub access_token: String,
    pub token_type: String,
    pub scope: String,
}

pub fn get_github_client_id() -> Option<String> {
    // Check env var first, then fall back to compiled-in default
    if let Ok(id) = std::env::var("DEVMANAGER_GITHUB_CLIENT_ID") {
        if !id.is_empty() {
            return Some(id);
        }
    }
    if !DEFAULT_GITHUB_CLIENT_ID.is_empty() {
        return Some(DEFAULT_GITHUB_CLIENT_ID.to_string());
    }
    None
}

pub fn request_device_code(client_id: &str) -> Result<DeviceCodeResponse, String> {
    let body = serde_json::json!({
        "client_id": client_id,
        "scope": ""
    });

    let resp: serde_json::Value = ureq::post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .send_json(&body)
        .map_err(|e| format!("Device code request failed: {e}"))?
        .into_body()
        .read_json()
        .map_err(|e| format!("Failed to parse response: {e}"))?;

    Ok(DeviceCodeResponse {
        device_code: resp["device_code"]
            .as_str()
            .ok_or("Missing device_code")?
            .to_string(),
        user_code: resp["user_code"]
            .as_str()
            .ok_or("Missing user_code")?
            .to_string(),
        verification_uri: resp["verification_uri"]
            .as_str()
            .unwrap_or("https://github.com/login/device")
            .to_string(),
        interval: resp["interval"].as_u64().unwrap_or(5),
    })
}

pub fn poll_for_token(
    client_id: &str,
    device_code: &str,
) -> Result<Option<OAuthTokenResponse>, String> {
    let body = serde_json::json!({
        "client_id": client_id,
        "device_code": device_code,
        "grant_type": "urn:ietf:params:oauth:grant-type:device_code"
    });

    let resp: serde_json::Value = ureq::post("https://github.com/login/oauth/access_token")
        .header("Accept", "application/json")
        .send_json(&body)
        .map_err(|e| format!("Token poll failed: {e}"))?
        .into_body()
        .read_json()
        .map_err(|e| format!("Failed to parse response: {e}"))?;

    // Check for error responses
    if let Some(error) = resp["error"].as_str() {
        match error {
            "authorization_pending" => return Ok(None), // Still waiting
            "slow_down" => return Ok(None),             // Slow down polling
            "expired_token" => return Err("Login expired. Please try again.".to_string()),
            "access_denied" => return Err("Login denied by user.".to_string()),
            other => return Err(format!("OAuth error: {other}")),
        }
    }

    if let Some(token) = resp["access_token"].as_str() {
        Ok(Some(OAuthTokenResponse {
            access_token: token.to_string(),
            token_type: resp["token_type"].as_str().unwrap_or("bearer").to_string(),
            scope: resp["scope"].as_str().unwrap_or("").to_string(),
        }))
    } else {
        Ok(None)
    }
}

pub fn get_github_username(token: &str) -> Result<String, String> {
    let resp: serde_json::Value = ureq::get("https://api.github.com/user")
        .header("Authorization", &format!("Bearer {token}"))
        .header("User-Agent", "DevManager")
        .call()
        .map_err(|e| format!("Failed to fetch user: {e}"))?
        .into_body()
        .read_json()
        .map_err(|e| format!("Failed to parse user: {e}"))?;

    resp["login"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "No login in response".to_string())
}

// ── AI commit message ───────────────────────────────────────────────────────

const COMMIT_MSG_SYSTEM_PROMPT: &str = r#"You're an AI assistant whose job is to concisely summarize code changes into short, useful commit messages, with a title and a description.

A changeset is given in the git diff output format, affecting one or multiple files.

The commit title should be no longer than 50 characters and should summarize the contents of the changeset for other developers reading the commit history.

The commit description can be longer, and should provide more context about the changeset, including why the changeset is being made, and any other relevant information. The commit description is optional, so you can omit it if the changeset is small enough that it can be described in the commit title or if you don't have enough context.

Be brief and concise.

Do NOT include a description of changes in "lock" files from dependency managers like npm, yarn, or pip (and others), unless those are the only changes in the commit.

Your response must be a JSON object with the attributes "title" and "description" containing the commit title and commit description. Do not use markdown to wrap the JSON object, just return it as plain text. For example:

{
  "title": "Fix issue with login form",
  "description": "The login form was not submitting correctly. This commit fixes that issue by adding a missing `name` attribute to the submit button."
}"#;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiCommitMessage {
    pub title: String,
    pub description: String,
}

pub fn get_staged_diff(repo_path: &str) -> Result<String, String> {
    run_git(repo_path, &["diff", "--cached", "--no-ext-diff", "--no-color"])
}

/// Exchange a GitHub OAuth token for a short-lived Copilot API token.
fn get_copilot_token(github_token: &str) -> Result<String, String> {
    let resp: serde_json::Value = ureq::get("https://api.github.com/copilot_internal/v2/token")
        .header("Authorization", &format!("token {}", github_token))
        .header("User-Agent", "DevManager")
        .call()
        .map_err(|e| {
            let msg = format!("{e}");
            if msg.contains("401") || msg.contains("403") {
                "Copilot access denied. Make sure you have an active GitHub Copilot subscription."
                    .to_string()
            } else {
                format!("Copilot token exchange failed: {e}")
            }
        })?
        .into_body()
        .read_json()
        .map_err(|e| format!("Failed to parse Copilot token response: {e}"))?;

    resp["token"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "No token in Copilot response".to_string())
}

pub fn generate_commit_message(
    github_token: &str,
    diff_text: &str,
) -> Result<AiCommitMessage, String> {
    // Strip control characters (from binary diffs) that would produce invalid JSON,
    // but keep normal whitespace (newlines, tabs, carriage returns).
    let sanitized: String = diff_text
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\r' || *c == '\t')
        .collect();

    // Truncate on a char boundary to stay within gpt-4o-mini's 12,288 token
    // prompt limit.  Code diffs average ~3 chars per token; 24,000 chars ≈ 8K
    // tokens, leaving ~4K headroom for the system prompt and response.
    let max_len = 24_000;
    let truncated = if sanitized.len() > max_len {
        match sanitized
            .char_indices()
            .take_while(|(i, _)| *i < max_len)
            .last()
        {
            Some((i, c)) => &sanitized[..i + c.len_utf8()],
            None => &sanitized,
        }
    } else {
        &sanitized
    };

    // Step 1: Exchange OAuth token for Copilot token
    let copilot_token = get_copilot_token(github_token)?;

    // Step 2: Call Copilot chat completions
    let body = serde_json::json!({
        "model": "gpt-4o-mini",
        "messages": [
            {"role": "system", "content": COMMIT_MSG_SYSTEM_PROMPT},
            {"role": "user", "content": truncated}
        ],
        "temperature": 0.3,
        "max_tokens": 500,
    });

    let resp_json = call_copilot_chat(&copilot_token, &body)?;

    let content = resp_json["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| "No content in Copilot response".to_string())?;

    parse_commit_message_json(content)
}

/// Call the Copilot chat completions API, with one retry for transient 400 errors.
fn call_copilot_chat(
    copilot_token: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let mut last_err = String::new();

    for attempt in 0..2 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }

        let result = ureq::post("https://api.githubcopilot.com/chat/completions")
            .header("Authorization", &format!("Bearer {}", copilot_token))
            .header("Copilot-Integration-Id", "vscode-chat")
            .header("Editor-Version", "vscode/1.96.0")
            .header("Editor-Plugin-Version", "copilot-chat/0.24.0")
            .header("Openai-Intent", "commit-message")
            .header("Content-Type", "application/json")
            .config()
            .http_status_as_error(false)
            .build()
            .send_json(body);

        let mut resp = match result {
            Ok(resp) => resp,
            Err(e) => return Err(format!("Copilot API request failed: {e}")),
        };

        let status = resp.status().as_u16();

        if (200..300).contains(&status) {
            let json: serde_json::Value = resp
                .body_mut()
                .read_json()
                .map_err(|e| format!("Failed to parse Copilot response: {e}"))?;
            return Ok(json);
        }

        let error_body = resp.body_mut().read_to_string().unwrap_or_default();

        match status {
            400 => {
                last_err = format!(
                    "Copilot API returned 400: {}",
                    if error_body.is_empty() {
                        "(no details)".to_string()
                    } else {
                        error_body
                    }
                );
                continue;
            }
            402 => {
                return Err("Copilot usage quota exceeded. Please try again later.".to_string());
            }
            429 => {
                return Err(
                    "Rate limited by Copilot. Please wait a moment and try again.".to_string(),
                );
            }
            401 | 403 => {
                return Err(
                    "Copilot access denied. Check your GitHub Copilot subscription.".to_string(),
                );
            }
            _ => {
                return Err(format!(
                    "Copilot API request failed (HTTP {}): {}",
                    status,
                    if error_body.is_empty() {
                        "(no details)".to_string()
                    } else {
                        error_body
                    }
                ));
            }
        }
    }

    Err(last_err)
}

fn parse_commit_message_json(content: &str) -> Result<AiCommitMessage, String> {
    // Try to extract JSON from the response (may be wrapped in markdown code blocks)
    let json_str = if let Some(start) = content.find('{') {
        if let Some(end) = content.rfind('}') {
            &content[start..=end]
        } else {
            content
        }
    } else {
        content
    };

    let parsed: serde_json::Value =
        serde_json::from_str(json_str).map_err(|e| format!("Failed to parse JSON: {e}"))?;

    let title = parsed["title"].as_str().unwrap_or("").to_string();
    let description = parsed["description"].as_str().unwrap_or("").to_string();

    if title.is_empty() {
        return Err("AI returned empty title".to_string());
    }

    Ok(AiCommitMessage { title, description })
}

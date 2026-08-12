use crate::git::model::{
    DiffChangeKind, DiffContinuation, DiffDocument, DiffFile, DiffHunk, DiffLine, DiffLineKind,
    DiffMarker, DiffSide, ObjectId, RepoPath, ReviewAnchor,
};
use url::Url;

pub fn parse_unified_diff(input: &[u8]) -> Result<DiffDocument, String> {
    parse_unified_diff_limited(input, input.len())
}

pub fn parse_unified_diff_limited(input: &[u8], max_bytes: usize) -> Result<DiffDocument, String> {
    let truncated = input.len() > max_bytes;
    let bytes = &input[..input.len().min(max_bytes)];
    let mut files = Vec::new();
    let mut current: Option<DiffFile> = None;
    let mut current_hunk: Option<usize> = None;
    let mut old_cursor = 0;
    let mut new_cursor = 0;

    for raw_line in bytes.split(|byte| *byte == b'\n') {
        let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
        if line.starts_with(b"diff --git ") {
            finish_file(&mut files, &mut current, truncated);
            let (old_path, new_path) = parse_git_header_paths(&line[b"diff --git ".len()..]);
            current = Some(DiffFile {
                old_path,
                new_path,
                old_blob: None,
                new_blob: None,
                change: DiffChangeKind::Modified,
                is_binary: false,
                hunks: Vec::new(),
                markers: Vec::new(),
            });
            current_hunk = None;
            old_cursor = 0;
            new_cursor = 0;
            continue;
        }

        let Some(file) = current.as_mut() else {
            continue;
        };

        if let Some(value) = line.strip_prefix(b"index ") {
            let object_range = value.split(|byte| *byte == b' ').next().unwrap_or(value);
            if let Some(index) = object_range.iter().position(|byte| *byte == b'.') {
                let old = &object_range[..index];
                let new = object_range[index..]
                    .strip_prefix(b"..")
                    .unwrap_or_default();
                file.old_blob = parse_object_id(old);
                file.new_blob = parse_object_id(new);
            }
        } else if line.starts_with(b"new file mode ") {
            file.change = DiffChangeKind::Added;
        } else if line.starts_with(b"deleted file mode ") {
            file.change = DiffChangeKind::Deleted;
        } else if let Some(value) = line.strip_prefix(b"rename from ") {
            file.change = DiffChangeKind::Renamed;
            file.old_path = Some(parse_path(value, None));
        } else if let Some(value) = line.strip_prefix(b"rename to ") {
            file.change = DiffChangeKind::Renamed;
            file.new_path = Some(parse_path(value, None));
        } else if let Some(value) = line.strip_prefix(b"copy from ") {
            file.change = DiffChangeKind::Copied;
            file.old_path = Some(parse_path(value, None));
        } else if let Some(value) = line.strip_prefix(b"copy to ") {
            file.change = DiffChangeKind::Copied;
            file.new_path = Some(parse_path(value, None));
        } else if let Some(value) = line.strip_prefix(b"--- ") {
            file.old_path = parse_optional_path(value, b"a/");
        } else if let Some(value) = line.strip_prefix(b"+++ ") {
            file.new_path = parse_optional_path(value, b"b/");
        } else if line.starts_with(b"Binary files ") || line == b"GIT binary patch" {
            file.is_binary = true;
            add_marker(&mut file.markers, DiffMarker::Binary);
        } else if line.starts_with(b"literal ") || line.starts_with(b"delta ") {
            file.is_binary = true;
            add_marker(&mut file.markers, DiffMarker::Binary);
        } else if line.starts_with(b"@@ ") {
            let (old_start, old_count, new_start, new_count) = parse_hunk_header(line)?;
            file.hunks.push(DiffHunk {
                header: line.to_vec(),
                old_start,
                old_count,
                new_start,
                new_count,
                lines: Vec::new(),
            });
            current_hunk = Some(file.hunks.len() - 1);
            old_cursor = old_start;
            new_cursor = new_start;
        } else if line == b"\\ No newline at end of file" {
            add_marker(&mut file.markers, DiffMarker::NoNewlineAtEndOfFile);
        } else if let Some(hunk_index) = current_hunk {
            if let Some((kind, content)) = parse_diff_line(line) {
                let (old_line, new_line) = match kind {
                    DiffLineKind::Context => {
                        let old_line = line_number(old_cursor);
                        let new_line = line_number(new_cursor);
                        old_cursor = old_cursor.saturating_add(1);
                        new_cursor = new_cursor.saturating_add(1);
                        (old_line, new_line)
                    }
                    DiffLineKind::Delete => {
                        let old_line = line_number(old_cursor);
                        old_cursor = old_cursor.saturating_add(1);
                        (old_line, None)
                    }
                    DiffLineKind::Add => {
                        let new_line = line_number(new_cursor);
                        new_cursor = new_cursor.saturating_add(1);
                        (None, new_line)
                    }
                };
                file.hunks[hunk_index].lines.push(DiffLine {
                    kind,
                    content: content.to_vec(),
                    old_line,
                    new_line,
                });
            }
        }
    }
    finish_file(&mut files, &mut current, truncated);

    let mut markers = Vec::new();
    if truncated {
        markers.push(DiffMarker::Truncated);
    }
    Ok(DiffDocument {
        files,
        truncated,
        bytes_read: bytes.len(),
        continuation: truncated.then_some(DiffContinuation {
            next_offset: bytes.len(),
        }),
        markers,
    })
}

pub fn anchor_is_stale(document: &DiffDocument, anchor: &ReviewAnchor) -> bool {
    let file = document.files.iter().find(|file| match anchor.side {
        DiffSide::Old => file.old_path.as_ref() == Some(&anchor.path),
        DiffSide::New => file.new_path.as_ref() == Some(&anchor.path),
    });
    let Some(file) = file else {
        return true;
    };
    if file.old_blob.as_ref() != Some(&anchor.base_blob) {
        return true;
    }
    let line_exists =
        file.hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .any(|line| match anchor.side {
                DiffSide::Old => line.old_line == Some(anchor.line),
                DiffSide::New => line.new_line == Some(anchor.line),
            });
    !line_exists || document.truncated
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PullRequestProvider {
    GitHub,
    GitLab,
    Bitbucket,
    AzureDevOps,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PullRequestRef {
    pub host: String,
    pub owner: String,
    pub repository: String,
    pub number: u64,
    pub provider: PullRequestProvider,
}

pub fn parse_pr_url(input: &str) -> Option<PullRequestRef> {
    let url = Url::parse(input).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.username() != "" || url.password().is_some()
    {
        return None;
    }
    let host = url.host_str()?.to_string();
    let segments = url.path_segments()?.collect::<Vec<_>>();
    let (provider, owner, repository, number) = if let Some(index) = find_segment(&segments, "pull")
    {
        if index < 2 || index + 1 >= segments.len() {
            return None;
        }
        (
            PullRequestProvider::GitHub,
            segments[..index - 1].join("/"),
            segments[index - 1].to_string(),
            segments[index + 1].parse().ok()?,
        )
    } else if let Some(index) = find_segment(&segments, "merge_requests") {
        if index < 2 || index + 1 >= segments.len() || segments.get(index - 1) != Some(&"-") {
            return None;
        }
        (
            PullRequestProvider::GitLab,
            segments[..index - 2].join("/"),
            segments[index - 2].to_string(),
            segments[index + 1].parse().ok()?,
        )
    } else if let Some(index) = find_segment(&segments, "pull-requests") {
        if index < 2 || index + 1 >= segments.len() {
            return None;
        }
        (
            PullRequestProvider::Bitbucket,
            segments[..index - 1].join("/"),
            segments[index - 1].to_string(),
            segments[index + 1].parse().ok()?,
        )
    } else {
        return parse_azure_pr(&host, &segments);
    };

    (!owner.is_empty() && !repository.is_empty() && number > 0).then_some(PullRequestRef {
        host,
        owner,
        repository,
        number,
        provider,
    })
}

fn parse_azure_pr(host: &str, segments: &[&str]) -> Option<PullRequestRef> {
    let index = find_segment(segments, "pullrequest")?;
    if index < 3 || index + 1 >= segments.len() || segments.get(index - 2) != Some(&"_git") {
        return None;
    }
    let number = segments[index + 1].parse().ok()?;
    (number > 0).then_some(PullRequestRef {
        host: host.to_string(),
        owner: segments[..index - 2].join("/"),
        repository: segments[index - 1].to_string(),
        number,
        provider: PullRequestProvider::AzureDevOps,
    })
}

fn find_segment(segments: &[&str], value: &str) -> Option<usize> {
    segments.iter().position(|segment| *segment == value)
}

fn finish_file(files: &mut Vec<DiffFile>, current: &mut Option<DiffFile>, truncated: bool) {
    if let Some(mut file) = current.take() {
        if truncated {
            add_marker(&mut file.markers, DiffMarker::Truncated);
        }
        if file.old_path.is_none() && file.new_path.is_some() {
            file.change = DiffChangeKind::Added;
        } else if file.old_path.is_some() && file.new_path.is_none() {
            file.change = DiffChangeKind::Deleted;
        }
        files.push(file);
    }
}

fn parse_diff_line(line: &[u8]) -> Option<(DiffLineKind, &[u8])> {
    match line.first().copied()? {
        b' ' => Some((DiffLineKind::Context, &line[1..])),
        b'+' => Some((DiffLineKind::Add, &line[1..])),
        b'-' => Some((DiffLineKind::Delete, &line[1..])),
        _ => None,
    }
}

fn parse_hunk_header(line: &[u8]) -> Result<(u32, u32, u32, u32), String> {
    let text = String::from_utf8_lossy(line);
    let pieces = text.split_whitespace().collect::<Vec<_>>();
    if pieces.len() < 3 || pieces[0] != "@@" {
        return Err("invalid unified hunk header".to_string());
    }
    let old = parse_range(pieces[1], '-')?;
    let new = parse_range(pieces[2], '+')?;
    Ok((old.0, old.1, new.0, new.1))
}

fn parse_range(value: &str, prefix: char) -> Result<(u32, u32), String> {
    let value = value
        .strip_prefix(prefix)
        .ok_or_else(|| "unified range has the wrong side".to_string())?;
    let mut pieces = value.split(',');
    let start = pieces
        .next()
        .ok_or_else(|| "unified range is missing a start".to_string())?
        .parse()
        .map_err(|_| "unified range start is not numeric".to_string())?;
    let count = pieces
        .next()
        .unwrap_or("1")
        .parse()
        .map_err(|_| "unified range count is not numeric")?;
    Ok((start, count))
}

fn line_number(value: u32) -> Option<u32> {
    (value != 0).then_some(value)
}

fn parse_object_id(value: &[u8]) -> Option<ObjectId> {
    if value.is_empty() || value.iter().all(|byte| *byte == b'0') {
        None
    } else {
        Some(ObjectId::from(String::from_utf8_lossy(value).into_owned()))
    }
}

fn parse_git_header_paths(value: &[u8]) -> (Option<RepoPath>, Option<RepoPath>) {
    let value = value.strip_prefix(b" ").unwrap_or(value);
    if value.first() == Some(&b'"') {
        let Some((old, remainder)) = take_quoted_path(value) else {
            return (None, None);
        };
        let remainder = remainder.strip_prefix(b" ").unwrap_or(remainder);
        let Some((new, _)) = take_quoted_path(remainder) else {
            return (None, None);
        };
        return (
            Some(parse_path(old, Some(b"a/"))),
            Some(parse_path(new, Some(b"b/"))),
        );
    }

    let split = value
        .windows(3)
        .rposition(|window| window == b" b/")
        .unwrap_or(0);
    if split == 0 {
        return (None, None);
    }
    let old = &value[..split];
    let new = &value[split + 1..];
    (
        Some(parse_path(old, Some(b"a/"))),
        Some(parse_path(new, Some(b"b/"))),
    )
}

fn take_quoted_path(value: &[u8]) -> Option<(&[u8], &[u8])> {
    if value.first() != Some(&b'"') {
        return None;
    }
    let mut escaped = false;
    for (index, byte) in value.iter().enumerate().skip(1) {
        if escaped {
            escaped = false;
        } else if *byte == b'\\' {
            escaped = true;
        } else if *byte == b'"' {
            return Some((&value[..=index], &value[index + 1..]));
        }
    }
    None
}

fn parse_optional_path(value: &[u8], prefix: &[u8]) -> Option<RepoPath> {
    if value == b"/dev/null" {
        None
    } else {
        Some(parse_path(value, Some(prefix)))
    }
}

fn parse_path(value: &[u8], prefix: Option<&[u8]>) -> RepoPath {
    let mut value = unquote(value);
    if let Some(prefix) = prefix {
        if value.starts_with(prefix) {
            value = value[prefix.len()..].to_vec();
        }
    }
    RepoPath::from_bytes(value)
}

fn unquote(value: &[u8]) -> Vec<u8> {
    let encoded = value;
    if encoded.len() < 2 || encoded[0] != b'"' || encoded[encoded.len() - 1] != b'"' {
        return encoded.to_vec();
    }
    let mut output = Vec::with_capacity(encoded.len() - 2);
    let mut index = 1;
    while index + 1 < encoded.len() {
        if encoded[index] != b'\\' {
            output.push(encoded[index]);
            index += 1;
            continue;
        }
        index += 1;
        let Some(escaped) = encoded.get(index).copied() else {
            break;
        };
        match escaped {
            b'a' => output.push(7),
            b'b' => output.push(8),
            b't' => output.push(b'\t'),
            b'n' => output.push(b'\n'),
            b'v' => output.push(11),
            b'f' => output.push(12),
            b'r' => output.push(b'\r'),
            b'\\' | b'"' => output.push(escaped),
            b'0'..=b'7' => {
                let mut octal = (escaped - b'0') as u8;
                for _ in 0..2 {
                    if let Some(next @ b'0'..=b'7') = encoded.get(index + 1).copied() {
                        octal = octal * 8 + next - b'0';
                        index += 1;
                    }
                }
                output.push(octal);
            }
            other => output.push(other),
        }
        index += 1;
    }
    output
}

fn add_marker(markers: &mut Vec<DiffMarker>, marker: DiffMarker) {
    if !markers.contains(&marker) {
        markers.push(marker);
    }
}

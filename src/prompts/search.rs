use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::domain::id::PromptHistoryId;

use super::history::{PromptHistoryError, PromptHistoryErrorCode, SOURCE_HISTORY, SOURCE_SAVED};

pub const MAX_PROMPT_SEARCH_QUERY_BYTES: usize = 512;
pub const MAX_PROMPT_SEARCH_PAGE: usize = 100;
pub const MAX_PROMPT_SEARCH_TERMS: usize = 16;
pub const MAX_PROMPT_SEARCH_PHRASES: usize = 8;
pub const MAX_PROMPT_SEARCH_TAGS: usize = 8;
pub const MAX_PROMPT_SEARCH_HIGHLIGHTS: usize = 16;

const DEFAULT_SEARCH_WORK_UNITS: usize = 8_192;
const DEFAULT_SEARCH_MAX_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptSearchSource {
    Saved,
    History,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSearchCursor {
    submitted_at_ms: i64,
    source_id: [u8; 16],
    inclusive: bool,
    source: PromptSearchSource,
    query_sha256: [u8; 32],
    epoch: i64,
    high_water: i64,
    schema_version: i64,
}

impl PromptSearchCursor {
    #[cfg(test)]
    pub(crate) fn with_bind(
        &self,
        source: PromptSearchSource,
        query_sha256: [u8; 32],
        epoch: i64,
        high_water: i64,
        schema_version: i64,
    ) -> Self {
        Self {
            submitted_at_ms: self.submitted_at_ms,
            source_id: self.source_id,
            inclusive: self.inclusive,
            source,
            query_sha256,
            epoch,
            high_water,
            schema_version,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PromptSearchQuery {
    pub text: String,
    pub source: PromptSearchSource,
    pub cursor: Option<PromptSearchCursor>,
    pub page_size: usize,
}

impl std::fmt::Debug for PromptSearchQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchQuery")
            .field("source", &self.source)
            .field("page_size", &self.page_size)
            .field("has_cursor", &self.cursor.is_some())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HighlightRange {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, PartialEq, Eq)]
pub struct PromptSearchHit {
    pub source: PromptSearchSource,
    pub history_id: Option<PromptHistoryId>,
    pub submitted_at_ms: i64,
    pub body: String,
    pub highlights: Vec<HighlightRange>,
    pub(crate) source_id: [u8; 16],
}

impl std::fmt::Debug for PromptSearchHit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchHit")
            .field("source", &self.source)
            .field("history_id", &self.history_id)
            .field("submitted_at_ms", &self.submitted_at_ms)
            .field("highlights", &self.highlights.len())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptSearchStatus {
    Complete,
    Partial,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSearchPage {
    pub hits: Vec<PromptSearchHit>,
    pub next: Option<PromptSearchCursor>,
    pub status: PromptSearchStatus,
}

struct ParsedSearch {
    match_sql: String,
    highlight_terms: Vec<String>,
}

#[derive(Debug)]
pub struct PromptSearchBudget<'a> {
    deadline: Option<Instant>,
    cancellation: Option<&'a AtomicBool>,
    max_bytes: Option<usize>,
    work_limit: Option<usize>,
}

impl Default for PromptSearchBudget<'_> {
    fn default() -> Self {
        Self {
            deadline: None,
            cancellation: None,
            max_bytes: Some(DEFAULT_SEARCH_MAX_BYTES),
            work_limit: Some(DEFAULT_SEARCH_WORK_UNITS),
        }
    }
}

impl<'a> PromptSearchBudget<'a> {
    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    pub fn with_cancellation(mut self, cancellation: &'a AtomicBool) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = Some(max_bytes);
        self
    }

    pub fn with_work_limit(mut self, work_limit: usize) -> Self {
        self.work_limit = Some(work_limit);
        self
    }

    pub(crate) fn check(
        &self,
        work_used: usize,
        bytes_used: usize,
    ) -> Result<bool, PromptHistoryError> {
        if self
            .cancellation
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
            || self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(PromptHistoryError::from_code(
                PromptHistoryErrorCode::Cancelled,
            ));
        }
        if self.work_limit.is_some_and(|limit| work_used > limit)
            || self.max_bytes.is_some_and(|limit| bytes_used > limit)
        {
            return Ok(false);
        }
        Ok(true)
    }
}

pub(crate) fn execute_search(
    conn: &Connection,
    query: &PromptSearchQuery,
    budget: PromptSearchBudget<'_>,
) -> Result<PromptSearchPage, PromptHistoryError> {
    if query.page_size == 0 || query.page_size > MAX_PROMPT_SEARCH_PAGE {
        return Err(PromptHistoryError::from_code(
            PromptHistoryErrorCode::PageTooLarge,
        ));
    }
    if !budget.check(0, 0)? {
        return Ok(PromptSearchPage {
            hits: Vec::new(),
            next: query.cursor.clone(),
            status: PromptSearchStatus::Partial,
        });
    }
    let parsed = parse_search_query(&query.text)?;
    let query_sha256 = query_fingerprint(&query.text);
    let (epoch, high_water) = load_search_cursor_bind(conn)?;
    let schema_version = cursor_schema_version(conn)?;
    if let Some(cursor) = query.cursor.as_ref() {
        if cursor.source != query.source
            || cursor.query_sha256 != query_sha256
            || cursor.epoch != epoch
            || cursor.high_water != high_water
            || cursor.schema_version != schema_version
        {
            return Err(PromptHistoryError::from_code(
                PromptHistoryErrorCode::InvalidQuery,
            ));
        }
    }
    let source = match query.source {
        PromptSearchSource::History => SOURCE_HISTORY,
        PromptSearchSource::Saved => SOURCE_SAVED,
    };
    let page_size = u32::try_from(query.page_size)
        .map_err(|_| PromptHistoryError::from_code(PromptHistoryErrorCode::PageTooLarge))?;
    let fetch_limit = i64::from(page_size) + 1;
    let cursor_ms = query.cursor.as_ref().map(|cursor| cursor.submitted_at_ms);
    let cursor_id = query
        .cursor
        .as_ref()
        .map(|cursor| cursor.source_id.to_vec());
    let inclusive = query.cursor.as_ref().is_some_and(|cursor| cursor.inclusive);
    let sql = "\
        SELECT s.source_kind, s.source_id, h.submitted_at_ms, h.prompt_history_id
        FROM prompt_search AS s
        LEFT JOIN prompt_history AS h
          ON s.source_kind = 'history'
         AND h.prompt_history_id = s.source_id
        WHERE s.source_kind = ?1
          AND prompt_search MATCH ?2
          AND (
                ?3 IS NULL
                OR COALESCE(h.submitted_at_ms, 0) < ?3
                OR (COALESCE(h.submitted_at_ms, 0) = ?3 AND s.source_id < ?4)
                OR (?6 != 0 AND COALESCE(h.submitted_at_ms, 0) = ?3 AND s.source_id = ?4)
              )
        ORDER BY COALESCE(h.submitted_at_ms, 0) DESC, s.source_id DESC
        LIMIT ?5";
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query(rusqlite::params![
        source,
        parsed.match_sql,
        cursor_ms,
        cursor_id,
        fetch_limit,
        i64::from(inclusive),
    ])?;
    struct SearchCandidate {
        source_kind: String,
        source_id: [u8; 16],
        body_len: usize,
        submitted_at_ms: i64,
        history_id: Option<PromptHistoryId>,
    }
    let mut candidates = Vec::new();
    while let Some(row) = rows.next()? {
        let history_id = match row.get::<_, Option<Vec<u8>>>(3)? {
            Some(bytes) => Some(id_from_blob(&bytes)?),
            None => None,
        };
        candidates.push(SearchCandidate {
            source_kind: row.get(0)?,
            source_id: blob16(&row.get::<_, Vec<u8>>(1)?)?,
            body_len: 0,
            submitted_at_ms: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
            history_id,
        });
    }
    drop(rows);
    drop(stmt);
    for candidate in &mut candidates {
        candidate.body_len =
            canonical_body_utf8_bytes(conn, &candidate.source_kind, &candidate.source_id)?;
    }
    let mut hits = Vec::new();
    let mut work_used = 0_usize;
    let mut bytes_used = 0_usize;
    for candidate in candidates {
        work_used = work_used.saturating_add(1);
        if !budget.check(work_used, bytes_used.saturating_add(candidate.body_len))? {
            let next = hits
                .last()
                .map(|hit: &PromptSearchHit| {
                    bind_cursor(
                        hit.submitted_at_ms,
                        hit.source_id,
                        false,
                        query.source,
                        query_sha256,
                        epoch,
                        high_water,
                        schema_version,
                    )
                })
                .or(Some(bind_cursor(
                    candidate.submitted_at_ms,
                    candidate.source_id,
                    true,
                    query.source,
                    query_sha256,
                    epoch,
                    high_water,
                    schema_version,
                )));
            return Ok(PromptSearchPage {
                hits,
                next,
                status: PromptSearchStatus::Partial,
            });
        }
        let body = load_canonical_search_body(conn, &candidate.source_kind, &candidate.source_id)?;
        bytes_used = bytes_used.saturating_add(body.len());
        let source = if candidate.source_kind == SOURCE_SAVED {
            PromptSearchSource::Saved
        } else {
            PromptSearchSource::History
        };
        let highlights = highlight_ranges(&body, &parsed.highlight_terms);
        hits.push(PromptSearchHit {
            source,
            history_id: candidate.history_id,
            submitted_at_ms: candidate.submitted_at_ms,
            body,
            highlights,
            source_id: candidate.source_id,
        });
        if hits.len() > query.page_size {
            let _extra = hits.pop().expect("extra keyset row");
            let last = hits.last().expect("kept keyset row");
            return Ok(PromptSearchPage {
                next: Some(bind_cursor(
                    last.submitted_at_ms,
                    last.source_id,
                    false,
                    query.source,
                    query_sha256,
                    epoch,
                    high_water,
                    schema_version,
                )),
                hits,
                status: PromptSearchStatus::Complete,
            });
        }
    }
    Ok(PromptSearchPage {
        hits,
        next: None,
        status: PromptSearchStatus::Complete,
    })
}

pub(crate) fn upsert_search_row(
    conn: &Connection,
    source_kind: &str,
    source_id: &[u8],
    title: &str,
    body: &str,
    tags: &str,
) -> Result<(), PromptHistoryError> {
    conn.execute(
        "DELETE FROM prompt_search WHERE source_kind = ?1 AND source_id = ?2",
        rusqlite::params![source_kind, source_id],
    )?;
    conn.execute(
        "INSERT INTO prompt_search(source_kind, source_id, title, body, tags)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![source_kind, source_id, title, body, tags],
    )?;
    Ok(())
}

pub(crate) fn delete_search_row(
    conn: &Connection,
    source_kind: &str,
    source_id: &[u8],
) -> Result<(), PromptHistoryError> {
    conn.execute(
        "DELETE FROM prompt_search WHERE source_kind = ?1 AND source_id = ?2",
        rusqlite::params![source_kind, source_id],
    )?;
    Ok(())
}

pub(crate) fn clear_search_index(conn: &Connection) -> Result<(), PromptHistoryError> {
    conn.execute("DELETE FROM prompt_search", [])?;
    Ok(())
}

fn parse_search_query(text: &str) -> Result<ParsedSearch, PromptHistoryError> {
    if text.len() > MAX_PROMPT_SEARCH_QUERY_BYTES {
        return Err(PromptHistoryError::from_code(
            PromptHistoryErrorCode::QueryTooLong,
        ));
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(PromptHistoryError::from_code(
            PromptHistoryErrorCode::InvalidQuery,
        ));
    }

    let mut match_parts = Vec::new();
    let mut highlight_terms = Vec::new();
    let mut term_count = 0_usize;
    let mut phrase_count = 0_usize;
    let mut tag_count = 0_usize;
    let mut rest = trimmed;
    while !rest.is_empty() {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        if let Some(after_tag) = rest.strip_prefix("tag:") {
            let (tag, remaining) = next_token(after_tag);
            if tag.is_empty() {
                return Err(PromptHistoryError::from_code(
                    PromptHistoryErrorCode::InvalidQuery,
                ));
            }
            tag_count += 1;
            if tag_count > MAX_PROMPT_SEARCH_TAGS || tag.len() > 48 {
                return Err(PromptHistoryError::from_code(
                    PromptHistoryErrorCode::InvalidQuery,
                ));
            }
            match_parts.push(format!(
                "{{tags}} : {}",
                quote_fts_token(&normalize_token(tag))
            ));
            rest = remaining;
            continue;
        }
        if let Some(stripped) = rest.strip_prefix('"') {
            let Some(end) = stripped.find('"') else {
                return Err(PromptHistoryError::from_code(
                    PromptHistoryErrorCode::InvalidQuery,
                ));
            };
            let phrase = &stripped[..end];
            if phrase.is_empty() {
                return Err(PromptHistoryError::from_code(
                    PromptHistoryErrorCode::InvalidQuery,
                ));
            }
            phrase_count += 1;
            term_count += 1;
            if phrase_count > MAX_PROMPT_SEARCH_PHRASES || term_count > MAX_PROMPT_SEARCH_TERMS {
                return Err(PromptHistoryError::from_code(
                    PromptHistoryErrorCode::InvalidQuery,
                ));
            }
            match_parts.push(quote_fts_phrase(phrase));
            highlight_terms.push(phrase.to_string());
            rest = &stripped[end + 1..];
            continue;
        }
        let (token, remaining) = next_token(rest);
        if token.is_empty() {
            break;
        }
        term_count += 1;
        if term_count > MAX_PROMPT_SEARCH_TERMS {
            return Err(PromptHistoryError::from_code(
                PromptHistoryErrorCode::InvalidQuery,
            ));
        }
        if let Some(prefix) = token.strip_suffix('*') {
            if prefix.is_empty() {
                return Err(PromptHistoryError::from_code(
                    PromptHistoryErrorCode::InvalidQuery,
                ));
            }
            let normalized = normalize_token(prefix);
            match_parts.push(format!("{}*", escape_fts_prefix(&normalized)));
            highlight_terms.push(prefix.to_string());
        } else {
            let normalized = normalize_token(token);
            match_parts.push(quote_fts_token(&normalized));
            highlight_terms.push(token.to_string());
        }
        rest = remaining;
    }
    if match_parts.is_empty() {
        return Err(PromptHistoryError::from_code(
            PromptHistoryErrorCode::InvalidQuery,
        ));
    }
    Ok(ParsedSearch {
        match_sql: match_parts.join(" AND "),
        highlight_terms,
    })
}

fn next_token(input: &str) -> (&str, &str) {
    let trimmed = input.trim_start();
    match trimmed.find(char::is_whitespace) {
        Some(index) => (&trimmed[..index], &trimmed[index..]),
        None => (trimmed, ""),
    }
}

fn normalize_token(token: &str) -> String {
    fold_search_text(token)
}

fn fold_search_text(input: &str) -> String {
    input.chars().flat_map(fold_search_char).collect()
}

fn query_fingerprint(text: &str) -> [u8; 32] {
    Sha256::digest(text.as_bytes()).into()
}

fn load_search_cursor_bind(conn: &Connection) -> Result<(i64, i64), PromptHistoryError> {
    conn.query_row(
        "SELECT current_seq, high_water_seq
         FROM prompt_search_state WHERE singleton_key = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .map_err(Into::into)
}

fn cursor_schema_version(conn: &Connection) -> Result<i64, PromptHistoryError> {
    conn.query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
        row.get(0)
    })
    .map_err(Into::into)
}

fn bind_cursor(
    submitted_at_ms: i64,
    source_id: [u8; 16],
    inclusive: bool,
    source: PromptSearchSource,
    query_sha256: [u8; 32],
    epoch: i64,
    high_water: i64,
    schema_version: i64,
) -> PromptSearchCursor {
    PromptSearchCursor {
        submitted_at_ms,
        source_id,
        inclusive,
        source,
        query_sha256,
        epoch,
        high_water,
        schema_version,
    }
}

fn canonical_body_utf8_bytes(
    conn: &Connection,
    source_kind: &str,
    source_id: &[u8; 16],
) -> Result<usize, PromptHistoryError> {
    let bytes: i64 = if source_kind == SOURCE_HISTORY {
        conn.query_row(
            "SELECT length(CAST(body AS BLOB)) FROM prompt_history
             WHERE prompt_history_id = ?1",
            [source_id.as_slice()],
            |row| row.get(0),
        )?
    } else {
        conn.query_row(
            "SELECT length(CAST(v.body AS BLOB))
             FROM saved_prompts AS p
             JOIN prompt_versions AS v
               ON v.prompt_id = p.prompt_id
              AND v.prompt_version_id = p.current_version_id
             WHERE p.prompt_id = ?1",
            [source_id.as_slice()],
            |row| row.get(0),
        )?
    };
    usize::try_from(bytes)
        .map_err(|_| PromptHistoryError::from_code(PromptHistoryErrorCode::Storage))
}

fn load_canonical_search_body(
    conn: &Connection,
    source_kind: &str,
    source_id: &[u8; 16],
) -> Result<String, PromptHistoryError> {
    if source_kind == SOURCE_HISTORY {
        let (body, digest): (String, Vec<u8>) = conn.query_row(
            "SELECT body, body_sha256 FROM prompt_history WHERE prompt_history_id = ?1",
            [source_id.as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let stored: [u8; 32] = digest
            .as_slice()
            .try_into()
            .map_err(|_| PromptHistoryError::from_code(PromptHistoryErrorCode::Storage))?;
        let recomputed: [u8; 32] = Sha256::digest(body.as_bytes()).into();
        if recomputed != stored {
            return Err(PromptHistoryError::from_code(
                PromptHistoryErrorCode::Storage,
            ));
        }
        return Ok(body);
    }
    let (body, digest): (String, Vec<u8>) = conn.query_row(
        "SELECT v.body, v.body_sha256
         FROM saved_prompts AS p
         JOIN prompt_versions AS v
           ON v.prompt_id = p.prompt_id
          AND v.prompt_version_id = p.current_version_id
         WHERE p.prompt_id = ?1",
        [source_id.as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let stored: [u8; 32] = digest
        .as_slice()
        .try_into()
        .map_err(|_| PromptHistoryError::from_code(PromptHistoryErrorCode::Storage))?;
    let recomputed: [u8; 32] = Sha256::digest(body.as_bytes()).into();
    if recomputed != stored {
        return Err(PromptHistoryError::from_code(
            PromptHistoryErrorCode::Storage,
        ));
    }
    Ok(body)
}

fn fold_search_char(ch: char) -> impl Iterator<Item = char> {
    unicode61_base(ch).to_lowercase().filter_map(|folded| {
        if is_combining_mark(folded) {
            None
        } else {
            Some(strip_latin_diacritic(folded))
        }
    })
}

fn unicode61_base(ch: char) -> char {
    match ch {
        'ſ' | 'ẛ' | '\u{1E9C}' | '\u{1E9D}' => 's',
        'ẞ' => 'S',
        _ => latin_extended_additional_base(ch).unwrap_or(ch),
    }
}

fn latin_extended_additional_base(ch: char) -> Option<char> {
    let code = ch as u32;
    if !(0x1E00..=0x1EFF).contains(&code) {
        return None;
    }
    if (0x1E00..=0x1E95).contains(&code) {
        let pair = (code - 0x1E00) / 2;
        let upper = code % 2 == 0;
        let base = match pair {
            0 => 'A',
            1..=3 => 'B',
            4 => 'C',
            5..=9 => 'D',
            10..=14 => 'E',
            15 => 'F',
            16 => 'G',
            17..=21 => 'H',
            22 | 23 => 'I',
            24..=26 => 'K',
            27..=30 => 'L',
            31..=33 => 'M',
            34..=37 => 'N',
            38..=41 => 'O',
            42 | 43 => 'P',
            44..=47 => 'R',
            48..=52 => 'S',
            53..=56 => 'T',
            57..=61 => 'U',
            62 | 63 => 'V',
            64..=68 => 'W',
            69 | 70 => 'X',
            71 => 'Y',
            72..=74 => 'Z',
            _ => return None,
        };
        return Some(if upper {
            base
        } else {
            base.to_ascii_lowercase()
        });
    }
    Some(match ch {
        '\u{1E96}' => 'h',
        '\u{1E97}' => 't',
        '\u{1E98}' => 'w',
        '\u{1E99}' => 'y',
        '\u{1E9A}' => 'a',
        '\u{1E9B}' | '\u{1E9C}' | '\u{1E9D}' => 's',
        '\u{1E9E}' => 'S',
        '\u{1E9F}' => 'd',
        '\u{1EA0}'..='\u{1EB7}' => {
            if code % 2 == 0 {
                'A'
            } else {
                'a'
            }
        }
        '\u{1EB8}'..='\u{1EC7}' => {
            if code % 2 == 0 {
                'E'
            } else {
                'e'
            }
        }
        '\u{1EC8}'..='\u{1ECB}' => {
            if code % 2 == 0 {
                'I'
            } else {
                'i'
            }
        }
        '\u{1ECC}'..='\u{1EE3}' => {
            if code % 2 == 0 {
                'O'
            } else {
                'o'
            }
        }
        '\u{1EE4}'..='\u{1EF1}' => {
            if code % 2 == 0 {
                'U'
            } else {
                'u'
            }
        }
        '\u{1EF2}'..='\u{1EF9}' => {
            if code % 2 == 0 {
                'Y'
            } else {
                'y'
            }
        }
        '\u{1EFA}' | '\u{1EFB}' => 'l',
        '\u{1EFC}' | '\u{1EFD}' => 'v',
        '\u{1EFE}' | '\u{1EFF}' => 'y',
        _ => return None,
    })
}

fn is_combining_mark(ch: char) -> bool {
    matches!(
        ch,
        '\u{0300}'..='\u{036F}'
            | '\u{1AB0}'..='\u{1AFF}'
            | '\u{1DC0}'..='\u{1DFF}'
            | '\u{20D0}'..='\u{20FF}'
            | '\u{FE20}'..='\u{FE2F}'
    )
}

fn strip_latin_diacritic(ch: char) -> char {
    match ch {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => 'a',
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => 'c',
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => 'e',
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' => 'i',
        'ñ' | 'ń' | 'ņ' | 'ň' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => 'o',
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => 'u',
        'ý' | 'ÿ' => 'y',
        'ś' | 'ŝ' | 'ş' | 'š' => 's',
        'ź' | 'ż' | 'ž' => 'z',
        other => other,
    }
}

fn quote_fts_token(token: &str) -> String {
    format!("\"{}\"", token.replace('"', "\"\""))
}

fn quote_fts_phrase(phrase: &str) -> String {
    format!("\"{}\"", phrase.replace('"', "\"\""))
}

fn escape_fts_prefix(token: &str) -> String {
    token.chars().filter(|ch| ch.is_alphanumeric()).collect()
}

fn highlight_ranges(body: &str, terms: &[String]) -> Vec<HighlightRange> {
    let mut ranges = Vec::new();
    for term in terms {
        if ranges.len() >= MAX_PROMPT_SEARCH_HIGHLIGHTS {
            break;
        }
        if let Some(range) = find_term_range(body, term) {
            if body.is_char_boundary(range.start) && body.is_char_boundary(range.end) {
                ranges.push(range);
            }
        }
    }
    ranges
}

fn find_term_range(body: &str, term: &str) -> Option<HighlightRange> {
    if term.is_empty() || body.is_empty() {
        return None;
    }
    let needle: Vec<char> = fold_search_text(term).chars().collect();
    if needle.is_empty() {
        return None;
    }
    for (start_byte, _) in body.char_indices() {
        let mut matched = 0_usize;
        let mut end_byte;
        for (rel, ch) in body[start_byte..].char_indices() {
            for folded in fold_search_char(ch) {
                if matched >= needle.len() || folded != needle[matched] {
                    matched = usize::MAX;
                    break;
                }
                matched += 1;
            }
            if matched == usize::MAX {
                break;
            }
            end_byte = start_byte + rel + ch.len_utf8();
            if matched == needle.len() {
                for (_, tail_ch) in body[end_byte..].char_indices() {
                    if fold_search_char(tail_ch).next().is_none() {
                        end_byte += tail_ch.len_utf8();
                    } else {
                        break;
                    }
                }
                if body.is_char_boundary(start_byte) && body.is_char_boundary(end_byte) {
                    return Some(HighlightRange {
                        start: start_byte,
                        end: end_byte,
                    });
                }
                break;
            }
        }
    }
    None
}

fn blob16(bytes: &[u8]) -> Result<[u8; 16], PromptHistoryError> {
    bytes
        .try_into()
        .map_err(|_| PromptHistoryError::from_code(PromptHistoryErrorCode::Storage))
}

fn id_from_blob(bytes: &[u8]) -> Result<PromptHistoryId, PromptHistoryError> {
    let bytes: [u8; 16] = bytes
        .try_into()
        .map_err(|_| PromptHistoryError::from_code(PromptHistoryErrorCode::Storage))?;
    PromptHistoryId::from_bytes(bytes)
        .map_err(|_| PromptHistoryError::from_code(PromptHistoryErrorCode::Storage))
}

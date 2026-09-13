//! Host stderr diagnostics.
//!
//! The durable host writes to a plain stderr file the client hands it
//! (`<profile>/logs/host-stderr.log`), so a slow launch is only attributable
//! when every line carries a wall clock. `host_log!` is the single writer for
//! `src/host` and `src/bin/devmanager-host.rs`; a bare `eprintln!` there is a
//! line nobody can place on a timeline.

use std::sync::OnceLock;

use time::format_description::{self, BorrowedFormatItem};
use time::OffsetDateTime;

/// Host log line with a local wall-clock prefix:
/// `2026-09-02 21:14:46.812 devmanager-host: ...`.
#[macro_export]
macro_rules! host_log {
    ($($arg:tt)*) => {
        eprintln!("{} {}", $crate::host::diag::stamp(), format_args!($($arg)*))
    };
}

/// `YYYY-MM-DD HH:MM:SS.mmm` in local time.
///
/// Windows can report an indeterminate local offset on some threads; that
/// falls back to UTC with a trailing `Z` so the reader is never misled about
/// which clock produced the line. Never panics: a format description that
/// failed to parse, or a formatter that refused the value, still renders the
/// same shape through [`fallback_stamp`].
pub fn stamp() -> String {
    let (now, suffix) = match OffsetDateTime::now_local() {
        Ok(local) => (local, ""),
        Err(_) => (OffsetDateTime::now_utc(), "Z"),
    };
    let rendered = stamp_format()
        .and_then(|format| now.format(format).ok())
        .unwrap_or_else(|| fallback_stamp(now));
    format!("{rendered}{suffix}")
}

/// Parsed once; a host under load must not reparse a format description per
/// log line.
fn stamp_format() -> Option<&'static [BorrowedFormatItem<'static>]> {
    static FORMAT: OnceLock<Option<Vec<BorrowedFormatItem<'static>>>> = OnceLock::new();
    FORMAT
        .get_or_init(|| {
            format_description::parse(
                "[year]-[month]-[day] [hour]:[minute]:[second].[subsecond digits:3]",
            )
            .ok()
        })
        .as_deref()
}

/// Same shape as [`stamp_format`], built from components so no failure path
/// can leave a log line without a time.
fn fallback_stamp(now: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.millisecond()
    )
}

#[cfg(test)]
mod tests {
    use super::{fallback_stamp, stamp};
    use regex::Regex;
    use time::OffsetDateTime;

    fn shape() -> Regex {
        Regex::new("^[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2}[.][0-9]{3}Z?$")
            .expect("valid stamp shape")
    }

    #[test]
    fn stamp_renders_wall_clock_shape() {
        let value = stamp();
        assert!(shape().is_match(&value), "unexpected stamp: {value}");
        // The shape must be able to fail, or the two assertions above are
        // vacuous: a stamp without milliseconds is the near miss to reject.
        assert!(
            !shape().is_match("2026-09-02 21:14:46"),
            "shape accepts a stamp with no milliseconds"
        );
    }

    #[test]
    fn consecutive_stamps_are_non_decreasing() {
        let first = stamp();
        let second = stamp();
        assert!(
            second >= first,
            "stamp went backwards: {first} then {second}"
        );
    }

    #[test]
    fn fallback_matches_the_same_shape() {
        let value = fallback_stamp(OffsetDateTime::now_utc());
        assert!(shape().is_match(&value), "unexpected fallback: {value}");
    }
}

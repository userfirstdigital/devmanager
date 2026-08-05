use rusqlite::Connection;

use crate::kernel::store::StoreError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StoreMaintenanceReport {
    pub wal: WalCheckpointOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WalCheckpointOutcome {
    NoWal,
    Complete {
        log_frames: u64,
        checkpointed_frames: u64,
    },
    Partial {
        log_frames: u64,
        checkpointed_frames: u64,
    },
}

pub(crate) fn run(conn: &Connection) -> Result<StoreMaintenanceReport, StoreError> {
    quick_health_check(conn)?;
    let wal = with_full_synchronous(conn, |conn| {
        let row: (i64, i64, i64) = conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        decode_wal_checkpoint_row(row.0, row.1, row.2)
    })?;
    Ok(StoreMaintenanceReport { wal })
}

fn quick_health_check(conn: &Connection) -> Result<(), StoreError> {
    let quick_check: String = conn.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
    if quick_check != "ok" {
        return Err(StoreError::IntegrityCheckFailed(quick_check));
    }

    let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
    let mut rows = stmt.query([])?;
    if rows.next()?.is_some() {
        return Err(StoreError::IntegrityCheckFailed(
            "foreign key check failed".into(),
        ));
    }
    Ok(())
}

pub(crate) fn with_full_synchronous<T>(
    conn: &Connection,
    action: impl FnOnce(&Connection) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    let action_result = set_synchronous(conn, "FULL", 2).and_then(|()| action(conn));
    let restore_result = set_synchronous(conn, "NORMAL", 1);
    match (action_result, restore_result) {
        (_, Err(restore_error)) => Err(restore_error),
        (Err(action_error), Ok(())) => Err(action_error),
        (Ok(value), Ok(())) => Ok(value),
    }
}

fn set_synchronous(
    conn: &Connection,
    value: &'static str,
    expected: i64,
) -> Result<(), StoreError> {
    conn.pragma_update(None, "synchronous", value)?;
    let actual: i64 = conn.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
    if actual != expected {
        return Err(StoreError::Sqlite(format!(
            "synchronous pragma expected {expected}, got {actual}"
        )));
    }
    Ok(())
}

fn decode_wal_checkpoint_row(
    busy: i64,
    log_frames: i64,
    checkpointed_frames: i64,
) -> Result<WalCheckpointOutcome, StoreError> {
    // PASSIVE always returns SQLITE_OK; partial progress is represented only
    // by checkpointed_frames < log_frames. A busy flag belongs to the blocking
    // checkpoint modes and is invalid at this passive-only boundary.
    if busy != 0 {
        return Err(StoreError::Corruption);
    }
    if log_frames == -1 && checkpointed_frames == -1 {
        return Ok(WalCheckpointOutcome::NoWal);
    }
    let log_frames = u64::try_from(log_frames).map_err(|_| StoreError::Corruption)?;
    let checkpointed_frames =
        u64::try_from(checkpointed_frames).map_err(|_| StoreError::Corruption)?;
    if checkpointed_frames > log_frames {
        return Err(StoreError::Corruption);
    }
    if checkpointed_frames == log_frames {
        Ok(WalCheckpointOutcome::Complete {
            log_frames,
            checkpointed_frames,
        })
    } else {
        Ok(WalCheckpointOutcome::Partial {
            log_frames,
            checkpointed_frames,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_row_decoder_accepts_valid_shapes_and_rejects_impossible_values() {
        assert_eq!(
            decode_wal_checkpoint_row(0, -1, -1),
            Ok(WalCheckpointOutcome::NoWal)
        );
        assert_eq!(
            decode_wal_checkpoint_row(0, 7, 7),
            Ok(WalCheckpointOutcome::Complete {
                log_frames: 7,
                checkpointed_frames: 7,
            })
        );
        assert_eq!(
            decode_wal_checkpoint_row(0, 7, 3),
            Ok(WalCheckpointOutcome::Partial {
                log_frames: 7,
                checkpointed_frames: 3,
            })
        );
        for invalid in [
            (2, 1, 1),
            (1, 7, 7),
            (1, -1, -1),
            (0, -1, 0),
            (0, 1, -1),
            (0, 1, 2),
        ] {
            assert_eq!(
                decode_wal_checkpoint_row(invalid.0, invalid.1, invalid.2),
                Err(StoreError::Corruption),
                "invalid checkpoint row must fail closed: {invalid:?}"
            );
        }
    }
}

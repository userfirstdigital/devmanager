//! Trusted-PC Settings / controller / autorestore for the native shell.
//!
//! Child of [`super`] via `#[path = "native_trusted_hosts_view.rs"] mod trusted_hosts_view;`.
//! Transport stays in existing HostFleet / NativeHostClientRuntime / RemoteTrustStore helpers.

use std::collections::{BTreeMap, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::{
    div, px, AnyElement, AppContext, ClickEvent, Context, Entity, FontWeight, IntoElement,
    ParentElement, SharedString, Styled, Window,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputState};
use gpui_component::{Disableable, IconName, Sizable};
use zeroize::Zeroizing;

use crate::ui::overlay_chrome;

use crate::client::{
    hex_encode, ConnectTrustedOptions, FleetError, FleetRemoval, HostFleet, HostId,
    PairEnrollRequest, RemoteTrustError, RemoteTrustStore, TrustedHostRecord,
    REMOTE_CA_PEM_MAX_BYTES,
};
use crate::remote::blocking_work::{RemoteBlockingWork, RemoteWorkAdmission, RemoteWorkError};

use super::trusted_hosts::{
    load_trusted_host_roster_until, trust_store_root_for_profile, ForgetPersistence,
    ForgetTrustedHostResult, RecoveryReason, RosterLoadResult, TrustedBootstrapOutcomeSlot,
    TrustedHostsCoordinator, TrustedHostsError, MAX_TRUSTED_REMOTE_HOSTS,
};
use super::{
    NativeHostClientRuntime, NativeHostRuntimeAttachment, NativeHostState, NativeShell,
    NativeShellError, NativeShellMode, PendingHostBootstrap,
};

const RESTORE_BACKOFF_INITIAL: Duration = Duration::from_secs(2);
const RESTORE_BACKOFF_MAX: Duration = Duration::from_secs(60);
const TRUSTED_SETUP_BUDGET: Duration = Duration::from_secs(90);
const TRUSTED_ROSTER_BUDGET: Duration = Duration::from_secs(15);
const TRUSTED_FORGET_BUDGET: Duration = Duration::from_secs(30);
const TRUSTED_DISPOSE_BUDGET: Duration = Duration::from_secs(20);

/// Pure controller decisions (no GPUI entities). Tested without an Application lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrustedHostRowPhase {
    Offline,
    Queued,
    Connecting,
    Connected,
    Failed,
    DisconnectSuppressed,
    Disconnecting,
    Forgetting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BootstrapAttachGate {
    Accept,
    RejectExpiredOrCancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TrustedHostRuntimeObservation {
    LiveOwnerMatch {
        endpoint: String,
    },
    /// Attachment still present — reconnect loop owns retries; UI must not spawn restore.
    AttachmentPresent {
        detail: Option<String>,
    },
    Disconnected,
    TransportFailed {
        message: String,
    },
}

/// Bounded CA PEM read for enrollment (metadata + take cap; no unbounded allocation).
pub(crate) fn read_additional_ca_pem_bounded(path: &Path) -> Result<String, String> {
    let meta =
        std::fs::metadata(path).map_err(|error| format!("CA PEM metadata failed: {error}"))?;
    if !meta.is_file() {
        return Err("CA PEM path is not a regular file".into());
    }
    if meta.len() > REMOTE_CA_PEM_MAX_BYTES as u64 {
        return Err(format!("CA PEM exceeds {REMOTE_CA_PEM_MAX_BYTES} bytes"));
    }
    let file = std::fs::File::open(path).map_err(|error| format!("CA PEM open failed: {error}"))?;
    let mut limited = file.take(REMOTE_CA_PEM_MAX_BYTES as u64 + 1);
    let mut bytes = Vec::new();
    limited
        .read_to_end(&mut bytes)
        .map_err(|error| format!("CA PEM read failed: {error}"))?;
    if bytes.len() > REMOTE_CA_PEM_MAX_BYTES {
        return Err(format!("CA PEM exceeds {REMOTE_CA_PEM_MAX_BYTES} bytes"));
    }
    String::from_utf8(bytes).map_err(|_| "CA PEM is not valid UTF-8".into())
}

#[derive(Debug, Clone)]
pub(crate) struct TrustedHostRow {
    pub record: TrustedHostRecord,
    pub phase: TrustedHostRowPhase,
    pub suppress_auto_retry: bool,
    pub last_error: Option<String>,
    pub next_retry_at: Option<Instant>,
    pub backoff: Duration,
    pub forget_confirm: bool,
}

#[derive(Debug, Default)]
pub(crate) struct TrustedHostsController {
    rows: Vec<TrustedHostRow>,
    /// Hosts with an admitted in-flight setup (restore/retry/enroll tracking).
    pending_setup_hosts: BTreeMap<[u8; 16], ()>,
    enroll_in_flight: bool,
    feedback: Option<String>,
    recovery: Option<RecoveryReason>,
    /// True only after a successful roster load that currently authorizes restore.
    roster_current: bool,
    /// Failed/pending refresh: rows may remain cached for display but restore is gated.
    roster_failed: bool,
    /// Retained after failed/uncertain forget — never discarded just to hide a row.
    /// Keyed by host + removal generation so success on B cannot erase A's ledger.
    retained_forget: BTreeMap<ForgetLedgerKey, RetainedForgetOutcome>,
    /// In-flight forget/dispose ops that may need a new ledger key. Never evict.
    ledger_slot_reservations: usize,
    /// Exact removal held when a settled insert could not take a new map slot.
    ledger_overflow_hold: Option<(ForgetLedgerKey, RetainedForgetOutcome)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ForgetLedgerKey {
    pub host_public_id: [u8; 16],
    pub generation: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct RetainedForgetOutcome {
    pub host_public_id: [u8; 16],
    pub persistence: ForgetPersistence,
    pub removal: Option<FleetRemoval>,
    pub persist_error: Option<RemoteTrustError>,
}

impl TrustedHostsController {
    pub(crate) fn rows(&self) -> &[TrustedHostRow] {
        &self.rows
    }

    pub(crate) fn feedback(&self) -> Option<&str> {
        self.feedback.as_deref()
    }

    pub(crate) fn recovery(&self) -> Option<RecoveryReason> {
        self.recovery
    }

    pub(crate) fn retained_forget_ledgers(
        &self,
    ) -> &BTreeMap<ForgetLedgerKey, RetainedForgetOutcome> {
        &self.retained_forget
    }

    pub(crate) fn acknowledge_retained_forget(&mut self, key: ForgetLedgerKey) {
        self.retained_forget.remove(&key);
    }

    pub(crate) fn ledger_overflow_hold(&self) -> Option<&(ForgetLedgerKey, RetainedForgetOutcome)> {
        self.ledger_overflow_hold.as_ref()
    }

    /// True when a distinct new host+generation ledger key may still be admitted.
    pub(crate) fn forget_ledger_has_capacity(&self) -> bool {
        self.retained_forget.len() + self.ledger_slot_reservations < MAX_TRUSTED_REMOTE_HOSTS
    }

    /// Reserve one new-key ledger slot before admitted forget/cleanup. No eviction.
    pub(crate) fn try_reserve_forget_ledger_slot(&mut self) -> bool {
        if !self.forget_ledger_has_capacity() {
            return false;
        }
        self.ledger_slot_reservations = self.ledger_slot_reservations.saturating_add(1);
        true
    }

    pub(crate) fn release_forget_ledger_slot(&mut self) {
        self.ledger_slot_reservations = self.ledger_slot_reservations.saturating_sub(1);
    }

    fn removal_has_unresolved_ledgers(removal: &Option<FleetRemoval>) -> bool {
        removal
            .as_ref()
            .is_some_and(|removal| !removal.retained.is_empty() || !removal.uncertain.is_empty())
    }

    /// Insert or update a retained forget outcome without evicting unresolved peers.
    /// Refuses a new key when the bounded map is full. Does not overwrite a nonempty
    /// same-generation removal with a later None/empty forget result.
    pub(crate) fn insert_retained_forget(
        &mut self,
        key: ForgetLedgerKey,
        outcome: RetainedForgetOutcome,
    ) -> bool {
        if let Some(existing) = self.retained_forget.get(&key) {
            if Self::removal_has_unresolved_ledgers(&existing.removal)
                && !Self::removal_has_unresolved_ledgers(&outcome.removal)
            {
                return true;
            }
            self.retained_forget.insert(key, outcome);
            return true;
        }
        if self.retained_forget.len() >= MAX_TRUSTED_REMOTE_HOSTS {
            // Never evict unresolved retained/uncertain command outcomes. Reclaim only
            // empty-removal ledger rows so a nonempty result can still be preserved.
            let reclaim = self
                .retained_forget
                .iter()
                .find(|(_, outcome)| !Self::removal_has_unresolved_ledgers(&outcome.removal))
                .map(|(key, _)| *key);
            if let Some(reclaim) = reclaim {
                self.retained_forget.remove(&reclaim);
            } else {
                return false;
            }
        }
        self.retained_forget.insert(key, outcome);
        self.release_forget_ledger_slot();
        true
    }

    /// Persist a refused new-key insert under bounded overflow custody (at most one).
    pub(crate) fn hold_refused_retained_forget(
        &mut self,
        key: ForgetLedgerKey,
        outcome: RetainedForgetOutcome,
    ) {
        if self.ledger_overflow_hold.is_none() {
            self.ledger_overflow_hold = Some((key, outcome));
        }
    }

    pub(crate) fn roster_authorizes_restore(&self) -> bool {
        self.roster_current && !self.roster_failed
    }

    pub(crate) fn roster_failed(&self) -> bool {
        self.roster_failed
    }

    pub(crate) fn should_fetch_roster(&self) -> bool {
        !self.roster_current && !self.roster_failed
    }

    pub(crate) fn set_feedback(&mut self, message: String) {
        self.feedback = Some(message);
    }

    pub(crate) fn clear_feedback(&mut self) {
        self.feedback = None;
    }

    pub(crate) fn set_recovery(&mut self, reason: Option<RecoveryReason>) {
        self.recovery = reason;
        if let Some(reason) = reason {
            let message = match reason {
                RecoveryReason::PersistenceUncertain => {
                    "Trusted-host recovery required. A durable trust write may still be settling; restart DevManager before pairing, restoring, or forgetting."
                }
                RecoveryReason::RemovalIncomplete => {
                    "Trusted-host recovery required. A fleet removal was admitted but did not finish joining; restart before installing or forgetting that PC."
                }
                RecoveryReason::CleanupIncomplete => {
                    "Trusted-host recovery required. Exact construction cleanup did not settle; restart before retrying the same host."
                }
            };
            self.feedback = Some(message.into());
        }
    }

    pub(crate) fn setup_busy(&self) -> bool {
        self.enroll_in_flight || !self.pending_setup_hosts.is_empty()
    }

    /// Remote-only gate: never treat a local profile HostId as a trusted-PC target.
    pub(crate) fn is_remote_trust_target(host_id: &HostId) -> bool {
        host_id.as_remote().is_some()
    }

    pub(crate) fn apply_roster(&mut self, records: Vec<TrustedHostRecord>) {
        self.roster_current = true;
        self.roster_failed = false;
        let mut next = Vec::with_capacity(records.len().min(MAX_TRUSTED_REMOTE_HOSTS));
        for record in records.into_iter().take(MAX_TRUSTED_REMOTE_HOSTS) {
            if HostId::remote(record.host_public_id).is_err() {
                continue;
            }
            let prior = self
                .rows
                .iter()
                .find(|row| row.record.host_public_id == record.host_public_id);
            let (phase, suppress, error, retry_at, backoff, confirm) = match prior {
                Some(row) => (
                    row.phase,
                    row.suppress_auto_retry,
                    row.last_error.clone(),
                    row.next_retry_at,
                    row.backoff,
                    row.forget_confirm,
                ),
                None => (
                    TrustedHostRowPhase::Queued,
                    false,
                    None,
                    Some(Instant::now()),
                    RESTORE_BACKOFF_INITIAL,
                    false,
                ),
            };
            next.push(TrustedHostRow {
                record,
                phase: if matches!(
                    phase,
                    TrustedHostRowPhase::Connected
                        | TrustedHostRowPhase::Connecting
                        | TrustedHostRowPhase::DisconnectSuppressed
                        | TrustedHostRowPhase::Disconnecting
                        | TrustedHostRowPhase::Forgetting
                        | TrustedHostRowPhase::Failed
                ) {
                    phase
                } else {
                    TrustedHostRowPhase::Queued
                },
                suppress_auto_retry: suppress,
                last_error: error,
                next_retry_at: retry_at,
                backoff,
                forget_confirm: confirm,
            });
        }
        self.rows = next;
    }

    pub(crate) fn mark_roster_failed(&mut self, message: impl Into<String>) {
        // Keep cached rows for display, but do not authorize restore from stale facts.
        self.roster_current = false;
        self.roster_failed = true;
        self.feedback = Some(message.into());
    }

    pub(crate) fn invalidate_roster(&mut self) {
        self.roster_current = false;
        self.roster_failed = false;
    }

    /// Admit one restore/retry for `host_public_id`. Rejects duplicates and recovery hold.
    pub(crate) fn admit_setup_for_host(
        &mut self,
        host_public_id: [u8; 16],
    ) -> Result<(), TrustedHostsError> {
        if self.recovery.is_some() {
            return Err(TrustedHostsError::RecoveryRequired);
        }
        if self.pending_setup_hosts.contains_key(&host_public_id) || self.enroll_in_flight {
            return Err(TrustedHostsError::Busy);
        }
        let Some(row) = self
            .rows
            .iter_mut()
            .find(|row| row.record.host_public_id == host_public_id)
        else {
            return Err(TrustedHostsError::Fleet(
                super::trusted_hosts::FleetBusyKind::NotFound,
            ));
        };
        if matches!(
            row.phase,
            TrustedHostRowPhase::Connecting | TrustedHostRowPhase::Forgetting
        ) {
            return Err(TrustedHostsError::Busy);
        }
        row.phase = TrustedHostRowPhase::Connecting;
        row.last_error = None;
        row.forget_confirm = false;
        self.pending_setup_hosts.insert(host_public_id, ());
        Ok(())
    }

    pub(crate) fn admit_enroll(&mut self) -> Result<(), TrustedHostsError> {
        if self.recovery.is_some() {
            return Err(TrustedHostsError::RecoveryRequired);
        }
        if self.setup_busy() {
            return Err(TrustedHostsError::Busy);
        }
        self.enroll_in_flight = true;
        Ok(())
    }

    pub(crate) fn release_setup_host(&mut self, host_public_id: Option<[u8; 16]>) {
        if let Some(id) = host_public_id {
            self.pending_setup_hosts.remove(&id);
        }
        self.enroll_in_flight = false;
    }

    /// Next host eligible for automatic restore (bounded queue, no suppress, due backoff).
    pub(crate) fn next_auto_restore_host(&self, now: Instant) -> Option<[u8; 16]> {
        if self.recovery.is_some() || self.setup_busy() || !self.roster_authorizes_restore() {
            return None;
        }
        self.rows.iter().find_map(|row| {
            if row.suppress_auto_retry {
                return None;
            }
            if !matches!(
                row.phase,
                TrustedHostRowPhase::Queued
                    | TrustedHostRowPhase::Failed
                    | TrustedHostRowPhase::Offline
            ) {
                return None;
            }
            if row.next_retry_at.is_some_and(|due| due > now) {
                return None;
            }
            Some(row.record.host_public_id)
        })
    }

    /// Apply authoritative host_slot / transport state onto controller rows.
    /// Live attachments are never treated as restore targets (reconnect owns retries).
    pub(crate) fn reconcile_runtime_status(
        &mut self,
        observations: &[([u8; 16], TrustedHostRuntimeObservation)],
    ) {
        for (host_public_id, observation) in observations {
            let Some(row) = self
                .rows
                .iter_mut()
                .find(|row| row.record.host_public_id == *host_public_id)
            else {
                continue;
            };
            if matches!(
                row.phase,
                TrustedHostRowPhase::Connecting | TrustedHostRowPhase::Forgetting
            ) {
                continue;
            }
            match observation {
                TrustedHostRuntimeObservation::LiveOwnerMatch { endpoint: _ } => {
                    row.phase = TrustedHostRowPhase::Connected;
                    row.last_error = None;
                    row.next_retry_at = None;
                }
                TrustedHostRuntimeObservation::AttachmentPresent { detail } => {
                    // Runtime still owns the slot; do not authorize a duplicate restore.
                    if let Some(detail) = detail {
                        row.phase = TrustedHostRowPhase::Failed;
                        row.last_error = Some(detail.clone());
                    } else if !row.suppress_auto_retry {
                        row.phase = TrustedHostRowPhase::Offline;
                    }
                }
                TrustedHostRuntimeObservation::Disconnected => {
                    if row.suppress_auto_retry {
                        row.phase = TrustedHostRowPhase::DisconnectSuppressed;
                    } else if !matches!(row.phase, TrustedHostRowPhase::Queued) {
                        row.phase = TrustedHostRowPhase::Offline;
                    }
                }
                TrustedHostRuntimeObservation::TransportFailed { message } => {
                    if row.suppress_auto_retry {
                        row.phase = TrustedHostRowPhase::DisconnectSuppressed;
                    } else {
                        row.phase = TrustedHostRowPhase::Failed;
                        row.last_error = Some(message.clone());
                    }
                }
            }
        }
    }

    pub(crate) fn mark_connected(&mut self, host_public_id: [u8; 16]) {
        self.release_setup_host(Some(host_public_id));
        if let Some(row) = self
            .rows
            .iter_mut()
            .find(|row| row.record.host_public_id == host_public_id)
        {
            row.phase = TrustedHostRowPhase::Connected;
            row.suppress_auto_retry = false;
            row.last_error = None;
            row.next_retry_at = None;
            row.backoff = RESTORE_BACKOFF_INITIAL;
        }
    }

    pub(crate) fn mark_setup_failed(&mut self, host_public_id: Option<[u8; 16]>, message: String) {
        self.release_setup_host(host_public_id);
        let Some(id) = host_public_id else {
            self.feedback = Some(message);
            return;
        };
        if let Some(row) = self
            .rows
            .iter_mut()
            .find(|row| row.record.host_public_id == id)
        {
            row.phase = TrustedHostRowPhase::Failed;
            row.last_error = Some(message);
            let next = row.backoff;
            row.next_retry_at = Some(Instant::now() + next);
            row.backoff = (row.backoff.saturating_mul(2)).min(RESTORE_BACKOFF_MAX);
        } else {
            self.feedback = Some(message);
        }
    }

    pub(crate) fn mark_disconnected(&mut self, host_public_id: [u8; 16]) {
        if let Some(row) = self
            .rows
            .iter_mut()
            .find(|row| row.record.host_public_id == host_public_id)
        {
            row.phase = TrustedHostRowPhase::DisconnectSuppressed;
            row.suppress_auto_retry = true;
            row.next_retry_at = None;
            row.forget_confirm = false;
        }
    }

    pub(crate) fn clear_suppress_for_retry(&mut self, host_public_id: [u8; 16]) {
        if let Some(row) = self
            .rows
            .iter_mut()
            .find(|row| row.record.host_public_id == host_public_id)
        {
            row.suppress_auto_retry = false;
            row.phase = TrustedHostRowPhase::Queued;
            row.next_retry_at = Some(Instant::now());
            row.backoff = RESTORE_BACKOFF_INITIAL;
            row.last_error = None;
        }
    }

    pub(crate) fn request_forget_confirm(&mut self, host_public_id: [u8; 16]) {
        for row in &mut self.rows {
            row.forget_confirm = row.record.host_public_id == host_public_id;
        }
    }

    pub(crate) fn cancel_forget_confirm(&mut self, host_public_id: [u8; 16]) {
        if let Some(row) = self
            .rows
            .iter_mut()
            .find(|row| row.record.host_public_id == host_public_id)
        {
            row.forget_confirm = false;
        }
    }

    /// Forget confirm does not erase the row; only a Forgotten result may.
    pub(crate) fn begin_forget(
        &mut self,
        host_public_id: [u8; 16],
    ) -> Result<(), TrustedHostsError> {
        if self.recovery.is_some() {
            return Err(TrustedHostsError::RecoveryRequired);
        }
        if self.setup_busy() {
            return Err(TrustedHostsError::Busy);
        }
        let Some(row) = self
            .rows
            .iter_mut()
            .find(|row| row.record.host_public_id == host_public_id)
        else {
            return Err(TrustedHostsError::Fleet(
                super::trusted_hosts::FleetBusyKind::NotFound,
            ));
        };
        if !row.forget_confirm {
            return Err(TrustedHostsError::Busy);
        }
        row.phase = TrustedHostRowPhase::Forgetting;
        Ok(())
    }

    pub(crate) fn apply_forget_result(
        &mut self,
        host_public_id: [u8; 16],
        result: ForgetTrustedHostResult,
    ) {
        let generation = result
            .removal
            .as_ref()
            .map(|removal| removal.generation)
            .unwrap_or(0);
        let key = ForgetLedgerKey {
            host_public_id,
            generation,
        };
        let retain_ledger =
            result.removal.as_ref().is_some_and(|removal| {
                !removal.retained.is_empty() || !removal.uncertain.is_empty()
            }) || !matches!(result.persistence, ForgetPersistence::Forgotten);

        match result.persistence {
            ForgetPersistence::Forgotten => {
                self.rows
                    .retain(|row| row.record.host_public_id != host_public_id);
                // Never wipe other hosts' unresolved ledgers on B's success.
                let had_key = self.retained_forget.contains_key(&key);
                if retain_ledger {
                    let outcome = RetainedForgetOutcome {
                        host_public_id,
                        persistence: ForgetPersistence::Forgotten,
                        removal: result.removal,
                        persist_error: result.persist_error,
                    };
                    if !self.insert_retained_forget(key, outcome.clone()) {
                        self.hold_refused_retained_forget(key, outcome);
                        self.set_recovery(Some(RecoveryReason::CleanupIncomplete));
                        self.release_forget_ledger_slot();
                    } else if had_key {
                        self.release_forget_ledger_slot();
                    }
                } else {
                    self.release_forget_ledger_slot();
                }
                self.feedback = Some("Forgotten trusted PC removed from this device.".into());
            }
            other => {
                if let Some(row) = self
                    .rows
                    .iter_mut()
                    .find(|row| row.record.host_public_id == host_public_id)
                {
                    row.phase = TrustedHostRowPhase::Failed;
                    row.forget_confirm = false;
                    row.last_error = Some(match other {
                        ForgetPersistence::PersistenceUncertain => {
                            "Forget did not settle on disk. Restart to recover; trust may still be present."
                                .into()
                        }
                        ForgetPersistence::DefinitelyPreserved => result
                            .persist_error
                            .map(|e| e.as_str().to_string())
                            .unwrap_or_else(|| "Forget failed; trust was retained.".into()),
                        ForgetPersistence::Forgotten => unreachable!(),
                    });
                }
                if matches!(other, ForgetPersistence::PersistenceUncertain) {
                    self.set_recovery(Some(RecoveryReason::PersistenceUncertain));
                }
                let had_key = self.retained_forget.contains_key(&key);
                if retain_ledger {
                    let outcome = RetainedForgetOutcome {
                        host_public_id,
                        persistence: other,
                        removal: result.removal,
                        persist_error: result.persist_error,
                    };
                    if !self.insert_retained_forget(key, outcome.clone()) {
                        self.hold_refused_retained_forget(key, outcome);
                        self.set_recovery(Some(RecoveryReason::CleanupIncomplete));
                        self.release_forget_ledger_slot();
                    } else if had_key {
                        self.release_forget_ledger_slot();
                    }
                } else {
                    self.release_forget_ledger_slot();
                }
            }
        }
    }

    pub(crate) fn gate_bootstrap_attach(
        cancelled: bool,
        deadline_expired: bool,
        outcome_present: bool,
    ) -> BootstrapAttachGate {
        if cancelled || deadline_expired || !outcome_present {
            BootstrapAttachGate::RejectExpiredOrCancelled
        } else {
            BootstrapAttachGate::Accept
        }
    }

    pub(crate) fn record(&self, host_public_id: [u8; 16]) -> Option<&TrustedHostRecord> {
        self.rows
            .iter()
            .find(|row| row.record.host_public_id == host_public_id)
            .map(|row| &row.record)
    }

    pub(crate) fn upsert_enrolled(&mut self, record: TrustedHostRecord) {
        let id = record.host_public_id;
        if let Some(row) = self
            .rows
            .iter_mut()
            .find(|row| row.record.host_public_id == id)
        {
            row.record = record;
            row.phase = TrustedHostRowPhase::Connected;
            row.suppress_auto_retry = false;
            row.last_error = None;
            row.next_retry_at = None;
            row.backoff = RESTORE_BACKOFF_INITIAL;
            row.forget_confirm = false;
        } else if self.rows.len() < MAX_TRUSTED_REMOTE_HOSTS {
            self.rows.push(TrustedHostRow {
                record,
                phase: TrustedHostRowPhase::Connected,
                suppress_auto_retry: false,
                last_error: None,
                next_retry_at: None,
                backoff: RESTORE_BACKOFF_INITIAL,
                forget_confirm: false,
            });
        }
        self.release_setup_host(Some(id));
        self.enroll_in_flight = false;
    }
}

struct TrustedHostsFields {
    endpoint: Entity<InputState>,
    pairing_code: Entity<InputState>,
    ca_pem_path: Entity<InputState>,
}

struct PendingTrustedSetup {
    pending: PendingHostBootstrap,
    outcome: Arc<TrustedBootstrapOutcomeSlot>,
    host_public_id: Option<[u8; 16]>,
    deadline: Instant,
    cancelled: bool,
}

enum RosterJobResult {
    Opened {
        store: RemoteTrustStore,
        roster: RosterLoadResult,
    },
    Failed(String),
}

enum EnrollPrepResult {
    Ready(PairEnrollRequest),
    Failed(String),
}

enum ForgetJobResult {
    Finished {
        host_public_id: [u8; 16],
        result: ForgetTrustedHostResult,
    },
    Failed {
        host_public_id: [u8; 16],
        message: String,
        /// True when the OS worker may have admitted a durable mutation.
        maybe_admitted: bool,
    },
}

#[derive(Debug)]
pub(crate) enum DisposeJobResult {
    /// Exact generation joined; nonempty retained/uncertain ledgers must be preserved.
    Settled {
        host_id: HostId,
        removal: FleetRemoval,
    },
    Failed {
        host_id: HostId,
        reason: DisposeFailureReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisposeFailureReason {
    CleanupIncomplete,
    StaleGeneration,
    Timeout,
    Unavailable,
}

/// Pure gate: deferred same-host work requires a typed settled removal proof.
pub(crate) fn dispose_permits_deferred(result: &DisposeJobResult) -> bool {
    matches!(result, DisposeJobResult::Settled { .. })
}

enum DeferredTrustedAction {
    Restore {
        host_public_id: [u8; 16],
        explicit_retry: bool,
    },
    Forget {
        host_public_id: [u8; 16],
    },
}

struct PendingHostDispose {
    host_id: HostId,
    job: RemoteBlockingWork<DisposeJobResult>,
    /// Shared until the OS worker takes the runtime; reclaim on spawn/poll failure.
    custody: Arc<Mutex<Option<NativeHostClientRuntime>>>,
    failed: bool,
}

struct StrandedDispose {
    host_id: HostId,
    attachment: NativeHostRuntimeAttachment,
}

/// Shell-owned trusted-PC state. Parent adds `trusted_hosts: NativeTrustedHostsState`.
pub(crate) struct NativeTrustedHostsState {
    /// Drop pending bootstrap receivers before jobs so Ok(runtime) settles on worker threads.
    pending_setup: Option<PendingTrustedSetup>,
    enroll_prep: Option<RemoteBlockingWork<EnrollPrepResult>>,
    roster_job: Option<RemoteBlockingWork<RosterJobResult>>,
    forget_job: Option<RemoteBlockingWork<ForgetJobResult>>,
    forget_host_public_id: Option<[u8; 16]>,
    dispose_jobs: Vec<PendingHostDispose>,
    stranded_disposals: Vec<StrandedDispose>,
    deferred_actions: VecDeque<DeferredTrustedAction>,
    coordinator: Arc<TrustedHostsCoordinator>,
    store_root: Option<PathBuf>,
    store: Option<RemoteTrustStore>,
    fields: Option<TrustedHostsFields>,
    pub(crate) controller: TrustedHostsController,
    root_resolved: bool,
}

impl Default for NativeTrustedHostsState {
    fn default() -> Self {
        Self {
            pending_setup: None,
            enroll_prep: None,
            roster_job: None,
            forget_job: None,
            forget_host_public_id: None,
            dispose_jobs: Vec::new(),
            stranded_disposals: Vec::new(),
            deferred_actions: VecDeque::new(),
            coordinator: Arc::new(TrustedHostsCoordinator::new()),
            store_root: None,
            store: None,
            fields: None,
            controller: TrustedHostsController::default(),
            root_resolved: false,
        }
    }
}

impl NativeTrustedHostsState {
    /// Call at the **start** of `NativeShell::drop`, before host-slot teardown, so pending
    /// setup/forget receivers are fenced while attached runtimes still exist.
    pub fn shutdown_pending(&mut self) {
        for pending in self.dispose_jobs.drain(..) {
            if let Ok(mut guard) = pending.custody.lock() {
                if let Some(runtime) = guard.take() {
                    self.stranded_disposals.push(StrandedDispose {
                        host_id: pending.host_id,
                        attachment: NativeHostRuntimeAttachment::Client(runtime),
                    });
                }
            }
            drop(pending.job);
            self.controller.release_forget_ledger_slot();
        }
        if self.forget_job.is_some() {
            self.controller.release_forget_ledger_slot();
        }
        self.pending_setup = None;
        self.enroll_prep = None;
        self.roster_job = None;
        self.forget_job = None;
        self.forget_host_public_id = None;
        self.deferred_actions.clear();
        self.stranded_disposals.clear();
    }
}

impl Drop for NativeTrustedHostsState {
    fn drop(&mut self) {
        self.shutdown_pending();
    }
}

fn poll_blocking_job<T: Send + 'static>(
    job: &mut RemoteBlockingWork<T>,
) -> Option<Result<T, RemoteWorkError>> {
    match job.try_take() {
        Ok(Some(value)) => Some(Ok(value)),
        Ok(None) => None,
        Err(error) => Some(Err(error)),
    }
}

fn safe_host_label(record: &TrustedHostRecord) -> String {
    let host = hex_encode(&record.host_public_id);
    let host_short = host.get(..12).unwrap_or(host.as_str());
    let pin = hex_encode(&record.host_key_pin.as_bytes());
    let pin_short = pin.get(..12).unwrap_or(pin.as_str());
    format!("{} · id {host_short}… · pin {pin_short}…", record.endpoint)
}

/// Exact OS-worker disposal: typed `remove_at_generation` proof before Drop.
fn dispose_client_runtime_exact(
    mut runtime: NativeHostClientRuntime,
    admission: RemoteWorkAdmission,
    deadline: Instant,
) -> DisposeJobResult {
    let host_id = runtime.host_id().clone();
    let _ = admission.try_admit();
    runtime.begin_shutdown();

    let fleet = Arc::clone(runtime.fleet());
    let expected_generation = match fleet.owner_metadata(&host_id) {
        Ok(meta) => meta.generation,
        Err(_) => {
            // Cannot prove exact removal; disarm Drop remove and fail closed.
            runtime.owns_fleet_slot = false;
            drop(runtime);
            return DisposeJobResult::Failed {
                host_id,
                reason: DisposeFailureReason::CleanupIncomplete,
            };
        }
    };

    // Never let Drop host-only-remove a possible replacement generation.
    runtime.owns_fleet_slot = false;
    let Some(runtime_guard) = runtime.runtime_guard.clone() else {
        drop(runtime);
        return DisposeJobResult::Failed {
            host_id,
            reason: DisposeFailureReason::Unavailable,
        };
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        drop(runtime);
        return DisposeJobResult::Failed {
            host_id,
            reason: DisposeFailureReason::Timeout,
        };
    }
    let removal = match runtime_guard.block_on(async {
        tokio::time::timeout(
            remaining,
            fleet.remove_at_generation(&host_id, expected_generation),
        )
        .await
    }) {
        Ok(Ok(removal)) if removal.generation == expected_generation => removal,
        Ok(Err(FleetError::StaleGeneration | FleetError::StaleReservation)) => {
            drop(runtime);
            return DisposeJobResult::Failed {
                host_id,
                reason: DisposeFailureReason::StaleGeneration,
            };
        }
        Ok(Ok(_)) | Ok(Err(_)) => {
            drop(runtime);
            return DisposeJobResult::Failed {
                host_id,
                reason: DisposeFailureReason::CleanupIncomplete,
            };
        }
        Err(_) => {
            drop(runtime);
            return DisposeJobResult::Failed {
                host_id,
                reason: DisposeFailureReason::Timeout,
            };
        }
    };
    drop(runtime);
    DisposeJobResult::Settled { host_id, removal }
}

impl NativeShell {
    pub(crate) fn ensure_trusted_host_fields(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.trusted_hosts.fields.is_some() {
            return;
        }
        self.trusted_hosts.fields = Some(TrustedHostsFields {
            endpoint: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("https://other-pc:8443")
                    .default_value("")
            }),
            pairing_code: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("One-time pairing code")
                    .masked(true)
                    .default_value("")
            }),
            ca_pem_path: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("Optional absolute CA PEM path")
                    .default_value("")
            }),
        });
    }

    /// Controller tick: roster load, autorestore, setup/forget/dispose polls.
    pub(crate) fn poll_trusted_hosts(&mut self) -> bool {
        let mut repaint = false;
        if let Some(reason) = self.trusted_hosts.coordinator.recovery_reason() {
            if self.trusted_hosts.controller.recovery() != Some(reason) {
                self.trusted_hosts.controller.set_recovery(Some(reason));
                repaint = true;
            }
        }
        if self.ensure_trusted_hosts_root_and_roster() {
            repaint = true;
        }
        if self.poll_trusted_roster_job() {
            repaint = true;
        }
        if self.reconcile_trusted_host_runtime_status() {
            repaint = true;
        }
        if self.poll_trusted_enroll_prep() {
            repaint = true;
        }
        if self.poll_trusted_pending_setup() {
            repaint = true;
        }
        if self.poll_trusted_forget_job() {
            repaint = true;
        }
        if self.poll_trusted_dispose_jobs() {
            repaint = true;
        }
        if self.drive_trusted_auto_restore() {
            repaint = true;
        }
        repaint
    }

    fn local_client_fleet(&self) -> Option<Arc<HostFleet>> {
        match self.local_slot().host_runtime.as_ref()? {
            NativeHostRuntimeAttachment::Client(runtime) => Some(Arc::clone(runtime.fleet())),
            NativeHostRuntimeAttachment::Injected(_) => None,
        }
    }

    fn ensure_trusted_hosts_root_and_roster(&mut self) -> bool {
        if self.local_client_fleet().is_none() {
            return false;
        }
        if !self.trusted_hosts.root_resolved {
            match trust_store_root_for_profile(self.profile()) {
                Ok(root) => {
                    // Isolated profiles resolve under host_config_base — never installed appdata.
                    if matches!(self.profile().mode(), NativeShellMode::IsolatedDebug)
                        && !root.starts_with(self.profile().host_config_base())
                    {
                        self.trusted_hosts.controller.mark_roster_failed(
                            "Isolated profile refused a trust root outside its host config base.",
                        );
                        self.trusted_hosts.root_resolved = true;
                        return true;
                    }
                    self.trusted_hosts.store_root = Some(root);
                    self.trusted_hosts.root_resolved = true;
                }
                Err(_) => {
                    self.trusted_hosts
                        .controller
                        .mark_roster_failed("Trusted-host profile root could not be resolved.");
                    self.trusted_hosts.root_resolved = true;
                    return true;
                }
            }
        }
        if !self.trusted_hosts.controller.should_fetch_roster()
            || self.trusted_hosts.roster_job.is_some()
            || self.trusted_hosts.store_root.is_none()
        {
            return false;
        }
        let root = self
            .trusted_hosts
            .store_root
            .clone()
            .expect("store root present");
        let deadline = Instant::now() + TRUSTED_ROSTER_BUDGET;
        match RemoteBlockingWork::spawn(
            "native-trusted-hosts-roster",
            deadline,
            move |admission: RemoteWorkAdmission| {
                if admission.cancellation_requested() || !admission.try_admit() {
                    return RosterJobResult::Failed("roster load cancelled".into());
                }
                let store = match RemoteTrustStore::open(root) {
                    Ok(store) => store,
                    Err(error) => {
                        return RosterJobResult::Failed(error.as_str().into());
                    }
                };
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(_) => return RosterJobResult::Failed("roster runtime unavailable".into()),
                };
                let roster = runtime.block_on(load_trusted_host_roster_until(&store, deadline));
                RosterJobResult::Opened { store, roster }
            },
        ) {
            Ok(job) => {
                self.trusted_hosts.roster_job = Some(job);
                true
            }
            Err(_) => {
                self.trusted_hosts
                    .controller
                    .mark_roster_failed("Trusted-host roster worker unavailable.");
                true
            }
        }
    }

    fn poll_trusted_roster_job(&mut self) -> bool {
        let Some(result) = self
            .trusted_hosts
            .roster_job
            .as_mut()
            .and_then(poll_blocking_job)
        else {
            return false;
        };
        let _ = self.trusted_hosts.roster_job.take();
        match result {
            Ok(RosterJobResult::Opened { store, roster }) => {
                self.trusted_hosts.store = Some(store);
                match roster {
                    RosterLoadResult::Listed(records) => {
                        self.trusted_hosts.controller.apply_roster(records);
                    }
                    RosterLoadResult::Failed(error) => {
                        self.trusted_hosts
                            .controller
                            .mark_roster_failed(error.as_str());
                    }
                }
            }
            Ok(RosterJobResult::Failed(message)) => {
                self.trusted_hosts.controller.mark_roster_failed(message);
            }
            Err(_) => {
                self.trusted_hosts
                    .controller
                    .mark_roster_failed("Trusted-host roster timed out.");
            }
        }
        true
    }

    fn drive_trusted_auto_restore(&mut self) -> bool {
        if self.trusted_hosts.pending_setup.is_some()
            || self.trusted_hosts.forget_job.is_some()
            || self.trusted_hosts.enroll_prep.is_some()
            || self.trusted_hosts.controller.recovery().is_some()
        {
            return false;
        }
        let Some(host_public_id) = self
            .trusted_hosts
            .controller
            .next_auto_restore_host(Instant::now())
        else {
            return false;
        };
        self.start_trusted_restore(host_public_id, false)
    }

    fn host_disposal_blocked(&self, host_id: &HostId) -> bool {
        self.trusted_hosts
            .dispose_jobs
            .iter()
            .any(|pending| &pending.host_id == host_id)
            || self
                .trusted_hosts
                .stranded_disposals
                .iter()
                .any(|stranded| &stranded.host_id == host_id)
    }

    fn reconcile_trusted_host_runtime_status(&mut self) -> bool {
        let mut observations = Vec::new();
        for row in self.trusted_hosts.controller.rows() {
            let Ok(host_id) = HostId::remote(row.record.host_public_id) else {
                continue;
            };
            let observation = match self.host_slot(&host_id) {
                None => TrustedHostRuntimeObservation::Disconnected,
                Some(slot) => match (&slot.host_runtime, &slot.host_state) {
                    (Some(NativeHostRuntimeAttachment::Client(runtime)), _) => {
                        match runtime.fleet().owner_metadata(runtime.host_id()) {
                            Ok(meta) if meta.client_id == row.record.assigned_client_id => {
                                TrustedHostRuntimeObservation::LiveOwnerMatch {
                                    endpoint: runtime.endpoint().to_string(),
                                }
                            }
                            Ok(_) => TrustedHostRuntimeObservation::AttachmentPresent {
                                detail: Some("live owner does not match trusted record".into()),
                            },
                            Err(_) => match &slot.host_state {
                                NativeHostState::Error { message } => {
                                    TrustedHostRuntimeObservation::AttachmentPresent {
                                        detail: Some(message.clone()),
                                    }
                                }
                                NativeHostState::Disconnected | NativeHostState::Connecting => {
                                    TrustedHostRuntimeObservation::AttachmentPresent {
                                        detail: None,
                                    }
                                }
                                NativeHostState::Connected { endpoint } => {
                                    // Attachment present; reconnect owns retries even if metadata is transiently busy.
                                    TrustedHostRuntimeObservation::AttachmentPresent {
                                        detail: Some(format!(
                                            "runtime attached at {endpoint}; reconnect in progress"
                                        )),
                                    }
                                }
                            },
                        }
                    }
                    (Some(NativeHostRuntimeAttachment::Injected(_)), _) => {
                        TrustedHostRuntimeObservation::AttachmentPresent { detail: None }
                    }
                    (None, NativeHostState::Error { message }) => {
                        TrustedHostRuntimeObservation::TransportFailed {
                            message: message.clone(),
                        }
                    }
                    (None, NativeHostState::Disconnected | NativeHostState::Connecting) => {
                        TrustedHostRuntimeObservation::Disconnected
                    }
                    (None, NativeHostState::Connected { .. }) => {
                        TrustedHostRuntimeObservation::Disconnected
                    }
                },
            };
            observations.push((row.record.host_public_id, observation));
        }
        let before = self
            .trusted_hosts
            .controller
            .rows()
            .iter()
            .map(|row| (row.record.host_public_id, row.phase, row.last_error.clone()))
            .collect::<Vec<_>>();
        self.trusted_hosts
            .controller
            .reconcile_runtime_status(&observations);
        let after = self
            .trusted_hosts
            .controller
            .rows()
            .iter()
            .map(|row| (row.record.host_public_id, row.phase, row.last_error.clone()))
            .collect::<Vec<_>>();
        before != after
    }

    fn queue_deferred_trusted_action(&mut self, action: DeferredTrustedAction) {
        let duplicate = match &action {
            DeferredTrustedAction::Restore { host_public_id, .. } => {
                self.trusted_hosts.deferred_actions.iter().any(|existing| {
                    matches!(
                        existing,
                        DeferredTrustedAction::Restore {
                            host_public_id: id,
                            ..
                        } if id == host_public_id
                    )
                })
            }
            DeferredTrustedAction::Forget { host_public_id } => {
                self.trusted_hosts.deferred_actions.iter().any(|existing| {
                    matches!(
                        existing,
                        DeferredTrustedAction::Forget {
                            host_public_id: id
                        } if id == host_public_id
                    )
                })
            }
        };
        if !duplicate {
            self.trusted_hosts.deferred_actions.push_back(action);
        }
    }

    fn take_host_runtime_attachment(
        &mut self,
        host_id: &HostId,
    ) -> Option<NativeHostRuntimeAttachment> {
        let slot = self.host_slot_mut(host_id)?;
        let attachment = slot.host_runtime.take()?;
        slot.host_state = NativeHostState::Disconnected;
        Some(attachment)
    }

    fn start_trusted_restore(&mut self, host_public_id: [u8; 16], explicit_retry: bool) -> bool {
        if self.trusted_hosts.controller.recovery().is_some() {
            self.trusted_hosts
                .controller
                .set_recovery(self.trusted_hosts.controller.recovery());
            return true;
        }
        if !self.trusted_hosts.controller.roster_authorizes_restore() {
            self.trusted_hosts.controller.set_feedback(
                "Trusted roster is not current; refresh Other PCs before restore.".to_string(),
            );
            return true;
        }
        if explicit_retry {
            self.trusted_hosts
                .controller
                .clear_suppress_for_retry(host_public_id);
        }
        let Ok(host_id) = HostId::remote(host_public_id) else {
            self.trusted_hosts
                .controller
                .mark_setup_failed(Some(host_public_id), "Invalid remote host identity.".into());
            return true;
        };
        if !TrustedHostsController::is_remote_trust_target(&host_id) {
            self.trusted_hosts.controller.mark_setup_failed(
                Some(host_public_id),
                "Refusing local-profile fallback for trusted PC restore.".into(),
            );
            return true;
        }
        if self.host_disposal_blocked(&host_id) {
            self.queue_deferred_trusted_action(DeferredTrustedAction::Restore {
                host_public_id,
                explicit_retry,
            });
            self.trusted_hosts
                .controller
                .set_feedback("Waiting for prior runtime disposal before restore.".to_string());
            return true;
        }
        // While an attachment remains, the runtime reconnect loop owns retries.
        if self
            .host_slot(&host_id)
            .is_some_and(|slot| slot.host_runtime.is_some())
        {
            self.trusted_hosts
                .controller
                .set_feedback("Host runtime still attached; reconnect owns retries.".to_string());
            return true;
        }
        if let Err(error) = self
            .trusted_hosts
            .controller
            .admit_setup_for_host(host_public_id)
        {
            self.trusted_hosts
                .controller
                .set_feedback(format!("Restore not started: {error:?}"));
            return true;
        }
        let Some(fleet) = self.local_client_fleet() else {
            self.trusted_hosts.controller.mark_setup_failed(
                Some(host_public_id),
                "Local Client runtime is not attached.".into(),
            );
            return true;
        };
        let Some(store_root) = self.trusted_hosts.store_root.clone() else {
            self.trusted_hosts
                .controller
                .mark_setup_failed(Some(host_public_id), "Trust store root missing.".into());
            return true;
        };
        let record = match self.trusted_hosts.controller.record(host_public_id) {
            Some(record) => record.clone(),
            None => {
                self.trusted_hosts
                    .controller
                    .mark_setup_failed(Some(host_public_id), "Trusted host record missing.".into());
                return true;
            }
        };
        let options = ConnectTrustedOptions {
            additional_ca_pem: record.additional_ca_pem.clone(),
            ..ConnectTrustedOptions::default()
        };
        let deadline = Instant::now() + TRUSTED_SETUP_BUDGET;
        match self.trusted_hosts.coordinator.spawn_restore(
            self.profile().clone(),
            fleet,
            store_root,
            host_public_id,
            options,
            deadline,
        ) {
            Ok((pending, outcome)) => {
                self.trusted_hosts.pending_setup = Some(PendingTrustedSetup {
                    pending,
                    outcome,
                    host_public_id: Some(host_public_id),
                    deadline,
                    cancelled: false,
                });
                true
            }
            Err(error) => {
                self.trusted_hosts
                    .controller
                    .mark_setup_failed(Some(host_public_id), error.to_string());
                true
            }
        }
    }

    fn poll_trusted_enroll_prep(&mut self) -> bool {
        let Some(result) = self
            .trusted_hosts
            .enroll_prep
            .as_mut()
            .and_then(poll_blocking_job)
        else {
            return false;
        };
        let _ = self.trusted_hosts.enroll_prep.take();
        match result {
            Ok(EnrollPrepResult::Ready(request)) => self.spawn_trusted_enroll(request),
            Ok(EnrollPrepResult::Failed(message)) => {
                self.trusted_hosts.controller.release_setup_host(None);
                self.trusted_hosts.controller.set_feedback(message);
                true
            }
            Err(_) => {
                self.trusted_hosts.controller.release_setup_host(None);
                self.trusted_hosts
                    .controller
                    .set_feedback("Pairing preparation timed out.".to_string());
                true
            }
        }
    }

    fn spawn_trusted_enroll(&mut self, request: PairEnrollRequest) -> bool {
        let Some(fleet) = self.local_client_fleet() else {
            self.trusted_hosts.controller.release_setup_host(None);
            self.trusted_hosts
                .controller
                .set_feedback("Local Client runtime is not attached.".to_string());
            return true;
        };
        let Some(store_root) = self.trusted_hosts.store_root.clone() else {
            self.trusted_hosts.controller.release_setup_host(None);
            self.trusted_hosts
                .controller
                .set_feedback("Trust store root missing.".to_string());
            return true;
        };
        let deadline = Instant::now() + TRUSTED_SETUP_BUDGET;
        match self.trusted_hosts.coordinator.spawn_enroll(
            self.profile().clone(),
            fleet,
            store_root,
            request,
            deadline,
        ) {
            Ok((pending, outcome)) => {
                self.trusted_hosts.pending_setup = Some(PendingTrustedSetup {
                    pending,
                    outcome,
                    host_public_id: None,
                    deadline,
                    cancelled: false,
                });
                true
            }
            Err(error) => {
                self.trusted_hosts.controller.release_setup_host(None);
                self.trusted_hosts
                    .controller
                    .set_feedback(error.to_string());
                true
            }
        }
    }

    fn poll_trusted_pending_setup(&mut self) -> bool {
        let Some(pending) = self.trusted_hosts.pending_setup.as_mut() else {
            return false;
        };
        let Some(result_rx) = pending.pending.result_rx.as_mut() else {
            return false;
        };
        let received = match result_rx.try_recv() {
            Ok(result) => Some(result),
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Some(Err(NativeShellError::HostConnect {
                    message: "trusted host bootstrap worker disconnected".into(),
                }))
            }
        };
        let Some(result) = received else {
            if Instant::now() >= pending.deadline {
                pending.cancelled = true;
                // Drop receiver without attaching; worker owns Ok(runtime).
                let mut setup = self
                    .trusted_hosts
                    .pending_setup
                    .take()
                    .expect("pending setup");
                drop(setup.pending.result_rx.take());
                drop(setup.pending);
                self.trusted_hosts.controller.mark_setup_failed(
                    setup.host_public_id,
                    "Trusted host setup deadline expired.".into(),
                );
                return true;
            }
            return false;
        };
        let mut setup = self
            .trusted_hosts
            .pending_setup
            .take()
            .expect("pending setup after recv");
        drop(setup.pending.result_rx.take());
        let worker = setup.pending.worker.take();
        let outcome = setup.outcome.take();
        let gate = TrustedHostsController::gate_bootstrap_attach(
            setup.cancelled,
            Instant::now() >= setup.deadline,
            outcome.is_some(),
        );
        match (result, gate, outcome) {
            (
                Ok(NativeHostRuntimeAttachment::Client(runtime)),
                BootstrapAttachGate::Accept,
                Some(success),
            ) => {
                let host_public_id = success.record.host_public_id;
                match self.attach_trusted_client_runtime(runtime) {
                    Ok(_) => {
                        if setup.host_public_id.is_some() {
                            self.trusted_hosts.controller.mark_connected(host_public_id);
                        } else {
                            self.trusted_hosts
                                .controller
                                .upsert_enrolled(success.record);
                        }
                        self.trusted_hosts.controller.clear_feedback();
                    }
                    Err((error, runtime)) => {
                        let dispose_host = runtime.host_id().clone();
                        self.schedule_trusted_runtime_dispose(
                            dispose_host,
                            NativeHostRuntimeAttachment::Client(runtime),
                        );
                        self.trusted_hosts
                            .controller
                            .mark_setup_failed(Some(host_public_id), error.to_string());
                    }
                }
            }
            (Ok(NativeHostRuntimeAttachment::Client(runtime)), _, _) => {
                let dispose_host = runtime.host_id().clone();
                self.schedule_trusted_runtime_dispose(
                    dispose_host,
                    NativeHostRuntimeAttachment::Client(runtime),
                );
                self.trusted_hosts.controller.mark_setup_failed(
                    setup.host_public_id,
                    "Trusted host setup result rejected.".into(),
                );
            }
            (Ok(NativeHostRuntimeAttachment::Injected(_)), _, _) => {
                self.trusted_hosts.controller.mark_setup_failed(
                    setup.host_public_id,
                    "Trusted host setup produced an unexpected injected runtime.".into(),
                );
            }
            (Err(error), _, _) => {
                self.trusted_hosts
                    .controller
                    .mark_setup_failed(setup.host_public_id, error.to_string());
            }
        }
        super::finish_pending_bootstrap_worker(worker);
        true
    }

    /// Attach a brand-new host or replace a runtime-absent disconnected slot.
    fn attach_trusted_client_runtime(
        &mut self,
        runtime: NativeHostClientRuntime,
    ) -> Result<HostId, (NativeShellError, NativeHostClientRuntime)> {
        let host_id = runtime.host_id().clone();
        if !TrustedHostsController::is_remote_trust_target(&host_id) {
            return Err((
                NativeShellError::HostConnect {
                    message: "refusing local-profile attach for trusted PC".into(),
                },
                runtime,
            ));
        }
        if let Some(NativeHostRuntimeAttachment::Client(local)) =
            self.local_slot().host_runtime.as_ref()
        {
            if !Arc::ptr_eq(local.fleet(), runtime.fleet()) {
                return Err((
                    NativeShellError::HostConnect {
                        message: "attached host must share the shell HostFleet Arc".into(),
                    },
                    runtime,
                ));
            }
        }
        if let Some(slot) = self.host_slot_mut(&host_id) {
            if slot.host_runtime.is_some() {
                return Err((
                    NativeShellError::HostConnect {
                        message: "dispose existing runtime before same-host install".into(),
                    },
                    runtime,
                ));
            }
            let endpoint = runtime.endpoint().to_string();
            slot.host_runtime = Some(NativeHostRuntimeAttachment::Client(runtime));
            slot.host_state = NativeHostState::Connected { endpoint };
            self.fleet_drain_cursor
                .sync_hosts(self.installed_fleet_host_ids());
            return Ok(host_id);
        }
        self.attach_installed_fleet_host(runtime)
    }

    fn schedule_trusted_runtime_dispose(
        &mut self,
        host_id: HostId,
        attachment: NativeHostRuntimeAttachment,
    ) {
        let NativeHostRuntimeAttachment::Client(runtime) = attachment else {
            self.trusted_hosts
                .coordinator
                .enter_recovery(RecoveryReason::CleanupIncomplete);
            self.trusted_hosts
                .controller
                .set_recovery(Some(RecoveryReason::CleanupIncomplete));
            self.trusted_hosts.stranded_disposals.push(StrandedDispose {
                host_id,
                attachment,
            });
            return;
        };
        if self.trusted_hosts.dispose_jobs.len() >= MAX_TRUSTED_REMOTE_HOSTS {
            self.trusted_hosts
                .coordinator
                .enter_recovery(RecoveryReason::CleanupIncomplete);
            self.trusted_hosts
                .controller
                .set_recovery(Some(RecoveryReason::CleanupIncomplete));
            self.trusted_hosts.controller.set_feedback(
                "Runtime disposal capacity exhausted; recovery required.".to_string(),
            );
            self.trusted_hosts.stranded_disposals.push(StrandedDispose {
                host_id,
                attachment: NativeHostRuntimeAttachment::Client(runtime),
            });
            return;
        }
        if !self
            .trusted_hosts
            .controller
            .try_reserve_forget_ledger_slot()
        {
            self.trusted_hosts
                .coordinator
                .enter_recovery(RecoveryReason::CleanupIncomplete);
            self.trusted_hosts
                .controller
                .set_recovery(Some(RecoveryReason::CleanupIncomplete));
            self.trusted_hosts.controller.set_feedback(
                "Retained forget ledger is full; recovery required before cleanup.".to_string(),
            );
            self.trusted_hosts.stranded_disposals.push(StrandedDispose {
                host_id,
                attachment: NativeHostRuntimeAttachment::Client(runtime),
            });
            return;
        }
        let deadline = Instant::now() + TRUSTED_DISPOSE_BUDGET;
        let custody = Arc::new(Mutex::new(Some(runtime)));
        let worker_custody = Arc::clone(&custody);
        let worker_host_id = host_id.clone();
        match RemoteBlockingWork::spawn(
            "native-trusted-host-dispose",
            deadline,
            move |admission: RemoteWorkAdmission| {
                let Some(runtime) = worker_custody
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                else {
                    return DisposeJobResult::Failed {
                        host_id: worker_host_id,
                        reason: DisposeFailureReason::Unavailable,
                    };
                };
                dispose_client_runtime_exact(runtime, admission, deadline)
            },
        ) {
            Ok(job) => self.trusted_hosts.dispose_jobs.push(PendingHostDispose {
                host_id,
                job,
                custody,
                failed: false,
            }),
            Err(_) => {
                self.trusted_hosts.controller.release_forget_ledger_slot();
                self.trusted_hosts
                    .coordinator
                    .enter_recovery(RecoveryReason::CleanupIncomplete);
                self.trusted_hosts
                    .controller
                    .set_recovery(Some(RecoveryReason::CleanupIncomplete));
                self.trusted_hosts.controller.set_feedback(
                    "Runtime disposal worker unavailable; recovery required.".to_string(),
                );
                if let Some(runtime) = custody.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    self.trusted_hosts.stranded_disposals.push(StrandedDispose {
                        host_id,
                        attachment: NativeHostRuntimeAttachment::Client(runtime),
                    });
                }
            }
        }
    }

    fn poll_trusted_dispose_jobs(&mut self) -> bool {
        let mut changed = false;
        let mut retained = Vec::new();
        let mut completed = Vec::new();
        for mut pending in self.trusted_hosts.dispose_jobs.drain(..) {
            if pending.failed {
                retained.push(pending);
                continue;
            }
            match poll_blocking_job(&mut pending.job) {
                Some(Ok(result)) => {
                    if dispose_permits_deferred(&result) {
                        if let DisposeJobResult::Settled { host_id, removal } = result {
                            if !removal.retained.is_empty() || !removal.uncertain.is_empty() {
                                if let Some(host_public_id) = host_id.as_remote() {
                                    let key = ForgetLedgerKey {
                                        host_public_id,
                                        generation: removal.generation,
                                    };
                                    let had_key = self
                                        .trusted_hosts
                                        .controller
                                        .retained_forget_ledgers()
                                        .contains_key(&key);
                                    let outcome = RetainedForgetOutcome {
                                        host_public_id,
                                        persistence: ForgetPersistence::DefinitelyPreserved,
                                        removal: Some(removal),
                                        persist_error: None,
                                    };
                                    if !self
                                        .trusted_hosts
                                        .controller
                                        .insert_retained_forget(key, outcome.clone())
                                    {
                                        self.trusted_hosts
                                            .controller
                                            .hold_refused_retained_forget(key, outcome);
                                        self.trusted_hosts
                                            .coordinator
                                            .enter_recovery(RecoveryReason::CleanupIncomplete);
                                        self.trusted_hosts
                                            .controller
                                            .set_recovery(Some(RecoveryReason::CleanupIncomplete));
                                        self.trusted_hosts.controller.release_forget_ledger_slot();
                                    } else if had_key {
                                        self.trusted_hosts.controller.release_forget_ledger_slot();
                                    }
                                } else {
                                    self.trusted_hosts.controller.release_forget_ledger_slot();
                                }
                            } else {
                                self.trusted_hosts.controller.release_forget_ledger_slot();
                            }
                            completed.push(host_id);
                        } else {
                            self.trusted_hosts.controller.release_forget_ledger_slot();
                        }
                    } else {
                        self.trusted_hosts.controller.release_forget_ledger_slot();
                        pending.failed = true;
                        self.trusted_hosts
                            .coordinator
                            .enter_recovery(RecoveryReason::CleanupIncomplete);
                        self.trusted_hosts
                            .controller
                            .set_recovery(Some(RecoveryReason::CleanupIncomplete));
                        if let Some(runtime) = pending
                            .custody
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .take()
                        {
                            self.trusted_hosts.stranded_disposals.push(StrandedDispose {
                                host_id: pending.host_id.clone(),
                                attachment: NativeHostRuntimeAttachment::Client(runtime),
                            });
                        }
                        retained.push(pending);
                    }
                    changed = true;
                }
                Some(Err(_)) => {
                    self.trusted_hosts.controller.release_forget_ledger_slot();
                    pending.failed = true;
                    self.trusted_hosts
                        .coordinator
                        .enter_recovery(RecoveryReason::CleanupIncomplete);
                    self.trusted_hosts
                        .controller
                        .set_recovery(Some(RecoveryReason::CleanupIncomplete));
                    if let Some(runtime) = pending
                        .custody
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take()
                    {
                        self.trusted_hosts.stranded_disposals.push(StrandedDispose {
                            host_id: pending.host_id.clone(),
                            attachment: NativeHostRuntimeAttachment::Client(runtime),
                        });
                    }
                    retained.push(pending);
                    changed = true;
                }
                None => retained.push(pending),
            }
        }
        self.trusted_hosts.dispose_jobs = retained;
        for host_id in completed {
            self.drain_deferred_trusted_actions_for(&host_id);
        }
        changed
    }

    fn drain_deferred_trusted_actions_for(&mut self, host_id: &HostId) {
        let mut rest = VecDeque::new();
        while let Some(action) = self.trusted_hosts.deferred_actions.pop_front() {
            let matches = match &action {
                DeferredTrustedAction::Restore { host_public_id, .. }
                | DeferredTrustedAction::Forget { host_public_id } => {
                    HostId::remote(*host_public_id).ok().as_ref() == Some(host_id)
                }
            };
            if matches {
                match action {
                    DeferredTrustedAction::Restore {
                        host_public_id,
                        explicit_retry,
                    } => {
                        let _ = self.start_trusted_restore(host_public_id, explicit_retry);
                    }
                    DeferredTrustedAction::Forget { host_public_id } => {
                        self.trusted_forget_host(host_public_id);
                    }
                }
            } else {
                rest.push_back(action);
            }
        }
        self.trusted_hosts.deferred_actions = rest;
    }

    fn poll_trusted_forget_job(&mut self) -> bool {
        let Some(result) = self
            .trusted_hosts
            .forget_job
            .as_mut()
            .and_then(poll_blocking_job)
        else {
            return false;
        };
        let _ = self.trusted_hosts.forget_job.take();
        let pending_host = self.trusted_hosts.forget_host_public_id.take();
        match result {
            Ok(ForgetJobResult::Finished {
                host_public_id,
                result,
            }) => {
                let forgotten = matches!(result.persistence, ForgetPersistence::Forgotten);
                let host_id = HostId::remote(host_public_id).ok();
                self.trusted_hosts
                    .controller
                    .apply_forget_result(host_public_id, result);
                if forgotten {
                    if let Some(host_id) = host_id.as_ref() {
                        self.forget_fleet_host(host_id);
                    }
                }
            }
            Ok(ForgetJobResult::Failed {
                host_public_id,
                message,
                maybe_admitted,
            }) => {
                self.trusted_hosts.controller.release_forget_ledger_slot();
                if maybe_admitted {
                    self.trusted_hosts
                        .coordinator
                        .enter_recovery(RecoveryReason::PersistenceUncertain);
                    self.trusted_hosts
                        .controller
                        .set_recovery(Some(RecoveryReason::PersistenceUncertain));
                }
                self.trusted_hosts
                    .controller
                    .mark_setup_failed(Some(host_public_id), message);
            }
            Err(_) => {
                // Deadline on the poll path is not proof the OS worker finished or rolled back.
                self.trusted_hosts.controller.release_forget_ledger_slot();
                let host = pending_host;
                self.trusted_hosts
                    .coordinator
                    .enter_recovery(RecoveryReason::PersistenceUncertain);
                self.trusted_hosts
                    .controller
                    .set_recovery(Some(RecoveryReason::PersistenceUncertain));
                if let Some(host_public_id) = host {
                    self.trusted_hosts.controller.mark_setup_failed(
                        Some(host_public_id),
                        "Forget timed out without a definite result; restart to recover.".into(),
                    );
                } else {
                    self.trusted_hosts.controller.set_feedback(
                        "Forget timed out without a definite result; restart to recover."
                            .to_string(),
                    );
                }
            }
        }
        true
    }

    fn trusted_connect_clicked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.trusted_hosts.controller.recovery().is_some() {
            self.trusted_hosts.controller.set_recovery(Some(
                self.trusted_hosts
                    .controller
                    .recovery()
                    .unwrap_or(RecoveryReason::PersistenceUncertain),
            ));
            return;
        }
        if let Err(error) = self.trusted_hosts.controller.admit_enroll() {
            self.trusted_hosts
                .controller
                .set_feedback(format!("Connect not started: {error:?}"));
            return;
        }
        let Some(fields) = self.trusted_hosts.fields.as_ref() else {
            self.trusted_hosts.controller.release_setup_host(None);
            return;
        };
        let endpoint = fields.endpoint.read(cx).value().trim().to_string();
        let code = Zeroizing::new(fields.pairing_code.read(cx).value().to_string());
        fields.pairing_code.update(cx, |input, cx| {
            input.set_value("", window, cx);
        });
        let ca_path = fields.ca_pem_path.read(cx).value().trim().to_string();
        if endpoint.is_empty() || code.trim().is_empty() {
            self.trusted_hosts.controller.release_setup_host(None);
            self.trusted_hosts
                .controller
                .set_feedback("Endpoint and pairing code are required.".to_string());
            return;
        }
        let deadline = Instant::now() + TRUSTED_SETUP_BUDGET;
        let pair_deadline = ConnectTrustedOptions::default().deadline;
        match RemoteBlockingWork::spawn(
            "native-trusted-enroll-prep",
            deadline,
            move |admission: RemoteWorkAdmission| {
                if admission.cancellation_requested() || !admission.try_admit() {
                    return EnrollPrepResult::Failed("pairing cancelled".into());
                }
                let additional_ca_pem = if ca_path.is_empty() {
                    None
                } else {
                    match read_additional_ca_pem_bounded(Path::new(&ca_path)) {
                        Ok(pem) => Some(pem),
                        Err(message) => return EnrollPrepResult::Failed(message),
                    }
                };
                EnrollPrepResult::Ready(PairEnrollRequest {
                    endpoint,
                    pairing_code: code,
                    label: None,
                    additional_ca_pem,
                    deadline: pair_deadline,
                })
            },
        ) {
            Ok(job) => self.trusted_hosts.enroll_prep = Some(job),
            Err(_) => {
                self.trusted_hosts.controller.release_setup_host(None);
                self.trusted_hosts
                    .controller
                    .set_feedback("Pairing worker unavailable.".to_string());
            }
        }
    }

    fn trusted_refresh_clicked(&mut self) {
        if self.trusted_hosts.roster_job.is_some() {
            return;
        }
        self.trusted_hosts.controller.invalidate_roster();
        self.trusted_hosts.store = None;
        let _ = self.ensure_trusted_hosts_root_and_roster();
    }

    fn trusted_disconnect_host(&mut self, host_public_id: [u8; 16]) {
        let Ok(host_id) = HostId::remote(host_public_id) else {
            return;
        };
        if let Some(attachment) = self.take_host_runtime_attachment(&host_id) {
            self.schedule_trusted_runtime_dispose(host_id, attachment);
        }
        self.trusted_hosts
            .controller
            .mark_disconnected(host_public_id);
        self.fleet_drain_cursor
            .sync_hosts(self.installed_fleet_host_ids());
    }

    fn trusted_forget_host(&mut self, host_public_id: [u8; 16]) {
        if self.trusted_hosts.controller.recovery().is_some() {
            self.trusted_hosts
                .controller
                .set_recovery(self.trusted_hosts.controller.recovery());
            return;
        }
        if self.trusted_hosts.forget_job.is_some() {
            self.trusted_hosts
                .controller
                .set_feedback("A forget operation is already in progress.".to_string());
            return;
        }
        let Ok(host_id) = HostId::remote(host_public_id) else {
            return;
        };
        if self.host_disposal_blocked(&host_id) {
            self.queue_deferred_trusted_action(DeferredTrustedAction::Forget { host_public_id });
            self.trusted_hosts
                .controller
                .set_feedback("Waiting for prior runtime disposal before forget.".to_string());
            return;
        }
        if let Some(attachment) = self.take_host_runtime_attachment(&host_id) {
            self.schedule_trusted_runtime_dispose(host_id, attachment);
            self.queue_deferred_trusted_action(DeferredTrustedAction::Forget { host_public_id });
            self.trusted_hosts
                .controller
                .set_feedback("Disposing prior runtime before forget.".to_string());
            return;
        }
        if !self
            .trusted_hosts
            .controller
            .try_reserve_forget_ledger_slot()
        {
            self.trusted_hosts.controller.set_feedback(
                "Retained forget ledger is full; acknowledge unresolved removals before forgetting."
                    .to_string(),
            );
            return;
        }
        if let Err(error) = self.trusted_hosts.controller.begin_forget(host_public_id) {
            self.trusted_hosts.controller.release_forget_ledger_slot();
            self.trusted_hosts
                .controller
                .set_feedback(format!("Forget not started: {error:?}"));
            return;
        }
        let Some(record) = self
            .trusted_hosts
            .controller
            .record(host_public_id)
            .cloned()
        else {
            self.trusted_hosts.controller.release_forget_ledger_slot();
            return;
        };
        // Quiesce pending setup for this host before durable forget.
        if let Some(mut setup) = self.trusted_hosts.pending_setup.take() {
            if setup.host_public_id == Some(host_public_id) {
                setup.cancelled = true;
                drop(setup.pending.result_rx.take());
                drop(setup.pending);
                self.trusted_hosts
                    .controller
                    .release_setup_host(Some(host_public_id));
            } else {
                self.trusted_hosts.pending_setup = Some(setup);
            }
        }
        let Some(fleet) = self.local_client_fleet() else {
            self.trusted_hosts.controller.release_forget_ledger_slot();
            self.trusted_hosts.controller.mark_setup_failed(
                Some(host_public_id),
                "Local Client runtime is not attached.".into(),
            );
            return;
        };
        let Some(store_root) = self.trusted_hosts.store_root.clone() else {
            self.trusted_hosts.controller.release_forget_ledger_slot();
            self.trusted_hosts
                .controller
                .mark_setup_failed(Some(host_public_id), "Trust store root missing.".into());
            return;
        };
        let coordinator = Arc::clone(&self.trusted_hosts.coordinator);
        let deadline = Instant::now() + TRUSTED_FORGET_BUDGET;
        match RemoteBlockingWork::spawn(
            "native-trusted-host-forget",
            deadline,
            move |admission: RemoteWorkAdmission| {
                if admission.cancellation_requested() || !admission.try_admit() {
                    return ForgetJobResult::Failed {
                        host_public_id,
                        message: "forget cancelled before admission".into(),
                        maybe_admitted: false,
                    };
                }
                let store = match RemoteTrustStore::open(store_root) {
                    Ok(store) => store,
                    Err(error) => {
                        return ForgetJobResult::Failed {
                            host_public_id,
                            message: error.as_str().into(),
                            maybe_admitted: false,
                        }
                    }
                };
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(_) => {
                        return ForgetJobResult::Failed {
                            host_public_id,
                            message: "forget runtime unavailable".into(),
                            maybe_admitted: false,
                        }
                    }
                };
                match runtime.block_on(coordinator.forget_exact(&fleet, &store, record, deadline)) {
                    Ok(result) => ForgetJobResult::Finished {
                        host_public_id,
                        result,
                    },
                    Err(TrustedHostsError::RecoveryRequired) => ForgetJobResult::Failed {
                        host_public_id,
                        message: "forget entered recovery".into(),
                        maybe_admitted: true,
                    },
                    Err(error) => {
                        let maybe_admitted = matches!(
                            error,
                            TrustedHostsError::Busy
                                | TrustedHostsError::Cancelled
                                | TrustedHostsError::Deadline
                                | TrustedHostsError::Fleet(_)
                        );
                        ForgetJobResult::Failed {
                            host_public_id,
                            message: format!("{error:?}"),
                            maybe_admitted,
                        }
                    }
                }
            },
        ) {
            Ok(job) => {
                self.trusted_hosts.forget_host_public_id = Some(host_public_id);
                self.trusted_hosts.forget_job = Some(job);
            }
            Err(_) => {
                self.trusted_hosts.controller.release_forget_ledger_slot();
                self.trusted_hosts
                    .coordinator
                    .enter_recovery(RecoveryReason::PersistenceUncertain);
                self.trusted_hosts
                    .controller
                    .set_recovery(Some(RecoveryReason::PersistenceUncertain));
                self.trusted_hosts.controller.mark_setup_failed(
                    Some(host_public_id),
                    "Forget worker unavailable without a definite result.".into(),
                );
            }
        }
    }

    pub(crate) fn render_trusted_hosts_content(
        &self,
        tokens: crate::ui::tokens::ThemeTokens,
        cx: &Context<Self>,
    ) -> AnyElement {
        let busy = self.trusted_hosts.controller.setup_busy()
            || self.trusted_hosts.forget_job.is_some()
            || self.trusted_hosts.controller.recovery().is_some();
        let mut section = div()
            .flex()
            .flex_col()
            .gap(px(overlay_chrome::REGION_PADDING))
            .mt(px(16.0))
            .text_size(px(overlay_chrome::BODY_FONT_SIZE))
            .child(overlay_chrome::field_label("Other PCs", tokens))
            .child(overlay_chrome::caption(
                "Pair another DevManager PC over your LAN. Saved PCs restore into this window's fleet; closing this window does not erase durable trust.",
                tokens,
            ));
        if let Some(fields) = &self.trusted_hosts.fields {
            section = section
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(overlay_chrome::CONTROL_GAP))
                        .child(
                            div()
                                .w(px(140.0))
                                .flex_shrink_0()
                                .text_size(px(overlay_chrome::SECTION_LABEL_FONT_SIZE))
                                .text_color(tokens.text.muted.to_gpui())
                                .child("ENDPOINT"),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Input::new(&fields.endpoint).small().disabled(busy)),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(overlay_chrome::CONTROL_GAP))
                        .child(
                            div()
                                .w(px(140.0))
                                .flex_shrink_0()
                                .text_size(px(overlay_chrome::SECTION_LABEL_FONT_SIZE))
                                .text_color(tokens.text.muted.to_gpui())
                                .child("PAIRING CODE"),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Input::new(&fields.pairing_code).small().disabled(busy)),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(overlay_chrome::CONTROL_GAP))
                        .child(
                            div()
                                .w(px(140.0))
                                .flex_shrink_0()
                                .text_size(px(overlay_chrome::SECTION_LABEL_FONT_SIZE))
                                .text_color(tokens.text.muted.to_gpui())
                                .child("CA PEM PATH"),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Input::new(&fields.ca_pem_path).small().disabled(busy)),
                        ),
                );
        }
        let mut actions = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(overlay_chrome::CONTROL_GAP))
            .child(
                Button::new("native-trusted-connect")
                    .label("Connect")
                    .primary()
                    .small()
                    .icon(IconName::Plus)
                    .disabled(busy)
                    .on_click(cx.listener(|shell, _: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        shell.trusted_connect_clicked(window, cx);
                        cx.notify();
                    })),
            );
        actions = actions.child(
            Button::new("native-trusted-refresh")
                .label("Refresh")
                .small()
                .ghost()
                .disabled(self.trusted_hosts.roster_job.is_some())
                .on_click(cx.listener(|shell, _: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    shell.trusted_refresh_clicked();
                    cx.notify();
                })),
        );
        section = section.child(actions);
        if let Some(feedback) = self.trusted_hosts.controller.feedback() {
            section = section.child(
                div()
                    .text_size(px(overlay_chrome::BODY_FONT_SIZE))
                    .text_color(tokens.status.warning.to_gpui())
                    .child(SharedString::from(feedback.to_string())),
            );
        }
        if !self
            .trusted_hosts
            .controller
            .retained_forget_ledgers()
            .is_empty()
        {
            for (key, retained) in self.trusted_hosts.controller.retained_forget_ledgers() {
                section = section.child(
                    div()
                        .text_size(px(overlay_chrome::ROW_META_FONT_SIZE))
                        .text_color(tokens.text.secondary.to_gpui())
                        .child(format!(
                            "Retained forget ledger {} gen {}: {:?}",
                            hex_encode(&key.host_public_id).get(..12).unwrap_or("host"),
                            key.generation,
                            retained.persistence
                        )),
                );
            }
        }
        for row in self.trusted_hosts.controller.rows() {
            let id = row.record.host_public_id;
            let status = match row.phase {
                TrustedHostRowPhase::Connected => "Active",
                TrustedHostRowPhase::Connecting | TrustedHostRowPhase::Queued => "Connecting…",
                TrustedHostRowPhase::Offline => "Offline",
                TrustedHostRowPhase::Failed => "Recovery needed",
                TrustedHostRowPhase::DisconnectSuppressed => "Disconnected (retry paused)",
                TrustedHostRowPhase::Disconnecting => "Disconnecting…",
                TrustedHostRowPhase::Forgetting => "Forgetting…",
            };
            let mut row_el = div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .px(px(overlay_chrome::ROW_PADDING_X))
                .py(px(overlay_chrome::ROW_PADDING_Y))
                .border_b(px(overlay_chrome::OVERLAY_BORDER_WIDTH))
                .border_color(tokens.borders.subtle.to_gpui())
                .child(
                    div()
                        .text_size(px(overlay_chrome::ROW_TITLE_FONT_SIZE))
                        .line_height(px(overlay_chrome::ROW_TITLE_LINE_HEIGHT))
                        .text_color(tokens.text.primary.to_gpui())
                        .child(safe_host_label(&row.record)),
                )
                .child(
                    div()
                        .text_size(px(overlay_chrome::ROW_META_FONT_SIZE))
                        .line_height(px(overlay_chrome::ROW_META_LINE_HEIGHT))
                        .text_color(tokens.text.muted.to_gpui())
                        .child(status),
                );
            if let Some(error) = &row.last_error {
                row_el = row_el.child(
                    div()
                        .text_size(px(overlay_chrome::ROW_META_FONT_SIZE))
                        .text_color(tokens.status.destructive.to_gpui())
                        .child(error.clone()),
                );
            }
            let mut buttons = div().flex().flex_wrap().gap(px(overlay_chrome::CHIP_GAP));
            if matches!(
                row.phase,
                TrustedHostRowPhase::Failed
                    | TrustedHostRowPhase::DisconnectSuppressed
                    | TrustedHostRowPhase::Offline
            ) {
                buttons = buttons.child(
                    Button::new(SharedString::from(format!(
                        "native-trusted-retry-{}",
                        hex_encode(&id)
                    )))
                    .label("Retry")
                    .small()
                    .disabled(busy)
                    .on_click(cx.listener(
                        move |shell, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            let _ = shell.start_trusted_restore(id, true);
                            cx.notify();
                        },
                    )),
                );
            }
            if matches!(row.phase, TrustedHostRowPhase::Connected) {
                buttons = buttons.child(
                    Button::new(SharedString::from(format!(
                        "native-trusted-disconnect-{}",
                        hex_encode(&id)
                    )))
                    .label("Disconnect")
                    .small()
                    .ghost()
                    .disabled(busy)
                    .on_click(cx.listener(
                        move |shell, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            shell.trusted_disconnect_host(id);
                            cx.notify();
                        },
                    )),
                );
            }
            if row.forget_confirm {
                buttons = buttons
                    .child(
                        Button::new(SharedString::from(format!(
                            "native-trusted-forget-yes-{}",
                            hex_encode(&id)
                        )))
                        .label("Confirm forget")
                        .small()
                        .disabled(busy)
                        .on_click(cx.listener(
                            move |shell, _: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                shell.trusted_forget_host(id);
                                cx.notify();
                            },
                        )),
                    )
                    .child(
                        Button::new(SharedString::from(format!(
                            "native-trusted-forget-no-{}",
                            hex_encode(&id)
                        )))
                        .label("Cancel")
                        .small()
                        .ghost()
                        .on_click(cx.listener(
                            move |shell, _: &ClickEvent, _, cx| {
                                cx.stop_propagation();
                                shell.trusted_hosts.controller.cancel_forget_confirm(id);
                                cx.notify();
                            },
                        )),
                    );
            } else {
                buttons = buttons.child(
                    Button::new(SharedString::from(format!(
                        "native-trusted-forget-{}",
                        hex_encode(&id)
                    )))
                    .label("Forget")
                    .small()
                    .ghost()
                    .disabled(busy)
                    .on_click(cx.listener(
                        move |shell, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            shell.trusted_hosts.controller.request_forget_confirm(id);
                            cx.notify();
                        },
                    )),
                );
            }
            row_el = row_el.child(buttons);
            section = section.child(row_el);
        }
        section.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::FleetRemoval;
    use crate::connect::ConnectNoiseStaticPublicKey;
    use crate::domain::ClientId;

    fn fixture_pin(byte: u8) -> ConnectNoiseStaticPublicKey {
        let mut bytes = [byte.max(1); 32];
        bytes[0] = byte.max(1);
        ConnectNoiseStaticPublicKey::from_bytes(bytes).expect("fixture pin")
    }

    fn fixture_uuid_bytes(seed: u8) -> [u8; 16] {
        let mut id = [seed.max(1); 16];
        id[6] = 0x70;
        id[8] = 0x80;
        id
    }

    fn fixture_record(tail: u8, endpoint: &str) -> TrustedHostRecord {
        TrustedHostRecord {
            host_public_id: fixture_uuid_bytes(tail),
            host_key_pin: fixture_pin(tail.max(1)),
            endpoint: endpoint.into(),
            connect_path: "/api/connect".into(),
            assigned_client_id: ClientId::from_bytes(fixture_uuid_bytes(tail.wrapping_add(0x40)))
                .expect("client"),
            additional_ca_pem: None,
        }
    }

    fn host_id(tail: u8) -> [u8; 16] {
        fixture_record(tail, "https://pc.example:8443").host_public_id
    }

    #[test]
    fn duplicate_restore_admission_is_rejected() {
        let mut ctl = TrustedHostsController::default();
        let record = fixture_record(0x11, "https://a.example:8443");
        let id = record.host_public_id;
        ctl.apply_roster(vec![record]);
        ctl.admit_setup_for_host(id).expect("first");
        assert!(matches!(
            ctl.admit_setup_for_host(id),
            Err(TrustedHostsError::Busy)
        ));
    }

    #[test]
    fn disconnect_suppresses_auto_retry_until_explicit_retry() {
        let mut ctl = TrustedHostsController::default();
        let record = fixture_record(0x12, "https://b.example:8443");
        let id = record.host_public_id;
        ctl.apply_roster(vec![record]);
        ctl.mark_connected(id);
        ctl.mark_disconnected(id);
        assert!(ctl.next_auto_restore_host(Instant::now()).is_none());
        ctl.clear_suppress_for_retry(id);
        assert_eq!(ctl.next_auto_restore_host(Instant::now()), Some(id));
    }

    #[test]
    fn failure_does_not_block_next_owner() {
        let mut ctl = TrustedHostsController::default();
        let a = fixture_record(0x21, "https://a.example:8443");
        let b = fixture_record(0x22, "https://b.example:8443");
        let id_a = a.host_public_id;
        let id_b = b.host_public_id;
        ctl.apply_roster(vec![a, b]);
        ctl.admit_setup_for_host(id_a).expect("a");
        ctl.mark_setup_failed(Some(id_a), "boom".into());
        ctl.admit_setup_for_host(id_b).expect("b after a failed");
        assert!(ctl.pending_setup_hosts.contains_key(&id_b));
        assert!(!ctl.pending_setup_hosts.contains_key(&id_a));
    }

    #[test]
    fn forget_confirmation_does_not_erase_record_early() {
        let mut ctl = TrustedHostsController::default();
        let record = fixture_record(0x31, "https://c.example:8443");
        let id = record.host_public_id;
        ctl.apply_roster(vec![record]);
        ctl.request_forget_confirm(id);
        assert_eq!(ctl.rows().len(), 1);
        assert!(ctl.rows()[0].forget_confirm);
        ctl.cancel_forget_confirm(id);
        assert_eq!(ctl.rows().len(), 1);
        assert!(!ctl.rows()[0].forget_confirm);
    }

    #[test]
    fn expired_or_cancelled_bootstrap_cannot_attach() {
        assert_eq!(
            TrustedHostsController::gate_bootstrap_attach(true, false, true),
            BootstrapAttachGate::RejectExpiredOrCancelled
        );
        assert_eq!(
            TrustedHostsController::gate_bootstrap_attach(false, true, true),
            BootstrapAttachGate::RejectExpiredOrCancelled
        );
        assert_eq!(
            TrustedHostsController::gate_bootstrap_attach(false, false, false),
            BootstrapAttachGate::RejectExpiredOrCancelled
        );
        assert_eq!(
            TrustedHostsController::gate_bootstrap_attach(false, false, true),
            BootstrapAttachGate::Accept
        );
    }

    #[test]
    fn no_local_profile_fallback_for_trust_targets() {
        let local = HostId::local_profile("wt_trusted_hosts_view_local").expect("local");
        assert!(!TrustedHostsController::is_remote_trust_target(&local));
        let remote = HostId::remote(host_id(0x41)).expect("remote");
        assert!(TrustedHostsController::is_remote_trust_target(&remote));
    }

    #[test]
    fn forget_uncertain_retains_fleet_removal_and_row() {
        let mut ctl = TrustedHostsController::default();
        let record = fixture_record(0x51, "https://d.example:8443");
        let id = record.host_public_id;
        ctl.apply_roster(vec![record.clone()]);
        ctl.request_forget_confirm(id);
        ctl.begin_forget(id).expect("forget");
        let removal = FleetRemoval {
            host: HostId::remote(id).expect("id"),
            generation: 3,
            client_id: Some(record.assigned_client_id),
            retained: Vec::new(),
            uncertain: Vec::new(),
        };
        ctl.apply_forget_result(
            id,
            ForgetTrustedHostResult {
                removal: Some(removal.clone()),
                persistence: ForgetPersistence::PersistenceUncertain,
                persist_error: Some(RemoteTrustError::PersistenceUncertain),
            },
        );
        assert_eq!(ctl.rows().len(), 1);
        let key = ForgetLedgerKey {
            host_public_id: id,
            generation: 3,
        };
        let retained = ctl.retained_forget_ledgers().get(&key).expect("retained");
        assert_eq!(
            retained.persistence,
            ForgetPersistence::PersistenceUncertain
        );
        assert_eq!(retained.removal.as_ref().map(|r| r.generation), Some(3));
        assert_eq!(ctl.recovery(), Some(RecoveryReason::PersistenceUncertain));
    }

    #[test]
    fn forget_success_on_b_does_not_erase_a_ledger() {
        let mut ctl = TrustedHostsController::default();
        let a = fixture_record(0x61, "https://a.example:8443");
        let b = fixture_record(0x62, "https://b.example:8443");
        let id_a = a.host_public_id;
        let id_b = b.host_public_id;
        ctl.apply_roster(vec![a.clone(), b]);
        ctl.request_forget_confirm(id_a);
        ctl.begin_forget(id_a).expect("forget a");
        ctl.apply_forget_result(
            id_a,
            ForgetTrustedHostResult {
                removal: Some(FleetRemoval {
                    host: HostId::remote(id_a).expect("a"),
                    generation: 7,
                    client_id: Some(a.assigned_client_id),
                    retained: Vec::new(),
                    uncertain: Vec::new(),
                }),
                persistence: ForgetPersistence::DefinitelyPreserved,
                persist_error: Some(RemoteTrustError::Unavailable),
            },
        );
        assert!(ctl.recovery().is_none());
        ctl.request_forget_confirm(id_b);
        ctl.begin_forget(id_b).expect("forget b");
        ctl.apply_forget_result(
            id_b,
            ForgetTrustedHostResult {
                removal: None,
                persistence: ForgetPersistence::Forgotten,
                persist_error: None,
            },
        );
        let key_a = ForgetLedgerKey {
            host_public_id: id_a,
            generation: 7,
        };
        assert!(ctl.retained_forget_ledgers().contains_key(&key_a));
        assert!(ctl
            .rows()
            .iter()
            .all(|row| row.record.host_public_id != id_b));
    }

    fn nonempty_removal(id: [u8; 16], generation: u64) -> FleetRemoval {
        use crate::client::FleetUncertainCommand;
        use crate::domain::CommandId;
        FleetRemoval {
            host: HostId::remote(id).expect("remote"),
            generation,
            client_id: None,
            retained: Vec::new(),
            uncertain: vec![FleetUncertainCommand {
                admission: crate::client::FleetAdmission {
                    host: HostId::remote(id).expect("remote"),
                    task_id: None,
                    generation,
                    client_id: ClientId::from_bytes(fixture_uuid_bytes(0xCC)).expect("client"),
                },
                command_id: CommandId::from_bytes(fixture_uuid_bytes(0xCD)).expect("command"),
            }],
        }
    }

    #[test]
    fn retained_forget_refuses_new_key_when_full_without_eviction() {
        let mut ctl = TrustedHostsController::default();
        let mut first_key = None;
        for seed in 1u8..=MAX_TRUSTED_REMOTE_HOSTS as u8 {
            let id = host_id(seed);
            let key = ForgetLedgerKey {
                host_public_id: id,
                generation: 1,
            };
            if first_key.is_none() {
                first_key = Some(key);
            }
            assert!(ctl.insert_retained_forget(
                key,
                RetainedForgetOutcome {
                    host_public_id: id,
                    persistence: ForgetPersistence::DefinitelyPreserved,
                    removal: Some(nonempty_removal(id, 1)),
                    persist_error: None,
                },
            ));
        }
        let overflow_id = host_id(0xFE);
        let overflow_key = ForgetLedgerKey {
            host_public_id: overflow_id,
            generation: 1,
        };
        let overflow = RetainedForgetOutcome {
            host_public_id: overflow_id,
            persistence: ForgetPersistence::DefinitelyPreserved,
            removal: Some(nonempty_removal(overflow_id, 1)),
            persist_error: None,
        };
        assert!(!ctl.insert_retained_forget(overflow_key, overflow.clone()));
        assert_eq!(
            ctl.retained_forget_ledgers().len(),
            MAX_TRUSTED_REMOTE_HOSTS
        );
        assert!(ctl
            .retained_forget_ledgers()
            .contains_key(&first_key.expect("first")));
        ctl.hold_refused_retained_forget(overflow_key, overflow);
        assert!(ctl.ledger_overflow_hold().is_some());
    }

    #[test]
    fn retained_forget_does_not_overwrite_nonempty_removal_with_empty() {
        let mut ctl = TrustedHostsController::default();
        let id = host_id(0xA1);
        let key = ForgetLedgerKey {
            host_public_id: id,
            generation: 9,
        };
        assert!(ctl.insert_retained_forget(
            key,
            RetainedForgetOutcome {
                host_public_id: id,
                persistence: ForgetPersistence::DefinitelyPreserved,
                removal: Some(nonempty_removal(id, 9)),
                persist_error: None,
            },
        ));
        assert!(ctl.insert_retained_forget(
            key,
            RetainedForgetOutcome {
                host_public_id: id,
                persistence: ForgetPersistence::Forgotten,
                removal: None,
                persist_error: None,
            },
        ));
        let kept = ctl.retained_forget_ledgers().get(&key).expect("kept");
        assert!(kept
            .removal
            .as_ref()
            .is_some_and(|r| !r.uncertain.is_empty()));
        assert_eq!(kept.persistence, ForgetPersistence::DefinitelyPreserved);
    }

    #[test]
    fn forgotten_result_removes_row_only_then() {
        let mut ctl = TrustedHostsController::default();
        let record = fixture_record(0x52, "https://e.example:8443");
        let id = record.host_public_id;
        ctl.apply_roster(vec![record]);
        ctl.request_forget_confirm(id);
        ctl.begin_forget(id).expect("forget");
        ctl.apply_forget_result(
            id,
            ForgetTrustedHostResult {
                removal: None,
                persistence: ForgetPersistence::Forgotten,
                persist_error: None,
            },
        );
        assert!(ctl.rows().is_empty());
        assert!(ctl.retained_forget_ledgers().is_empty());
    }

    #[test]
    fn dispose_failed_result_does_not_permit_deferred_retry() {
        let host = HostId::remote(host_id(0x71)).expect("remote");
        let failed = DisposeJobResult::Failed {
            host_id: host.clone(),
            reason: DisposeFailureReason::Timeout,
        };
        assert!(!dispose_permits_deferred(&failed));
        let settled = DisposeJobResult::Settled {
            host_id: host,
            removal: FleetRemoval {
                host: HostId::remote(host_id(0x71)).expect("remote"),
                generation: 4,
                client_id: None,
                retained: Vec::new(),
                uncertain: Vec::new(),
            },
        };
        assert!(dispose_permits_deferred(&settled));
    }

    #[test]
    fn failed_roster_gates_auto_restore_while_keeping_cached_rows() {
        let mut ctl = TrustedHostsController::default();
        let record = fixture_record(0x72, "https://roster.example:8443");
        let id = record.host_public_id;
        ctl.apply_roster(vec![record]);
        assert!(ctl.roster_authorizes_restore());
        assert_eq!(ctl.next_auto_restore_host(Instant::now()), Some(id));
        ctl.mark_roster_failed("list failed");
        assert!(!ctl.roster_authorizes_restore());
        assert_eq!(ctl.rows().len(), 1);
        assert!(ctl.next_auto_restore_host(Instant::now()).is_none());
        ctl.invalidate_roster();
        assert!(ctl.should_fetch_roster());
        assert!(ctl.next_auto_restore_host(Instant::now()).is_none());
    }

    #[test]
    fn runtime_status_reconcile_marks_transport_failure_without_manual_disconnect() {
        let mut ctl = TrustedHostsController::default();
        let record = fixture_record(0x73, "https://status.example:8443");
        let id = record.host_public_id;
        ctl.apply_roster(vec![record]);
        ctl.reconcile_runtime_status(&[(
            id,
            TrustedHostRuntimeObservation::TransportFailed {
                message: "peer reset".into(),
            },
        )]);
        assert_eq!(ctl.rows()[0].phase, TrustedHostRowPhase::Failed);
        assert_eq!(ctl.rows()[0].last_error.as_deref(), Some("peer reset"));
        ctl.reconcile_runtime_status(&[(
            id,
            TrustedHostRuntimeObservation::AttachmentPresent { detail: None },
        )]);
        assert_eq!(ctl.rows()[0].phase, TrustedHostRowPhase::Offline);
        ctl.reconcile_runtime_status(&[(
            id,
            TrustedHostRuntimeObservation::LiveOwnerMatch {
                endpoint: "https://status.example:8443".into(),
            },
        )]);
        assert_eq!(ctl.rows()[0].phase, TrustedHostRowPhase::Connected);
    }

    #[test]
    fn bounded_ca_pem_read_rejects_oversized_without_unbounded_allocation() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("ca.pem");
        let oversized = vec![b'A'; REMOTE_CA_PEM_MAX_BYTES + 8];
        std::fs::write(&path, &oversized).expect("write");
        let err = read_additional_ca_pem_bounded(&path).expect_err("oversize");
        assert!(err.contains("exceeds"));
        // Retain TempDir until assertions finish.
        drop(dir);
    }

    #[test]
    fn bounded_ca_pem_read_accepts_small_utf8_file() {
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("ca.pem");
        std::fs::write(
            &path,
            b"-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n",
        )
        .expect("write");
        let pem = read_additional_ca_pem_bounded(&path).expect("read");
        assert!(pem.contains("BEGIN CERTIFICATE"));
        drop(dir);
    }
}

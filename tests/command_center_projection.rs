//! Task 6.9 thin Command Center view.
//!
//! Consumes ClientModel + ActionCatalog only. ServiceCatalog is not live health.
//! Does not probe Git, accept a caller path, or take ServiceEvidence.

use std::cell::Cell;
use std::rc::Rc;

use devmanager::client::action::{
    catalog, ACTION_HOST_ACTIONS, ACTION_HOST_STATUS, ACTION_TASK_CREATE, ACTION_TASK_LIST,
    ACTION_TASK_RENAME, ACTION_TASK_SHOW,
};
use devmanager::client::command_center::{
    collect_unique, project_command_center, request_action, CanonicalProcessLabel,
    CommandCenterBoundError, CommandCenterInput, HoldDependency, ProcessFactError,
    UnavailableReason, MAX_COMMAND_CENTER_LABEL_BYTES, MAX_COMMAND_CENTER_SERVICE_ROWS,
};
use devmanager::services::model::{ServiceCatalog, ServiceId};

fn valid_catalog() -> ServiceCatalog {
    ServiceCatalog::decode_json(include_bytes!("fixtures/services/valid.json"))
        .expect("valid catalog fixture")
}

struct CountedIter<T> {
    item: T,
    remaining: usize,
    next_calls: Rc<Cell<usize>>,
}

impl<T: Clone> Iterator for CountedIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        self.next_calls.set(self.next_calls.get().saturating_add(1));
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        Some(self.item.clone())
    }
}

#[test]
fn absent_inputs_are_unavailable_never_zero_or_green() {
    let snapshot = project_command_center(&CommandCenterInput {
        model: None,
        actions: None,
    });

    assert_eq!(
        snapshot.tasks().unavailable_reason(),
        Some(UnavailableReason::HostFactMissing)
    );
    assert_eq!(
        snapshot.services().unavailable_reason(),
        Some(UnavailableReason::Hold {
            dependency: HoldDependency::ServiceHealth,
        })
    );
    assert_eq!(
        snapshot.ports().unavailable_reason(),
        Some(UnavailableReason::Hold {
            dependency: HoldDependency::PortInventory,
        })
    );
    assert_eq!(
        snapshot.processes().unavailable_reason(),
        Some(UnavailableReason::Hold {
            dependency: HoldDependency::ProcessAccounting,
        })
    );
    assert_eq!(
        snapshot.git().unavailable_reason(),
        Some(UnavailableReason::Hold {
            dependency: HoldDependency::GitWorkspaceAuthority,
        })
    );
    assert_eq!(
        snapshot.worktree().unavailable_reason(),
        Some(UnavailableReason::Hold {
            dependency: HoldDependency::GitWorkspaceAuthority,
        })
    );
    assert_eq!(
        snapshot.actions().unavailable_reason(),
        Some(UnavailableReason::Hold {
            dependency: HoldDependency::ActionCatalog,
        })
    );
    assert!(snapshot.tasks().ready().is_none());
    assert!(snapshot.services().ready().is_none());
    assert!(snapshot.actions().ready().is_none());

    let debug = format!("{snapshot:?}");
    assert!(
        !debug.to_ascii_lowercase().contains("healthy"),
        "missing facts must not project healthy/green: {debug}"
    );
    assert!(
        !debug.contains("cpu_percent: 0") && !debug.contains("memory_bytes: 0"),
        "missing accounting must not collapse to zero: {debug}"
    );
}

#[test]
fn service_catalog_config_cannot_imply_health_ports_process_or_action_availability() {
    let service_catalog = valid_catalog();
    assert!(
        service_catalog.definitions().count() >= 2,
        "fixture must contain configured services so this RED is not vacuous"
    );

    let snapshot = project_command_center(&CommandCenterInput {
        model: None,
        actions: Some(catalog()),
    });

    assert_eq!(
        snapshot.services().unavailable_reason(),
        Some(UnavailableReason::Hold {
            dependency: HoldDependency::ServiceHealth,
        }),
        "ServiceCatalog definitions must not open a Ready/live service section"
    );
    assert!(
        snapshot.services().ready().is_none(),
        "Hold-on-row Ready services are still live presentation"
    );
    assert_eq!(
        snapshot.ports().unavailable_reason(),
        Some(UnavailableReason::Hold {
            dependency: HoldDependency::PortInventory,
        })
    );
    assert_eq!(
        snapshot.processes().unavailable_reason(),
        Some(UnavailableReason::Hold {
            dependency: HoldDependency::ProcessAccounting,
        })
    );
    assert!(snapshot.ports().ready().is_none());
    assert!(snapshot.processes().ready().is_none());

    assert_eq!(
        snapshot.actions().unavailable_reason(),
        Some(UnavailableReason::Hold {
            dependency: HoldDependency::ActionCatalog,
        }),
        "ActionCatalog descriptors must not become a Ready action history"
    );
    assert!(snapshot.actions().ready().is_none());
    assert_eq!(
        request_action(Some(catalog()), "service.start").unwrap_err(),
        UnavailableReason::HostFactMissing
    );
    assert_eq!(
        request_action(Some(catalog()), "git.commit").unwrap_err(),
        UnavailableReason::HostFactMissing
    );
    assert_eq!(
        request_action(Some(catalog()), ACTION_TASK_SHOW).unwrap_err(),
        UnavailableReason::Hold {
            dependency: HoldDependency::ActionCatalog,
        }
    );

    let debug = format!("{snapshot:?}");
    assert!(
        !debug.to_ascii_lowercase().contains("healthy"),
        "config must not imply health: {debug}"
    );
}

#[test]
fn collect_unique_rejects_cap_plus_one_without_scanning_the_rest() {
    let next_calls = Rc::new(Cell::new(0));
    let overflow = CountedIter {
        item: ServiceId::new("api").expect("id"),
        remaining: MAX_COMMAND_CENTER_SERVICE_ROWS + 8,
        next_calls: Rc::clone(&next_calls),
    };

    let error = collect_unique(overflow, MAX_COMMAND_CENTER_SERVICE_ROWS, |id| id.clone())
        .expect_err("cap+1 must reject");
    assert_eq!(
        error,
        CommandCenterBoundError::TooMany {
            limit: MAX_COMMAND_CENTER_SERVICE_ROWS,
            inspected: MAX_COMMAND_CENTER_SERVICE_ROWS + 1,
        }
    );
    assert_eq!(
        next_calls.get(),
        MAX_COMMAND_CENTER_SERVICE_ROWS + 1,
        "collector must stop after the overflowing item"
    );
}

#[test]
fn process_label_rejects_path_command_line_and_secrets() {
    assert_eq!(
        CanonicalProcessLabel::try_from_untrusted_bytes(b"").unwrap_err(),
        ProcessFactError::EmptyLabel
    );
    let too_long = "a".repeat(MAX_COMMAND_CENTER_LABEL_BYTES + 1);
    assert!(matches!(
        CanonicalProcessLabel::try_from_untrusted_bytes(too_long.as_bytes()),
        Err(ProcessFactError::LabelTooLong { .. })
    ));
    assert_eq!(
        CanonicalProcessLabel::try_from_untrusted_bytes(br"C:\repo\node.exe").unwrap_err(),
        ProcessFactError::PathOrCommandLineLabel
    );
    assert_eq!(
        CanonicalProcessLabel::try_from_untrusted_bytes(b"--inspect").unwrap_err(),
        ProcessFactError::PathOrCommandLineLabel
    );
    assert_eq!(
        CanonicalProcessLabel::try_from_untrusted_bytes(b"TOKEN=secret").unwrap_err(),
        ProcessFactError::UntrustedLabel
    );

    for rejected in ["api\u{202e}db".as_bytes(), "api\u{fffe}".as_bytes()] {
        let error = CanonicalProcessLabel::try_from_untrusted_bytes(rejected).unwrap_err();
        assert_eq!(error, ProcessFactError::UntrustedLabel);
        assert!(format!("{error}").starts_with("cc.label."));
    }

    let trusted = CanonicalProcessLabel::from_service_id(ServiceId::new("api").expect("id"));
    assert_eq!(trusted.as_str(), "api");
}

#[test]
fn request_action_without_catalog_or_action_id_is_unavailable() {
    assert_eq!(
        request_action(None, ACTION_TASK_SHOW).unwrap_err(),
        UnavailableReason::HostFactMissing
    );
    assert_eq!(
        request_action(Some(catalog()), "service.start").unwrap_err(),
        UnavailableReason::HostFactMissing
    );
    assert_eq!(
        request_action(Some(catalog()), "git.commit").unwrap_err(),
        UnavailableReason::HostFactMissing
    );

    for id in [
        ACTION_HOST_ACTIONS,
        ACTION_HOST_STATUS,
        ACTION_TASK_LIST,
        ACTION_TASK_SHOW,
        ACTION_TASK_CREATE,
        ACTION_TASK_RENAME,
    ] {
        assert_eq!(
            request_action(Some(catalog()), id).unwrap_err(),
            UnavailableReason::Hold {
                dependency: HoldDependency::ActionCatalog,
            },
            "{id} must stay disabled until CC is given a host-issued ClientRequest factory"
        );
    }
}

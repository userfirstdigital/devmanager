use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use devmanager::browser::domain::{
    browser_integration_holds, decode_browser_request_json, decode_browser_request_wire,
    perform_browser_host_effect, replay_browser_snapshot, BrowserBook, BrowserDurableFact,
    BrowserHealth, BrowserHostHoldSettler, BrowserHostOutcome, BrowserHostSettleIntent,
    BrowserIntegrationHold, BrowserPageKey, BrowserServiceAuthority, BrowserServiceIssuer,
    BrowserServiceSettlerToken, BrowserSettlement, BrowserSnapshotSection, BrowserTabKind,
    MAX_BROWSER_CONTEXTS, MAX_BROWSER_FACT_URL_BYTES, MAX_BROWSER_IDENTITY_BYTES,
    MAX_BROWSER_JOURNAL_FACTS, MAX_BROWSER_OPEN_TASKS, MAX_BROWSER_RECEIPTS, MAX_BROWSER_TABS,
};
use devmanager::browser::protocol::{
    grant_browser_service_settler, settle_accepted_browser_hold, BrowserAction,
    BrowserContractError, BrowserHoldSettleError, BrowserRequest,
};
use devmanager::browser::BrowserCommand;
use devmanager::client::action::{
    catalog, execute_from_catalog, require_unique_ids, ActionArgumentSchema, ActionAvailability,
    ActionExecuteError, ActionRisk, ActionScope, ACTION_BROWSER_AUTOMATE, ACTION_BROWSER_BACK,
    ACTION_BROWSER_BOUNDS_SET, ACTION_BROWSER_CANCEL, ACTION_BROWSER_CAPTURE,
    ACTION_BROWSER_CONTEXT_CLOSE, ACTION_BROWSER_CONTEXT_CREATE, ACTION_BROWSER_DOWNLOAD_DECIDE,
    ACTION_BROWSER_FOCUS_SET, ACTION_BROWSER_FORWARD, ACTION_BROWSER_NAVIGATE,
    ACTION_BROWSER_PERMISSION_DECIDE, ACTION_BROWSER_RECORD, ACTION_BROWSER_RECOVER,
    ACTION_BROWSER_RELOAD, ACTION_BROWSER_REPLAY, ACTION_BROWSER_STOP, ACTION_BROWSER_TAB_CLOSE,
    ACTION_BROWSER_TAB_OPEN, ACTION_BROWSER_TAB_SELECT, ACTION_BROWSER_VISIBILITY_SET,
};
use devmanager::client::model::ClientModelBuilder;
use devmanager::domain::artifact::PrivacyClass;
use devmanager::domain::command::{
    decide, Command, CommandEnvelope, CommandReceipt, CreateTaskIntent, RejectionCode,
};
use devmanager::domain::event::{apply, apply_all, ApplyError, DomainEvent, Event};
use devmanager::domain::id::{
    ArtifactId, BrowserContextId, BrowserRequestId, BrowserTabId, ClientId, CommandId,
    EnvironmentId, EventId, OperationId, ProjectId, SnapshotId, TaskId,
};
use devmanager::domain::operation::OperationState;
use devmanager::domain::snapshot::{
    PageLimits, SnapshotItem, SnapshotPage, SnapshotSection, TaskSnapshotItem,
};
use devmanager::domain::task::{
    ReviewReadiness, TaskActivity, TaskAssignment, TaskAttention, TaskConnectivity, TaskFacts,
    TaskLifecycle, WorkspaceRef,
};
use devmanager::kernel::CommandBus;
use devmanager::kernel::{DispatchCompletion, Effect, ReplayPolicy};
use devmanager::protocol::{Capability, CapabilitySet, MAX_MESSAGEPACK_COLLECTION_ITEMS};
use static_assertions::assert_not_impl_any;
use tempfile::TempDir;

assert_not_impl_any!(BrowserTabId: From<BrowserContextId>, From<BrowserRequestId>, From<TaskId>);
assert_not_impl_any!(BrowserRequestId: From<BrowserTabId>, From<BrowserContextId>, From<TaskId>);
assert_not_impl_any!(
    BrowserCommand: From<Command>,
    From<BrowserRequest>,
    From<Effect>,
    From<DispatchCompletion>,
    From<BrowserHostSettleIntent>,
    From<BrowserServiceSettlerToken>,
    From<BrowserServiceAuthority>,
    From<BrowserServiceIssuer>,
    BrowserHostHoldSettler
);
assert_not_impl_any!(Command: From<BrowserCommand>);
assert_not_impl_any!(BrowserRequest: From<BrowserCommand>);
assert_not_impl_any!(Effect: From<BrowserCommand>);
assert_not_impl_any!(DispatchCompletion: From<BrowserCommand>);
assert_not_impl_any!(BrowserHostSettleIntent: From<BrowserCommand>, Default);
assert_not_impl_any!(BrowserServiceSettlerToken: From<BrowserCommand>, Default, From<bool>);
assert_not_impl_any!(
    BrowserServiceAuthority: From<BrowserCommand>,
    Default,
    From<bool>,
    From<()>
);
assert_not_impl_any!(
    BrowserServiceIssuer: From<BrowserCommand>,
    Default,
    From<bool>,
    From<()>
);
assert_not_impl_any!(BrowserServiceSettlerToken: PartialEq, std::fmt::Debug);
assert_not_impl_any!(BrowserServiceAuthority: PartialEq, std::fmt::Debug);

fn grant_must_hold(
    granted: CapabilitySet,
    authority: Option<&BrowserServiceAuthority>,
) -> BrowserIntegrationHold {
    match grant_browser_service_settler(granted, authority) {
        Err(hold) => hold,
        Ok(_) => panic!("BrowserServiceSettlerToken must remain uninhabited"),
    }
}

fn open_book(task_id: TaskId) -> BrowserBook {
    open_task_snapshot(task_id).browser
}

fn create_context(task_id: TaskId, context_id: BrowserContextId) -> BrowserRequest {
    BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id,
        context_id,
        tab_id: None,
        generation: 1,
        action: BrowserAction::CreateContext,
    }
}

fn open_page(
    task_id: TaskId,
    context_id: BrowserContextId,
    tab_id: BrowserTabId,
    url: &str,
) -> BrowserRequest {
    BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id,
        context_id,
        tab_id: Some(tab_id),
        generation: 1,
        action: BrowserAction::OpenTab {
            url: url.to_string(),
            kind: BrowserTabKind::Page,
        },
    }
}

#[test]
fn domain_context_and_tab_ids_are_central_typed_ids() {
    let context_id = BrowserContextId::new();
    let tab_id = BrowserTabId::new();
    let request_id = BrowserRequestId::new();
    assert_eq!(tab_id.as_bytes()[6] >> 4, 7);
    assert_eq!(request_id.as_bytes()[6] >> 4, 7);
    let tab_json = serde_json::to_string(&tab_id).expect("serialize tab id");
    let restored: BrowserTabId = serde_json::from_str(&tab_json).expect("tab id round-trip");
    assert_eq!(tab_id, restored);
    assert_ne!(format!("{context_id}"), format!("{tab_id}"));
}

#[test]
fn domain_context_has_exactly_one_task_owner() {
    let owner = TaskId::new();
    let other = TaskId::new();
    let context_id = BrowserContextId::new();
    let mut book = open_book(owner);
    book.open_task(other).expect("open other task");

    book.admit(&create_context(owner, context_id))
        .expect("owner can create the context");

    let stolen = BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id: other,
        context_id,
        tab_id: None,
        generation: 1,
        action: BrowserAction::CloseContext,
    };
    assert_eq!(
        book.admit(&stolen)
            .expect_err("foreign task cannot own context"),
        BrowserContractError::CrossTask
    );

    let page = book
        .snapshot_page(BrowserSnapshotSection::Contexts, None, 16, 64 * 1024)
        .expect("page contexts");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].task_id(), Some(owner));
}

#[test]
fn domain_generation_mismatch_is_rejected() {
    let task_id = TaskId::new();
    let context_id = BrowserContextId::new();
    let mut book = open_book(task_id);
    book.admit(&create_context(task_id, context_id))
        .expect("create context");

    let stale = BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id,
        context_id,
        tab_id: Some(BrowserTabId::new()),
        generation: 2,
        action: BrowserAction::OpenTab {
            url: "https://example.test".into(),
            kind: BrowserTabKind::Page,
        },
    };
    assert_eq!(
        book.admit(&stale).expect_err("stale generation"),
        BrowserContractError::GenerationMismatch
    );
}

#[test]
fn domain_popup_inherits_opener_task_and_context() {
    let task_id = TaskId::new();
    let context_id = BrowserContextId::new();
    let opener = BrowserTabId::new();
    let popup = BrowserTabId::new();
    let mut book = open_book(task_id);
    book.admit(&create_context(task_id, context_id))
        .expect("create context");
    book.admit(&open_page(
        task_id,
        context_id,
        opener,
        "https://example.test/app",
    ))
    .expect("open opener tab");

    let result = book
        .admit(&BrowserRequest {
            request_id: BrowserRequestId::new(),
            task_id,
            context_id,
            tab_id: Some(popup),
            generation: 1,
            action: BrowserAction::OpenTab {
                url: "https://example.test/popup".into(),
                kind: BrowserTabKind::Popup { opener },
            },
        })
        .expect("popup stays on opener lineage");

    assert!(result.facts.iter().any(|fact| matches!(
        fact,
        BrowserDurableFact::TabOpened {
            tab_id,
            context_id: opened_context,
            task_id: opened_task,
            kind: BrowserTabKind::Popup { opener: parent },
            ..
        } if *tab_id == popup
            && *opened_context == context_id
            && *opened_task == task_id
            && *parent == opener
    )));
}

#[test]
fn domain_closed_task_rejects_browser_requests() {
    let task_id = TaskId::new();
    let context_id = BrowserContextId::new();
    let mut book = open_book(task_id);
    book.admit(&create_context(task_id, context_id))
        .expect("create context");
    book.close_task(task_id).expect("close task");

    let error = book
        .admit(&open_page(
            task_id,
            context_id,
            BrowserTabId::new(),
            "https://example.test",
        ))
        .expect_err("closed task cannot admit work");
    assert_eq!(error, BrowserContractError::ClosedTask);
}

#[test]
fn domain_cross_task_ids_are_rejected_even_when_otherwise_valid() {
    let owner = TaskId::new();
    let other = TaskId::new();
    let context_id = BrowserContextId::new();
    let tab_id = BrowserTabId::new();
    let mut book = open_book(owner);
    book.open_task(other).expect("open other");
    book.admit(&create_context(owner, context_id))
        .expect("create context");
    book.admit(&open_page(
        owner,
        context_id,
        tab_id,
        "https://example.test",
    ))
    .expect("open tab");

    let error = book
        .admit(&BrowserRequest {
            request_id: BrowserRequestId::new(),
            task_id: other,
            context_id,
            tab_id: Some(tab_id),
            generation: 1,
            action: BrowserAction::Navigate {
                url: "https://example.test/other".into(),
            },
        })
        .expect_err("valid ids still require the owning task");
    assert_eq!(error, BrowserContractError::CrossTask);
}

#[test]
fn domain_duplicate_request_id_returns_same_receipt_and_does_not_perform_again() {
    let task_id = TaskId::new();
    let context_id = BrowserContextId::new();
    let tab_id = BrowserTabId::new();
    let mut book = open_book(task_id);
    book.admit(&create_context(task_id, context_id))
        .expect("create context");

    let request = open_page(task_id, context_id, tab_id, "https://example.test/once");
    let first = book.admit(&request).expect("first admission");
    let fact_count = book.facts().len();
    let second = book.admit(&request).expect("identical retry");
    assert_eq!(first, second);
    assert_eq!(book.facts().len(), fact_count);

    let navigate = BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id,
        context_id,
        tab_id: Some(tab_id),
        generation: 1,
        action: BrowserAction::Navigate {
            url: "https://example.test/ready".into(),
        },
    };
    let accepted = book.admit(&navigate).expect("admit navigation");
    assert!(accepted.facts.is_empty(), "admit is not host settlement");
    let settled = book
        .settle(&BrowserHostOutcome {
            request_id: navigate.request_id,
            task_id,
            context_id,
            tab_id: Some(tab_id),
            generation: 1,
            settlement: BrowserSettlement::NavigationCommitted {
                url: "https://example.test/ready".into(),
                document_id: "doc-1".into(),
            },
        })
        .expect("host settlement");
    assert!(settled.facts.iter().any(|fact| matches!(
        fact,
        BrowserDurableFact::NavigationCommitted { document_id, .. } if document_id == "doc-1"
    )));
    let retried = book.admit(&navigate).expect("retry after settle");
    assert_eq!(retried, accepted);
    assert_eq!(
        book.facts()
            .iter()
            .filter(|fact| matches!(fact, BrowserDurableFact::NavigationCommitted { .. }))
            .count(),
        1
    );

    let conflicting = BrowserRequest {
        request_id: request.request_id,
        task_id,
        context_id,
        tab_id: Some(BrowserTabId::new()),
        generation: 1,
        action: BrowserAction::OpenTab {
            url: "https://example.test/other".into(),
            kind: BrowserTabKind::Page,
        },
    };
    assert_eq!(
        book.admit(&conflicting)
            .expect_err("same request id cannot mean a different command"),
        BrowserContractError::IdempotencyConflict
    );
}

#[test]
fn domain_snapshot_replay_is_bounded_and_deterministic() {
    let task_id = TaskId::new();
    let context_id = BrowserContextId::new();
    let first_tab = BrowserTabId::new();
    let second_tab = BrowserTabId::new();
    let mut book = open_book(task_id);
    book.admit(&create_context(task_id, context_id))
        .expect("create context");
    book.admit(&open_page(
        task_id,
        context_id,
        first_tab,
        "https://example.test/a",
    ))
    .expect("open first tab");
    let navigate = BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id,
        context_id,
        tab_id: Some(first_tab),
        generation: 1,
        action: BrowserAction::Navigate {
            url: "https://example.test/a/ready".into(),
        },
    };
    book.admit(&navigate).expect("admit navigation");
    book.settle(&BrowserHostOutcome {
        request_id: navigate.request_id,
        task_id,
        context_id,
        tab_id: Some(first_tab),
        generation: 1,
        settlement: BrowserSettlement::NavigationCommitted {
            url: "https://example.test/a/ready".into(),
            document_id: "doc-ready".into(),
        },
    })
    .expect("settle navigation");
    book.admit(&open_page(
        task_id,
        context_id,
        second_tab,
        "https://example.test/b",
    ))
    .expect("open second tab");
    book.admit(&BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id,
        context_id,
        tab_id: Some(second_tab),
        generation: 1,
        action: BrowserAction::SelectTab,
    })
    .expect("select tab");
    let decide = BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id,
        context_id,
        tab_id: Some(first_tab),
        generation: 1,
        action: BrowserAction::DecidePermission {
            permission: devmanager::domain::browser::BrowserPermission::Capture,
            allowed: false,
        },
    };
    book.admit(&decide).expect("admit permission");
    book.settle(&BrowserHostOutcome {
        request_id: decide.request_id,
        task_id,
        context_id,
        tab_id: Some(first_tab),
        generation: 1,
        settlement: BrowserSettlement::PermissionDecided {
            permission: devmanager::domain::browser::BrowserPermission::Capture,
            allowed: false,
        },
    })
    .expect("settle permission");
    book.admit(&BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id,
        context_id,
        tab_id: Some(first_tab),
        generation: 1,
        action: BrowserAction::LinkArtifact {
            artifact_id: ArtifactId::new(),
        },
    })
    .expect("link artifact");
    let recover = BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id,
        context_id,
        tab_id: None,
        generation: 1,
        action: BrowserAction::Recover,
    };
    book.admit(&recover).expect("admit recover");
    book.settle(&BrowserHostOutcome {
        request_id: recover.request_id,
        task_id,
        context_id,
        tab_id: None,
        generation: 1,
        settlement: BrowserSettlement::Recovered { generation: 2 },
    })
    .expect("settle recover");

    let facts = book.facts().to_vec();
    let first = replay_browser_snapshot(&facts).expect("replay facts");
    let second = replay_browser_snapshot(&facts).expect("replay is deterministic");
    assert_eq!(first, second);
    let live = book
        .snapshot_page(BrowserSnapshotSection::Contexts, None, 16, 64 * 1024)
        .expect("live page");
    assert_eq!(first.contexts[0].generation, 2);
    assert_eq!(first.contexts[0].selected_tab_id, Some(second_tab));
    assert_eq!(first.contexts[0].health, BrowserHealth::Recovering);
    assert_eq!(live.items[0].generation(), Some(2));
}

#[test]
fn domain_caps_reject_cap_plus_one_and_100k_adversaries_are_bounded() {
    let mut book = open_task_snapshot(TaskId::new()).browser;
    for _ in 0..MAX_BROWSER_OPEN_TASKS.saturating_sub(1) {
        book.open_task(TaskId::new()).expect("fill task cap");
    }
    assert_eq!(
        book.open_task(TaskId::new()).expect_err("task cap+1"),
        BrowserContractError::BoundExceeded
    );

    let task_id = TaskId::new();
    let mut book = open_book(task_id);
    for _ in 0..MAX_BROWSER_CONTEXTS {
        book.admit(&create_context(task_id, BrowserContextId::new()))
            .expect("fill context cap");
    }
    assert_eq!(
        book.admit(&create_context(task_id, BrowserContextId::new()))
            .expect_err("context cap+1"),
        BrowserContractError::BoundExceeded
    );

    let mut book = open_book(task_id);
    let context_id = BrowserContextId::new();
    book.admit(&create_context(task_id, context_id))
        .expect("create context");
    for _ in 0..MAX_BROWSER_TABS {
        book.admit(&open_page(
            task_id,
            context_id,
            BrowserTabId::new(),
            "https://example.test/tab",
        ))
        .expect("fill tab cap");
    }
    assert_eq!(
        book.admit(&open_page(
            task_id,
            context_id,
            BrowserTabId::new(),
            "https://example.test/overflow",
        ))
        .expect_err("tab cap+1"),
        BrowserContractError::BoundExceeded
    );

    let mut book = open_book(task_id);
    let context_id = BrowserContextId::new();
    book.admit(&create_context(task_id, context_id))
        .expect("create context");
    let tab_id = BrowserTabId::new();
    book.admit(&open_page(
        task_id,
        context_id,
        tab_id,
        "https://example.test",
    ))
    .expect("open tab");
    for _ in 0..(MAX_BROWSER_RECEIPTS.saturating_sub(book.receipt_count())) {
        book.admit(&BrowserRequest {
            request_id: BrowserRequestId::new(),
            task_id,
            context_id,
            tab_id: Some(tab_id),
            generation: 1,
            action: BrowserAction::Stop,
        })
        .expect("zero-fact receipt still counts");
    }
    assert_eq!(
        book.admit(&BrowserRequest {
            request_id: BrowserRequestId::new(),
            task_id,
            context_id,
            tab_id: Some(tab_id),
            generation: 1,
            action: BrowserAction::Back,
        })
        .expect_err("receipt cap+1"),
        BrowserContractError::BoundExceeded
    );

    let fact = BrowserDurableFact::ContextCreated {
        context_id: BrowserContextId::new(),
        task_id,
        generation: 1,
    };
    let overflow: Vec<_> =
        std::iter::repeat_n(fact.clone(), MAX_BROWSER_JOURNAL_FACTS + 1).collect();
    let huge: Vec<_> = std::iter::repeat_n(fact, 100_000).collect();
    let started = Instant::now();
    assert_eq!(
        replay_browser_snapshot(&overflow).expect_err("fact cap+1"),
        BrowserContractError::BoundExceeded
    );
    assert_eq!(
        replay_browser_snapshot(&huge).expect_err("100k fact adversary"),
        BrowserContractError::BoundExceeded
    );
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "cap checks must not apply a 100k journal"
    );

    let oversize_url = format!("https://example.test/{}", "a".repeat(100_000));
    assert!(oversize_url.len() > MAX_BROWSER_FACT_URL_BYTES);
    let mut book = open_book(task_id);
    let context_id = BrowserContextId::new();
    book.admit(&create_context(task_id, context_id))
        .expect("create context");
    assert_eq!(
        book.admit(&BrowserRequest {
            request_id: BrowserRequestId::new(),
            task_id,
            context_id,
            tab_id: Some(BrowserTabId::new()),
            generation: 1,
            action: BrowserAction::OpenTab {
                url: oversize_url,
                kind: BrowserTabKind::Page,
            },
        })
        .expect_err("100k URL adversary"),
        BrowserContractError::BoundExceeded
    );
}

#[test]
fn domain_snapshot_pages_are_keyset_not_full_clones() {
    let task_id = TaskId::new();
    let mut book = open_book(task_id);
    let context_id = BrowserContextId::new();
    book.admit(&create_context(task_id, context_id))
        .expect("create context");
    let mut tab_ids = Vec::new();
    for _ in 0..8 {
        let tab_id = BrowserTabId::new();
        book.admit(&open_page(
            task_id,
            context_id,
            tab_id,
            "https://example.test/page",
        ))
        .expect("open tab");
        tab_ids.push(tab_id);
    }
    tab_ids.sort();
    let limits = PageLimits::new(3, 64 * 1024).unwrap();
    let first = book
        .snapshot_page(
            BrowserSnapshotSection::Tabs,
            None,
            limits.max_items,
            limits.max_encoded_bytes,
        )
        .expect("first page");
    assert_eq!(first.items.len(), 3);
    assert_eq!(first.section, BrowserSnapshotSection::Tabs);
    assert!(first.next_cursor.is_some());
    let second = book
        .snapshot_page(
            BrowserSnapshotSection::Tabs,
            first.next_after,
            limits.max_items,
            limits.max_encoded_bytes,
        )
        .expect("second page");
    assert_eq!(second.items.len(), 3);
    assert_ne!(
        first.items[0].tab_id(),
        second.items[0].tab_id(),
        "keyset must advance"
    );
}

#[test]
fn domain_multi_fact_apply_is_atomic() {
    let task_id = TaskId::new();
    let context_id = BrowserContextId::new();
    let tab_id = BrowserTabId::new();
    let mut book = open_book(task_id);
    book.admit(&create_context(task_id, context_id))
        .expect("create context");
    let before = book.facts().to_vec();
    let error = book
        .apply_facts(&[
            BrowserDurableFact::TabOpened {
                tab_id,
                context_id,
                task_id,
                kind: BrowserTabKind::Page,
                url: "https://example.test".into(),
            },
            BrowserDurableFact::TabOpened {
                tab_id,
                context_id,
                task_id,
                kind: BrowserTabKind::Page,
                url: "https://example.test/dup".into(),
            },
        ])
        .expect_err("second fact rejects");
    assert_eq!(error, BrowserContractError::InvalidRequest);
    assert_eq!(book.facts(), before.as_slice());
    assert!(book
        .snapshot_page(BrowserSnapshotSection::Tabs, None, 16, 64 * 1024,)
        .expect("tabs")
        .items
        .is_empty());
}

#[test]
fn domain_close_context_and_task_close_child_tabs() {
    let task_id = TaskId::new();
    let context_id = BrowserContextId::new();
    let tab_id = BrowserTabId::new();
    let mut book = open_book(task_id);
    book.admit(&create_context(task_id, context_id))
        .expect("create context");
    book.admit(&open_page(
        task_id,
        context_id,
        tab_id,
        "https://example.test",
    ))
    .expect("open tab");
    book.admit(&BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id,
        context_id,
        tab_id: None,
        generation: 1,
        action: BrowserAction::CloseContext,
    })
    .expect("close context");
    assert!(book.facts().iter().any(|fact| matches!(
        fact,
        BrowserDurableFact::TabClosed { tab_id: closed, .. } if *closed == tab_id
    )));
    let tabs = book
        .snapshot_page(BrowserSnapshotSection::Tabs, None, 16, 64 * 1024)
        .expect("tabs");
    assert!(tabs.items.iter().all(|tab| tab.closed()));

    let task_id = TaskId::new();
    let context_id = BrowserContextId::new();
    let tab_id = BrowserTabId::new();
    let mut book = open_book(task_id);
    book.admit(&create_context(task_id, context_id))
        .expect("create context");
    book.admit(&open_page(
        task_id,
        context_id,
        tab_id,
        "https://example.test",
    ))
    .expect("open tab");
    book.close_task(task_id).expect("close task");
    assert!(book.facts().iter().any(|fact| matches!(
        fact,
        BrowserDurableFact::ContextClosed { context_id: closed, .. } if *closed == context_id
    )));
    assert!(book.facts().iter().any(|fact| matches!(
        fact,
        BrowserDurableFact::TabClosed { tab_id: closed, .. } if *closed == tab_id
    )));
}

#[test]
fn domain_urls_are_local_only_redacted_and_serde_fail_closed() {
    let task_id = TaskId::new();
    let context_id = BrowserContextId::new();
    let tab_id = BrowserTabId::new();
    let mut book = open_book(task_id);
    book.admit(&create_context(task_id, context_id))
        .expect("create context");
    assert_eq!(
        book.admit(&open_page(
            task_id,
            context_id,
            tab_id,
            "https://user:secret@example.test/path?token=abc#frag",
        ))
        .expect_err("credentials are not admissible"),
        BrowserContractError::InvalidRequest
    );

    assert_eq!(
        book.admit(&open_page(
            task_id,
            context_id,
            tab_id,
            "https://example.test/path?token=abc#frag",
        ))
        .expect_err("query and fragment never enter the aggregate"),
        BrowserContractError::InvalidRequest
    );
    let open = open_page(task_id, context_id, tab_id, "https://example.test/path");
    assert_eq!(open.action.privacy_class(), PrivacyClass::LocalOnly);
    book.admit(&open).expect("local navigation identity");
    let page = book
        .snapshot_page(BrowserSnapshotSection::Tabs, None, 16, 64 * 1024)
        .expect("tabs");
    let shareable = page.items[0].shareable_url().expect("origin only");
    assert!(!shareable.contains("token"));
    assert!(!shareable.contains("frag"));
    assert!(!shareable.contains("secret"));
    assert_eq!(shareable, "https://example.test/");

    let oversize = "x".repeat(MAX_BROWSER_IDENTITY_BYTES + 1);
    let json = serde_json::json!({
        "context_created": {
            "context_id": BrowserContextId::new(),
            "task_id": task_id,
            "generation": 1,
            "unexpected": true
        }
    });
    assert!(serde_json::from_value::<BrowserDurableFact>(json).is_err());

    let huge_doc = serde_json::json!({
        "navigation_committed": {
            "tab_id": tab_id,
            "context_id": context_id,
            "task_id": task_id,
            "url": "https://example.test",
            "document_id": oversize
        }
    });
    assert!(serde_json::from_value::<BrowserDurableFact>(huge_doc).is_err());
}

#[test]
fn domain_rejects_invalid_bounds_stale_popup_and_duplicate_ids() {
    let task_id = TaskId::new();
    let context_id = BrowserContextId::new();
    let opener = BrowserTabId::new();
    let mut book = open_book(task_id);
    book.admit(&create_context(task_id, context_id))
        .expect("create context");
    book.admit(&open_page(
        task_id,
        context_id,
        opener,
        "https://example.test",
    ))
    .expect("open opener");

    assert_eq!(
        book.admit(&BrowserRequest {
            request_id: BrowserRequestId::new(),
            task_id,
            context_id,
            tab_id: Some(opener),
            generation: 1,
            action: BrowserAction::SetBounds {
                width: 0,
                height: 600,
            },
        })
        .expect_err("zero bounds"),
        BrowserContractError::InvalidRequest
    );
    assert_eq!(
        book.admit(&BrowserRequest {
            request_id: BrowserRequestId::new(),
            task_id,
            context_id,
            tab_id: Some(opener),
            generation: 1,
            action: BrowserAction::SetBounds {
                width: 80_000,
                height: 600,
            },
        })
        .expect_err("huge bounds"),
        BrowserContractError::InvalidRequest
    );

    book.admit(&BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id,
        context_id,
        tab_id: Some(opener),
        generation: 1,
        action: BrowserAction::CloseTab,
    })
    .expect("close opener");
    assert_eq!(
        book.admit(&BrowserRequest {
            request_id: BrowserRequestId::new(),
            task_id,
            context_id,
            tab_id: Some(BrowserTabId::new()),
            generation: 1,
            action: BrowserAction::OpenTab {
                url: "https://example.test/popup".into(),
                kind: BrowserTabKind::Popup { opener },
            },
        })
        .expect_err("stale popup opener"),
        BrowserContractError::InvalidRequest
    );

    let other_context = BrowserContextId::new();
    book.admit(&create_context(task_id, other_context))
        .expect("second context");
    let foreign_tab = BrowserTabId::new();
    book.admit(&open_page(
        task_id,
        other_context,
        foreign_tab,
        "https://example.test/other",
    ))
    .expect("foreign tab");
    assert_eq!(
        book.admit(&BrowserRequest {
            request_id: BrowserRequestId::new(),
            task_id,
            context_id,
            tab_id: Some(foreign_tab),
            generation: 1,
            action: BrowserAction::SelectTab,
        })
        .expect_err("cross-context tab id"),
        BrowserContractError::InvalidRequest
    );

    let duplicate = BrowserTabId::new();
    book.admit(&open_page(
        task_id,
        context_id,
        duplicate,
        "https://example.test/dup",
    ))
    .expect("first tab id");
    assert_eq!(
        book.admit(&open_page(
            task_id,
            context_id,
            duplicate,
            "https://example.test/dup2",
        ))
        .expect_err("duplicate tab id"),
        BrowserContractError::InvalidRequest
    );
}

#[test]
fn domain_durable_facts_never_claim_com_handles_or_current_pixels() {
    let facts = [
        BrowserDurableFact::RequestAccepted {
            request_id: BrowserRequestId::new(),
            task_id: TaskId::new(),
            context_id: BrowserContextId::new(),
            tab_id: None,
            generation: 1,
            action: BrowserAction::CreateContext,
            privacy_class: PrivacyClass::Shareable,
            permission: devmanager::browser::domain::BrowserPermission::CreateContext,
            payload_hash: [1; 32],
            action_epoch: 0,
            command_id: None,
        },
        BrowserDurableFact::ContextCreated {
            context_id: BrowserContextId::new(),
            task_id: TaskId::new(),
            generation: 1,
        },
        BrowserDurableFact::NavigationCommitted {
            tab_id: BrowserTabId::new(),
            context_id: BrowserContextId::new(),
            task_id: TaskId::new(),
            url: "https://example.test/ready".into(),
            document_id: "doc-1".into(),
        },
    ];
    let mut keys = BTreeSet::new();
    for fact in &facts {
        assert!(!fact.claims_ephemeral_runtime());
        collect_object_keys(
            serde_json::to_value(fact).expect("serialize fact"),
            &mut keys,
        );
    }
    for forbidden in [
        "hwnd",
        "com",
        "com_handle",
        "controller",
        "webview",
        "pixels",
        "screenshot",
        "frame_bytes",
        "bitmap",
    ] {
        assert!(
            !keys.contains(forbidden),
            "forbidden key {forbidden}: {keys:?}"
        );
    }
}

#[test]
fn domain_shared_action_catalog_owns_browser_commands() {
    let ids: BTreeSet<_> = catalog().iter().map(|entry| entry.id).collect();
    for required in [
        ACTION_BROWSER_CONTEXT_CREATE,
        ACTION_BROWSER_CONTEXT_CLOSE,
        ACTION_BROWSER_TAB_OPEN,
        ACTION_BROWSER_TAB_CLOSE,
        ACTION_BROWSER_TAB_SELECT,
        ACTION_BROWSER_NAVIGATE,
        ACTION_BROWSER_BACK,
        ACTION_BROWSER_FORWARD,
        ACTION_BROWSER_RELOAD,
        ACTION_BROWSER_STOP,
        ACTION_BROWSER_BOUNDS_SET,
        ACTION_BROWSER_VISIBILITY_SET,
        ACTION_BROWSER_FOCUS_SET,
        ACTION_BROWSER_CAPTURE,
        ACTION_BROWSER_AUTOMATE,
        ACTION_BROWSER_DOWNLOAD_DECIDE,
        ACTION_BROWSER_PERMISSION_DECIDE,
        ACTION_BROWSER_RECORD,
        ACTION_BROWSER_REPLAY,
        ACTION_BROWSER_CANCEL,
        ACTION_BROWSER_RECOVER,
    ] {
        assert!(
            ids.contains(required),
            "missing shared catalog id {required}"
        );
    }
    require_unique_ids().expect("unique ids");
    let navigate = catalog()
        .iter()
        .find(|entry| entry.id == ACTION_BROWSER_NAVIGATE)
        .expect("navigate");
    assert_eq!(navigate.scope, ActionScope::Task);
    assert_eq!(navigate.risk, ActionRisk::Mutating);
    assert_eq!(
        navigate.argument_schema,
        ActionArgumentSchema::BrowserRequestV1
    );
    assert_eq!(
        navigate.required_capability,
        Some(Capability::BrowserProjection)
    );
    assert_eq!(navigate.privacy_class, Some(PrivacyClass::LocalOnly));
    assert!(!navigate.is_available(&ActionAvailability {
        task_open: false,
        capabilities: CapabilitySet::from_capabilities([Capability::BrowserProjection]),
    }));
    assert!(navigate.is_available(&ActionAvailability {
        task_open: true,
        capabilities: CapabilitySet::from_capabilities([Capability::BrowserProjection]),
    }));

    let request = create_context(TaskId::new(), BrowserContextId::new());
    assert!(matches!(
        Command::Browser(request.clone()),
        Command::Browser(_)
    ));
    assert!(matches!(
        Event::Browser(BrowserDurableFact::ContextCreated {
            context_id: request.context_id,
            task_id: request.task_id,
            generation: 1,
        }),
        Event::Browser(_)
    ));
}

#[test]
fn domain_event_apply_projects_browser_state_and_golden_replay() {
    let task_id = TaskId::new();
    let context_id = BrowserContextId::new();
    let tab_id = BrowserTabId::new();
    let artifact_id = ArtifactId::new();
    let mut snap = open_task_snapshot(task_id);
    let create = create_context(task_id, context_id);
    let events = decide_browser(&snap, &create);
    snap = apply_all(Some(snap), &events).expect("create context apply");
    let open = open_page(task_id, context_id, tab_id, "https://example.test/home");
    let events = decide_browser(&snap, &open);
    snap = apply_all(Some(snap), &events).expect("open tab apply");
    let link = BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id,
        context_id,
        tab_id: None,
        generation: 1,
        action: BrowserAction::LinkArtifact { artifact_id },
    };
    let events = decide_browser(&snap, &link);
    snap = apply_all(Some(snap), &events).expect("link artifact");

    let navigate = BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id,
        context_id,
        tab_id: Some(tab_id),
        generation: 1,
        action: BrowserAction::Navigate {
            url: "https://example.test/ready".into(),
        },
    };
    let events = decide_browser(&snap, &navigate);
    assert!(
        events.iter().all(|event| !matches!(
            event,
            Event::Browser(BrowserDurableFact::NavigationCommitted { .. })
        )),
        "decide must not mint host navigation success"
    );
    snap = apply_all(Some(snap), &events).expect("admit navigate");
    snap.browser
        .settle(&BrowserHostOutcome {
            request_id: navigate.request_id,
            task_id,
            context_id,
            tab_id: Some(tab_id),
            generation: 1,
            settlement: BrowserSettlement::NavigationCommitted {
                url: "https://example.test/ready".into(),
                document_id: "doc-ready".into(),
            },
        })
        .expect("settle navigate");
    let permission = BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id,
        context_id,
        tab_id: Some(tab_id),
        generation: 1,
        action: BrowserAction::DecidePermission {
            permission: devmanager::browser::domain::BrowserPermission::Clipboard,
            allowed: true,
        },
    };
    snap = apply_browser(snap, &permission);
    snap.browser
        .settle(&BrowserHostOutcome {
            request_id: permission.request_id,
            task_id,
            context_id,
            tab_id: Some(tab_id),
            generation: 1,
            settlement: BrowserSettlement::PermissionDecided {
                permission: devmanager::browser::domain::BrowserPermission::Clipboard,
                allowed: true,
            },
        })
        .expect("settle permission");
    let record = BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id,
        context_id,
        tab_id: None,
        generation: 1,
        action: BrowserAction::Record,
    };
    snap = apply_browser(snap, &record);
    snap.browser
        .settle(&BrowserHostOutcome {
            request_id: record.request_id,
            task_id,
            context_id,
            tab_id: None,
            generation: 1,
            settlement: BrowserSettlement::RecordingIdentified {
                recording_id: "rec-1".into(),
            },
        })
        .expect("settle record");
    let replay = BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id,
        context_id,
        tab_id: None,
        generation: 1,
        action: BrowserAction::Replay,
    };
    snap = apply_browser(snap, &replay);
    snap.browser
        .settle(&BrowserHostOutcome {
            request_id: replay.request_id,
            task_id,
            context_id,
            tab_id: None,
            generation: 1,
            settlement: BrowserSettlement::RecipeIdentified {
                recipe_id: "recipe-1".into(),
            },
        })
        .expect("settle replay");
    let recover = BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id,
        context_id,
        tab_id: None,
        generation: 1,
        action: BrowserAction::Recover,
    };
    snap = apply_browser(snap, &recover);
    snap.browser
        .settle(&BrowserHostOutcome {
            request_id: recover.request_id,
            task_id,
            context_id,
            tab_id: None,
            generation: 1,
            settlement: BrowserSettlement::Recovered { generation: 2 },
        })
        .expect("settle recover");

    let rebuilt = replay_browser_snapshot(snap.browser.facts()).expect("rebuild from facts");
    let live_context = snap.browser_context(context_id).expect("projected context");
    let live_tab = snap.browser_tab(tab_id).expect("projected tab");
    assert_eq!(live_context.generation, 2);
    assert_eq!(live_context.health, BrowserHealth::Recovering);
    assert_eq!(live_context.selected_tab_id, Some(tab_id));
    assert_eq!(live_context.recipe_id.as_deref(), Some("recipe-1"));
    assert_eq!(live_context.recording_id.as_deref(), Some("rec-1"));
    assert!(live_context.linked_artifacts.contains(&artifact_id));
    assert_eq!(
        live_context
            .permissions
            .get(&devmanager::browser::domain::BrowserPermission::Clipboard),
        Some(&true)
    );
    assert_eq!(
        live_tab.committed_url.as_deref(),
        Some("https://example.test/ready")
    );
    assert_eq!(rebuilt.contexts[0].generation, live_context.generation);
    assert_eq!(rebuilt.tabs[0].committed_url, live_tab.committed_url);
    assert_eq!(rebuilt.contexts[0].health, live_context.health);
    assert_eq!(
        rebuilt.contexts[0].selected_tab_id,
        live_context.selected_tab_id
    );
}

#[test]
fn domain_event_apply_fails_closed_for_stale_cross_task_and_unknown() {
    let task_id = TaskId::new();
    let other = TaskId::new();
    let context_id = BrowserContextId::new();
    let mut snap = open_task_snapshot(task_id);
    snap = apply_browser(snap, &create_context(task_id, context_id));
    let foreign = domain_browser_event(
        Some(task_id),
        Event::Browser(BrowserDurableFact::TabOpened {
            tab_id: BrowserTabId::new(),
            context_id,
            task_id: other,
            kind: BrowserTabKind::Page,
            url: "https://example.test".into(),
        }),
    );
    assert_eq!(
        apply(Some(snap.clone()), &foreign).expect_err("cross-task fact"),
        ApplyError::OwnershipConflict
    );
    let stale = domain_browser_event(
        Some(task_id),
        Event::Browser(BrowserDurableFact::RequestAccepted {
            request_id: BrowserRequestId::new(),
            task_id,
            context_id,
            tab_id: None,
            generation: 9,
            action: BrowserAction::Recover,
            privacy_class: PrivacyClass::Shareable,
            permission: devmanager::browser::domain::BrowserPermission::Recover,
            payload_hash: [2; 32],
            action_epoch: 0,
            command_id: None,
        }),
    );
    assert_eq!(
        apply(Some(snap.clone()), &stale).expect_err("stale generation"),
        ApplyError::InvalidTransition
    );
    let unknown = serde_json::json!({
        "future_fact": { "task_id": task_id }
    });
    assert!(serde_json::from_value::<BrowserDurableFact>(unknown).is_err());
}

#[test]
fn domain_decide_browser_uses_aggregate_and_rejects_bypass() {
    let task_id = TaskId::new();
    let context_id = BrowserContextId::new();
    let snap = open_task_snapshot(task_id);
    let bad_url = BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id,
        context_id,
        tab_id: None,
        generation: 1,
        action: BrowserAction::CreateContext,
    };
    // CreateContext is valid; a navigate without a context must not mint RequestAccepted.
    let navigate = BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id,
        context_id,
        tab_id: Some(BrowserTabId::new()),
        generation: 1,
        action: BrowserAction::Navigate {
            url: "https://user:secret@example.test".into(),
        },
    };
    assert_eq!(
        decide(
            Some(&snap),
            &browser_envelope(task_id, 1, Command::Browser(navigate)),
        )
        .expect_err("decide must not bypass BrowserBook"),
        RejectionCode::InvalidTransition
    );
    let created = decide(
        Some(&snap),
        &browser_envelope(
            task_id,
            1,
            Command::Browser(create_context(task_id, context_id)),
        ),
    )
    .expect("valid create");
    assert!(created.iter().any(|event| matches!(
        event,
        Event::Browser(BrowserDurableFact::ContextCreated { .. })
    )));
    let _ = bad_url;
}

#[test]
fn domain_receipt_binds_original_action_and_settle_preserves_it() {
    let task_id = TaskId::new();
    let context_id = BrowserContextId::new();
    let tab_id = BrowserTabId::new();
    let mut book = open_book(task_id);
    book.admit(&create_context(task_id, context_id))
        .expect("context");
    book.admit(&open_page(
        task_id,
        context_id,
        tab_id,
        "https://example.test",
    ))
    .expect("tab");
    let recover = BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id,
        context_id,
        tab_id: None,
        generation: 1,
        action: BrowserAction::Recover,
    };
    let accepted = book.admit(&recover).expect("admit recover");
    assert_eq!(accepted.action, BrowserAction::Recover);
    assert_eq!(accepted.privacy_class, PrivacyClass::Shareable);
    assert_eq!(
        accepted.permission,
        devmanager::browser::domain::BrowserPermission::Recover
    );
    assert_eq!(accepted.task_id, task_id);
    assert_eq!(accepted.context_id, context_id);
    assert_eq!(accepted.generation, 1);
    assert_ne!(accepted.payload_hash, [0_u8; 32]);
    let settled = book
        .settle(&BrowserHostOutcome {
            request_id: recover.request_id,
            task_id,
            context_id,
            tab_id: None,
            generation: 1,
            settlement: BrowserSettlement::Recovered { generation: 2 },
        })
        .expect("settle recover");
    assert_eq!(settled.permission, accepted.permission);
    assert_eq!(settled.privacy_class, accepted.privacy_class);
    assert_eq!(settled.action, accepted.action);
    assert_eq!(settled.payload_hash, accepted.payload_hash);
    let before = book.facts().to_vec();
    assert_eq!(
        book.settle(&BrowserHostOutcome {
            request_id: recover.request_id,
            task_id,
            context_id,
            tab_id: None,
            generation: 1,
            settlement: BrowserSettlement::Recovered { generation: 99 },
        })
        .expect_err("conflicting settlement"),
        BrowserContractError::IdempotencyConflict
    );
    assert_eq!(book.facts(), before.as_slice());
}

#[test]
fn domain_close_crash_replay_is_deterministic_and_idempotent() {
    let task_id = TaskId::new();
    let context_id = BrowserContextId::new();
    let first_tab = BrowserTabId::new();
    let second_tab = BrowserTabId::new();
    let mut snap = open_task_snapshot(task_id);
    snap = apply_browser(snap, &create_context(task_id, context_id));
    snap = apply_browser(
        snap,
        &open_page(task_id, context_id, first_tab, "https://example.test/a"),
    );
    snap = apply_browser(
        snap,
        &open_page(task_id, context_id, second_tab, "https://example.test/b"),
    );
    let close = BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id,
        context_id,
        tab_id: None,
        generation: 1,
        action: BrowserAction::CloseContext,
    };
    let events = decide_browser(&snap, &close);
    assert!(events.iter().any(|event| matches!(
        event,
        Event::Browser(BrowserDurableFact::TabClosed { tab_id, .. }) if *tab_id == first_tab
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        Event::Browser(BrowserDurableFact::TabClosed { tab_id, .. }) if *tab_id == second_tab
    )));
    let mut prefix = snap.clone();
    for (index, payload) in events.iter().enumerate() {
        prefix = apply(
            Some(prefix),
            &domain_browser_event(Some(task_id), payload.clone()),
        )
        .expect("prefix apply");
        let replayed = replay_browser_snapshot(prefix.browser.facts()).expect("prefix replay");
        if index + 1 < events.len() {
            assert!(
                replayed.tabs.iter().any(|tab| !tab.closed)
                    || replayed.contexts.iter().any(|context| !context.closed),
                "intermediate close prefix stays consistent"
            );
        }
    }
    assert!(prefix.browser_tab(first_tab).expect("first").closed);
    assert!(prefix.browser_tab(second_tab).expect("second").closed);
    assert!(prefix.browser_context(context_id).expect("ctx").closed);
    let again = decide_browser(&prefix, &close);
    assert!(
        again.iter().all(|event| matches!(
            event,
            Event::Browser(BrowserDurableFact::RequestAccepted { .. })
        )),
        "duplicate close is idempotent and does not reopen children"
    );
    let closed = apply_all(Some(prefix), &again).expect("duplicate close");
    assert!(closed.browser_tab(first_tab).expect("first").closed);
    assert!(closed.browser_tab(second_tab).expect("second").closed);
}

#[test]
fn domain_wire_decode_rejects_oversize_unknown_duplicate_and_secrets() {
    let task_id = TaskId::new();
    let context_id = BrowserContextId::new();
    let request_id = BrowserRequestId::new();
    let huge = "a".repeat(MAX_BROWSER_FACT_URL_BYTES + 1);
    let oversize = format!(
        r#"{{"request_id":"{request_id}","task_id":"{task_id}","context_id":"{context_id}","tab_id":null,"generation":1,"action":{{"navigate":{{"url":"https://example.test/{huge}"}}}}}}"#
    );
    assert!(
        decode_browser_request_json(&oversize).is_err(),
        "oversize URL must fail during decode"
    );
    let unknown = format!(
        r#"{{"request_id":"{request_id}","task_id":"{task_id}","context_id":"{context_id}","generation":1,"action":{{"teleport":{{}}}}}}"#
    );
    assert!(decode_browser_request_json(&unknown).is_err());
    let duplicate = format!(
        r#"{{"request_id":"{request_id}","request_id":"{request_id}","task_id":"{task_id}","context_id":"{context_id}","generation":1,"action":"reload"}}"#
    );
    assert!(decode_browser_request_json(&duplicate).is_err());
    let secret = format!(
        r#"{{"request_id":"{request_id}","task_id":"{task_id}","context_id":"{context_id}","generation":1,"action":{{"navigate":{{"url":"https://user:secret@example.test/path?token=1#frag"}}}}}}"#
    );
    assert!(decode_browser_request_json(&secret).is_err());
    let nested = serde_json::json!([vec![0_u8; MAX_MESSAGEPACK_COLLECTION_ITEMS as usize + 1]]);
    assert!(decode_browser_request_wire(&serde_json::to_vec(&nested).unwrap()).is_err());
}

#[test]
fn domain_snapshot_pages_are_page_plus_one_and_byte_capped() {
    let task_id = TaskId::new();
    let context_id = BrowserContextId::new();
    let mut book = open_book(task_id);
    book.admit(&create_context(task_id, context_id))
        .expect("context");
    for _ in 0..8 {
        book.admit(&open_page(
            task_id,
            context_id,
            BrowserTabId::new(),
            "https://example.test/page",
        ))
        .expect("tab");
    }
    let page = book
        .snapshot_page(BrowserSnapshotSection::Tabs, None, 3, 64 * 1024)
        .expect("page");
    assert_eq!(page.items.len(), 3);
    assert_eq!(page.examined, 4, "must inspect page+1 to decide next_after");
    assert!(page.encoded_bytes > 0);
    assert!(page.encoded_bytes < 64 * 1024);
    let tiny = book
        .snapshot_page(
            BrowserSnapshotSection::Tabs,
            Some(BrowserPageKey::Tab(page.items[0].tab_id().unwrap())),
            8,
            32,
        )
        .expect_err("byte cap");
    assert_eq!(tiny, BrowserContractError::BoundExceeded);
}

#[test]
fn domain_host_effects_are_explicitly_unavailable_from_metadata() {
    let task_id = TaskId::new();
    let context_id = BrowserContextId::new();
    let tab_id = BrowserTabId::new();
    let mut book = open_book(task_id);
    book.admit(&create_context(task_id, context_id))
        .expect("context");
    book.admit(&open_page(
        task_id,
        context_id,
        tab_id,
        "https://example.test",
    ))
    .expect("tab");
    let navigate = BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id,
        context_id,
        tab_id: Some(tab_id),
        generation: 1,
        action: BrowserAction::Navigate {
            url: "https://example.test/next".into(),
        },
    };
    book.admit(&navigate).expect("admit is not execution");
    assert_eq!(
        perform_browser_host_effect(&navigate).expect_err("8.3 host effect"),
        BrowserContractError::HostEffectUnavailable
    );
    assert_eq!(
        execute_from_catalog(ACTION_BROWSER_NAVIGATE).expect_err("metadata is not execution"),
        ActionExecuteError::MetadataOnly
    );
    assert!(
        browser_integration_holds().contains(&BrowserIntegrationHold::WebViewSurfaceAbsent),
        "Phase 8 host surface remains a typed HOLD"
    );
    assert!(
        browser_integration_holds().contains(&BrowserIntegrationHold::BrowserServiceAbsent),
        "Phase 8 BrowserService remains a typed HOLD"
    );
    assert!(
        !browser_integration_holds().is_empty(),
        "holds must be explicit rather than silent success"
    );
}

#[test]
fn legacy_browser_command_cannot_grant_projection_or_settle_hold() {
    let hello = CapabilitySet::empty();
    assert!(
        !hello.contains(Capability::BrowserProjection),
        "host hello must not grant browser projection"
    );
    assert_eq!(
        grant_must_hold(hello, None),
        BrowserIntegrationHold::HostCapabilityUngranted
    );
    assert_eq!(
        grant_must_hold(
            CapabilitySet::from_capabilities([Capability::OperationSettlement]),
            None
        ),
        BrowserIntegrationHold::HostCapabilityUngranted,
        "OperationSettlement is not BrowserProjection"
    );
    assert_eq!(
        grant_must_hold(
            CapabilitySet::from_capabilities([Capability::BrowserProjection]),
            None
        ),
        BrowserIntegrationHold::BrowserServiceAbsent,
        "capability without 8.3 authority cannot mint a token"
    );

    let intent = BrowserHostSettleIntent::bind(
        CommandId::new(),
        OperationId::new(),
        BrowserRequestId::new(),
        TaskId::new(),
        BrowserContextId::new(),
        1,
        0,
    )
    .expect("hold identity");
    let hold = Effect::HoldBrowserHost {
        task_id: intent.task_id(),
        action_epoch: intent.action_epoch(),
        request_id: intent.request_id(),
        context_id: intent.context_id(),
        generation: intent.generation(),
        hold: BrowserIntegrationHold::WebViewSurfaceAbsent,
    };
    assert_eq!(
        hold.browser_host_hold_identity(),
        Some((
            intent.task_id(),
            intent.action_epoch(),
            intent.request_id(),
            intent.context_id(),
            intent.generation(),
        ))
    );
    assert!(
        Effect::BeginTaskTeardown {
            task_id: intent.task_id(),
            action_epoch: 1,
        }
        .browser_host_hold_identity()
        .is_none(),
        "teardown is not a browser settler permit"
    );
    let _no_first_claim = ReplayPolicy::NoAutomaticRetry;
    assert_eq!(
        settle_accepted_browser_hold(None, &intent, &hold, &hello),
        Err(BrowserHoldSettleError::Hold(
            BrowserIntegrationHold::HostCapabilityUngranted
        )),
        "legacy chrome hello cannot settle an accepted HOLD"
    );
}

#[test]
fn accepted_hold_settlement_requires_exact_identity_and_hello_capability() {
    let command_id = CommandId::new();
    let operation_id = OperationId::new();
    let request_id = BrowserRequestId::new();
    let task_id = TaskId::new();
    let context_id = BrowserContextId::new();
    let intent = BrowserHostSettleIntent::bind(
        command_id,
        operation_id,
        request_id,
        task_id,
        context_id,
        2,
        7,
    )
    .expect("hold identity");
    assert_eq!(
        BrowserHostSettleIntent::bind(
            command_id,
            operation_id,
            request_id,
            task_id,
            context_id,
            0,
            7,
        ),
        Err(BrowserContractError::GenerationMismatch)
    );
    assert_eq!(
        intent.matches_accepted_hold(task_id, 7, request_id, context_id, 2),
        Ok(())
    );
    assert_eq!(
        intent.matches_accepted_hold(TaskId::new(), 7, request_id, context_id, 2),
        Err(BrowserContractError::CrossTask)
    );
    assert_eq!(
        intent.matches_accepted_hold(task_id, 7, request_id, context_id, 3),
        Err(BrowserContractError::GenerationMismatch)
    );
    assert_eq!(
        intent.matches_accepted_hold(task_id, 7, request_id, BrowserContextId::new(), 2),
        Err(BrowserContractError::InvalidRequest)
    );
    assert_eq!(
        intent.matches_accepted_hold(task_id, 7, BrowserRequestId::new(), context_id, 2),
        Err(BrowserContractError::InvalidRequest)
    );
    assert_eq!(
        intent.matches_accepted_hold(task_id, 8, request_id, context_id, 2),
        Err(BrowserContractError::InvalidRequest)
    );

    let hold = Effect::HoldBrowserHost {
        task_id,
        action_epoch: 7,
        request_id,
        context_id,
        generation: 2,
        hold: BrowserIntegrationHold::WebViewSurfaceAbsent,
    };
    let projection = CapabilitySet::from_capabilities([Capability::BrowserProjection]);
    assert_eq!(
        settle_accepted_browser_hold(None, &intent, &hold, &CapabilitySet::empty()),
        Err(BrowserHoldSettleError::Hold(
            BrowserIntegrationHold::HostCapabilityUngranted
        ))
    );
    assert_eq!(
        settle_accepted_browser_hold(
            None,
            &intent,
            &hold,
            &CapabilitySet::from_capabilities([Capability::OperationSettlement])
        ),
        Err(BrowserHoldSettleError::Hold(
            BrowserIntegrationHold::HostCapabilityUngranted
        )),
        "OperationSettlement is not current hello BrowserProjection"
    );
    let wrong_task = Effect::HoldBrowserHost {
        task_id: TaskId::new(),
        action_epoch: 7,
        request_id,
        context_id,
        generation: 2,
        hold: BrowserIntegrationHold::WebViewSurfaceAbsent,
    };
    assert_eq!(
        settle_accepted_browser_hold(None, &intent, &wrong_task, &projection),
        Err(BrowserHoldSettleError::Contract(
            BrowserContractError::CrossTask
        ))
    );
    let wrong_generation = Effect::HoldBrowserHost {
        task_id,
        action_epoch: 7,
        request_id,
        context_id,
        generation: 1,
        hold: BrowserIntegrationHold::WebViewSurfaceAbsent,
    };
    assert_eq!(
        settle_accepted_browser_hold(None, &intent, &wrong_generation, &projection),
        Err(BrowserHoldSettleError::Contract(
            BrowserContractError::GenerationMismatch
        ))
    );
    assert_eq!(
        settle_accepted_browser_hold(
            None,
            &intent,
            &Effect::BeginTaskTeardown {
                task_id,
                action_epoch: 7,
            },
            &projection
        ),
        Err(BrowserHoldSettleError::Contract(
            BrowserContractError::InvalidRequest
        )),
        "non-HOLD effects cannot settle"
    );
    assert_eq!(
        settle_accepted_browser_hold(None, &intent, &hold, &projection),
        Err(BrowserHoldSettleError::Hold(
            BrowserIntegrationHold::BrowserServiceAbsent
        )),
        "exact identity without 8.3 issuer still cannot mint or settle"
    );
    assert_eq!(
        grant_must_hold(projection, None),
        BrowserIntegrationHold::BrowserServiceAbsent
    );
}

#[test]
fn domain_decide_binds_command_id_and_action_epoch() {
    let task_id = TaskId::new();
    let snap = open_task_snapshot(task_id);
    let request = create_context(task_id, BrowserContextId::new());
    let envelope = browser_envelope(
        task_id,
        snap.task.revision,
        Command::Browser(request.clone()),
    );
    let events = decide(Some(&snap), &envelope).expect("decide create context");
    let Event::Browser(BrowserDurableFact::RequestAccepted {
        command_id,
        action_epoch,
        request_id,
        ..
    }) = &events[0]
    else {
        panic!("expected bound RequestAccepted, got {:?}", events[0]);
    };
    assert_eq!(*command_id, Some(envelope.command_id));
    assert_eq!(*action_epoch, snap.task.action_epoch);
    assert_eq!(*request_id, request.request_id);
}

#[test]
fn kernel_create_context_loads_trusted_book_and_survives_reopen() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("browser-kernel.sqlite3");
    let task = TaskId::new();
    let context_id = BrowserContextId::new();
    let mut bus = CommandBus::open(&path).expect("open bus");
    accept_create_task(&mut bus, task);

    let created = bus
        .execute(browser_envelope(
            task,
            1,
            Command::Browser(create_context(task, context_id)),
        ))
        .expect("execute create context");
    assert!(
        matches!(created, CommandReceipt::Accepted { .. }),
        "trusted book must admit create context, got {created:?}"
    );

    let snap = bus.task_snapshot(task).expect("load").expect("task exists");
    assert!(
        snap.browser.is_ready(),
        "kernel load must reconstruct trusted browser authority"
    );
    assert!(snap.browser_context(context_id).is_some());

    drop(bus);
    let reopened = CommandBus::open(&path).expect("reopen");
    let again = reopened
        .task_snapshot(task)
        .expect("reload")
        .expect("task exists after reopen");
    assert!(again.browser.is_ready());
    assert_eq!(
        again.browser_context(context_id).map(|view| view.task_id),
        Some(task)
    );
}

#[test]
fn kernel_rejects_stale_revision_wrong_task_and_wrong_generation() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("browser-fences.sqlite3");
    let task = TaskId::new();
    let other = TaskId::new();
    let context_id = BrowserContextId::new();
    let mut bus = CommandBus::open(&path).expect("open bus");
    accept_create_task(&mut bus, task);
    accept_create_task(&mut bus, other);
    bus.execute(browser_envelope(
        task,
        1,
        Command::Browser(create_context(task, context_id)),
    ))
    .expect("owner create context");

    let stale = bus
        .execute(browser_envelope(
            task,
            99,
            Command::Browser(create_context(task, BrowserContextId::new())),
        ))
        .expect("stale revision execute");
    assert!(
        matches!(
            stale,
            CommandReceipt::Rejected {
                code: RejectionCode::RevisionConflict,
                ..
            }
        ),
        "stale revision must reject, got {stale:?}"
    );

    let stolen = BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id: task,
        context_id,
        tab_id: None,
        generation: 1,
        action: BrowserAction::CloseContext,
    };
    let cross = bus
        .execute(browser_envelope(other, 1, Command::Browser(stolen)))
        .expect("cross-task execute");
    assert!(
        matches!(
            cross,
            CommandReceipt::Rejected {
                code: RejectionCode::OwnershipConflict,
                ..
            }
        ),
        "wrong task must reject, got {cross:?}"
    );

    let stale_gen = BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id: task,
        context_id,
        tab_id: Some(BrowserTabId::new()),
        generation: 9,
        action: BrowserAction::OpenTab {
            url: "https://example.test".into(),
            kind: BrowserTabKind::Page,
        },
    };
    let generation = bus
        .execute(browser_envelope(task, 1, Command::Browser(stale_gen)))
        .expect("wrong generation execute");
    assert!(
        matches!(
            generation,
            CommandReceipt::Rejected {
                code: RejectionCode::InvalidTransition,
                ..
            }
        ),
        "wrong generation must reject, got {generation:?}"
    );
}

#[test]
fn kernel_navigate_admits_without_settling_and_holds_host_surface() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("browser-hold.sqlite3");
    let task = TaskId::new();
    let context_id = BrowserContextId::new();
    let tab_id = BrowserTabId::new();
    let mut bus = CommandBus::open(&path).expect("open bus");
    accept_create_task(&mut bus, task);
    bus.execute(browser_envelope(
        task,
        1,
        Command::Browser(create_context(task, context_id)),
    ))
    .expect("context");
    bus.execute(browser_envelope(
        task,
        1,
        Command::Browser(open_page(
            task,
            context_id,
            tab_id,
            "https://example.test/home",
        )),
    ))
    .expect("tab");

    let navigate = BrowserRequest {
        request_id: BrowserRequestId::new(),
        task_id: task,
        context_id,
        tab_id: Some(tab_id),
        generation: 1,
        action: BrowserAction::Navigate {
            url: "https://example.test/next".into(),
        },
    };
    let accepted = bus
        .execute(browser_envelope(
            task,
            1,
            Command::Browser(navigate.clone()),
        ))
        .expect("admit navigate");
    let CommandReceipt::Accepted { operation_id, .. } = accepted else {
        panic!("navigate must admit, got {accepted:?}");
    };
    let state = bus
        .operation_status(operation_id)
        .expect("status")
        .expect("operation");
    assert!(
        matches!(state, OperationState::Accepted),
        "requested browser effect must not auto-settle, got {state:?}"
    );
    assert_eq!(
        perform_browser_host_effect(&navigate).expect_err("no webview"),
        BrowserContractError::HostEffectUnavailable
    );
}

#[test]
fn kernel_close_then_reopen_replays_closed_children() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("browser-close.sqlite3");
    let task = TaskId::new();
    let context_id = BrowserContextId::new();
    let tab_id = BrowserTabId::new();
    let mut bus = CommandBus::open(&path).expect("open bus");
    accept_create_task(&mut bus, task);
    bus.execute(browser_envelope(
        task,
        1,
        Command::Browser(create_context(task, context_id)),
    ))
    .expect("context");
    bus.execute(browser_envelope(
        task,
        1,
        Command::Browser(open_page(
            task,
            context_id,
            tab_id,
            "https://example.test/home",
        )),
    ))
    .expect("tab");

    let close = bus
        .execute(CommandEnvelope {
            command_id: CommandId::new(),
            client_id: ClientId::new(),
            task_id: Some(task),
            issued_at_ms: 1,
            expected_task_revision: Some(1),
            command: Command::BeginCloseTask,
        })
        .expect("begin close");
    assert!(
        matches!(close, CommandReceipt::Accepted { .. }),
        "{close:?}"
    );

    drop(bus);
    let reopened = CommandBus::open(&path).expect("reopen after close");
    let snap = reopened.task_snapshot(task).expect("load").expect("task");
    let context = snap.browser_context(context_id).expect("context survived");
    let tab = snap.browser_tab(tab_id).expect("tab survived");
    assert!(context.closed, "close must persist closed context");
    assert!(tab.closed, "close must persist closed tab");
}

#[test]
fn kernel_rejects_query_url_so_deletion_never_needs_redaction() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("browser-redact.sqlite3");
    let task = TaskId::new();
    let context_id = BrowserContextId::new();
    let mut bus = CommandBus::open(&path).expect("open bus");
    accept_create_task(&mut bus, task);
    bus.execute(browser_envelope(
        task,
        1,
        Command::Browser(create_context(task, context_id)),
    ))
    .expect("context");
    let leak = bus
        .execute(browser_envelope(
            task,
            1,
            Command::Browser(open_page(
                task,
                context_id,
                BrowserTabId::new(),
                "https://user:secret@example.test/path?token=abc#frag",
            )),
        ))
        .expect("execute leak url");
    assert!(
        matches!(
            leak,
            CommandReceipt::Rejected {
                code: RejectionCode::InvalidTransition,
                ..
            }
        ),
        "secret URL must never enter the journal, got {leak:?}"
    );
}

#[test]
fn kernel_client_model_ingests_bounded_browser_pages() {
    let task = TaskId::new();
    let context_id = BrowserContextId::new();
    let snapshot = SnapshotId::new();
    let mut builder = ClientModelBuilder::new();
    builder
        .ingest_page(SnapshotPage {
            snapshot_id: snapshot,
            through_sequence: 3,
            section: SnapshotSection::Tasks,
            after_item: None,
            items: vec![SnapshotItem::Task(TaskSnapshotItem {
                task: TaskFacts {
                    id: task,
                    environment_id: EnvironmentId::new(),
                    title: "browser".into(),
                    description: None,
                    project_id: ProjectId::new(),
                    workspace: WorkspaceRef::Main,
                    assignment: TaskAssignment::LocalOwner,
                    lifecycle: TaskLifecycle::Open,
                    action_epoch: 0,
                    revision: 1,
                    created_at_ms: 1,
                },
                connectivity: TaskConnectivity::Connected,
                attention: TaskAttention::None,
                activity: TaskActivity::Idle,
                review_readiness: ReviewReadiness::NotReady,
                primary_agent_id: None,
            })],
            encoded_bytes: 1,
            next_cursor: None,
        })
        .expect("tasks");
    for section in [
        SnapshotSection::AgentSessions,
        SnapshotSection::Artifacts,
        SnapshotSection::Resources,
        SnapshotSection::Operations,
    ] {
        builder
            .ingest_page(SnapshotPage {
                snapshot_id: snapshot,
                through_sequence: 3,
                section,
                after_item: None,
                items: Vec::new(),
                encoded_bytes: 1,
                next_cursor: None,
            })
            .expect("required section");
    }
    builder
        .ingest_page(SnapshotPage {
            snapshot_id: snapshot,
            through_sequence: 3,
            section: SnapshotSection::BrowserContexts,
            after_item: None,
            items: vec![SnapshotItem::BrowserContext(
                devmanager::domain::browser::BrowserContextView {
                    context_id,
                    task_id: task,
                    generation: 1,
                    selected_tab_id: None,
                    health: BrowserHealth::Healthy,
                    closed: false,
                    permissions: Default::default(),
                    linked_artifacts: Default::default(),
                    recipe_id: None,
                    recording_id: None,
                },
            )],
            encoded_bytes: 1,
            next_cursor: None,
        })
        .expect("browser contexts must be ingestible");
    builder
        .ingest_page(SnapshotPage {
            snapshot_id: snapshot,
            through_sequence: 3,
            section: SnapshotSection::BrowserTabs,
            after_item: None,
            items: Vec::new(),
            encoded_bytes: 1,
            next_cursor: None,
        })
        .expect("browser tabs must be ingestible");
    let model = builder
        .finish()
        .expect("finish with optional browser pages");
    assert!(
        model
            .tasks()
            .get(&task)
            .and_then(|snap| snap.browser_context(context_id))
            .is_some(),
        "ClientModel must project ingested browser identity"
    );
}

#[test]
fn domain_apply_rejects_forged_foreign_task_and_secret_url_facts() {
    let owner = TaskId::new();
    let foreign = TaskId::new();
    let context_id = BrowserContextId::new();
    let tab_id = BrowserTabId::new();
    let mut book = open_book(owner);
    assert_eq!(
        book.apply_facts(&[BrowserDurableFact::ContextCreated {
            context_id,
            task_id: foreign,
            generation: 1,
        }])
        .expect_err("foreign context must not join this book"),
        BrowserContractError::ClosedTask
    );
    book.admit(&create_context(owner, context_id))
        .expect("owned context");
    assert_eq!(
        book.apply_facts(&[BrowserDurableFact::TabOpened {
            tab_id,
            context_id,
            task_id: owner,
            kind: BrowserTabKind::Page,
            url: "https://example.test/path?token=abc".into(),
        }])
        .expect_err("query URL must not survive replay"),
        BrowserContractError::InvalidRequest
    );
    assert!(book.tab_view(tab_id).is_none());
}

#[test]
fn kernel_client_model_rejects_query_and_strips_path_from_tab_views() {
    let task = TaskId::new();
    let context_id = BrowserContextId::new();
    let tab_id = BrowserTabId::new();
    let snapshot = SnapshotId::new();
    let required = |section| SnapshotPage {
        snapshot_id: snapshot,
        through_sequence: 4,
        section,
        after_item: None,
        items: Vec::new(),
        encoded_bytes: 1,
        next_cursor: None,
    };
    let mut leak = ClientModelBuilder::new();
    leak.ingest_page(SnapshotPage {
        snapshot_id: snapshot,
        through_sequence: 4,
        section: SnapshotSection::Tasks,
        after_item: None,
        items: vec![SnapshotItem::Task(TaskSnapshotItem {
            task: TaskFacts {
                id: task,
                environment_id: EnvironmentId::new(),
                title: "browser".into(),
                description: None,
                project_id: ProjectId::new(),
                workspace: WorkspaceRef::Main,
                assignment: TaskAssignment::LocalOwner,
                lifecycle: TaskLifecycle::Open,
                action_epoch: 0,
                revision: 1,
                created_at_ms: 1,
            },
            connectivity: TaskConnectivity::Connected,
            attention: TaskAttention::None,
            activity: TaskActivity::Idle,
            review_readiness: ReviewReadiness::NotReady,
            primary_agent_id: None,
        })],
        encoded_bytes: 1,
        next_cursor: None,
    })
    .expect("tasks");
    for section in [
        SnapshotSection::AgentSessions,
        SnapshotSection::Artifacts,
        SnapshotSection::Resources,
        SnapshotSection::Operations,
        SnapshotSection::BrowserContexts,
    ] {
        leak.ingest_page(required(section)).expect("required");
    }
    let secret = leak.ingest_page(SnapshotPage {
        snapshot_id: snapshot,
        through_sequence: 4,
        section: SnapshotSection::BrowserTabs,
        after_item: None,
        items: vec![SnapshotItem::BrowserTab(
            devmanager::domain::browser::BrowserTabView {
                tab_id,
                context_id,
                task_id: task,
                kind: BrowserTabKind::Page,
                committed_url: Some("https://example.test/secret?token=abc".into()),
                closed: false,
            },
        )],
        encoded_bytes: 1,
        next_cursor: None,
    });
    assert!(
        secret.is_err(),
        "query URL must not enter ClientModel, got {secret:?}"
    );

    let mut builder = ClientModelBuilder::new();
    builder
        .ingest_page(SnapshotPage {
            snapshot_id: snapshot,
            through_sequence: 4,
            section: SnapshotSection::Tasks,
            after_item: None,
            items: vec![SnapshotItem::Task(TaskSnapshotItem {
                task: TaskFacts {
                    id: task,
                    environment_id: EnvironmentId::new(),
                    title: "browser".into(),
                    description: None,
                    project_id: ProjectId::new(),
                    workspace: WorkspaceRef::Main,
                    assignment: TaskAssignment::LocalOwner,
                    lifecycle: TaskLifecycle::Open,
                    action_epoch: 0,
                    revision: 1,
                    created_at_ms: 1,
                },
                connectivity: TaskConnectivity::Connected,
                attention: TaskAttention::None,
                activity: TaskActivity::Idle,
                review_readiness: ReviewReadiness::NotReady,
                primary_agent_id: None,
            })],
            encoded_bytes: 1,
            next_cursor: None,
        })
        .expect("tasks");
    for section in [
        SnapshotSection::AgentSessions,
        SnapshotSection::Artifacts,
        SnapshotSection::Resources,
        SnapshotSection::Operations,
    ] {
        builder.ingest_page(required(section)).expect("required");
    }
    builder
        .ingest_page(SnapshotPage {
            snapshot_id: snapshot,
            through_sequence: 4,
            section: SnapshotSection::BrowserContexts,
            after_item: None,
            items: vec![SnapshotItem::BrowserContext(
                devmanager::domain::browser::BrowserContextView {
                    context_id,
                    task_id: task,
                    generation: 1,
                    selected_tab_id: Some(tab_id),
                    health: BrowserHealth::Healthy,
                    closed: false,
                    permissions: Default::default(),
                    linked_artifacts: Default::default(),
                    recipe_id: None,
                    recording_id: None,
                },
            )],
            encoded_bytes: 1,
            next_cursor: None,
        })
        .expect("context");
    builder
        .ingest_page(SnapshotPage {
            snapshot_id: snapshot,
            through_sequence: 4,
            section: SnapshotSection::BrowserTabs,
            after_item: None,
            items: vec![SnapshotItem::BrowserTab(
                devmanager::domain::browser::BrowserTabView {
                    tab_id,
                    context_id,
                    task_id: task,
                    kind: BrowserTabKind::Page,
                    committed_url: Some("https://example.test/secret/path".into()),
                    closed: false,
                },
            )],
            encoded_bytes: 1,
            next_cursor: None,
        })
        .expect("path must be admitted then stripped");
    let model = builder.finish().expect("finish");
    let tab = model
        .tasks()
        .get(&task)
        .and_then(|snap| snap.browser_tab(tab_id))
        .expect("tab");
    assert_eq!(tab.committed_url.as_deref(), Some("https://example.test/"));
    assert!(!tab
        .committed_url
        .as_deref()
        .unwrap_or_default()
        .contains("secret"));
}

fn accept_create_task(bus: &mut CommandBus, task: TaskId) {
    let receipt = bus
        .execute(CommandEnvelope {
            command_id: CommandId::new(),
            client_id: ClientId::new(),
            task_id: None,
            issued_at_ms: 1,
            expected_task_revision: None,
            command: Command::CreateTask(CreateTaskIntent {
                id: task,
                environment_id: EnvironmentId::new(),
                title: "browser".into(),
                description: None,
                project_id: ProjectId::new(),
                workspace: WorkspaceRef::Main,
                assignment: TaskAssignment::LocalOwner,
                created_at_ms: 1,
                connectivity: TaskConnectivity::Connected,
                attention: TaskAttention::None,
                activity: TaskActivity::Idle,
                review_readiness: ReviewReadiness::NotReady,
            }),
        })
        .expect("create task");
    assert!(
        matches!(receipt, CommandReceipt::Accepted { .. }),
        "create task must accept, got {receipt:?}"
    );
}

fn open_task_snapshot(task_id: TaskId) -> devmanager::domain::snapshot::TaskSnapshot {
    let envelope = CommandEnvelope {
        command_id: CommandId::new(),
        client_id: ClientId::new(),
        task_id: None,
        issued_at_ms: 1,
        expected_task_revision: None,
        command: Command::CreateTask(CreateTaskIntent {
            id: task_id,
            environment_id: EnvironmentId::new(),
            title: "browser".into(),
            description: None,
            project_id: ProjectId::new(),
            workspace: WorkspaceRef::Main,
            assignment: TaskAssignment::LocalOwner,
            created_at_ms: 1,
            connectivity: TaskConnectivity::Connected,
            attention: TaskAttention::None,
            activity: TaskActivity::Idle,
            review_readiness: ReviewReadiness::NotReady,
        }),
    };
    let payloads = decide(None, &envelope).expect("create task");
    apply(
        None,
        &DomainEvent {
            id: EventId::new(),
            task_id: Some(task_id),
            sequence: 1,
            task_revision: Some(1),
            occurred_at_ms: 1,
            payload: payloads[0].clone(),
        },
    )
    .expect("apply create")
}

fn browser_envelope(task_id: TaskId, revision: u64, command: Command) -> CommandEnvelope {
    CommandEnvelope {
        command_id: CommandId::new(),
        client_id: ClientId::new(),
        task_id: Some(task_id),
        issued_at_ms: 1,
        expected_task_revision: Some(revision),
        command,
    }
}

fn decide_browser(
    snap: &devmanager::domain::snapshot::TaskSnapshot,
    request: &BrowserRequest,
) -> Vec<Event> {
    decide(
        Some(snap),
        &browser_envelope(
            snap.task.id,
            snap.task.revision,
            Command::Browser(request.clone()),
        ),
    )
    .expect("decide browser")
}

fn apply_browser(
    snap: devmanager::domain::snapshot::TaskSnapshot,
    request: &BrowserRequest,
) -> devmanager::domain::snapshot::TaskSnapshot {
    let events = decide_browser(&snap, request);
    apply_all(Some(snap), &events).expect("apply browser")
}

fn domain_browser_event(task_id: Option<TaskId>, payload: Event) -> DomainEvent {
    DomainEvent {
        id: EventId::new(),
        task_id,
        sequence: 2,
        task_revision: None,
        occurred_at_ms: 1,
        payload,
    }
}

fn collect_object_keys(value: serde_json::Value, keys: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                keys.insert(key);
                collect_object_keys(child, keys);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_object_keys(item, keys);
            }
        }
        _ => {}
    }
}

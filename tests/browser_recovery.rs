use devmanager::browser::{
    BrowserGenerationError, BrowserRecoveryCause, BrowserRecoveryController, BrowserRecoveryError,
    BrowserTaskService, BrowserTeardownStage, MAX_BROWSER_GENERATION_QUEUE,
};
use devmanager::domain::browser::BrowserHealth;
use devmanager::domain::id::{BrowserContextId, BrowserTabId, TaskId};

fn open_service() -> (BrowserTaskService, TaskId, BrowserContextId, u64) {
    let task_id = TaskId::new();
    let context_id = BrowserContextId::new();
    let mut service = BrowserTaskService::new();
    let generation = service
        .open_context(task_id, context_id)
        .expect("open context");
    (service, task_id, context_id, generation)
}

#[test]
fn navigation_renderer_and_process_failures_mint_a_new_generation() {
    for cause in [
        BrowserRecoveryCause::NavigationFailure,
        BrowserRecoveryCause::UnresponsiveRenderer,
        BrowserRecoveryCause::WebViewProcessCrash,
    ] {
        let (mut service, task_id, context_id, generation) = open_service();
        service
            .enqueue_wait(task_id, context_id, Some(BrowserTabId::new()), generation)
            .expect("queue wait");
        service
            .enqueue_capture(task_id, context_id, None, generation)
            .expect("queue capture");
        let outcome = service
            .recover(task_id, context_id, cause)
            .expect("recover");
        assert_eq!(outcome.from_generation, generation);
        assert_eq!(outcome.to_generation, Some(generation + 1));
        assert_eq!(outcome.health, BrowserHealth::Recovering);
        assert!(outcome.interruption);
        assert_eq!(outcome.helper_residue, 0);
        assert_eq!(service.queued_count(task_id, context_id), 0);
        assert_eq!(
            service
                .enqueue_wait(task_id, context_id, None, generation)
                .expect_err("old generation is dead"),
            devmanager::browser::BrowserTaskServiceError::Generation(
                BrowserGenerationError::GenerationMismatch
            )
        );
        service
            .enqueue_wait(task_id, context_id, None, generation + 1)
            .expect("new generation admits work");
    }
}

#[test]
fn client_crash_parks_before_reattach_and_sleep_invalidates_epochs() {
    let (mut service, task_id, context_id, generation) = open_service();
    let crash = service
        .recover(task_id, context_id, BrowserRecoveryCause::ClientCrash)
        .expect("client crash");
    assert_eq!(crash.to_generation, Some(generation + 1));
    assert!(crash.surface_parked);
    service
        .recovery()
        .accept_attach(task_id, context_id)
        .expect("parked surface may reattach");

    let sleep = service
        .recover(task_id, context_id, BrowserRecoveryCause::SleepWake)
        .expect("sleep");
    assert_eq!(sleep.to_generation, None);
    assert_eq!(sleep.bounds_epoch, crash.bounds_epoch + 1);
    assert_eq!(sleep.focus_epoch, crash.focus_epoch + 1);
    assert_eq!(
        service
            .recovery()
            .accept_input(task_id, context_id, crash.bounds_epoch, crash.focus_epoch)
            .expect_err("stale sleep epochs"),
        BrowserRecoveryError::StaleInputEpoch
    );
    assert_eq!(
        service
            .recovery()
            .accept_input(task_id, context_id, sleep.bounds_epoch, sleep.focus_epoch)
            .expect_err("input denied until fresh layout"),
        BrowserRecoveryError::InputDenied
    );
    service
        .recovery_mut()
        .record_fresh_layout(task_id, context_id, sleep.bounds_epoch, sleep.focus_epoch)
        .expect("fresh layout");
    service
        .recovery()
        .accept_input(task_id, context_id, sleep.bounds_epoch, sleep.focus_epoch)
        .expect("fresh evidence");

    let dpi = service
        .recover(task_id, context_id, BrowserRecoveryCause::DisplayDpiChange)
        .expect("dpi");
    assert_eq!(dpi.bounds_epoch, sleep.bounds_epoch + 1);
    assert_eq!(dpi.focus_epoch, sleep.focus_epoch + 1);
}

#[test]
fn teardown_is_ordered_idempotent_and_reports_helper_residue() {
    let mut controller = BrowserRecoveryController::new();
    let task_id = TaskId::new();
    let context_id = BrowserContextId::new();
    controller
        .open_context(task_id, context_id)
        .expect("open");
    assert_eq!(
        controller
            .advance_teardown(task_id, context_id, BrowserTeardownStage::CloseControllers)
            .expect_err("out of order"),
        BrowserRecoveryError::OutOfOrderTeardown
    );
    for stage in BrowserTeardownStage::ORDER {
        assert_eq!(
            controller
                .advance_teardown(task_id, context_id, stage)
                .expect("in order"),
            stage
        );
        assert_eq!(
            controller
                .advance_teardown(task_id, context_id, stage)
                .expect("repeat is idempotent"),
            stage
        );
    }
    assert_eq!(
        controller
            .advance_teardown(task_id, context_id, BrowserTeardownStage::MarkClosed)
            .expect("closed stays closed"),
        BrowserTeardownStage::MarkClosed
    );

    let mut leaky = BrowserRecoveryController::new();
    let leak_task = TaskId::new();
    let leak_context = BrowserContextId::new();
    leaky.open_context(leak_task, leak_context).expect("open leak");
    leaky.inject_helper_residue_for_test(leak_context, 1);
    for stage in [
        BrowserTeardownStage::CancelOperations,
        BrowserTeardownStage::DenyNewInput,
        BrowserTeardownStage::DetachParkSurface,
        BrowserTeardownStage::CloseControllers,
    ] {
        leaky
            .advance_teardown(leak_task, leak_context, stage)
            .expect("prefix");
    }
    assert_eq!(
        leaky
            .advance_teardown(
                leak_task,
                leak_context,
                BrowserTeardownStage::AwaitHelperDisappearance
            )
            .expect_err("visible leak"),
        BrowserRecoveryError::HelperResidue
    );
}

#[test]
fn host_shutdown_and_failed_create_leave_no_queued_orphans() {
    let (mut service, task_id, context_id, generation) = open_service();
    for _ in 0..4 {
        service
            .enqueue_wait(task_id, context_id, None, generation)
            .expect("queue");
    }
    let shutdown = service
        .recover(task_id, context_id, BrowserRecoveryCause::HostShutdown)
        .expect("shutdown");
    assert_eq!(shutdown.teardown_stage, Some(BrowserTeardownStage::MarkClosed));
    assert_eq!(shutdown.helper_residue, 0);
    assert_eq!(service.queued_count(task_id, context_id), 0);
    assert_eq!(
        service
            .enqueue_wait(task_id, context_id, None, generation)
            .expect_err("closed"),
        devmanager::browser::BrowserTaskServiceError::Generation(BrowserGenerationError::Closed)
    );

    let (mut failed, failed_task, failed_context, _) = open_service();
    let outcome = failed
        .recover(
            failed_task,
            failed_context,
            BrowserRecoveryCause::FailedCreate,
        )
        .expect("failed create teardown");
    assert_eq!(outcome.cause, BrowserRecoveryCause::FailedCreate);
    assert_eq!(outcome.helper_residue, 0);
    assert_eq!(failed.queued_count(failed_task, failed_context), 0);
}

#[test]
fn recovery_queue_stays_bounded() {
    let (mut service, task_id, context_id, generation) = open_service();
    for _ in 0..MAX_BROWSER_GENERATION_QUEUE {
        service
            .enqueue_wait(task_id, context_id, None, generation)
            .expect("fill");
    }
    assert!(service
        .enqueue_capture(task_id, context_id, None, generation)
        .is_err());
    service
        .recover(task_id, context_id, BrowserRecoveryCause::NavigationFailure)
        .expect("recover");
    assert_eq!(service.queued_count(task_id, context_id), 0);
}

//! Browser host/service seam tests.
//!
//! These tests remain portable and fail closed: they never create a Wry view,
//! launch WebView2, start a provider, or touch the installed app.

use devmanager::browser::protocol::{
    BrowserHoldSettleError, BrowserHostSettleIntent, BrowserIntegrationHold,
};
use devmanager::browser::{BrowserTaskService, BrowserTaskServiceError, BrowserWebViewHost};
use devmanager::domain::{
    BrowserContextId, BrowserRequestId, CommandId, OperationId, ResourceId, TaskId,
};
use devmanager::kernel::Effect;
use devmanager::protocol::{BrowserSurfaceIdentity, Capability, CapabilitySet};

#[test]
fn unavailable_host_keeps_accepted_browser_hold_typed_until_surface_exists() {
    let task_id = TaskId::new();
    let context_id = BrowserContextId::new();
    let resource_id = ResourceId::new();
    let request_id = BrowserRequestId::new();
    let intent = BrowserHostSettleIntent::bind_host_surface(
        CommandId::new(),
        OperationId::new(),
        request_id,
        task_id,
        context_id,
        resource_id,
        1,
        1,
    )
    .expect("intent");
    let hold = Effect::HoldBrowserHost {
        task_id,
        action_epoch: 1,
        request_id,
        context_id,
        generation: 1,
        hold: BrowserIntegrationHold::WebViewSurfaceAbsent,
    };
    let mut host = BrowserWebViewHost::unavailable("test host");

    let result = host.settle_accepted_browser_hold(
        CapabilitySet::from_capabilities([Capability::BrowserProjection]),
        &intent,
        &hold,
        &BrowserSurfaceIdentity {
            task_id,
            context_id,
            resource_id,
        },
    );

    assert_eq!(
        result,
        Err(BrowserHoldSettleError::Hold(
            BrowserIntegrationHold::WebViewSurfaceAbsent,
        ))
    );
}

#[test]
fn task_service_rejects_work_after_task_close_instead_of_leaving_orphaned_queue() {
    let task_id = TaskId::new();
    let context_id = BrowserContextId::new();
    let mut service = BrowserTaskService::new();
    let generation = service
        .open_context(task_id, context_id)
        .expect("open context");
    service
        .enqueue_wait(task_id, context_id, None, generation)
        .expect("enqueue wait");

    service.close_task(task_id).expect("close task");

    assert_eq!(
        service.enqueue_wait(task_id, context_id, None, generation),
        Err(BrowserTaskServiceError::Generation(
            devmanager::browser::BrowserGenerationError::Closed,
        ))
    );
    assert_eq!(service.queued_count(task_id, context_id), 0);
}

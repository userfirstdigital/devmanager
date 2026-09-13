//! Durable identity for a task's native browser, independent of provider PTYs.

use serde::{Deserialize, Serialize};

use super::browser::{BrowserAction, BrowserRequest, BrowserTabKind};
use super::command::{CommandEnvelope, RejectionCode};
use super::event::Event;
use super::id::{
    AgentSessionId, BrowserContextId, BrowserRequestId, BrowserTabId, ResourceId, TaskId,
};
use super::resource::{OwnerKind, ResourceFacts, ResourceKind, ResourceLifecycle, ResourceRecipe};
use super::snapshot::TaskSnapshot;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenTaskBrowserIntent {
    pub agent_session_id: AgentSessionId,
    pub resource: ResourceFacts,
    pub tab_id: BrowserTabId,
    pub create_request_id: BrowserRequestId,
    pub open_request_id: BrowserRequestId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeBrowserSessionProjection {
    pub task_id: TaskId,
    pub agent_session_id: AgentSessionId,
    pub context_id: BrowserContextId,
    pub resource_id: ResourceId,
    pub generation: u64,
    pub tab_id: BrowserTabId,
    pub start_url: String,
}

impl NativeBrowserSessionProjection {
    /// A browser routing key, never a provider conversation or process identity.
    pub fn routing_key(&self) -> String {
        format!("native-browser:{}", self.resource_id)
    }

    /// Require one exact context/resource association. Partial or ambiguous state
    /// is an error; callers must not repair it by picking arbitrary first rows.
    pub fn from_snapshot(snapshot: &TaskSnapshot) -> Result<Option<Self>, RejectionCode> {
        if !matches!(
            snapshot.task.lifecycle,
            super::task::TaskLifecycle::Open | super::task::TaskLifecycle::Settled
        ) {
            return Err(RejectionCode::InvalidTransition);
        }
        let browser = snapshot.browser.identity_snapshot();
        let resources: Vec<_> = snapshot
            .resources
            .values()
            .filter(|resource| {
                resource.resource_kind == ResourceKind::BrowserContext
                    && resource.lifecycle != ResourceLifecycle::Released
            })
            .collect();
        let contexts: Vec<_> = browser
            .contexts
            .iter()
            .filter(|context| !context.closed)
            .collect();
        if resources.is_empty() && contexts.is_empty() {
            return Ok(None);
        }
        if resources.len() != 1 || contexts.len() != 1 {
            return Err(RejectionCode::OwnershipConflict);
        }
        let resource = resources[0];
        let context = contexts[0];
        let ResourceRecipe::Browser {
            context_id: Some(context_id),
            ..
        } = &resource.recipe
        else {
            return Err(RejectionCode::OwnershipConflict);
        };
        if resource.task_id != Some(snapshot.task.id)
            || resource.owner_kind != OwnerKind::Task
            || resource.lifecycle != ResourceLifecycle::Active
            || *context_id != context.context_id
            || context.task_id != snapshot.task.id
            || context.generation == 0
            || resource.runtime_generation != context.generation
        {
            return Err(RejectionCode::OwnershipConflict);
        }
        let agent_session_id = snapshot.primary_agent_id.ok_or(RejectionCode::NotFound)?;
        let agent = snapshot
            .agents
            .get(&agent_session_id)
            .ok_or(RejectionCode::NotFound)?;
        if agent.task_id != snapshot.task.id
            || agent.lifecycle != super::agent::AgentSessionLifecycle::Open
        {
            return Err(RejectionCode::InvalidTransition);
        }
        let tab = browser
            .tabs
            .iter()
            .find(|tab| {
                Some(tab.tab_id) == context.selected_tab_id
                    && tab.context_id == context.context_id
                    && tab.task_id == snapshot.task.id
                    && !tab.closed
            })
            .ok_or(RejectionCode::InvalidTransition)?;
        Ok(Some(Self {
            task_id: snapshot.task.id,
            agent_session_id,
            context_id: context.context_id,
            resource_id: resource.id,
            generation: context.generation,
            tab_id: tab.tab_id,
            start_url: tab
                .committed_url
                .clone()
                .unwrap_or_else(|| "about:blank".into()),
        }))
    }
}

pub(super) fn decide_open(
    snapshot: &TaskSnapshot,
    envelope: &CommandEnvelope,
    intent: &OpenTaskBrowserIntent,
) -> Result<Vec<Event>, RejectionCode> {
    if NativeBrowserSessionProjection::from_snapshot(snapshot)?.is_some() {
        return Err(RejectionCode::AlreadyExists);
    }
    let agent = snapshot
        .agents
        .get(&intent.agent_session_id)
        .ok_or(RejectionCode::NotFound)?;
    if snapshot.primary_agent_id != Some(intent.agent_session_id)
        || agent.task_id != snapshot.task.id
        || agent.lifecycle != super::agent::AgentSessionLifecycle::Open
    {
        return Err(RejectionCode::OwnershipConflict);
    }
    let resource = &intent.resource;
    let ResourceRecipe::Browser {
        context_id: Some(context_id),
        start_url,
    } = &resource.recipe
    else {
        return Err(RejectionCode::InvalidTransition);
    };
    if resource.validate().is_err()
        || resource.resource_kind != ResourceKind::BrowserContext
        || resource.owner_kind != OwnerKind::Task
        || resource.task_id != Some(snapshot.task.id)
        || resource.lifecycle != ResourceLifecycle::Active
        || resource.runtime_generation != 1
        || start_url != "about:blank"
        || intent.create_request_id == intent.open_request_id
    {
        return Err(RejectionCode::OwnershipConflict);
    }
    if snapshot.resources.contains_key(&resource.id) {
        return Err(RejectionCode::AlreadyExists);
    }
    let mut browser = snapshot.browser.clone();
    let mut events = Vec::new();
    for (request_id, tab_id, action) in [
        (intent.create_request_id, None, BrowserAction::CreateContext),
        (
            intent.open_request_id,
            Some(intent.tab_id),
            BrowserAction::OpenTab {
                url: start_url.clone(),
                kind: BrowserTabKind::Page,
            },
        ),
    ] {
        let request = BrowserRequest {
            request_id,
            task_id: snapshot.task.id,
            context_id: *context_id,
            tab_id,
            generation: resource.runtime_generation,
            action,
        };
        let mut accepted = browser
            .plan_admit(&request)
            .map_err(|_| RejectionCode::InvalidTransition)?;
        accepted.bind_command(envelope.command_id, snapshot.task.action_epoch);
        browser
            .apply_facts(&accepted.facts)
            .map_err(|_| RejectionCode::InvalidTransition)?;
        events.extend(accepted.facts.into_iter().map(Event::Browser));
    }
    events.push(Event::ResourceRegistered {
        resource: resource.clone(),
    });
    Ok(events)
}

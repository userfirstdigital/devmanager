//! Read-only terminal/conversation inspection for an explicitly named isolated smoke host.
//! Never starts a provider, submits input, or reads an installed profile.
use devmanager::client::{ClientSubscription, HostClient, HostClientConfig};
use devmanager::domain::{ClientId, TaskCockpitQuery, TaskCockpitResult, TaskId};
use devmanager::protocol::{Capability, CapabilitySet, FrameLimits};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !cfg!(debug_assertions)
        || !(args.len() == 2
            || (args.len() == 3 && matches!(args[2].as_str(), "--readiness" | "--conversation")))
        || !(args[0].starts_with("devmanager-native-ui-a-")
            || args[0].starts_with("devmanager-native-ui-b-"))
    {
        return Err(
            "usage: remote-native-diagnostic <isolated native-ui profile> <task-id> [--readiness|--conversation]"
                .into(),
        );
    }
    let task_id = TaskId::parse(&args[1])?;
    let mut client = HostClient::connect(HostClientConfig {
        named_profile: args[0].clone(),
        client_build: "native-fixture-read-only-diagnostic".into(),
        client_id: ClientId::new(),
        requested: CapabilitySet::from_capabilities([
            Capability::TaskCockpit,
            Capability::SemanticConversation,
            Capability::PagedSnapshots,
            Capability::EventReplay,
        ]),
        limits: FrameLimits::v1_default(),
    })
    .await?;
    let query = match args.get(2).map(String::as_str) {
        Some("--readiness") => TaskCockpitQuery::TerminalReadiness,
        Some("--conversation") => TaskCockpitQuery::Conversation { after_sequence: 0 },
        _ => TaskCockpitQuery::Terminal,
    };
    let result = client.query_task_cockpit(task_id, query).await?;
    match result {
        Ok(TaskCockpitResult::Terminal(terminal)) => {
            println!(
                "task={} runtime={} terminal_sequence={} modes={:?}",
                terminal.task_id,
                terminal.runtime_generation,
                terminal.sequence,
                terminal.screen.mode
            );
            for line in terminal.text_lines {
                println!("{line}");
            }
        }
        Ok(TaskCockpitResult::Conversation(page)) => {
            println!("{}", serde_json::to_string_pretty(&page)?);
            let mut subscription = ClientSubscription::new();
            subscription.synchronize(&mut client).await?;
            let model = subscription.model().ok_or("canonical model missing")?;
            let journal = devmanager::ui::renderers::SemanticJournalView::from_live_page(
                model, task_id, &page,
            )?;
            let timeline = devmanager::ui::task_cockpit::Timeline::project(
                model,
                task_id,
                client.granted_capabilities(),
                &journal,
                &devmanager::ui::renderers::RendererRegistry::standard()?,
                devmanager::ui::task_cockpit::TimelineViewport {
                    height: 500,
                    scroll_offset: 0,
                },
            )?;
            println!("Canonical render rows: {:#?}", timeline.rows());
            subscription.release(&mut client).await?;
        }
        other => println!("{other:?}"),
    }
    Ok(())
}

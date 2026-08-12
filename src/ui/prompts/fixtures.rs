//! Local lifecycle and visual fixtures for the Prompt Library UI.

use super::super::shell::{
    ColorScheme, DataFixtureKind, Density, LayoutWidth, PromptLibraryViewport, ScalePercent,
};
use super::super::task_cockpit::composer::ProviderCommandSuggestion;
use super::history::RecentHistoryRecord;
use crate::domain::id::{
    AgentSessionId, PromptChainId, PromptChainLinkId, PromptHistoryId, PromptId, PromptVersionId,
    TaskId,
};
use crate::prompts::model::{PromptChain, PromptVersion, SavedPrompt};
use crate::prompts::projection::PromptChainLinkRecord;
use serde::Serialize;
use sha2::{Digest, Sha256};

fn id_bytes(n: u32) -> [u8; 16] {
    let mut bytes = [
        0x01, 0x92, 0xf5, 0xd0, 0x00, 0x00, 0x70, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00,
    ];
    bytes[12..16].copy_from_slice(&n.to_be_bytes());
    bytes
}

pub fn prompt_id(n: u32) -> PromptId {
    PromptId::from_bytes(id_bytes(n)).expect("UUIDv7 prompt id")
}

pub fn version_id(n: u32) -> PromptVersionId {
    PromptVersionId::from_bytes(id_bytes(n)).expect("UUIDv7 version id")
}

pub fn chain_id(n: u32) -> PromptChainId {
    PromptChainId::from_bytes(id_bytes(n)).expect("UUIDv7 chain id")
}

pub fn link_id(n: u32) -> PromptChainLinkId {
    PromptChainLinkId::from_bytes(id_bytes(n)).expect("UUIDv7 link id")
}

pub fn history_id(n: u32) -> PromptHistoryId {
    PromptHistoryId::from_bytes(id_bytes(n)).expect("UUIDv7 history id")
}

pub fn task_id(n: u32) -> TaskId {
    TaskId::from_bytes(id_bytes(n)).expect("UUIDv7 task id")
}

pub fn agent_session_id(n: u32) -> AgentSessionId {
    AgentSessionId::from_bytes(id_bytes(n)).expect("UUIDv7 agent session id")
}

pub fn saved_prompt(n: u32, title: &str, version: u32, archived: bool) -> SavedPrompt {
    SavedPrompt {
        id: prompt_id(n),
        title: title.to_string(),
        description: Some("Lifecycle fixture prompt".into()),
        tags: vec!["review".into(), "unicode".into()],
        current_version_id: version_id(version),
        revision: if archived { 2 } else { 1 },
        archived_at_ms: archived.then_some(1_728_000_100_000),
    }
}

pub fn version(n: u32, prompt: u32, version: u32, body: &str) -> PromptVersion {
    PromptVersion::new(
        version_id(n),
        prompt_id(prompt),
        version,
        body.to_string(),
        1_728_000_000_000 + i64::from(n),
    )
    .expect("valid prompt version")
}

pub fn chain_link(
    n: u32,
    chain: u32,
    position: u32,
    prompt: u32,
    version: u32,
    previous: Option<u32>,
    next: Option<u32>,
    update_available: bool,
) -> PromptChainLinkRecord {
    PromptChainLinkRecord::try_new(
        link_id(n),
        chain_id(chain),
        position,
        prompt_id(prompt),
        version_id(version),
        previous.map(link_id),
        next.map(link_id),
        update_available,
    )
    .expect("bounded chain link")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleFixture {
    pub prompts: Vec<SavedPrompt>,
    pub versions: Vec<PromptVersion>,
    pub chains: Vec<PromptChain>,
    pub links: Vec<PromptChainLinkRecord>,
    pub history: Vec<RecentHistoryRecord>,
    pub provider_commands: Vec<ProviderCommandSuggestion>,
}

pub fn lifecycle_fixture() -> LifecycleFixture {
    let unicode_body = "Review café changes:\n```rust\nfn main() {}\n```\nこんにちは";
    let long_body = format!("{}\n{}", unicode_body, "word ".repeat(400));
    let versions = vec![
        version(2, 1, 1, "Inspect the first draft."),
        version(
            3,
            1,
            2,
            "Inspect the second draft with a Markdown list:\n- one\n- two",
        ),
        version(4, 1, 3, &long_body),
        version(6, 5, 1, "Archived prompt body stays readable."),
        version(12, 11, 1, "Step one"),
        version(14, 13, 1, "Step two"),
        version(16, 15, 1, "Step three"),
        version(18, 17, 1, "Step four"),
        version(20, 19, 1, "Step five"),
        version(22, 21, 1, "Arbitrary extra prompt"),
    ];
    let prompts = vec![
        saved_prompt(1, "Unicode review café", 4, false),
        saved_prompt(5, "Archived helper", 6, true),
        saved_prompt(11, "Chain step 1", 12, false),
        saved_prompt(13, "Chain step 2", 14, false),
        saved_prompt(15, "Chain step 3", 16, false),
        saved_prompt(17, "Chain step 4", 18, false),
        saved_prompt(19, "Chain step 5", 20, false),
        saved_prompt(21, "Arbitrary insert", 22, false),
    ];
    let chains = vec![
        PromptChain {
            id: chain_id(30),
            title: "Five-link review".into(),
            description: Some("Manual ordered chain".into()),
            revision: 1,
            archived_at_ms: None,
        },
        PromptChain {
            id: chain_id(31),
            title: "Second chain".into(),
            description: None,
            revision: 1,
            archived_at_ms: None,
        },
    ];
    let links = vec![
        chain_link(40, 30, 1, 11, 12, None, Some(41), false),
        chain_link(41, 30, 2, 13, 14, Some(40), Some(42), false),
        chain_link(42, 30, 3, 15, 16, Some(41), Some(43), false),
        chain_link(43, 30, 4, 17, 18, Some(42), Some(44), true),
        chain_link(44, 30, 5, 19, 20, Some(43), None, false),
        chain_link(50, 31, 1, 1, 4, None, None, false),
    ];
    let history = (0..500u32)
        .map(|index| {
            RecentHistoryRecord::delivered(
                history_id(200 + index),
                task_id(8),
                agent_session_id(9),
                "claude",
                format!("Delivered prompt {index:03} — café"),
                1_728_000_200_000 + i64::from(index),
            )
        })
        .collect();
    LifecycleFixture {
        prompts,
        versions,
        chains,
        links,
        history,
        provider_commands: vec![ProviderCommandSuggestion {
            label: "Review".into(),
            command: "/review".into(),
            provider_kind: "claude".into(),
        }],
    }
}

pub fn large_prompt_set(count: usize) -> Vec<SavedPrompt> {
    (0..count as u32)
        .map(|index| saved_prompt(1_000 + index, &format!("p{index:05}"), 2, false))
        .collect()
}

pub fn viewport_matrix() -> Vec<PromptLibraryViewport> {
    let mut out = Vec::new();
    for scheme in [ColorScheme::Light, ColorScheme::Dark] {
        for density in [Density::Compact, Density::Comfortable] {
            for scale in [
                ScalePercent::OneHundred,
                ScalePercent::OneTwentyFive,
                ScalePercent::OneFifty,
                ScalePercent::TwoHundred,
            ] {
                for width in [LayoutWidth::Narrow, LayoutWidth::Wide] {
                    for data in [
                        DataFixtureKind::Empty,
                        DataFixtureKind::Error,
                        DataFixtureKind::LargeData,
                        DataFixtureKind::Populated,
                    ] {
                        out.push(PromptLibraryViewport {
                            scheme,
                            density,
                            scale,
                            width,
                            data,
                        });
                    }
                }
            }
        }
    }
    out
}

#[derive(Serialize)]
pub struct LifecycleFixtureManifest {
    pub schema_version: u32,
    pub kind: &'static str,
    pub unicode_title: bool,
    pub version_count: usize,
    pub history_count: usize,
    pub chain_count: usize,
    pub archived_prompt: bool,
    pub provider_slash_commands: usize,
    pub five_link_chain: bool,
    pub structure_sha256: String,
}

pub fn lifecycle_manifest(fixture: &LifecycleFixture) -> LifecycleFixtureManifest {
    let mut hasher = Sha256::new();
    hasher.update((fixture.prompts.len() as u64).to_be_bytes());
    hasher.update((fixture.versions.len() as u64).to_be_bytes());
    hasher.update((fixture.history.len() as u64).to_be_bytes());
    hasher.update((fixture.links.len() as u64).to_be_bytes());
    for prompt in &fixture.prompts {
        hasher.update(prompt.id.as_bytes());
        hasher.update(prompt.current_version_id.as_bytes());
        hasher.update(prompt.revision.to_be_bytes());
    }
    LifecycleFixtureManifest {
        schema_version: 1,
        kind: "prompt_library_lifecycle_v1",
        unicode_title: fixture
            .prompts
            .iter()
            .any(|prompt| prompt.title.contains('é')),
        version_count: fixture.versions.len(),
        history_count: fixture.history.len(),
        chain_count: fixture.chains.len(),
        archived_prompt: fixture
            .prompts
            .iter()
            .any(|prompt| prompt.archived_at_ms.is_some()),
        provider_slash_commands: fixture.provider_commands.len(),
        five_link_chain: fixture
            .links
            .iter()
            .filter(|link| link.chain_id() == chain_id(30))
            .count()
            == 5,
        structure_sha256: format!("{:x}", hasher.finalize()),
    }
}

#[derive(Serialize)]
pub struct PerformanceFixtureManifest {
    pub schema_version: u32,
    pub kind: &'static str,
    pub prompt_count: usize,
    pub link_count: usize,
    pub history_count: usize,
    pub virtualize_window: usize,
    pub fts_on_input_path: bool,
    pub declared_virtualize_budget_us: u64,
}

pub fn performance_manifest() -> PerformanceFixtureManifest {
    PerformanceFixtureManifest {
        schema_version: 1,
        kind: "prompt_library_performance_v1",
        prompt_count: 5_000,
        link_count: 2_000,
        history_count: 500,
        virtualize_window: 80,
        fts_on_input_path: false,
        declared_virtualize_budget_us: 50_000,
    }
}

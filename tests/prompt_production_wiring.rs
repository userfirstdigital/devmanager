//! Compile/wiring assertions for prompt-production settlement and mutation seams.

use devmanager::domain::command::{Command, CommandEnvelope};
use devmanager::domain::id::{ClientId, CommandId, PromptChainLinkId, PromptId};
use devmanager::prompts::projection::testing;
use devmanager::prompts::{
    ArchivePrompt, DurableProviderDeliveryAdapter, PromptChainCommand, PromptCommand,
    PromptHistoryErrorCode, ProviderDurableSettlement, RemovePromptChainLink,
    UnsupportedProviderDurableSettlement, ValidatedDeliveredInputProof,
};
use devmanager::ui::mutation::{
    require_prompt_mutation_grant, PromptMutationAuthority, PromptMutationError,
    PromptMutationExecutor, PROMPT_CHAIN_INDEX_BASE,
};

fn history_source() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/prompts/history.rs"
    ))
    .expect("history source")
}

fn mutation_source() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/ui/prompts/mutation.rs"
    ))
    .expect("mutation source")
}

fn prompt_library_envelope(client_id: ClientId) -> CommandEnvelope {
    CommandEnvelope {
        command_id: CommandId::new(),
        client_id,
        task_id: None,
        issued_at_ms: 1,
        expected_task_revision: Some(1),
        command: Command::PromptLibrary(PromptCommand::ArchivePrompt(ArchivePrompt {
            prompt_id: PromptId::new(),
            archived_at_ms: 1,
            expected_revision: 1,
        })),
    }
}

#[test]
fn durable_settlement_rejects_unsupported_provider_receipt() {
    let unsupported = UnsupportedProviderDurableSettlement;
    let error = ValidatedDeliveredInputProof::from_provider_durable_settlement(&unsupported)
        .expect_err("unsupported provider settlement must not become history");
    assert_eq!(
        error.code(),
        PromptHistoryErrorCode::ProviderInputUnavailable
    );
    let still_unavailable = ValidatedDeliveredInputProof::from_provider_input_settlement()
        .expect_err("zero-arg constructor stays fail-closed");
    assert_eq!(
        still_unavailable.code(),
        PromptHistoryErrorCode::ProviderInputUnavailable
    );
}

#[test]
fn durable_settlement_adapter_stays_on_existing_tx_path() {
    let source = history_source();
    for required in [
        "pub trait ProviderDurableSettlement",
        "pub struct ProviderOwnedDurableSettlement",
        "pub struct DurableProviderDeliveryAdapter",
        "pub fn apply_provider_durable_settlement_in_tx",
        "pub fn commit_provider_durable_settlement",
        "PromptHistoryStore::apply_delivered_in_tx(tx, &proof)",
        "from_provider_durable_settlement",
        "ProviderInputUnavailable",
    ] {
        assert!(
            source.contains(required),
            "history settlement adapter must expose {required}"
        );
    }
    assert!(
        !source.contains("INSERT INTO prompt_history")
            || source.contains("pub fn apply_delivered_in_tx"),
        "settlement must reuse the existing atomic apply_delivered_in_tx path"
    );
    let _ = DurableProviderDeliveryAdapter::new(&UnsupportedProviderDurableSettlement);
    let _: &dyn ProviderDurableSettlement = &UnsupportedProviderDurableSettlement;
}

#[test]
fn owner_gated_mutation_fails_closed_for_readonly_and_ungranted() {
    let client_id = ClientId::new();
    let envelope = prompt_library_envelope(client_id);
    assert_eq!(
        require_prompt_mutation_grant(PromptMutationAuthority::Ungranted, &envelope),
        Err(PromptMutationError::Ungranted)
    );
    assert_eq!(
        require_prompt_mutation_grant(PromptMutationAuthority::ReadOnly, &envelope),
        Err(PromptMutationError::ReadOnly)
    );
    let foreign = testing::owner_grant(ClientId::new()).expect("owner grant");
    assert_eq!(
        require_prompt_mutation_grant(PromptMutationAuthority::OwnerGranted(&foreign), &envelope),
        Err(PromptMutationError::Ungranted)
    );
    let grant = testing::owner_grant(client_id).expect("matching owner grant");
    assert!(require_prompt_mutation_grant(
        PromptMutationAuthority::OwnerGranted(&grant),
        &envelope
    )
    .is_ok());
    let _ = PromptMutationExecutor;
}

#[test]
fn owner_gated_mutation_preserves_zero_based_chain_and_host_bus_path() {
    assert_eq!(PROMPT_CHAIN_INDEX_BASE, 0);
    let source = mutation_source();
    for required in [
        "execute_with_owner_grant",
        "pub fn require_prompt_mutation_grant",
        "PromptMutationAuthority::ReadOnly",
        "PromptMutationAuthority::Ungranted",
        "query_prompt_library",
        "execute_command",
        "hydrate_active_session",
        "mutate_active_session",
        "PROMPT_CHAIN_INDEX_BASE: u32 = 0",
    ] {
        assert!(
            source.contains(required),
            "owner-gated mutation executor must expose {required}"
        );
    }
    assert!(
        !source.contains("position + 1") && !source.contains("position.saturating_add(1)"),
        "mutation executor must not remap 0-based chain positions"
    );
}

#[test]
fn authenticated_command_envelope_carries_prompt_chain_mutations() {
    let client_id = ClientId::new();
    let envelope = CommandEnvelope {
        command_id: CommandId::new(),
        client_id,
        task_id: None,
        issued_at_ms: 1,
        expected_task_revision: Some(1),
        command: Command::PromptChain(PromptChainCommand::RemovePromptChainLink(
            RemovePromptChainLink {
                chain_id: devmanager::domain::id::PromptChainId::new(),
                link_id: PromptChainLinkId::new(),
                expected_revision: 1,
            },
        )),
    };
    let payload = rmp_serde::to_vec_named(&envelope).expect("chain command envelope encodes");
    let round_trip: CommandEnvelope =
        rmp_serde::from_slice(&payload).expect("chain command envelope decodes");
    assert_eq!(round_trip, envelope);
}

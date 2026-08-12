//! Native Prompt Library UI projection/action tests for Tasks 7.5, 7.6, and 7.8.
//!
//! These stay host-local and never open Connect, session.json, or the installed app.

use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use devmanager::client::action::{
    action_by_id, catalog, put_prompt_version_in_composer, ACTION_PROMPT_PUT_IN_COMPOSER,
};
use devmanager::client::composer::{
    apply_put_prompt_version, suggest_provider_commands, ComposerDraft, ComposerInsertionMode,
    ExactPromptPayload,
};
use devmanager::prompts::projection::PromptNamespace;
use devmanager::prompts::ui::composer::ProviderCommandSuggestion;
use devmanager::prompts::ui::editor::PromptEditorAction;
use devmanager::prompts::ui::fixtures::{
    agent_session_id, chain_id, chain_link, large_prompt_set, lifecycle_fixture,
    lifecycle_manifest, link_id, performance_manifest, prompt_id, saved_prompt, task_id, version,
    version_id, viewport_matrix,
};
use devmanager::prompts::ui::history::PromptHistoryPolicy;
use devmanager::prompts::ui::library::virtualize;
use devmanager::prompts::ui::picker::PromptPickerSource;
use devmanager::prompts::ui::shell::{
    ColorScheme, DataFixtureKind, Density, LayoutWidth, LibrarySection, OrganizationCatalogHook,
    PromptLibraryViewport, ScalePercent, SyncHookStatus, PROMPT_LIBRARY_RAIL_ID,
};
use devmanager::prompts::ui::{
    put_in_composer_action, PromptLibraryAction, PromptLibraryKey, PromptLibraryLoadState,
    PromptLibrarySession, PromptLibraryUiError, MAX_VIRTUALIZED_LINKS, MAX_VIRTUALIZED_PROMPTS,
};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/prompts/v1")
}

fn conformance_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/conformance/prompts/v1")
}

fn assert_golden_json(dir: PathBuf, name: &str, value: &serde_json::Value) {
    let path = dir.join(name);
    let expected = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("missing golden fixture {}: {error}", path.display()));
    let expected: serde_json::Value =
        serde_json::from_str(&expected).expect("golden fixture must be JSON");
    assert_eq!(value, &expected, "golden fixture drift: {}", path.display());
}

fn populated_session() -> PromptLibrarySession {
    let fixture = lifecycle_fixture();
    let mut session = PromptLibrarySession::new(PromptLibraryViewport {
        scheme: ColorScheme::Light,
        density: Density::Comfortable,
        scale: ScalePercent::OneHundred,
        width: LayoutWidth::Wide,
        data: DataFixtureKind::Populated,
    });
    session.saved = fixture.prompts;
    session.versions = fixture.versions;
    session.chains = fixture.chains;
    session.links = fixture.links;
    session.history = fixture.history;
    session.provider_commands = fixture.provider_commands;
    session.library_revision = 4;
    session.load = PromptLibraryLoadState::Ready;
    session
}

#[test]
fn library_chrome_keeps_saved_history_and_chains_distinct() {
    let session = populated_session();
    assert_eq!(session.chrome.rail_id, PROMPT_LIBRARY_RAIL_ID);
    assert_eq!(
        session.chrome.sections,
        [
            LibrarySection::SavedPrompts,
            LibrarySection::RecentHistory,
            LibrarySection::Chains
        ]
    );
    for section in LibrarySection::ALL {
        assert!(!section.admits_provider_commands());
        assert_ne!(section.label(), "Provider commands");
    }
    assert!(!session.provider_commands_in_library);
    assert_eq!(session.chrome.hooks.namespace, PromptNamespace::Personal);
    assert_eq!(
        session.chrome.hooks.sync,
        SyncHookStatus::LocalAuthoritative
    );
    assert_eq!(
        session.chrome.hooks.organization,
        OrganizationCatalogHook::Unavailable
    );
    assert!(!session.chrome.hooks.encrypts_prompt_bodies);
}

#[test]
fn search_filter_create_edit_archive_restore_and_stale_revision() {
    let mut session = populated_session();
    session
        .apply(PromptLibraryAction::Search("café".into()))
        .expect("search");
    let list = session.list_state();
    assert!(list.visible.iter().any(|row| row.title.contains('é')));
    assert!(!list.includes_provider_commands);

    let created = saved_prompt(80, "Fresh prompt", 81, false);
    let created_version = version(81, 80, 1, "Brand new body");
    session
        .apply(PromptLibraryAction::CreatePrompt {
            prompt: created.clone(),
            version: created_version.clone(),
        })
        .expect("create");

    session
        .apply(PromptLibraryAction::EditPrompt(
            PromptEditorAction::SetTitle("Fresh prompt edited".into()),
        ))
        .expect("edit title");
    let mut next = created.clone();
    next.title = "Fresh prompt edited".into();
    next.revision = 2;
    next.current_version_id = version_id(82);
    session
        .apply(PromptLibraryAction::EditPrompt(
            PromptEditorAction::SaveAsNewVersion {
                prompt: next,
                version: version(82, 80, 2, "Brand new body v2"),
            },
        ))
        .expect("save version");

    session
        .apply(PromptLibraryAction::ArchivePrompt {
            prompt_id: prompt_id(80),
            expected_revision: 2,
            archived_at_ms: 9,
        })
        .expect("archive");
    assert!(session
        .saved
        .iter()
        .any(|prompt| prompt.id == prompt_id(80) && prompt.archived_at_ms == Some(9)));

    let stale = session.apply(PromptLibraryAction::RestorePrompt {
        prompt_id: prompt_id(80),
        expected_revision: 2,
    });
    assert_eq!(stale, Err(PromptLibraryUiError::StaleRevision));
    assert!(matches!(
        session.load,
        PromptLibraryLoadState::StaleRevision {
            expected: 2,
            actual: 3
        }
    ));

    session.load = PromptLibraryLoadState::Ready;
    session
        .apply(PromptLibraryAction::RestorePrompt {
            prompt_id: prompt_id(80),
            expected_revision: 3,
        })
        .expect("restore");
}

#[test]
fn version_list_and_native_diff_preserve_bodies() {
    let mut session = populated_session();
    session
        .apply(PromptLibraryAction::SelectVersion {
            version_id: version_id(2),
        })
        .expect("select v1");
    session
        .apply(PromptLibraryAction::DiffVersions {
            old_version_id: version_id(2),
            new_version_id: version_id(3),
        })
        .expect("diff");
    let diff = session.editor.diff.expect("diff view");
    assert!(diff.preserves_original_bodies);
    assert_eq!(diff.old_version, 1);
    assert_eq!(diff.new_version, 2);
    assert!(!diff.hunks.is_empty());
    assert!(diff.accessible_status().contains("diff"));
}

#[test]
fn empty_loading_error_and_keyboard_navigation() {
    let mut session = PromptLibrarySession::new(PromptLibraryViewport {
        scheme: ColorScheme::Dark,
        density: Density::Compact,
        scale: ScalePercent::TwoHundred,
        width: LayoutWidth::Narrow,
        data: DataFixtureKind::Empty,
    });
    assert_eq!(session.load, PromptLibraryLoadState::Empty);
    session.load = PromptLibraryLoadState::Loading;
    session.load = PromptLibraryLoadState::Error {
        message: "host unavailable".into(),
    };
    assert_eq!(session.focus_accessible_name().role, "option");

    session = populated_session();
    session
        .handle_key(PromptLibraryKey::ArrowDown)
        .expect("down");
    assert_eq!(session.focused_index, 1);
    session.handle_key(PromptLibraryKey::Tab).expect("tab");
    assert_eq!(session.chrome.active_section, LibrarySection::RecentHistory);
    session
        .handle_key(PromptLibraryKey::LibraryShortcut)
        .expect("library shortcut");
    assert_eq!(
        session.picker.as_ref().map(|picker| picker.source),
        Some(PromptPickerSource::Saved)
    );
    session.handle_key(PromptLibraryKey::Slash).expect("slash");
    let picker = session.picker.expect("slash picker");
    assert_eq!(picker.source, PromptPickerSource::ProviderCommands);
    assert!(picker.hits.is_empty());
    assert!(picker
        .notice
        .is_some_and(|notice| notice.contains("provider-native")));
}

#[test]
fn chain_overview_inserts_between_adjacent_links_and_suggests_next_manually() {
    let mut session = populated_session();
    session
        .apply(PromptLibraryAction::SelectSection(LibrarySection::Chains))
        .expect("chains");
    let before = session.chain_projection(chain_id(30)).expect("chain");
    assert_eq!(before.total_links, 5);
    assert!(!before.auto_advance);
    assert!(!before.has_run_button);
    assert!(!before.has_graph_canvas);
    assert!(before.gaps.iter().any(
        |gap| gap.after_link_id == Some(link_id(41)) && gap.before_link_id == Some(link_id(42))
    ));
    assert!(before
        .links
        .iter()
        .all(|link| link.connector_visible || link.link.next_link_id().is_none()));

    let inserted = chain_link(60, 30, 3, 21, 22, Some(41), Some(42), false);
    session
        .apply(PromptLibraryAction::InsertChainLinkBetween {
            chain_id: chain_id(30),
            after_link_id: link_id(41),
            before_link_id: link_id(42),
            link: inserted,
        })
        .expect("insert between 2 and 3");
    let after = session
        .chain_projection(chain_id(30))
        .expect("updated chain");
    assert_eq!(after.total_links, 6);
    let positions: Vec<_> = session
        .links
        .iter()
        .filter(|link| link.chain_id() == chain_id(30))
        .map(|link| (link.position(), link.id(), link.prompt_id()))
        .collect();
    assert_eq!(positions[1].1, link_id(41));
    assert_eq!(positions[2].1, link_id(60));
    assert_eq!(positions[3].1, link_id(42));
    assert_eq!(positions[2].2, prompt_id(21));

    session
        .apply(PromptLibraryAction::UpdateLinkToCurrent {
            chain_id: chain_id(30),
            link_id: link_id(43),
            current_version_id: version_id(18),
        })
        .expect("pin current");
    assert!(!session
        .links
        .iter()
        .find(|link| link.id() == link_id(43))
        .expect("link")
        .update_available());

    session
        .apply(PromptLibraryAction::PutInComposer(put_in_composer_action(
            task_id(8),
            agent_session_id(9),
            version_id(12),
            ComposerInsertionMode::ReplaceDraft,
            Some(link_id(40)),
        )))
        .expect("put first link");
    let suggested = session
        .suggested_next
        .as_ref()
        .expect("manual suggested next");
    assert!(!suggested.automatic);
    assert_eq!(suggested.link_id, link_id(41));
    let revision_before_send = session
        .chains
        .iter()
        .find(|chain| chain.id == chain_id(30))
        .expect("chain")
        .revision;
    session.draft.mark_sent();
    assert_eq!(
        session
            .chains
            .iter()
            .find(|chain| chain.id == chain_id(30))
            .expect("chain")
            .revision,
        revision_before_send
    );
    assert_eq!(
        session.suggested_next.as_ref().map(|next| next.link_id),
        Some(link_id(41))
    );
}

#[test]
fn insert_between_rejects_non_adjacent_links() {
    let mut session = populated_session();
    let err = session.apply(PromptLibraryAction::InsertChainLinkBetween {
        chain_id: chain_id(30),
        after_link_id: link_id(40),
        before_link_id: link_id(43),
        link: chain_link(99, 30, 2, 21, 22, Some(40), Some(43), false),
    });
    assert_eq!(err, Err(PromptLibraryUiError::AdjacentLinksRequired));
}

#[test]
fn history_search_save_as_prompt_and_clear_does_not_touch_task_facts() {
    let mut session = populated_session();
    session
        .apply(PromptLibraryAction::SelectSection(
            LibrarySection::RecentHistory,
        ))
        .expect("history");
    session
        .apply(PromptLibraryAction::Search("042".into()))
        .expect("history search");
    let visible = session.visible_history();
    assert_eq!(visible.len(), 1);
    assert!(visible[0].body.contains("042"));

    session
        .apply(PromptLibraryAction::SaveHistoryAsPrompt {
            history_id: visible[0].id,
            prompt: saved_prompt(90, "Saved from history", 91, false),
            version: version(91, 90, 1, &visible[0].body),
        })
        .expect("save as prompt");
    assert!(session
        .saved
        .iter()
        .any(|prompt| prompt.title == "Saved from history"));

    let cleared = session.clear_history();
    assert_eq!(cleared.removed_history_rows, 500);
    assert_eq!(cleared.removed_task_facts, 0);
    assert_eq!(cleared.removed_saved_prompts, 0);
    assert!(session
        .saved
        .iter()
        .any(|prompt| prompt.title == "Saved from history"));
}

#[test]
fn composer_inserts_exact_payload_without_sending_or_advancing() {
    let session = populated_session();
    let mut draft = ComposerDraft {
        text: "keep prefix ".into(),
        cursor: 12,
        ..ComposerDraft::default()
    };
    let action = put_prompt_version_in_composer(
        task_id(8),
        agent_session_id(9),
        version_id(3),
        ComposerInsertionMode::InsertAtCursor,
        None,
    );
    assert!(!action.sends_provider_input);
    assert!(!action.advances_chain);
    assert!(action_by_id(ACTION_PROMPT_PUT_IN_COMPOSER).is_none());
    assert!(catalog()
        .iter()
        .all(|entry| entry.id != ACTION_PROMPT_PUT_IN_COMPOSER));

    let payload = session.exact_payload(version_id(3)).expect("payload");
    apply_put_prompt_version(&mut draft, &action, &payload).expect("insert");
    assert!(draft.text.starts_with("keep prefix "));
    assert!(draft.text.contains("Markdown list"));
    assert_eq!(
        draft.provenance.as_ref().map(|prov| prov.prompt_version_id),
        Some(version_id(3))
    );

    let mut stale = payload.clone();
    stale.body.push_str(" mutated");
    let mut forbidden = action.clone();
    forbidden.sends_provider_input = true;
    assert_eq!(
        apply_put_prompt_version(&mut draft, &forbidden, &payload),
        Err(PromptLibraryUiError::PayloadMismatch)
    );
    assert_eq!(
        apply_put_prompt_version(&mut draft, &action, &stale),
        Err(PromptLibraryUiError::PayloadMismatch)
    );

    let original = draft.text.clone();
    draft.edit(format!("{original} plus local edits"), original.len());
    assert!(draft.provenance.is_none());
    draft.mark_sent();
    assert!(draft.sent);
}

#[test]
fn slash_prefix_searches_provider_commands_not_library() {
    let catalog = [ProviderCommandSuggestion {
        label: "Review".into(),
        command: "/review".into(),
        provider_kind: "claude".into(),
    }];
    let hits = suggest_provider_commands("/rev", &catalog);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].command, "/review");
    let library = lifecycle_fixture();
    assert!(library
        .prompts
        .iter()
        .all(|prompt| !prompt.title.starts_with('/')));
}

#[test]
fn replace_draft_uses_durable_exact_bytes() {
    let session = populated_session();
    let payload = session.exact_payload(version_id(4)).expect("long payload");
    let expected: [u8; 32] = {
        use sha2::{Digest, Sha256};
        Sha256::digest(payload.body.as_bytes()).into()
    };
    assert_eq!(payload.body_sha256, expected);
    let mut draft = ComposerDraft::default();
    apply_put_prompt_version(
        &mut draft,
        &put_in_composer_action(
            task_id(8),
            agent_session_id(9),
            version_id(4),
            ComposerInsertionMode::ReplaceDraft,
            None,
        ),
        &payload,
    )
    .expect("replace");
    assert_eq!(draft.text, payload.body);
    let exact = ExactPromptPayload::from_version(
        session
            .versions
            .iter()
            .find(|version| version.id == version_id(4))
            .expect("version"),
    );
    assert_eq!(exact.body, draft.text);
}

#[test]
fn history_policy_hides_rows_when_disabled() {
    let mut session = populated_session();
    session.history_policy = PromptHistoryPolicy {
        enabled: false,
        ..PromptHistoryPolicy::default()
    };
    assert!(session.visible_history().is_empty());
}

#[test]
fn virtualizes_five_thousand_prompts_and_two_thousand_links() {
    let mut session = populated_session();
    session.saved = large_prompt_set(MAX_VIRTUALIZED_PROMPTS);
    session
        .apply(PromptLibraryAction::SelectSection(
            LibrarySection::SavedPrompts,
        ))
        .expect("saved");
    let started = Instant::now();
    let list = session.list_state();
    let elapsed = started.elapsed();
    assert_eq!(list.total, MAX_VIRTUALIZED_PROMPTS);
    assert_eq!(list.visible.len(), 80);
    assert!(elapsed.as_micros() < 50_000 || elapsed.as_millis() < 50);

    let mut links = Vec::with_capacity(MAX_VIRTUALIZED_LINKS);
    for index in 0..MAX_VIRTUALIZED_LINKS as u32 {
        let previous = (index > 0).then_some(2_000 + index - 1);
        let next = (index + 1 < MAX_VIRTUALIZED_LINKS as u32).then_some(2_000 + index + 1);
        links.push(chain_link(
            2_000 + index,
            30,
            index + 1,
            11,
            12,
            previous,
            next,
            false,
        ));
    }
    session.links = links;
    let projection = session.chain_projection(chain_id(30)).expect("large chain");
    assert_eq!(projection.total_links, MAX_VIRTUALIZED_LINKS);
    assert_eq!(projection.links.len(), 80);
    let window = virtualize(&session.saved, 4_920, 80);
    assert_eq!(window.visible.len(), 80);
    assert_eq!(window.total, MAX_VIRTUALIZED_PROMPTS);
}

#[test]
fn viewport_matrix_and_lifecycle_fixtures_are_body_free() {
    let viewports = viewport_matrix();
    assert_eq!(viewports.len(), 2 * 2 * 4 * 2 * 4);
    let tokens: Vec<serde_json::Value> = viewports
        .iter()
        .map(|viewport| {
            let tokens = viewport.token_set();
            serde_json::json!({
                "scheme": tokens.scheme,
                "density": tokens.density,
                "scale_percent": tokens.scale_percent,
                "list_width_px": tokens.list_width_px,
                "detail_min_px": tokens.detail_min_px,
                "data": format!("{:?}", viewport.data),
            })
        })
        .collect();
    assert_golden_json(
        fixtures_dir(),
        "ui_viewport_matrix.json",
        &serde_json::Value::Array(tokens),
    );

    let fixture = lifecycle_fixture();
    let manifest = lifecycle_manifest(&fixture);
    let value = serde_json::json!({
        "schema_version": manifest.schema_version,
        "kind": manifest.kind,
        "unicode_title": manifest.unicode_title,
        "version_count": manifest.version_count,
        "history_count": manifest.history_count,
        "chain_count": manifest.chain_count,
        "archived_prompt": manifest.archived_prompt,
        "provider_slash_commands": manifest.provider_slash_commands,
        "five_link_chain": manifest.five_link_chain,
    });
    assert_golden_json(conformance_dir(), "lifecycle_v1.json", &value);
    assert_eq!(manifest.structure_sha256.len(), 64);
    assert!(!value.to_string().contains("Inspect the first draft"));

    let perf = performance_manifest();
    assert_golden_json(
        conformance_dir(),
        "performance_v1.json",
        &serde_json::json!({
            "schema_version": perf.schema_version,
            "kind": perf.kind,
            "prompt_count": perf.prompt_count,
            "link_count": perf.link_count,
            "history_count": perf.history_count,
            "virtualize_window": perf.virtualize_window,
            "fts_on_input_path": perf.fts_on_input_path,
            "declared_virtualize_budget_us": perf.declared_virtualize_budget_us,
        }),
    );
}

#[test]
fn editor_discard_and_restore_by_new_version() {
    let mut session = populated_session();
    session.editor.load(
        session.saved.first().expect("prompt"),
        session.versions.first().expect("version"),
    );
    session
        .apply(PromptLibraryAction::EditPrompt(
            PromptEditorAction::SetBody("unsaved".into()),
        ))
        .expect("dirty");
    session
        .apply(PromptLibraryAction::EditPrompt(
            PromptEditorAction::DiscardUnsaved,
        ))
        .expect("discard");
    assert!(session.editor.confirm_discard);
    session
        .apply(PromptLibraryAction::EditPrompt(
            PromptEditorAction::RestoreByCreatingNewVersion {
                prompt: saved_prompt(1, "Unicode review café", 4, false),
                version: version(70, 1, 4, "restored via new version"),
            },
        ))
        .expect("restore by new version");
    assert!(session
        .versions
        .iter()
        .any(|version| version.id == version_id(70)));
}

use devmanager::domain::{
    PromptChainId, PromptChainLinkId, PromptHistoryId, PromptId, PromptVersionId,
};
use devmanager::prompts::{
    ArchivePrompt, ArchivePromptChain, CreatePrompt, CreatePromptChain, CreatePromptVersion,
    InsertPromptChainLink, MovePromptChainLink, PromptChain, PromptChainCommand, PromptChainEvent,
    PromptChainLink, PromptCommand, PromptValidationError, PromptVersion, RemovePromptChainLink,
    RenamePrompt, RenamePromptChain, RestorePrompt, RestorePromptChain, SetPromptTags,
    UpdatePromptChainLinkVersion, MAX_PROMPT_BODY_BYTES, MAX_PROMPT_CHAIN_DESCRIPTION_SCALARS,
    MAX_PROMPT_CHAIN_LINKS, MAX_PROMPT_CHAIN_TITLE_SCALARS, MAX_PROMPT_DESCRIPTION_SCALARS,
    MAX_PROMPT_PUBLIC_WIRE_BYTES, MAX_PROMPT_TAGS, MAX_PROMPT_TAG_SCALARS,
    MAX_PROMPT_TITLE_SCALARS, MAX_PROMPT_VARIABLES, MAX_PROMPT_VARIABLE_NAME_SCALARS,
};

#[test]
fn public_and_durable_wire_contracts_are_named_separately() {
    assert_eq!(MAX_PROMPT_PUBLIC_WIRE_BYTES, 4 * 1024 * 1024);
    assert_eq!(MAX_PROMPT_CHAIN_LINKS, 2_000);
}

#[test]
fn prompt_ids_are_not_interchangeable() {
    fn accepts_prompt_id(_: PromptId) {}

    accepts_prompt_id(PromptId::new());
    let _ = PromptVersionId::new();
    let _ = PromptChainId::new();
    let _ = PromptChainLinkId::new();
    let _ = PromptHistoryId::new();

    assert_ne!(
        std::any::TypeId::of::<PromptId>(),
        std::any::TypeId::of::<PromptVersionId>()
    );
    assert_ne!(
        std::any::TypeId::of::<PromptId>(),
        std::any::TypeId::of::<PromptChainId>()
    );
}

#[test]
fn prompt_limits_count_scalars_and_bytes() {
    let command = CreatePrompt {
        prompt_id: PromptId::new(),
        prompt_version_id: PromptVersionId::new(),
        title: "x".repeat(MAX_PROMPT_TITLE_SCALARS + 1),
        description: None,
        tags: Vec::new(),
        variables: Vec::new(),
        body: "body".into(),
        created_at_ms: 1,
    };
    assert!(matches!(
        command.validate(),
        Err(PromptValidationError::TitleTooLong { .. })
    ));

    let valid_command = CreatePrompt {
        title: "Prompt".into(),
        ..command.clone()
    };
    let description = "d".repeat(MAX_PROMPT_DESCRIPTION_SCALARS + 1);
    assert!(matches!(
        CreatePrompt {
            description: Some(description),
            ..valid_command.clone()
        }
        .validate(),
        Err(PromptValidationError::DescriptionTooLong { .. })
    ));

    let body = "b".repeat(MAX_PROMPT_BODY_BYTES + 1);
    let body_error = CreatePrompt {
        body,
        ..valid_command
    }
    .validate()
    .unwrap_err();
    assert!(matches!(
        body_error,
        PromptValidationError::BodyTooLarge { .. }
    ));
}

#[test]
fn prompt_tags_are_bounded_and_normalized() {
    let command = CreatePrompt {
        prompt_id: PromptId::new(),
        prompt_version_id: PromptVersionId::new(),
        title: "Prompt".into(),
        description: None,
        tags: (0..=MAX_PROMPT_TAGS)
            .map(|index| format!(" tag-{index} "))
            .collect(),
        variables: Vec::new(),
        body: "body".into(),
        created_at_ms: 1,
    };
    assert!(matches!(
        command.validate(),
        Err(PromptValidationError::TooManyTags { .. })
    ));

    let long_tag = "x".repeat(MAX_PROMPT_TAG_SCALARS + 1);
    let error = SetPromptTags {
        prompt_id: PromptId::new(),
        tags: vec![long_tag],
        expected_revision: 1,
    }
    .validate()
    .unwrap_err();
    assert!(matches!(error, PromptValidationError::TagTooLong { .. }));
}

#[test]
fn prompt_tags_use_printable_lowercase_ascii_policy() {
    let non_ascii = CreatePrompt {
        prompt_id: PromptId::new(),
        prompt_version_id: PromptVersionId::new(),
        title: "Prompt".into(),
        description: None,
        tags: vec!["café".into()],
        variables: Vec::new(),
        body: "body".into(),
        created_at_ms: 1,
    };
    assert!(matches!(
        non_ascii.validate(),
        Err(PromptValidationError::InvalidTag { .. })
    ));

    let normalized = SetPromptTags {
        prompt_id: PromptId::new(),
        tags: vec![" Review-1 ".into()],
        expected_revision: 1,
    };
    assert_eq!(
        normalized.validate().expect("ASCII tags normalize"),
        vec!["review-1"]
    );
}

#[test]
fn prompt_variables_are_bounded_and_normalized() {
    let command = CreatePrompt {
        prompt_id: PromptId::new(),
        prompt_version_id: PromptVersionId::new(),
        title: "Prompt".into(),
        description: None,
        tags: Vec::new(),
        variables: vec![" reviewer ".into(), "reviewer".into()],
        body: "Review {{ reviewer }}".into(),
        created_at_ms: 1,
    };
    assert_eq!(
        command.normalized_variables().expect("variables normalize"),
        vec!["reviewer"]
    );

    let too_many = CreatePrompt {
        variables: (0..=MAX_PROMPT_VARIABLES)
            .map(|index| format!("variable_{index}"))
            .collect(),
        ..command.clone()
    };
    assert!(matches!(
        too_many.validate(),
        Err(PromptValidationError::TooManyVariables { .. })
    ));

    let too_long = CreatePrompt {
        variables: vec!["x".repeat(MAX_PROMPT_VARIABLE_NAME_SCALARS + 1)],
        ..command
    };
    assert!(matches!(
        too_long.validate(),
        Err(PromptValidationError::VariableTooLong { .. })
    ));
}

#[test]
fn prompt_body_debug_output_is_redacted() {
    let sentinel = "private prompt body sentinel";
    let command = CreatePrompt {
        prompt_id: PromptId::new(),
        prompt_version_id: PromptVersionId::new(),
        title: "Prompt".into(),
        description: None,
        tags: Vec::new(),
        variables: Vec::new(),
        body: sentinel.into(),
        created_at_ms: 1,
    };
    assert!(!format!("{command:?}").contains(sentinel));

    let version = PromptVersion::new(
        PromptVersionId::new(),
        command.prompt_id,
        1,
        sentinel.into(),
        1,
    )
    .expect("valid version");
    assert!(!format!("{version:?}").contains(sentinel));
}

#[test]
fn prompt_command_wire_encoding_is_deterministic_and_framed() {
    let command = PromptCommand::CreatePrompt(CreatePrompt {
        prompt_id: PromptId::new(),
        prompt_version_id: PromptVersionId::new(),
        title: "Prompt".into(),
        description: None,
        tags: vec!["rust".into()],
        variables: vec!["reviewer".into()],
        body: "Review this code.".into(),
        created_at_ms: 1,
    });
    let first = command.encode().expect("encode prompt command");
    let second = command.encode().expect("encode prompt command again");
    assert_eq!(first, second);
    assert_eq!(
        PromptCommand::decode(&first).expect("decode prompt command"),
        command
    );
}

#[test]
fn chain_metadata_uses_the_same_unicode_scalar_bounds() {
    let command = CreatePromptChain {
        chain_id: PromptChainId::new(),
        title: "x".repeat(MAX_PROMPT_CHAIN_TITLE_SCALARS + 1),
        description: None,
        created_at_ms: 1,
    };
    assert!(matches!(
        command.validate(),
        Err(PromptValidationError::TitleTooLong { .. })
    ));

    let error = CreatePromptChain {
        description: Some("d".repeat(MAX_PROMPT_CHAIN_DESCRIPTION_SCALARS + 1)),
        title: "Chain".into(),
        ..command
    }
    .validate()
    .unwrap_err();
    assert!(matches!(
        error,
        PromptValidationError::DescriptionTooLong { .. }
    ));
}

#[test]
fn prompt_chain_event_codec_enforces_the_two_thousand_link_cap() {
    let chain_id = PromptChainId::new();
    let links = (0..=MAX_PROMPT_CHAIN_LINKS)
        .map(|position| PromptChainLink {
            id: PromptChainLinkId::new(),
            chain_id,
            position: u32::try_from(position).expect("test position fits u32"),
            prompt_id: PromptId::new(),
            prompt_version_id: PromptVersionId::new(),
        })
        .collect();
    let event = PromptChainEvent::PromptChainLinksReplaced {
        chain_id,
        links,
        revision: 1,
    };
    let payload = event.encode().expect("encode bounded chain event fixture");
    assert!(
        PromptChainEvent::decode(&payload).is_err(),
        "public codec must enforce the SQLite/store 2,000-link cap"
    );
}

#[test]
fn prompt_models_reject_unknown_future_fields() {
    let id = PromptId::new().to_string();
    let error = serde_json::from_value::<devmanager::prompts::SavedPrompt>(serde_json::json!({
        "id": id,
        "title": "Prompt",
        "description": null,
        "tags": [],
        "current_version_id": PromptVersionId::new().to_string(),
        "revision": 1,
        "archived_at_ms": null,
        "future_field": "must not be ignored"
    }))
    .expect_err("unknown prompt fields must fail closed");
    assert!(error.to_string().contains("unknown field"));

    let error = serde_json::from_value::<PromptChainLink>(serde_json::json!({
        "id": PromptChainLinkId::new().to_string(),
        "chain_id": PromptChainId::new().to_string(),
        "position": 0,
        "prompt_id": PromptId::new().to_string(),
        "prompt_version_id": PromptVersionId::new().to_string(),
        "future_selection": "execute"
    }))
    .expect_err("unknown chain fields must fail closed");
    assert!(error.to_string().contains("unknown field"));
}

#[test]
fn public_prompt_serde_rejects_invalid_bounds_and_canonicality() {
    let prompt = serde_json::json!({
        "prompt_id": PromptId::new().to_string(),
        "prompt_version_id": PromptVersionId::new().to_string(),
        "title": "x".repeat(MAX_PROMPT_TITLE_SCALARS + 1),
        "description": null,
        "tags": [],
        "variables": [],
        "body": "body",
        "created_at_ms": 1
    });
    assert!(serde_json::from_value::<CreatePrompt>(prompt).is_err());

    let version = serde_json::json!({
        "id": PromptVersionId::new().to_string(),
        "prompt_id": PromptId::new().to_string(),
        "version": 1,
        "body": "body",
        "variables": [" reviewer "],
        "body_sha256": vec![0; 32],
        "created_at_ms": 1
    });
    assert!(serde_json::from_value::<PromptVersion>(version).is_err());

    let saved = serde_json::json!({
        "id": PromptId::new().to_string(),
        "title": " Prompt ",
        "description": Some(" description "),
        "tags": [" Review "],
        "current_version_id": PromptVersionId::new().to_string(),
        "revision": 1,
        "archived_at_ms": null
    });
    assert!(serde_json::from_value::<devmanager::prompts::SavedPrompt>(saved).is_err());

    let chain = serde_json::json!({
        "id": PromptChainId::new().to_string(),
        "title": " Chain ",
        "description": " description ",
        "revision": 1,
        "archived_at_ms": null
    });
    assert!(serde_json::from_value::<devmanager::prompts::PromptChain>(chain).is_err());
}

#[test]
fn prompt_command_decode_rejects_invalid_bounds_and_canonicality() {
    let oversized = PromptCommand::CreatePrompt(CreatePrompt {
        prompt_id: PromptId::new(),
        prompt_version_id: PromptVersionId::new(),
        title: "x".repeat(MAX_PROMPT_TITLE_SCALARS + 1),
        description: None,
        tags: Vec::new(),
        variables: Vec::new(),
        body: "body".into(),
        created_at_ms: 1,
    });
    let payload = oversized.encode().expect("encode oversized command");
    assert!(PromptCommand::decode(&payload).is_err());

    let uncanonical = PromptCommand::CreatePrompt(CreatePrompt {
        prompt_id: PromptId::new(),
        prompt_version_id: PromptVersionId::new(),
        title: " Prompt ".into(),
        description: Some(" description ".into()),
        tags: vec![" review ".into()],
        variables: vec![" reviewer ".into()],
        body: "body".into(),
        created_at_ms: 1,
    });
    let payload = uncanonical.encode().expect("encode uncanonical command");
    assert!(PromptCommand::decode(&payload).is_err());
}

#[test]
fn prompt_command_decode_rejects_noncanonical_wire_bytes() {
    let command = PromptCommand::CreatePrompt(CreatePrompt {
        prompt_id: PromptId::new(),
        prompt_version_id: PromptVersionId::new(),
        title: "Prompt".into(),
        description: None,
        tags: Vec::new(),
        variables: Vec::new(),
        body: "body".into(),
        created_at_ms: 1,
    });
    let command_payload = rmp_serde::to_vec_named(&command).expect("encode command body");
    let mut payload = Vec::new();
    rmp::encode::write_map_len(&mut payload, 2).expect("write command wire map");
    rmp::encode::write_str(&mut payload, "command").expect("write command key");
    payload.extend_from_slice(&command_payload);
    rmp::encode::write_str(&mut payload, "schema_version").expect("write schema key");
    rmp::encode::write_uint(&mut payload, 1).expect("write schema version");

    assert!(PromptCommand::decode(&payload).is_err());
}

#[test]
fn public_prompt_and_chain_mutations_reject_zero_expected_revision() {
    let prompt_id = devmanager::domain::PromptId::new();
    let version_id = PromptVersionId::new();
    let chain_id = PromptChainId::new();
    let link_id = PromptChainLinkId::new();

    let prompt_commands = [
        PromptCommand::CreatePromptVersion(CreatePromptVersion {
            prompt_id,
            prompt_version_id: version_id,
            variables: Vec::new(),
            body: "body".into(),
            created_at_ms: 1,
            expected_revision: 0,
        }),
        PromptCommand::RenamePrompt(RenamePrompt {
            prompt_id,
            title: "Prompt".into(),
            expected_revision: 0,
        }),
        PromptCommand::SetPromptTags(SetPromptTags {
            prompt_id,
            tags: Vec::new(),
            expected_revision: 0,
        }),
        PromptCommand::ArchivePrompt(ArchivePrompt {
            prompt_id,
            archived_at_ms: 1,
            expected_revision: 0,
        }),
        PromptCommand::RestorePrompt(RestorePrompt {
            prompt_id,
            expected_revision: 0,
        }),
    ];
    assert!(prompt_commands
        .iter()
        .all(|command| command.validate().is_err()));

    let chain_commands = [
        PromptChainCommand::RenamePromptChain(RenamePromptChain {
            chain_id,
            title: "Chain".into(),
            expected_revision: 0,
        }),
        PromptChainCommand::InsertPromptChainLink(InsertPromptChainLink {
            chain_id,
            link_id,
            prompt_id,
            prompt_version_id: None,
            before_link_id: None,
            expected_revision: 0,
        }),
        PromptChainCommand::MovePromptChainLink(MovePromptChainLink {
            chain_id,
            link_id,
            before_link_id: None,
            expected_revision: 0,
        }),
        PromptChainCommand::RemovePromptChainLink(RemovePromptChainLink {
            chain_id,
            link_id,
            expected_revision: 0,
        }),
        PromptChainCommand::UpdatePromptChainLinkVersion(UpdatePromptChainLinkVersion {
            chain_id,
            link_id,
            expected_revision: 0,
        }),
        PromptChainCommand::ArchivePromptChain(ArchivePromptChain {
            chain_id,
            archived_at_ms: 1,
            expected_revision: 0,
        }),
        PromptChainCommand::RestorePromptChain(RestorePromptChain {
            chain_id,
            expected_revision: 0,
        }),
    ];
    assert!(chain_commands
        .iter()
        .all(|command| command.validate().is_err()));
}

#[test]
fn prompt_chain_event_decode_rejects_duplicate_links_and_noncanonical_bytes() {
    let chain_id = PromptChainId::new();
    let duplicate_link_id = PromptChainLinkId::new();
    let duplicate = PromptChainEvent::PromptChainLinksReplaced {
        chain_id,
        links: vec![
            PromptChainLink {
                id: duplicate_link_id,
                chain_id,
                position: 0,
                prompt_id: devmanager::domain::PromptId::new(),
                prompt_version_id: PromptVersionId::new(),
            },
            PromptChainLink {
                id: duplicate_link_id,
                chain_id,
                position: 1,
                prompt_id: devmanager::domain::PromptId::new(),
                prompt_version_id: PromptVersionId::new(),
            },
        ],
        revision: 2,
    };
    let duplicate_payload = duplicate.encode().expect("encode duplicate links");
    assert!(PromptChainEvent::decode(&duplicate_payload).is_err());

    let event = PromptChainEvent::PromptChainCreated {
        chain: PromptChain {
            id: chain_id,
            title: "Chain".into(),
            description: None,
            revision: 1,
            archived_at_ms: None,
        },
    };
    let event_body = rmp_serde::to_vec_named(&event).expect("encode chain event body");
    let mut noncanonical = Vec::new();
    rmp::encode::write_map_len(&mut noncanonical, 2).expect("write event wire map");
    rmp::encode::write_str(&mut noncanonical, "event").expect("write event key");
    noncanonical.extend_from_slice(&event_body);
    rmp::encode::write_str(&mut noncanonical, "schema_version").expect("write schema key");
    rmp::encode::write_uint(&mut noncanonical, 1).expect("write schema version");
    assert!(PromptChainEvent::decode(&noncanonical).is_err());
}

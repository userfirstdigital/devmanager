use devmanager::domain::{
    PromptChainId, PromptChainLinkId, PromptHistoryId, PromptId, PromptVersionId,
};
use devmanager::prompts::{
    CreatePrompt, CreatePromptChain, PromptChainLink, PromptCommand, PromptValidationError,
    PromptVersion, SetPromptTags, MAX_PROMPT_BODY_BYTES, MAX_PROMPT_CHAIN_DESCRIPTION_SCALARS,
    MAX_PROMPT_CHAIN_TITLE_SCALARS, MAX_PROMPT_DESCRIPTION_SCALARS, MAX_PROMPT_TAGS,
    MAX_PROMPT_TAG_SCALARS, MAX_PROMPT_TITLE_SCALARS, MAX_PROMPT_VARIABLES,
    MAX_PROMPT_VARIABLE_NAME_SCALARS,
};

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

use devmanager::prompts::search::PromptSearchSource;
use devmanager::prompts::{
    PromptHistoryErrorCode, PromptSearchQuery, ValidatedDeliveredInputProof,
};

#[test]
fn production_provider_input_wiring_is_unavailable() {
    let error = ValidatedDeliveredInputProof::from_provider_input_settlement()
        .expect_err("provider input is not in this base");
    assert_eq!(
        error.code(),
        PromptHistoryErrorCode::ProviderInputUnavailable
    );
    let rendered = format!("{error} {error:?}");
    assert!(!rendered.contains("sqlite"));
    assert!(!rendered.contains("SELECT"));
    assert!(!rendered.contains("prompt"));
    assert!(!rendered.contains('\\') && !rendered.contains('/'));
}

#[test]
fn production_proof_debug_is_redacted() {
    let error =
        ValidatedDeliveredInputProof::from_provider_input_settlement().expect_err("unavailable");
    assert!(!format!("{error:?}").contains("body"));
}

#[test]
fn production_search_query_debug_is_redacted() {
    let query = PromptSearchQuery {
        text: "UNIQUE_SECRET_QUERY_TEXT".into(),
        source: PromptSearchSource::History,
        cursor: None,
        page_size: 10,
    };
    let rendered = format!("{query:?}");
    assert!(!rendered.contains("UNIQUE_SECRET_QUERY_TEXT"));
    assert!(!rendered.contains("UNIQUE_SECRET"));
    assert!(!rendered.contains('\\') && !rendered.contains('/'));
}

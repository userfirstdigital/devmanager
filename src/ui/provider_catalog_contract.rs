//! Source-contract proof that the TypeScript catalog mirror matches Rust ownership.

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::providers::ProviderKind;
    use crate::ui::provider_catalog::{catalog_entry_count, catalog_reviewed_at};
    use crate::ui::provider_catalog_seeds::{CLAUDE_SEEDS, CODEX_SEEDS};

    #[test]
    fn typescript_mirror_matches_rust_const_seed_owner() {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let seeds = manifest.join("src/ui/provider_catalog_seeds.rs");
        let generated = manifest.join("web/src/tasks/commands/builtinCatalog.generated.ts");
        let shim = manifest.join("web/src/tasks/commands/builtinCatalog.ts");
        assert!(seeds.is_file(), "Rust const seed module must exist");
        assert!(generated.is_file(), "TypeScript mirror must exist");
        let seeds_source = std::fs::read_to_string(&seeds).expect("seeds");
        let generated_source = std::fs::read_to_string(&generated).expect("generated");
        let shim_source = std::fs::read_to_string(&shim).expect("shim");
        assert!(
            shim_source.contains("builtinCatalog.generated"),
            "builtinCatalog.ts must re-export the generated mirror, not own seeds"
        );
        assert!(
            !shim_source.contains("const CLAUDE_SEEDS"),
            "shim must not reintroduce scraped seed tables"
        );
        assert!(
            generated_source.contains("from src/ui/provider_catalog_seeds.rs"),
            "generated mirror must cite the Rust const owner"
        );
        assert!(
            generated_source.contains(catalog_reviewed_at()),
            "generated reviewed_at must match Rust const"
        );
        assert_eq!(
            catalog_entry_count(&ProviderKind::ClaudeCode),
            CLAUDE_SEEDS.len()
        );
        assert_eq!(catalog_entry_count(&ProviderKind::Codex), CODEX_SEEDS.len());
        let generated_claude = generated_source.matches("[\"/").count();
        // Each seed emits at least one tuple; aliases stay inside options.
        assert!(
            generated_claude >= CLAUDE_SEEDS.len() + CODEX_SEEDS.len(),
            "generated TS must include every Rust seed row without silent fallback"
        );
        assert!(
            !seeds_source.contains("serde_json::from_str"),
            "Rust seed owner must not parse JSON at runtime"
        );
    }

    #[test]
    fn mismatch_contract_rejects_empty_silent_catalog() {
        assert!(CLAUDE_SEEDS.len() >= 90);
        assert!(CODEX_SEEDS.len() >= 40);
        assert!(!catalog_reviewed_at().is_empty());
    }
}

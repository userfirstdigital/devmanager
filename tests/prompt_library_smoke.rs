//! Bounded Phase 7 prompt-library smoke against current public APIs.
//!
//! Fixture/local verification only. This test never launches a provider, never
//! sends, and never advances a chain. It does not read or write production
//! `config.json`, `remote.json`, or `session.json`.

use std::fs;
use std::path::{Path, PathBuf};

use devmanager::connect::{ConnectHostId, OrganizationPromptAdapter, OrganizationPromptSnapshot};
use devmanager::domain::{CommandId, PromptChainId, PromptChainLinkId, PromptId, PromptVersionId};
use devmanager::org::{
    ExternalAccount, HostMembership, MembershipRole, OrgError, OrganizationPolicyDocument,
    PortalAccountId, PortalTenantId, SyncOutcome,
};
use devmanager::prompts::{
    diff_versions, ComposerInsertion, CreatePrompt, CreatePromptChain, CreatePromptVersion,
    DiffStatus, InsertPromptChainLink, LineChangeKind, OrgPrompt, PromptChainCommand,
    PromptChainService, PromptCommand, PromptLifecycle, PromptStore, PromptVersion,
    ORG_PROMPT_CACHE_TTL_MS,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SmokeFixture {
    schema_version: u32,
    title: String,
    description: String,
    tags: Vec<String>,
    v1_body: String,
    v2_body: String,
    chain_title: String,
    org_namespace: String,
    org_name: String,
    org_body: String,
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("prompts")
        .join("smoke")
        .join("local-library.json")
}

fn load_fixture() -> SmokeFixture {
    let path = fixture_path();
    assert!(
        path.starts_with(env!("CARGO_MANIFEST_DIR")),
        "smoke fixture escaped the worktree"
    );
    let raw = fs::read_to_string(&path).expect("read smoke fixture");
    let fixture: SmokeFixture = serde_json::from_str(&raw).expect("parse smoke fixture");
    assert_eq!(fixture.schema_version, 1);
    fixture
}

fn reject_production_profile_env() {
    let profile = std::env::var("DEVMANAGER_PROFILE").unwrap_or_default();
    if profile.is_empty() {
        return;
    }
    let normalized = profile.trim().to_ascii_lowercase();
    let forbidden = [
        "production",
        "installed",
        "default",
        "unprofiled",
        "com.userfirst.devmanager",
    ];
    assert!(
        !forbidden.iter().any(|name| normalized.contains(name)),
        "refusing production/installed DEVMANAGER_PROFILE"
    );
}

fn isolated_store_path(root: &TempDir) -> PathBuf {
    let profile = root.path().join("isolated-profile");
    fs::create_dir_all(&profile).expect("create isolated profile root");
    if let Ok(appdata) = std::env::var("APPDATA") {
        if !appdata.trim().is_empty() {
            let production = Path::new(&appdata).join("com.userfirst.devmanager");
            assert!(
                !profile.starts_with(&production),
                "isolated profile must not resolve under production"
            );
        }
    }
    profile.join("prompts.sqlite3")
}

fn hash_body(body: &str) -> String {
    let digest = Sha256::digest(body.as_bytes());
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn enroll_owner() -> (HostMembership, OrganizationPolicyDocument) {
    let tenant = PortalTenantId::parse("acme-smoke").expect("tenant");
    let policy = OrganizationPolicyDocument::deny_minimal(tenant.clone()).expect("policy");
    let account = ExternalAccount::new(
        tenant,
        PortalAccountId::parse("owner-smoke").expect("account"),
        None,
    );
    let mut membership = HostMembership::pending(
        ConnectHostId::new(),
        account,
        MembershipRole::Owner,
        &policy,
        "prompt-smoke-owner",
    )
    .expect("pending owner");
    membership
        .confirm_locally(1_000, &policy)
        .expect("confirm owner");
    (membership, policy)
}

fn enroll_member(policy: &OrganizationPolicyDocument) -> HostMembership {
    let account = ExternalAccount::new(
        policy.tenant_id.clone(),
        PortalAccountId::parse("member-smoke").expect("account"),
        None,
    );
    let mut membership = HostMembership::pending(
        ConnectHostId::new(),
        account,
        MembershipRole::Member,
        policy,
        "prompt-smoke-member",
    )
    .expect("pending member");
    membership
        .confirm_locally(1_000, policy)
        .expect("confirm member");
    membership
}

#[test]
fn phase7_prompt_library_smoke_public_api_contract() {
    reject_production_profile_env();
    let fixture = load_fixture();
    let isolated = TempDir::new().expect("unique tempfile isolated profile");
    let db_path = isolated_store_path(&isolated);
    let mut store = PromptStore::open(&db_path).expect("open isolated prompt store");

    let prompt_id = PromptId::new();
    let version_one = PromptVersionId::new();
    let version_two = PromptVersionId::new();
    store
        .execute(
            CommandId::new(),
            PromptCommand::CreatePrompt(CreatePrompt {
                prompt_id,
                prompt_version_id: version_one,
                title: fixture.title.clone(),
                description: Some(fixture.description.clone()),
                tags: fixture.tags.clone(),
                variables: Vec::new(),
                body: fixture.v1_body.clone(),
                created_at_ms: 1_725_000_000_000,
            }),
        )
        .expect("create local prompt");
    store
        .execute(
            CommandId::new(),
            PromptCommand::CreatePromptVersion(CreatePromptVersion {
                prompt_id,
                prompt_version_id: version_two,
                variables: Vec::new(),
                body: fixture.v2_body.clone(),
                created_at_ms: 1_725_000_000_001,
                expected_revision: 1,
            }),
        )
        .expect("create next immutable version");

    let current = store
        .get_prompt(prompt_id)
        .expect("load prompt")
        .expect("prompt");
    assert_eq!(current.current_version_id, version_two);
    assert_eq!(current.revision, 2);
    let first = store
        .get_version(version_one)
        .expect("load v1")
        .expect("v1");
    let second = store
        .get_version(version_two)
        .expect("load v2")
        .expect("v2");
    assert_eq!(first.version, 1);
    assert_eq!(second.version, 2);
    assert_eq!(first.body, fixture.v1_body);
    assert_eq!(second.body, fixture.v2_body);
    assert_eq!(
        first.body_sha256,
        <[u8; 32]>::from(Sha256::digest(first.body.as_bytes()))
    );
    assert_eq!(
        second.body_sha256,
        <[u8; 32]>::from(Sha256::digest(second.body.as_bytes()))
    );

    let diff = diff_versions(&first.body, &second.body);
    assert_eq!(diff.status(), DiffStatus::Complete);
    assert_eq!(diff.old_body_sha256(), &first.body_sha256);
    assert_eq!(diff.new_body_sha256(), &second.body_sha256);
    assert!(diff.hunks().iter().any(|hunk| {
        hunk.changes.iter().any(|change| {
            change.kind() == LineChangeKind::Added
                && change.text_sha256()
                    == <[u8; 32]>::from(Sha256::digest(b"Record the exact version hash."))
        })
    }));

    let chain_id = PromptChainId::new();
    let first_link = PromptChainLinkId::new();
    let second_link = PromptChainLinkId::new();
    let inserted_link = PromptChainLinkId::new();
    {
        let mut chains = PromptChainService::new(&mut store);
        chains
            .apply(
                CommandId::new(),
                PromptChainCommand::CreatePromptChain(CreatePromptChain {
                    chain_id,
                    title: fixture.chain_title.clone(),
                    description: Some("Read-only next/previous; no automatic execution".into()),
                    created_at_ms: 2,
                }),
            )
            .expect("create manual chain");
        chains
            .apply(
                CommandId::new(),
                PromptChainCommand::InsertPromptChainLink(InsertPromptChainLink {
                    chain_id,
                    link_id: first_link,
                    prompt_id,
                    prompt_version_id: Some(version_one),
                    before_link_id: None,
                    expected_revision: 1,
                }),
            )
            .expect("append first link");
        chains
            .apply(
                CommandId::new(),
                PromptChainCommand::InsertPromptChainLink(InsertPromptChainLink {
                    chain_id,
                    link_id: second_link,
                    prompt_id,
                    prompt_version_id: Some(version_two),
                    before_link_id: None,
                    expected_revision: 2,
                }),
            )
            .expect("append second link");
        chains
            .apply(
                CommandId::new(),
                PromptChainCommand::InsertPromptChainLink(InsertPromptChainLink {
                    chain_id,
                    link_id: inserted_link,
                    prompt_id,
                    prompt_version_id: Some(version_one),
                    before_link_id: Some(second_link),
                    expected_revision: 3,
                }),
            )
            .expect("insert between two links");
    }

    let chains = PromptChainService::new(&mut store);
    let links = chains.links(chain_id).expect("list links");
    assert_eq!(
        links.iter().map(|link| link.id()).collect::<Vec<_>>(),
        vec![first_link, inserted_link, second_link]
    );
    assert_eq!(
        links.iter().map(|link| link.position()).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    let head = chains
        .link_context(chain_id, first_link)
        .expect("head context")
        .expect("head");
    assert_eq!(head.previous_link_id, None);
    assert_eq!(head.next_link_id, Some(inserted_link));
    let middle = chains
        .link_context(chain_id, inserted_link)
        .expect("middle context")
        .expect("middle");
    assert_eq!(middle.previous_link_id, Some(first_link));
    assert_eq!(middle.next_link_id, Some(second_link));
    let tail = chains
        .link_context(chain_id, second_link)
        .expect("tail context")
        .expect("tail");
    assert_eq!(tail.previous_link_id, Some(inserted_link));
    assert_eq!(tail.next_link_id, None);

    let exact: PromptVersion = chains
        .version(version_two)
        .expect("exact version payload")
        .expect("selected version");
    assert_eq!(exact.id, version_two);
    assert_eq!(exact.body, fixture.v2_body);
    drop(chains);
    let links_after_select = PromptChainService::new(&mut store)
        .links(chain_id)
        .expect("links after exact select");
    assert_eq!(
        links_after_select
            .iter()
            .map(|link| link.position())
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(
        PromptChainService::new(&mut store)
            .chain(chain_id)
            .expect("chain after select")
            .expect("chain")
            .revision,
        4
    );

    let local_count_before = store.count_prompts().expect("local prompt count");
    let (owner, policy) = enroll_owner();
    let mut org = devmanager::prompts::OrganizationPromptProjection::new();
    let published = org
        .publish(
            &owner,
            &policy,
            owner.tenant_id.clone(),
            fixture.org_namespace.clone(),
            fixture.org_name.clone(),
            fixture.title.clone(),
            fixture.tags.clone(),
            fixture.org_body.clone(),
            1_000,
        )
        .expect("owner publish");
    assert_eq!(
        org.mutate_old_version(published.version_id, "mutated"),
        Err(OrgError::ImmutableVersion)
    );
    let superseded = org
        .supersede(
            &owner,
            &policy,
            published.version_id,
            "Superseding body stays a new immutable version.",
            1_100,
        )
        .expect("supersede");
    assert_eq!(
        org.version(published.version_id)
            .expect("immutable published version")
            .body,
        fixture.org_body
    );
    assert_ne!(superseded.version_id, published.version_id);

    let member = enroll_member(&policy);
    assert_eq!(
        org.publish(
            &member,
            &policy,
            member.tenant_id.clone(),
            "ops",
            "denied",
            "Denied",
            Vec::new(),
            "member cannot publish",
            1_200,
        )
        .expect_err("member publish denied"),
        OrgError::RoleDenied
    );

    org.cache_version(&owner, published.clone(), 1_000, 10_000)
        .expect("cache entitled version");
    assert_eq!(
        org.read_cached_body(published.version_id, 1_500)
            .expect("live cache"),
        fixture.org_body
    );
    assert_eq!(
        org.read_cached_body(
            published.version_id,
            1_000 + i64::try_from(ORG_PROMPT_CACHE_TTL_MS).expect("ttl")
        )
        .expect_err("ttl expired"),
        OrgError::Expired
    );
    org.cache_version(&owner, published.clone(), 2_000, 2_500)
        .expect("cache short entitlement");
    assert_eq!(
        org.read_cached_body(published.version_id, 2_600)
            .expect_err("entitlement expired"),
        OrgError::Expired
    );
    org.cache_version(&owner, published.clone(), 3_000, 8_000)
        .expect("cache for composer");
    let insertion = org
        .put_in_composer(published.version_id, 3_500)
        .expect("org composer insertion");
    assert_eq!(
        insertion,
        ComposerInsertion {
            version_id: published.version_id,
            body: fixture.org_body.clone(),
            sent: false,
            advanced: false,
        }
    );

    let mut adapter = OrganizationPromptAdapter::new();
    let snapshot = OrganizationPromptSnapshot {
        tenant_id: owner.tenant_id.clone(),
        revision: 1,
        prompts: vec![OrgPrompt {
            prompt_id: published.prompt_id,
            tenant_id: owner.tenant_id.clone(),
            namespace: fixture.org_namespace.clone(),
            name: fixture.org_name.clone(),
            current_version_id: published.version_id,
            lifecycle: PromptLifecycle::Published,
        }],
        versions: vec![published.clone()],
        chains: Vec::new(),
    };
    assert_eq!(
        adapter
            .sync_snapshot(&owner, snapshot.clone(), 1_000, 10_000)
            .expect("adapter sync"),
        SyncOutcome::Applied
    );
    let adapter_insert = adapter
        .put_in_composer(published.version_id, 1_500)
        .expect("adapter composer");
    assert!(!adapter_insert.sent && !adapter_insert.advanced);
    assert_eq!(adapter_insert.body, fixture.org_body);
    assert_eq!(
        adapter.mutate_old_version(published.version_id, "nope"),
        Err(OrgError::ImmutableVersion)
    );

    let mut denied = snapshot.clone();
    denied.revision = 0;
    assert_eq!(
        adapter
            .sync_snapshot(&owner, denied, 1_000, 10_000)
            .expect_err("revision 0 snapshot"),
        OrgError::StalePolicy
    );
    let mut foreign = snapshot.clone();
    foreign.tenant_id = PortalTenantId::parse("other-tenant").expect("foreign tenant");
    foreign.revision = 2;
    assert_eq!(
        adapter
            .sync_snapshot(&owner, foreign, 1_000, 10_000)
            .expect_err("cross-tenant snapshot"),
        OrgError::CrossTenant
    );
    let mut invalid_hash = snapshot;
    invalid_hash.revision = 2;
    invalid_hash.versions[0].content_hash_hex = hash_body("not-the-body");
    assert_eq!(
        adapter
            .sync_snapshot(&owner, invalid_hash, 1_000, 10_000)
            .expect_err("invalid snapshot hash"),
        OrgError::ImmutableVersion
    );

    assert_eq!(
        store.count_prompts().expect("local count after org sync"),
        local_count_before
    );
    let local_after = store
        .get_prompt(prompt_id)
        .expect("local prompt after org")
        .expect("still present");
    assert_eq!(local_after.current_version_id, version_two);
    assert_eq!(local_after.revision, 2);
    assert!(store
        .get_version(version_one)
        .expect("v1 still local")
        .is_some());
}

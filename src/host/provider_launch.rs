//! Host-side resolution of provider instance settings into one immutable
//! probe/launch context. Always installs validated host config fields.
//!
//! Binding persistence is deferred until after a trusted exact start/resume
//! succeeds — failed launches leave no phantom binding.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

use crate::domain::TaskId;
use crate::providers::adapter::ProviderLaunchOptions;
use crate::providers::registry::ProviderDiscoveryConfig;
use crate::providers::session::ProviderLaunchSpec;
use crate::providers::settings::{
    default_instance_id_for_kind, normalize_model_slug, prepare_codex_shadow_home,
    resolve_launch_config, ProviderInstanceBinding, ProviderInstanceBindingError,
    ProviderInstanceScope, ProviderProfileOwner, ProviderSettingsDocument,
};
use crate::providers::ProviderKind;

/// Binding action deferred until after trusted exact start/resume succeeds.
#[derive(Clone, Debug)]
pub enum DeferredProviderBinding {
    /// First launch: persist binding only after successful start.
    FirstLaunch {
        task_id: TaskId,
        instance_id: String,
        driver: String,
        launch_identity_fingerprint: String,
    },
    /// Legacy Unbound resume: upgrade binding only after trusted exact resume.
    LegacyUpgrade {
        task_id: TaskId,
        instance_id: String,
        driver: String,
        launch_identity_fingerprint: String,
    },
}

#[derive(Clone)]
pub struct ResolvedHostProviderLaunch {
    pub scope: ProviderInstanceScope,
    pub discovery: ProviderDiscoveryConfig,
    pub environment: BTreeMap<OsString, OsString>,
    pub launch_options: ProviderLaunchOptions,
    pub instance_id: String,
    /// Present when persistence must wait for trusted start/resume success.
    pub deferred_binding: Option<DeferredProviderBinding>,
}

impl fmt::Debug for ResolvedHostProviderLaunch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedHostProviderLaunch")
            .field("scope", &self.scope)
            .field("discovery", &self.discovery)
            .field("environment_entry_count", &self.environment.len())
            .field("launch_options", &self.launch_options)
            .field("instance_id", &self.instance_id)
            .field("deferred_binding", &self.deferred_binding)
            .finish()
    }
}

/// Persist a deferred binding after trusted exact start/resume succeeded.
pub fn commit_deferred_provider_binding(
    owner: &ProviderProfileOwner,
    deferred: &DeferredProviderBinding,
) -> Result<ProviderInstanceBinding, String> {
    let document = owner.settings.snapshot();
    let (task_id, instance_id, driver, fingerprint) = match deferred {
        DeferredProviderBinding::FirstLaunch {
            task_id,
            instance_id,
            driver,
            launch_identity_fingerprint,
        }
        | DeferredProviderBinding::LegacyUpgrade {
            task_id,
            instance_id,
            driver,
            launch_identity_fingerprint,
        } => (
            *task_id,
            instance_id.as_str(),
            driver.as_str(),
            Some(launch_identity_fingerprint.clone()),
        ),
    };
    owner
        .bindings
        .bind_on_first_launch(&task_id, instance_id, driver, fingerprint, &document)
        .map_err(|error| error.to_string())
}

/// Resolve instance policy for a task launch/resume. Fail-closed on missing,
/// disabled, stub, changed, or ambiguous bindings. Never falls back fresh.
pub fn resolve_host_provider_launch(
    owner: &ProviderProfileOwner,
    task_id: TaskId,
    provider_kind: ProviderKind,
    mut launch_options: ProviderLaunchOptions,
    for_resume: bool,
    legacy_launch: Option<&ProviderLaunchSpec>,
) -> Result<ResolvedHostProviderLaunch, String> {
    let document = owner.settings.snapshot();

    let (instance_id, deferred_binding) = if for_resume {
        resolve_resume_instance(&owner, &document, task_id, provider_kind, legacy_launch)?
    } else if let Some(existing) = owner.bindings.get(&task_id) {
        // Bound task: honor existing instance; refuse silent provider switch.
        if let Some(explicit) = launch_options
            .provider_instance_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if explicit != existing.instance_id {
                return Err(format!(
                    "task is bound to provider instance `{}`; refusing `{}`",
                    existing.instance_id, explicit
                ));
            }
        }
        (existing.instance_id, None)
    } else if let Some(explicit) = launch_options
        .provider_instance_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let instance = document
            .require_enabled_launchable(explicit)
            .map_err(|error| error.to_string())?;
        (
            explicit.to_string(),
            Some(DeferredProviderBinding::FirstLaunch {
                task_id,
                instance_id: explicit.to_string(),
                driver: instance.driver.as_str().to_string(),
                launch_identity_fingerprint: instance.launch_identity_fingerprint(),
            }),
        )
    } else {
        let default_id = default_instance_id_for_kind(provider_kind).to_string();
        let instance = document
            .require_enabled_launchable(&default_id)
            .map_err(|error| error.to_string())?;
        (
            default_id.clone(),
            Some(DeferredProviderBinding::FirstLaunch {
                task_id,
                instance_id: default_id,
                driver: instance.driver.as_str().to_string(),
                launch_identity_fingerprint: instance.launch_identity_fingerprint(),
            }),
        )
    };

    let instance = document
        .require_enabled_launchable(&instance_id)
        .map_err(|error| error.to_string())?;
    if instance
        .driver
        .to_provider_kind()
        .is_none_or(|kind| kind != provider_kind)
    {
        return Err(format!(
            "provider instance `{instance_id}` does not match requested provider"
        ));
    }

    // Existing binding: verify fingerprint still matches (changed identity refuses).
    if deferred_binding.is_none() {
        if let Some(binding) = owner.bindings.get(&task_id) {
            owner
                .bindings
                .require_binding_for_resume(&task_id, &document)
                .map_err(|error| error.to_string())?;
            if let Some(expected) = binding.launch_identity_fingerprint.as_deref() {
                if !instance.matches_launch_identity_fingerprint(expected) {
                    return Err(format!(
                        "provider instance `{instance_id}` launch identity changed; refusing resume"
                    ));
                }
            }
        }
    }

    let scope = owner.settings.custody_scope_for_instance(&instance_id);
    // Always install host-validated fields; never preserve caller bypass args.
    let selected_model = match launch_options.custom_model_slug.take() {
        Some(raw) => Some(normalize_model_slug(&raw).map_err(|e| e.to_string())?),
        None => launch_options.model.cli_name().map(str::to_string),
    };
    let mut resolved =
        resolve_launch_config(instance, &scope, selected_model).map_err(|e| e.to_string())?;

    if let (Some(home), Some(shadow)) = (
        resolved.home_path.as_deref(),
        resolved.shadow_home_path.as_deref(),
    ) {
        // Shared-home links only. Effective CODEX_HOME was sealed as shadow in
        // resolve_launch_config before discovery/commitment.
        prepare_codex_shadow_home(home, shadow).map_err(|e| e.to_string())?;
    }

    launch_options.provider_instance_id = Some(instance_id.clone());
    launch_options.extra_launch_args = resolved.extra_launch_args.clone();
    launch_options.custom_model_slug = resolved.selected_model.clone();

    Ok(ResolvedHostProviderLaunch {
        scope: resolved.scope,
        discovery: resolved.discovery,
        environment: resolved.environment,
        launch_options,
        instance_id,
        deferred_binding,
    })
}

fn resolve_resume_instance(
    owner: &ProviderProfileOwner,
    document: &ProviderSettingsDocument,
    task_id: TaskId,
    provider_kind: ProviderKind,
    legacy_launch: Option<&ProviderLaunchSpec>,
) -> Result<(String, Option<DeferredProviderBinding>), String> {
    match owner
        .bindings
        .require_binding_for_resume(&task_id, document)
    {
        Ok(binding) => Ok((binding.instance_id, None)),
        Err(ProviderInstanceBindingError::Unbound(_)) => {
            let candidate = match_legacy_unbound(document, task_id, provider_kind, legacy_launch)?;
            Ok((
                candidate.instance_id.clone(),
                Some(DeferredProviderBinding::LegacyUpgrade {
                    task_id,
                    instance_id: candidate.instance_id,
                    driver: candidate.driver,
                    launch_identity_fingerprint: candidate.launch_identity_fingerprint,
                }),
            ))
        }
        Err(error) => Err(error.to_string()),
    }
}

struct LegacyMatch {
    instance_id: String,
    driver: String,
    launch_identity_fingerprint: String,
}

/// Match Unbound legacy resume against one unambiguous default/custom instance.
/// Does not persist. Foreign home for empty canonical default is rejected.
fn match_legacy_unbound(
    document: &ProviderSettingsDocument,
    task_id: TaskId,
    provider_kind: ProviderKind,
    legacy_launch: Option<&ProviderLaunchSpec>,
) -> Result<LegacyMatch, String> {
    let Some(legacy) = legacy_launch else {
        return Err(format!(
            "task `{task_id}` has no provider instance binding and no legacy launch context"
        ));
    };
    if legacy.provider_kind() != provider_kind {
        return Err("legacy launch provider kind mismatch".into());
    }
    let home_key = match provider_kind {
        ProviderKind::ClaudeCode => "CLAUDE_CONFIG_DIR",
        ProviderKind::Codex => "CODEX_HOME",
        _ => "",
    };
    let legacy_home = if home_key.is_empty() {
        None
    } else {
        legacy.environment().get(&OsString::from(home_key)).cloned()
    };
    let candidates: Vec<_> = document
        .instances
        .iter()
        .filter(|instance| {
            instance
                .driver
                .to_provider_kind()
                .is_some_and(|kind| kind == provider_kind)
                && instance.enabled
                && !instance.driver.is_stub()
        })
        .collect();
    let default_id = default_instance_id_for_kind(provider_kind);
    let mut matches = Vec::new();
    for instance in &candidates {
        let canonical_empty = instance.instance_id.as_str() == default_id
            && instance.binary_path.as_ref().is_none_or(|p| p.is_empty())
            && instance.home_path.as_ref().is_none_or(|p| p.is_empty())
            && instance.environment.is_empty()
            && instance.launch_args.is_empty();
        // Exact home compare only — never accept foreign legacy home for empty default.
        let home_ok = match (instance.home_path.as_deref(), legacy_home.as_ref()) {
            (None | Some(""), None) => true,
            (Some(configured), Some(persisted))
                if !configured.is_empty() && OsString::from(configured) == *persisted =>
            {
                true
            }
            _ => false,
        };
        let binary_ok = match instance.binary_path.as_deref() {
            None | Some("") => {
                // Unconfigured binary: only unambiguous when this is the sole
                // candidate or the canonical empty default.
                true
            }
            Some(path) => PathBuf::from(path) == *legacy.executable().canonical_path(),
        };
        if home_ok && binary_ok {
            // Custom instances must have an explicit home when legacy carried one.
            if !canonical_empty && instance.home_path.as_ref().is_none_or(|p| p.is_empty()) {
                if legacy_home.is_some() {
                    continue;
                }
            }
            matches.push(*instance);
        }
    }
    if matches.len() != 1 {
        return Err(format!(
            "legacy provider resume for task `{task_id}` is ambiguous or changed; refuse fallback"
        ));
    }
    let instance = matches[0];
    Ok(LegacyMatch {
        instance_id: instance.instance_id.to_string(),
        driver: instance.driver.as_str().to_string(),
        launch_identity_fingerprint: instance.launch_identity_fingerprint(),
    })
}

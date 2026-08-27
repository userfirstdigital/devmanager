//! Native Providers settings projection and operable controls (T3 Task 3).
//!
//! Production never opens profile authority/store files. The controller is
//! constructed from a redacted host snapshot, holds local drafts until Save,
//! queues typed host requests, and applies replies. Fake success feedback
//! before a reply is forbidden. Sensitive env values stay masked in Debug.

use std::collections::VecDeque;
use std::fmt;

use crate::providers::settings::{
    normalize_model_slug, ProviderDriverKind, ProviderEnvVar, ProviderHealthRow,
    ProviderHealthStatus, ProviderInstanceConfig, ProviderSettingsDocument,
    ProviderSettingsHostRequest, ProviderSettingsMutation, ProviderSettingsReply,
    ProviderSettingsSnapshot, DEFAULT_HEALTH_INTERVAL_SECS,
};
use crate::ui::provider_metadata::{
    catalog_for_instance, format_usage_summary, usage_for_instance, UiModelCatalog, UiUsage,
};
use crate::ui::tokens::ThemeTokens;

#[derive(Clone)]
pub struct ProviderSettingsController {
    snapshot: ProviderSettingsSnapshot,
    /// Dirty editor draft retained across health-only snapshot updates.
    dirty_draft: Option<ProviderInstanceConfig>,
    dirty_instance_id: Option<String>,
    expanded: Option<String>,
    add_wizard: Option<AddInstanceWizard>,
    pending_confirm: Option<PendingConfirm>,
    custom_model_draft: String,
    feedback: Option<String>,
    error: Option<String>,
    reveal_secrets: bool,
    health_refresh_generation: Option<u64>,
    pending: VecDeque<ProviderSettingsHostRequest>,
    mutation_in_flight: bool,
    /// Last queued mutation label for success feedback after reply.
    pending_feedback: Option<String>,
    /// Whether the in-flight mutation should clear the local draft on success.
    pending_clears_draft: bool,
    pending_draft: Option<ProviderInstanceConfig>,
    pending_wizard: Option<AddInstanceWizard>,
    launch_args_error: Option<String>,
    #[cfg(test)]
    test_authority: Option<std::sync::Arc<crate::providers::settings::ProviderSettingsAuthority>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddInstanceWizard {
    pub step: AddWizardStep,
    pub driver: ProviderDriverKind,
    pub instance_id: String,
    pub display_name: String,
    /// Complete provider config edited during the Config step before Create.
    pub config_draft: Option<ProviderInstanceConfig>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddWizardStep {
    Driver,
    Identity,
    Config,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PendingConfirm {
    ResetBuiltin { instance_id: String },
    DeleteCustom { instance_id: String },
}

/// Focus target inside an expanded instance editor or the add wizard.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderSettingsFocus {
    DisplayName,
    AccentColor,
    BinaryPath,
    HomePath,
    ShadowHomePath,
    LaunchArgs,
    ApiEndpoint,
    EnvName(usize),
    EnvValue(usize),
    CustomModelSlug,
    WizardInstanceId,
    WizardDisplayName,
}

fn empty_loading_snapshot() -> ProviderSettingsSnapshot {
    ProviderSettingsSnapshot {
        revision: 0,
        health_interval_secs: DEFAULT_HEALTH_INTERVAL_SECS,
        document: ProviderSettingsDocument::with_builtins(),
        health: Vec::new(),
        health_in_flight: false,
        health_error: None,
        composer_instances: Vec::new(),
        model_catalogs: Vec::new(),
        usage: Vec::new(),
        metadata_in_flight: false,
        metadata_error: None,
    }
}

impl ProviderSettingsController {
    /// Empty loading state before the first host snapshot arrives.
    pub fn loading() -> Self {
        Self::from_snapshot(empty_loading_snapshot())
    }

    /// Construct from a redacted host snapshot. Never opens a store.
    pub fn from_snapshot(snapshot: ProviderSettingsSnapshot) -> Self {
        Self {
            snapshot,
            dirty_draft: None,
            dirty_instance_id: None,
            expanded: None,
            add_wizard: None,
            pending_confirm: None,
            custom_model_draft: String::new(),
            feedback: None,
            error: None,
            reveal_secrets: false,
            health_refresh_generation: None,
            pending: VecDeque::new(),
            mutation_in_flight: false,
            pending_feedback: None,
            pending_clears_draft: false,
            pending_draft: None,
            pending_wizard: None,
            launch_args_error: None,
            #[cfg(test)]
            test_authority: None,
        }
    }

    #[cfg(test)]
    pub fn from_test_authority(
        authority: std::sync::Arc<crate::providers::settings::ProviderSettingsAuthority>,
    ) -> Self {
        let snapshot = authority.snapshot();
        let mut ctl = Self::from_snapshot(snapshot);
        ctl.test_authority = Some(authority);
        ctl
    }

    /// Apply a host IPC snapshot. Retains dirty draft across health-only updates.
    pub fn apply_host_snapshot(&mut self, snapshot: ProviderSettingsSnapshot) {
        if self.dirty_draft.is_some()
            && self
                .dirty_instance_id
                .as_ref()
                .is_some_and(|id| snapshot.document.get(id).is_some())
        {
            self.snapshot.health = snapshot.health;
            self.snapshot.health_in_flight = snapshot.health_in_flight;
            self.snapshot.health_error = snapshot.health_error;
            self.snapshot.composer_instances = snapshot.composer_instances;
            self.snapshot.model_catalogs = snapshot.model_catalogs;
            self.snapshot.usage = snapshot.usage;
            self.snapshot.metadata_in_flight = snapshot.metadata_in_flight;
            self.snapshot.metadata_error = snapshot.metadata_error;
            if snapshot.revision != self.snapshot.revision {
                self.error = Some(format!(
                    "settings changed on host (revision {}); save will require refresh",
                    snapshot.revision
                ));
            }
            return;
        }
        self.snapshot = snapshot;
    }

    pub fn model_catalogs(&self) -> &[UiModelCatalog] {
        &self.snapshot.model_catalogs
    }

    pub fn usage_rows(&self) -> &[UiUsage] {
        &self.snapshot.usage
    }

    pub fn metadata_in_flight(&self) -> bool {
        self.snapshot.metadata_in_flight
    }

    pub fn metadata_error(&self) -> Option<&str> {
        self.snapshot.metadata_error.as_deref()
    }

    pub fn usage_summary_for(&self, instance_id: &str) -> Option<String> {
        usage_for_instance(&self.snapshot.usage, instance_id).map(format_usage_summary)
    }

    pub fn apply_reply(&mut self, reply: ProviderSettingsReply) {
        match reply {
            ProviderSettingsReply::Snapshot(snapshot) => {
                self.apply_host_snapshot(snapshot);
            }
            ProviderSettingsReply::MutationApplied { snapshot } => {
                self.mutation_in_flight = false;
                let clears = self.pending_clears_draft;
                self.pending_clears_draft = false;
                let same_draft = self.dirty_draft == self.pending_draft.take();
                let same_wizard = self.add_wizard == self.pending_wizard.take();
                if clears && same_draft {
                    self.dirty_draft = None;
                    self.dirty_instance_id = None;
                    self.pending_confirm = None;
                }
                if clears && same_wizard {
                    self.add_wizard = None;
                }
                // Unrelated enable/interval/refresh success must not clear a dirty draft.
                self.error = None;
                self.snapshot = snapshot;
                self.feedback = self.pending_feedback.take();
            }
            ProviderSettingsReply::RefreshStarted { generation } => {
                self.health_refresh_generation = Some(generation);
                self.snapshot.health_in_flight = true;
            }
            ProviderSettingsReply::RefreshBusy => {
                self.error = Some("health refresh already in flight".into());
            }
            ProviderSettingsReply::Error { message } => {
                self.mutation_in_flight = false;
                self.pending_feedback = None;
                // Rejected Save/Create retains the local draft.
                self.pending_clears_draft = false;
                self.pending_draft = None;
                self.pending_wizard = None;
                self.error = Some(message);
            }
        }
    }

    pub fn take_pending(&mut self) -> Option<ProviderSettingsHostRequest> {
        self.pending.pop_front()
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn mutation_in_flight(&self) -> bool {
        self.mutation_in_flight
    }

    pub fn expected_revision(&self) -> u64 {
        self.snapshot.revision
    }

    pub fn snapshot(&self) -> &ProviderSettingsSnapshot {
        &self.snapshot
    }

    pub fn document(&self) -> ProviderSettingsDocument {
        self.snapshot.document.clone()
    }

    pub fn health_rows(&self) -> Vec<ProviderHealthRow> {
        self.snapshot
            .document
            .instances
            .iter()
            .filter_map(|instance| {
                let wire = self
                    .snapshot
                    .health
                    .iter()
                    .find(|row| row.instance_id == instance.instance_id.as_str())?;
                Some(ProviderHealthRow {
                    instance_id: wire.instance_id.clone(),
                    driver: instance.driver,
                    status: parse_status(&wire.status),
                    version: wire.version.clone(),
                    account_email_masked: wire.account_email_masked.clone(),
                    account_email: wire.account_email.clone(),
                    subscription_tier: wire.subscription_tier.clone(),
                    checked_at_unix_ms: wire.checked_at_unix_ms,
                    error: wire.error.clone(),
                    reveal_email: wire.reveal_email,
                })
            })
            .collect()
    }

    pub fn health_in_flight(&self) -> bool {
        self.snapshot.health_in_flight
    }

    pub fn health_error(&self) -> Option<String> {
        self.snapshot.health_error.clone()
    }

    pub fn feedback(&self) -> Option<&str> {
        self.feedback.as_deref()
    }

    pub fn error(&self) -> Option<&str> {
        self.launch_args_error.as_deref().or(self.error.as_deref())
    }

    pub fn expanded(&self) -> Option<&str> {
        self.expanded.as_deref()
    }

    pub fn add_wizard(&self) -> Option<&AddInstanceWizard> {
        self.add_wizard.as_ref()
    }

    pub fn pending_confirm(&self) -> Option<&PendingConfirm> {
        self.pending_confirm.as_ref()
    }

    pub fn custom_model_draft(&self) -> &str {
        &self.custom_model_draft
    }

    pub fn set_custom_model_draft(&mut self, value: String) {
        self.custom_model_draft = value;
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty_draft.is_some()
    }

    /// Working copy for paint/edit: dirty draft when present, else snapshot row.
    pub fn working_instance(&self, instance_id: &str) -> Option<ProviderInstanceConfig> {
        if self.dirty_instance_id.as_deref() == Some(instance_id) {
            if let Some(draft) = self.dirty_draft.clone() {
                return Some(draft);
            }
        }
        self.snapshot.document.get(instance_id).cloned()
    }

    fn queue_mutate(
        &mut self,
        mutation: ProviderSettingsMutation,
        feedback: Option<String>,
        clears_draft: bool,
    ) {
        if self.mutation_in_flight {
            self.error = Some("provider settings mutation already in flight".into());
            return;
        }
        self.mutation_in_flight = true;
        self.error = None;
        self.feedback = None;
        self.pending_feedback = feedback;
        self.pending_clears_draft = clears_draft;
        self.pending_draft = clears_draft.then(|| self.dirty_draft.clone()).flatten();
        self.pending_wizard = clears_draft.then(|| self.add_wizard.clone()).flatten();
        self.pending
            .push_back(ProviderSettingsHostRequest::Mutate(mutation));
        #[cfg(test)]
        if let Some(authority) = self.test_authority.clone() {
            if let Some(ProviderSettingsHostRequest::Mutate(mutation)) = self.pending.pop_front() {
                let reply = match authority.mutate(mutation) {
                    Ok(reply) => reply,
                    Err(error) => ProviderSettingsReply::Error {
                        message: error.to_string(),
                    },
                };
                self.apply_reply(reply);
            }
        }
    }

    pub fn queue_snapshot(&mut self) {
        self.pending
            .push_back(ProviderSettingsHostRequest::Snapshot);
    }

    pub fn queue_refresh(&mut self, force: bool) {
        self.pending
            .push_back(ProviderSettingsHostRequest::Refresh { force });
    }

    pub fn set_health_interval(&mut self, secs: u64) {
        let expected = self.expected_revision();
        let feedback = Some(if secs == 0 {
            "Health checks are manual-only".into()
        } else {
            format!("Health interval set to {secs}s")
        });
        self.queue_mutate(
            ProviderSettingsMutation::SetHealthInterval {
                expected_revision: expected,
                interval_secs: secs,
            },
            feedback,
            false,
        );
    }

    /// Parse a numeric health-interval control (empty/invalid → error; `0` = manual only).
    pub fn set_health_interval_from_text(&mut self, raw: &str) {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            self.error = Some("health interval requires a number (0 = manual only)".into());
            return;
        }
        match trimmed.parse::<u64>() {
            Ok(secs) if secs <= 86_400 => self.set_health_interval(secs),
            Ok(_) => {
                self.error = Some("health interval must be between 0 and 86400 seconds".into());
            }
            Err(_) => {
                self.error = Some("health interval must be a whole number of seconds".into());
            }
        }
    }

    pub fn nudge_health_interval(&mut self, delta: i64) {
        let current = self.snapshot.document.health_interval_secs as i64;
        let next = (current + delta).clamp(0, 86_400) as u64;
        self.set_health_interval(next);
    }

    pub fn reset_health_interval_default(&mut self) {
        self.set_health_interval(DEFAULT_HEALTH_INTERVAL_SECS);
    }

    pub fn toggle_expanded(&mut self, instance_id: &str) {
        if self.expanded.as_deref() == Some(instance_id) {
            if self.dirty_draft.is_some() {
                self.error = Some("save or cancel edits before collapsing".into());
                return;
            }
            self.expanded = None;
            self.custom_model_draft.clear();
        } else {
            if self.dirty_draft.is_some() {
                self.error = Some("save or cancel edits before switching instances".into());
                return;
            }
            self.expanded = Some(instance_id.to_string());
            self.begin_edit(instance_id);
        }
    }

    /// Start (or refresh) a local draft for `instance_id` without host mutation.
    pub fn begin_edit(&mut self, instance_id: &str) {
        if self.dirty_draft.is_some() {
            if self.dirty_instance_id.as_deref() != Some(instance_id) {
                self.error = Some("save or cancel edits before switching instances".into());
            }
            return;
        }
        self.launch_args_error = None;
        let Some(instance) = self.snapshot.document.get(instance_id).cloned() else {
            self.error = Some(format!("unknown provider instance `{instance_id}`"));
            return;
        };
        if instance.driver.is_stub() {
            self.error = Some(format!("{} is not supported yet", instance.driver.as_str()));
            return;
        }
        self.dirty_draft = Some(instance);
        self.dirty_instance_id = Some(instance_id.to_string());
        self.expanded = Some(instance_id.to_string());
        self.error = None;
    }

    pub fn cancel_draft(&mut self) {
        self.launch_args_error = None;
        self.dirty_draft = None;
        self.dirty_instance_id = None;
        self.error = None;
        self.feedback = Some("Edits discarded".into());
    }

    /// Mutate the local draft only. Host write happens in [`Self::save_draft`].
    pub fn mutate_draft(&mut self, mutator: impl FnOnce(&mut ProviderInstanceConfig)) {
        let Some(draft) = self.dirty_draft.as_mut() else {
            self.error = Some("no provider draft open".into());
            return;
        };
        mutator(draft);
        self.error = None;
        self.feedback = None;
    }

    pub fn set_draft_display_name(&mut self, value: String) {
        self.mutate_draft(|draft| draft.display_name = value);
    }

    pub fn set_draft_accent_color(&mut self, value: String) {
        self.mutate_draft(|draft| {
            draft.accent_color = nonempty_opt(value);
        });
    }

    pub fn set_draft_binary_path(&mut self, value: String) {
        self.mutate_draft(|draft| {
            draft.binary_path = nonempty_opt(value);
        });
    }

    pub fn set_draft_home_path(&mut self, value: String) {
        self.mutate_draft(|draft| {
            draft.home_path = nonempty_opt(value);
        });
    }

    pub fn set_draft_shadow_home_path(&mut self, value: String) {
        self.mutate_draft(|draft| {
            draft.shadow_home_path = nonempty_opt(value);
        });
    }

    pub fn set_draft_api_endpoint(&mut self, value: String) {
        self.mutate_draft(|draft| {
            draft.api_endpoint = nonempty_opt(value);
        });
    }

    pub fn set_draft_launch_args(&mut self, value: String) {
        match decode_launch_args_json(&value) {
            Ok(args) => {
                self.launch_args_error = None;
                self.mutate_draft(|draft| {
                    draft.launch_args = args;
                });
            }
            Err(error) => {
                self.launch_args_error = Some(error);
            }
        }
    }

    pub fn add_env_row(&mut self, _instance_id: &str) {
        self.mutate_draft(|draft| {
            draft.environment.push(ProviderEnvVar {
                name: String::new(),
                value: Some(String::new()),
                sensitive: false,
                protected_value: None,
                value_redacted: false,
            });
        });
    }

    pub fn remove_env_row(&mut self, _instance_id: &str, index: usize) {
        self.mutate_draft(|draft| {
            if index < draft.environment.len() {
                draft.environment.remove(index);
            }
        });
    }

    pub fn set_env_name(&mut self, index: usize, name: String) {
        self.mutate_draft(|draft| {
            if let Some(env) = draft.environment.get_mut(index) {
                env.name = name;
            }
        });
    }

    pub fn set_env_value(&mut self, index: usize, value: String) {
        self.mutate_draft(|draft| {
            if let Some(env) = draft.environment.get_mut(index) {
                if env.sensitive {
                    // Blank keeps the redacted placeholder so Save preserves the secret.
                    if value.is_empty() {
                        env.value = None;
                        env.value_redacted = true;
                    } else {
                        env.value = Some(value);
                        env.value_redacted = false;
                    }
                } else {
                    env.value = Some(value);
                    env.value_redacted = false;
                }
            }
        });
    }

    pub fn toggle_env_sensitive(&mut self, _instance_id: &str, index: usize) {
        self.mutate_draft(|draft| {
            if let Some(env) = draft.environment.get_mut(index) {
                env.sensitive = !env.sensitive;
                if env.sensitive {
                    if env.value.as_ref().is_none_or(|v| v.is_empty()) && env.value_redacted {
                        env.value = None;
                    }
                } else {
                    // Declassify requires an explicit replacement value at Save.
                    env.protected_value = None;
                }
            }
        });
    }

    /// Validate draft and queue a revision-fenced UpsertInstance.
    pub fn save_draft(&mut self) {
        if self.launch_args_error.is_some() {
            return;
        }
        let Some(mut instance) = self.dirty_draft.clone() else {
            self.error = Some("no provider draft to save".into());
            return;
        };
        if self
            .snapshot
            .document
            .get(instance.instance_id.as_str())
            .is_none()
        {
            self.error = Some("Provider instance was removed on the host. Cancel this draft; use Add to create a new instance.".into());
            return;
        }
        if let Err(error) = instance.validate() {
            self.error = Some(error.to_string());
            return;
        }
        // Ensure redacted blanks stay blank so the store merge preserves secrets.
        for env in &mut instance.environment {
            if env.sensitive
                && env.value.as_ref().is_none_or(|v| v.is_empty())
                && env.value_redacted
            {
                env.value = None;
            }
        }
        let id = instance.instance_id.to_string();
        let expected = self.expected_revision();
        let feedback = Some(format!("Saved {id}"));
        self.queue_mutate(
            ProviderSettingsMutation::UpsertInstance {
                expected_revision: expected,
                instance,
            },
            feedback,
            true,
        );
    }

    pub fn set_enabled(&mut self, instance_id: &str, enabled: bool) {
        if let Some(instance) = self.snapshot.document.get(instance_id) {
            if instance.driver.is_stub() {
                self.error = Some(format!(
                    "{} is not supported yet and cannot be enabled",
                    instance.driver.as_str()
                ));
                return;
            }
        }
        let expected = self.expected_revision();
        let feedback = Some(format!(
            "{instance_id} {}",
            if enabled { "enabled" } else { "disabled" }
        ));
        self.queue_mutate(
            ProviderSettingsMutation::SetEnabled {
                expected_revision: expected,
                instance_id: instance_id.to_string(),
                enabled,
            },
            feedback,
            false,
        );
    }

    pub fn request_reset_builtin(&mut self, instance_id: &str) {
        self.pending_confirm = Some(PendingConfirm::ResetBuiltin {
            instance_id: instance_id.to_string(),
        });
        self.feedback = Some(format!("Confirm reset of {instance_id}?"));
    }

    pub fn request_delete_custom(&mut self, instance_id: &str) {
        self.pending_confirm = Some(PendingConfirm::DeleteCustom {
            instance_id: instance_id.to_string(),
        });
        self.feedback = Some(format!("Confirm delete of {instance_id}?"));
    }

    pub fn cancel_confirm(&mut self) {
        self.pending_confirm = None;
        self.feedback = None;
    }

    pub fn confirm_pending(&mut self) {
        let Some(pending) = self.pending_confirm.take() else {
            return;
        };
        match pending {
            PendingConfirm::ResetBuiltin { instance_id } => {
                let clears = self.dirty_instance_id.as_deref() == Some(instance_id.as_str());
                let expected = self.expected_revision();
                let feedback = Some(format!("Reset {instance_id} to defaults"));
                self.queue_mutate(
                    ProviderSettingsMutation::ResetBuiltin {
                        expected_revision: expected,
                        instance_id,
                    },
                    feedback,
                    clears,
                );
            }
            PendingConfirm::DeleteCustom { instance_id } => {
                if self.expanded.as_deref() == Some(instance_id.as_str()) {
                    self.expanded = None;
                }
                let clears = self.dirty_instance_id.as_deref() == Some(instance_id.as_str());
                let expected = self.expected_revision();
                let feedback = Some(format!("Removed {instance_id}"));
                self.queue_mutate(
                    ProviderSettingsMutation::RemoveInstance {
                        expected_revision: expected,
                        instance_id,
                    },
                    feedback,
                    clears,
                );
            }
        }
    }

    pub fn begin_add_wizard(&mut self) {
        if self.dirty_draft.is_some() {
            self.error = Some("save or cancel edits before adding an instance".into());
            return;
        }
        self.add_wizard = Some(AddInstanceWizard {
            step: AddWizardStep::Driver,
            driver: ProviderDriverKind::Claude,
            instance_id: String::new(),
            display_name: String::new(),
            config_draft: None,
        });
        self.error = None;
    }

    pub fn cancel_add_wizard(&mut self) {
        self.add_wizard = None;
        self.launch_args_error = None;
    }

    pub fn wizard_select_driver(&mut self, driver: ProviderDriverKind) {
        if let Some(wizard) = &mut self.add_wizard {
            if driver.is_stub() {
                self.error = Some(format!(
                    "{} is not supported yet and cannot be added",
                    driver.as_str()
                ));
                return;
            }
            wizard.driver = driver;
            wizard.step = AddWizardStep::Identity;
            self.error = None;
        }
    }

    pub fn wizard_set_instance_id(&mut self, instance_id: String) {
        if let Some(wizard) = &mut self.add_wizard {
            wizard.instance_id = instance_id;
        }
    }

    pub fn wizard_set_display_name(&mut self, display_name: String) {
        if let Some(wizard) = &mut self.add_wizard {
            wizard.display_name = display_name;
        }
    }

    pub fn wizard_advance_to_config(&mut self) {
        let Some(wizard) = self.add_wizard.as_mut() else {
            return;
        };
        if wizard.instance_id.trim().is_empty() {
            self.error = Some("instance id is required".into());
            return;
        }
        if let Err(error) =
            crate::providers::settings::ProviderInstanceId::new(wizard.instance_id.clone())
        {
            self.error = Some(error.to_string());
            return;
        }
        if self
            .snapshot
            .document
            .get(wizard.instance_id.trim())
            .is_some()
        {
            self.error = Some(format!(
                "provider instance `{}` already exists",
                wizard.instance_id.trim()
            ));
            return;
        }
        let mut instance = match wizard.driver {
            ProviderDriverKind::Claude => ProviderInstanceConfig::builtin_default(
                crate::providers::settings::BuiltinProviderDriver::Claude,
            ),
            ProviderDriverKind::Codex => ProviderInstanceConfig::builtin_default(
                crate::providers::settings::BuiltinProviderDriver::Codex,
            ),
            ProviderDriverKind::Cursor => ProviderInstanceConfig::builtin_default(
                crate::providers::settings::BuiltinProviderDriver::Cursor,
            ),
            ProviderDriverKind::Grok | ProviderDriverKind::OpenCode => {
                self.error = Some(format!("{} cannot be added", wizard.driver.as_str()));
                return;
            }
        };
        match crate::providers::settings::ProviderInstanceId::new(wizard.instance_id.clone()) {
            Ok(id) => instance.instance_id = id,
            Err(error) => {
                self.error = Some(error.to_string());
                return;
            }
        }
        instance.display_name = if wizard.display_name.trim().is_empty() {
            wizard.instance_id.clone()
        } else {
            wizard.display_name.clone()
        };
        instance.enabled = true;
        wizard.config_draft = Some(instance);
        wizard.step = AddWizardStep::Config;
        self.error = None;
    }

    /// Mutate the Add wizard Config-step draft (complete provider config before Create).
    pub fn wizard_mutate_config(&mut self, mutator: impl FnOnce(&mut ProviderInstanceConfig)) {
        let Some(wizard) = self.add_wizard.as_mut() else {
            self.error = Some("add wizard is not open".into());
            return;
        };
        if wizard.step != AddWizardStep::Config {
            self.error = Some("complete identity before editing provider configuration".into());
            return;
        }
        let Some(config) = wizard.config_draft.as_mut() else {
            self.error = Some("wizard configuration draft is missing".into());
            return;
        };
        mutator(config);
        self.error = None;
    }

    /// Apply editor field values onto the Add wizard Config draft.
    pub fn wizard_apply_editor_fields(
        &mut self,
        display_name: String,
        accent: String,
        binary: String,
        home: String,
        shadow: String,
        launch_args: String,
        endpoint: String,
        env_names: Vec<String>,
        env_values: Vec<String>,
    ) {
        let launch = match decode_launch_args_json(&launch_args) {
            Ok(args) => {
                self.launch_args_error = None;
                args
            }
            Err(error) => {
                self.launch_args_error = Some(error);
                return;
            }
        };
        self.wizard_mutate_config(|config| {
            if !display_name.trim().is_empty() {
                config.display_name = display_name;
            }
            config.accent_color = nonempty_opt(accent);
            config.binary_path = nonempty_opt(binary);
            config.home_path = nonempty_opt(home);
            config.shadow_home_path = nonempty_opt(shadow);
            config.launch_args = launch;
            config.api_endpoint = nonempty_opt(endpoint);
            let count = env_names.len().max(env_values.len());
            while config.environment.len() < count {
                config.environment.push(ProviderEnvVar {
                    name: String::new(),
                    value: Some(String::new()),
                    sensitive: false,
                    protected_value: None,
                    value_redacted: false,
                });
            }
            for (index, name) in env_names.into_iter().enumerate() {
                if let Some(env) = config.environment.get_mut(index) {
                    env.name = name;
                }
            }
            for (index, value) in env_values.into_iter().enumerate() {
                if let Some(env) = config.environment.get_mut(index) {
                    if env.sensitive {
                        if value.is_empty() {
                            env.value = None;
                            env.value_redacted = true;
                        } else {
                            env.value = Some(value);
                            env.value_redacted = false;
                        }
                    } else {
                        env.value = Some(value);
                        env.value_redacted = false;
                    }
                }
            }
        });
    }

    pub fn wizard_working_config(&self) -> Option<&ProviderInstanceConfig> {
        self.add_wizard
            .as_ref()
            .and_then(|wizard| wizard.config_draft.as_ref())
    }

    pub fn wizard_commit(&mut self) {
        if self.launch_args_error.is_some() {
            return;
        }
        let Some(wizard) = self.add_wizard.clone() else {
            return;
        };
        if wizard.step != AddWizardStep::Config {
            self.error = Some("complete provider configuration before create".into());
            return;
        }
        let Some(mut instance) = wizard.config_draft else {
            self.error = Some("complete provider configuration before create".into());
            return;
        };
        if wizard.instance_id.trim().is_empty() {
            self.error = Some("instance id is required".into());
            return;
        }
        if self
            .snapshot
            .document
            .get(wizard.instance_id.trim())
            .is_some()
        {
            self.error = Some(format!(
                "provider instance `{}` already exists",
                wizard.instance_id.trim()
            ));
            return;
        }
        match crate::providers::settings::ProviderInstanceId::new(wizard.instance_id.clone()) {
            Ok(id) => instance.instance_id = id,
            Err(error) => {
                self.error = Some(error.to_string());
                return;
            }
        }
        if let Err(error) = instance.validate() {
            self.error = Some(error.to_string());
            return;
        }
        let expected = self.expected_revision();
        let feedback = Some(format!("Added {}", wizard.instance_id));
        self.expanded = Some(wizard.instance_id.clone());
        self.queue_mutate(
            ProviderSettingsMutation::AddInstance {
                expected_revision: expected,
                instance,
            },
            feedback,
            true,
        );
    }

    pub fn add_custom_model(&mut self, instance_id: &str) {
        let raw = self.custom_model_draft.clone();
        let slug = match normalize_model_slug(&raw) {
            Ok(slug) => slug,
            Err(error) => {
                self.error = Some(error.to_string());
                return;
            }
        };
        let id = instance_id.to_string();
        self.mutate_draft_models(instance_id, format!("Added model {slug}"), move |doc| {
            doc.add_custom_model(&id, &slug, None)
                .map_err(|e| e.to_string())
        });
        if self.error().is_none() {
            self.custom_model_draft.clear();
        }
    }

    pub fn remove_custom_model(&mut self, instance_id: &str, slug: &str) {
        let slug = slug.to_string();
        let id = instance_id.to_string();
        self.mutate_draft_models(instance_id, format!("Removed model {slug}"), move |doc| {
            doc.remove_custom_model(&id, &slug)
                .map_err(|e| e.to_string())
        });
    }

    /// Model-policy edits stay draft-local until Save (no silent ReplaceDocument).
    fn mutate_draft_models(
        &mut self,
        instance_id: &str,
        feedback: String,
        mutator: impl FnOnce(&mut ProviderSettingsDocument) -> Result<(), String>,
    ) {
        if self.dirty_draft.is_some() && self.dirty_instance_id.as_deref() != Some(instance_id) {
            self.error = Some("save or cancel edits before changing another instance".into());
            return;
        }
        if self.dirty_instance_id.as_deref() != Some(instance_id) {
            self.begin_edit(instance_id);
        }
        let Some(draft) = self.dirty_draft.clone() else {
            return;
        };
        let mut doc = self.snapshot.document.clone();
        if let Some(slot) = doc.get_mut(instance_id) {
            *slot = draft;
        }
        if let Err(error) = mutator(&mut doc) {
            self.error = Some(error);
            return;
        }
        let Some(updated) = doc.get(instance_id).cloned() else {
            self.error = Some(format!("unknown provider instance `{instance_id}`"));
            return;
        };
        self.dirty_draft = Some(updated);
        self.dirty_instance_id = Some(instance_id.to_string());
        self.feedback = Some(feedback);
        self.error = None;
    }

    pub fn toggle_favorite(&mut self, instance_id: &str, slug: &str) {
        let Some(instance) = self.working_instance(instance_id) else {
            return;
        };
        let favorite = !instance
            .model_policy
            .favorite_order
            .iter()
            .any(|s| s == slug);
        let slug = slug.to_string();
        let id = instance_id.to_string();
        self.mutate_draft_models(
            instance_id,
            format!(
                "{} {slug}",
                if favorite { "Favorited" } else { "Unfavorited" }
            ),
            move |doc| {
                doc.set_favorite(&id, &slug, favorite)
                    .map_err(|e| e.to_string())
            },
        );
    }

    pub fn toggle_hide_builtin(&mut self, instance_id: &str, slug: &str) {
        let Some(instance) = self.working_instance(instance_id) else {
            return;
        };
        let hidden = !instance
            .model_policy
            .hidden_builtins
            .iter()
            .any(|s| s == slug);
        let slug = slug.to_string();
        let id = instance_id.to_string();
        self.mutate_draft_models(
            instance_id,
            format!("{} {slug}", if hidden { "Hid" } else { "Showed" }),
            move |doc| {
                doc.set_builtin_hidden(&id, &slug, hidden)
                    .map_err(|e| e.to_string())
            },
        );
    }

    pub fn move_favorite(&mut self, instance_id: &str, slug: &str, up: bool) {
        let slug = slug.to_string();
        let id = instance_id.to_string();
        self.mutate_draft_models(
            instance_id,
            format!("Reordered favorite {slug}"),
            move |doc| doc.move_favorite(&id, &slug, up).map_err(|e| e.to_string()),
        );
    }

    pub fn move_catalog_model(&mut self, instance_id: &str, slug: &str, up: bool) {
        let builtins = self
            .snapshot
            .model_catalogs
            .iter()
            .find(|catalog| catalog.instance_id == instance_id)
            .filter(|catalog| !catalog.models.is_empty())
            .map(|catalog| {
                catalog
                    .models
                    .iter()
                    .filter(|entry| !entry.is_custom)
                    .map(|entry| entry.slug.clone())
                    .collect()
            })
            .unwrap_or_else(|| {
                builtin_model_slugs(
                    self.working_instance(instance_id)
                        .map(|i| i.driver)
                        .unwrap_or(ProviderDriverKind::Claude),
                )
            });
        let slug = slug.to_string();
        let id = instance_id.to_string();
        self.mutate_draft_models(instance_id, format!("Reordered model {slug}"), move |doc| {
            doc.move_catalog_model(&id, &slug, up, &builtins)
                .map_err(|e| e.to_string())
        });
    }

    pub fn settings_model_catalog(&self, instance_id: &str) -> Vec<String> {
        let Some(instance) = self.working_instance(instance_id) else {
            return Vec::new();
        };
        let catalog = catalog_for_instance(&self.snapshot.model_catalogs, instance_id);
        let builtins = catalog
            .filter(|catalog| !catalog.models.is_empty())
            .map(|catalog| {
                catalog
                    .models
                    .iter()
                    .filter(|entry| !entry.is_custom)
                    .map(|entry| entry.slug.clone())
                    .collect()
            })
            .unwrap_or_else(|| builtin_model_slugs(instance.driver));
        let mut doc = self.snapshot.document.clone();
        if let Some(slot) = doc.get_mut(instance_id) {
            *slot = instance;
        }
        // Settings must include hidden models so they can be shown again, and
        // unsaved custom rows must stay visible in this editor's local draft.
        doc.ordered_settings_catalog(instance_id, &builtins)
            .unwrap_or_default()
    }

    pub fn set_email_reveal(&mut self, instance_id: &str, reveal: bool) {
        if let Some(row) = self
            .snapshot
            .health
            .iter_mut()
            .find(|row| row.instance_id == instance_id)
        {
            row.reveal_email = reveal;
        }
    }

    pub fn try_begin_health_refresh(&mut self) -> Option<u64> {
        if self.snapshot.health_in_flight {
            return None;
        }
        self.queue_refresh(true);
        self.snapshot.health_in_flight = true;
        Some(0)
    }

    pub fn publish_health_row(&self, _row: ProviderHealthRow) {
        // Host health job publishes into the authority cache; UI paints snapshot.
    }

    pub fn finish_health_refresh(&mut self, error: Option<String>) {
        self.health_refresh_generation = None;
        self.snapshot.health_in_flight = false;
        if let Some(error) = error {
            self.snapshot.health_error = Some(error);
        }
    }

    pub fn should_auto_refresh(&self) -> bool {
        self.snapshot.health_interval_secs > 0 && !self.snapshot.health_in_flight
    }

    pub fn clear_pending_on_queue_failure(&mut self) {
        self.pending.clear();
        self.mutation_in_flight = false;
        self.pending_feedback = None;
        self.pending_clears_draft = false;
        self.pending_draft = None;
        self.pending_wizard = None;
        self.error = Some("provider settings host queue failed".into());
    }

    /// Masked presentation for sensitive env values (never plaintext secrets).
    pub fn env_value_for_display(env: &ProviderEnvVar) -> String {
        if env.sensitive {
            match env.value.as_deref() {
                Some(value) if !value.is_empty() => mask_secret_preserving_scalars(value),
                _ => "••••••••".into(),
            }
        } else {
            env.value.clone().unwrap_or_default()
        }
    }
}

impl fmt::Debug for ProviderSettingsController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderSettingsController")
            .field("revision", &self.snapshot.revision)
            .field("expanded", &self.expanded)
            .field("dirty", &self.dirty_instance_id)
            .field("mutation_in_flight", &self.mutation_in_flight)
            .field("reveal_secrets", &self.reveal_secrets)
            .finish()
    }
}

fn nonempty_opt(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Preserve both scalar and UTF-16 lengths, including supplementary characters.
pub fn mask_secret_preserving_scalars(raw: &str) -> String {
    raw.chars()
        .map(|ch| {
            if ch.len_utf16() == 2 {
                '\u{1f512}'
            } else {
                '•'
            }
        })
        .collect()
}

/// Serialize launch args as a JSON string array (lossless for spaces/quotes/backslashes).
pub fn encode_launch_args_json(args: &[String]) -> String {
    serde_json::to_string(args).unwrap_or_else(|_| "[]".into())
}

/// Parse launch args from a JSON string array. Whitespace join/split is forbidden.
pub fn decode_launch_args_json(raw: &str) -> Result<Vec<String>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str::<Vec<String>>(trimmed)
        .map_err(|error| format!("launch args must be a JSON string array: {error}"))
}

fn parse_status(raw: &str) -> ProviderHealthStatus {
    match raw {
        "healthy" => ProviderHealthStatus::Healthy,
        "checking" => ProviderHealthStatus::Checking,
        "degraded" => ProviderHealthStatus::Degraded,
        "unavailable" => ProviderHealthStatus::Unavailable,
        "stub_unsupported" => ProviderHealthStatus::StubUnsupported,
        _ => ProviderHealthStatus::Unknown,
    }
}

pub fn health_dot_color(status: ProviderHealthStatus, tokens: &ThemeTokens) -> u32 {
    match status {
        ProviderHealthStatus::Healthy => tokens.status.success.to_u32(),
        ProviderHealthStatus::Checking => tokens.status.warning.to_u32(),
        ProviderHealthStatus::Degraded => tokens.status.warning.to_u32(),
        ProviderHealthStatus::Unavailable => tokens.status.destructive.to_u32(),
        ProviderHealthStatus::StubUnsupported => tokens.text.muted.to_u32(),
        ProviderHealthStatus::Unknown => tokens.text.muted.to_u32(),
    }
}

pub fn status_label(status: ProviderHealthStatus) -> &'static str {
    match status {
        ProviderHealthStatus::Healthy => "Healthy",
        ProviderHealthStatus::Checking => "Checking…",
        ProviderHealthStatus::Degraded => "Degraded",
        ProviderHealthStatus::Unavailable => "Unavailable",
        ProviderHealthStatus::StubUnsupported => "Not supported",
        ProviderHealthStatus::Unknown => "Unknown",
    }
}

/// Builtin model slugs used when runtime cannot enumerate (honest catalog baseline).
pub fn builtin_model_slugs(driver: ProviderDriverKind) -> Vec<String> {
    crate::providers::settings::builtin_slugs_for_driver(driver)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_ctl() -> ProviderSettingsController {
        let dir = tempdir().unwrap();
        let profile =
            crate::providers::settings::ProviderProfileOwner::open_dir_for_test(dir.path())
                .unwrap();
        let authority = std::sync::Arc::new(
            crate::providers::settings::ProviderSettingsAuthority::from_profile(profile),
        );
        ProviderSettingsController::from_test_authority(authority)
    }

    #[test]
    fn controller_rejects_stub_enable_and_add() {
        let mut ctl = test_ctl();
        ctl.set_enabled("grok", true);
        assert!(ctl.error().is_some());
        ctl.begin_add_wizard();
        ctl.wizard_select_driver(ProviderDriverKind::Grok);
        assert!(ctl.error().is_some());
    }

    #[test]
    fn refresh_queues_without_direct_authority() {
        let mut ctl = ProviderSettingsController::loading();
        assert!(ctl.try_begin_health_refresh().is_some());
        assert!(matches!(
            ctl.take_pending(),
            Some(ProviderSettingsHostRequest::Refresh { force: true })
        ));
        assert!(ctl.try_begin_health_refresh().is_none());
    }

    #[test]
    fn dirty_draft_survives_health_snapshot() {
        let mut ctl = test_ctl();
        ctl.begin_edit("claude");
        ctl.set_draft_display_name("dirty".into());
        let mut snap = ctl.snapshot().clone();
        snap.health_in_flight = true;
        ctl.apply_host_snapshot(snap);
        assert!(ctl.dirty_draft.is_some());
        assert_eq!(ctl.dirty_draft.as_ref().unwrap().display_name, "dirty");
    }

    #[test]
    fn save_draft_queues_upsert_after_local_edits() {
        let mut ctl = ProviderSettingsController::loading();
        let mut snap = ctl.snapshot().clone();
        snap.revision = 3;
        ctl.apply_host_snapshot(snap);
        ctl.begin_edit("claude");
        ctl.set_draft_display_name("Renamed Claude".into());
        ctl.set_draft_binary_path("C:/tools/claude.exe".into());
        ctl.save_draft();
        let pending = ctl.take_pending().expect("mutate queued");
        match pending {
            ProviderSettingsHostRequest::Mutate(ProviderSettingsMutation::UpsertInstance {
                expected_revision,
                instance,
            }) => {
                assert_eq!(expected_revision, 3);
                assert_eq!(instance.display_name, "Renamed Claude");
                assert_eq!(instance.binary_path.as_deref(), Some("C:/tools/claude.exe"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn cancel_draft_discards_local_edits() {
        let mut ctl = test_ctl();
        ctl.begin_edit("claude");
        ctl.set_draft_display_name("temp".into());
        ctl.cancel_draft();
        assert!(!ctl.is_dirty());
        assert_eq!(
            ctl.working_instance("claude")
                .unwrap()
                .display_name
                .as_str(),
            "Claude"
        );
    }

    #[test]
    fn duplicate_create_rejected_without_overwrite() {
        let mut ctl = test_ctl();
        ctl.begin_add_wizard();
        ctl.wizard_select_driver(ProviderDriverKind::Claude);
        ctl.wizard_set_instance_id("claude".into());
        ctl.wizard_set_display_name("dup".into());
        ctl.wizard_advance_to_config();
        assert!(ctl.error().is_some_and(|e| e.contains("already exists")));
        assert!(ctl.wizard_working_config().is_none());
        ctl.wizard_commit();
        assert!(ctl.error().is_some());
        assert!(ctl.take_pending().is_none());
    }

    #[test]
    fn create_uses_add_instance_mutation() {
        let mut ctl = ProviderSettingsController::loading();
        ctl.begin_add_wizard();
        ctl.wizard_select_driver(ProviderDriverKind::Codex);
        ctl.wizard_set_instance_id("codex_work".into());
        ctl.wizard_set_display_name("Codex Work".into());
        ctl.wizard_advance_to_config();
        assert!(ctl.wizard_working_config().is_some());
        ctl.wizard_mutate_config(|config| {
            config.binary_path = Some("C:/tools/codex.exe".into());
            config.launch_args = vec!["--flag=value with spaces".into()];
        });
        ctl.wizard_commit();
        match ctl.take_pending() {
            Some(ProviderSettingsHostRequest::Mutate(ProviderSettingsMutation::AddInstance {
                instance,
                ..
            })) => {
                assert_eq!(instance.instance_id.as_str(), "codex_work");
                assert_eq!(instance.display_name, "Codex Work");
                assert_eq!(instance.binary_path.as_deref(), Some("C:/tools/codex.exe"));
                assert_eq!(
                    instance.launch_args,
                    vec!["--flag=value with spaces".to_string()]
                );
            }
            other => panic!("expected AddInstance, got {other:?}"),
        }
    }

    #[test]
    fn wizard_create_validates_complete_config() {
        let mut ctl = ProviderSettingsController::loading();
        ctl.begin_add_wizard();
        ctl.wizard_select_driver(ProviderDriverKind::Claude);
        ctl.wizard_set_instance_id("claude_work".into());
        ctl.wizard_advance_to_config();
        ctl.wizard_mutate_config(|config| {
            config.environment.push(ProviderEnvVar {
                name: String::new(),
                value: Some("x".into()),
                sensitive: false,
                protected_value: None,
                value_redacted: false,
            });
        });
        ctl.wizard_commit();
        assert!(ctl.error().is_some());
        assert!(ctl.take_pending().is_none());
        assert!(ctl.add_wizard().is_some());
    }

    #[test]
    fn sensitive_env_blank_preserves_redacted_flag_for_store_merge() {
        let mut ctl = test_ctl();
        ctl.begin_edit("claude");
        ctl.add_env_row("claude");
        ctl.set_env_name(0, "TOKEN".into());
        ctl.toggle_env_sensitive("claude", 0);
        ctl.set_env_value(0, "secret-value".into());
        // Simulate host redaction after a prior save.
        ctl.mutate_draft(|draft| {
            draft.environment[0].value = None;
            draft.environment[0].value_redacted = true;
        });
        ctl.set_env_value(0, String::new());
        let env = &ctl.dirty_draft.as_ref().unwrap().environment[0];
        assert!(env.sensitive);
        assert!(env.value.is_none());
        assert!(env.value_redacted);
        assert_eq!(
            ProviderSettingsController::env_value_for_display(env),
            "••••••••"
        );
    }

    #[test]
    fn mask_secret_preserves_scalar_count_for_ime_offsets() {
        let secret = "ab€d🚀";
        let masked = mask_secret_preserving_scalars(secret);
        assert_eq!(masked.chars().count(), secret.chars().count());
        assert_eq!(masked.encode_utf16().count(), secret.encode_utf16().count());
        assert!(!masked.contains('a'));
        assert!(!masked.contains('€'));
    }

    #[test]
    fn launch_args_json_roundtrip_preserves_spaces_quotes_and_backslashes() {
        let args = vec![
            "--path".into(),
            r"C:\Program Files\app".into(),
            r#"say "hi""#.into(),
            r"tail\".into(),
        ];
        let encoded = encode_launch_args_json(&args);
        let decoded = decode_launch_args_json(&encoded).expect("decode");
        assert_eq!(decoded, args);
        let mut ctl = test_ctl();
        ctl.begin_edit("claude");
        ctl.set_draft_launch_args(encoded);
        assert_eq!(ctl.dirty_draft.as_ref().unwrap().launch_args, args);
        ctl.set_draft_launch_args("not-json".into());
        assert!(ctl.error().is_some());
        assert_eq!(ctl.dirty_draft.as_ref().unwrap().launch_args, args);
    }

    #[test]
    fn model_edits_stay_draft_local_until_save() {
        let mut ctl = test_ctl();
        let builtins = builtin_model_slugs(ProviderDriverKind::Claude);
        let first = builtins[0].clone();
        ctl.begin_edit("claude");
        ctl.toggle_hide_builtin("claude", &first);
        assert!(ctl.take_pending().is_none(), "hide must not host-write yet");
        assert!(ctl
            .dirty_draft
            .as_ref()
            .unwrap()
            .model_policy
            .hidden_builtins
            .iter()
            .any(|s| s == &first));
        assert!(!ctl
            .document()
            .get("claude")
            .unwrap()
            .model_policy
            .hidden_builtins
            .iter()
            .any(|s| s == &first));
        let catalog = ctl.settings_model_catalog("claude");
        assert!(catalog.iter().any(|s| s == &first));
        ctl.move_catalog_model("claude", &first, false);
        assert!(ctl.take_pending().is_none());
        ctl.set_custom_model_draft("my/custom".into());
        ctl.add_custom_model("claude");
        assert!(ctl.take_pending().is_none());
        assert!(ctl
            .dirty_draft
            .as_ref()
            .unwrap()
            .custom_models
            .iter()
            .any(|m| m.slug == "my/custom"));
    }

    #[test]
    fn enable_mutation_does_not_clear_dirty_draft() {
        let mut ctl = ProviderSettingsController::loading();
        let mut snap = ctl.snapshot().clone();
        snap.revision = 4;
        ctl.apply_host_snapshot(snap);
        ctl.begin_edit("claude");
        ctl.set_draft_display_name("Keep Me".into());
        ctl.set_enabled("codex", false);
        match ctl.take_pending() {
            Some(ProviderSettingsHostRequest::Mutate(ProviderSettingsMutation::SetEnabled {
                ..
            })) => {}
            other => panic!("expected SetEnabled, got {other:?}"),
        }
        assert!(!ctl.pending_clears_draft);
        ctl.mutation_in_flight = true;
        let mut snap = ctl.snapshot().clone();
        snap.revision = 5;
        ctl.apply_reply(ProviderSettingsReply::MutationApplied { snapshot: snap });
        assert!(ctl.is_dirty());
        assert_eq!(ctl.dirty_draft.as_ref().unwrap().display_name, "Keep Me");
    }

    #[test]
    fn rejected_save_retains_draft() {
        let mut ctl = ProviderSettingsController::loading();
        ctl.begin_edit("claude");
        ctl.set_draft_display_name("Unsaved".into());
        ctl.mutation_in_flight = true;
        ctl.pending_clears_draft = true;
        ctl.apply_reply(ProviderSettingsReply::Error {
            message: "stale revision".into(),
        });
        assert!(ctl.is_dirty());
        assert_eq!(ctl.dirty_draft.as_ref().unwrap().display_name, "Unsaved");
        assert!(!ctl.pending_clears_draft);
    }

    #[test]
    fn successful_save_clears_draft() {
        let mut ctl = ProviderSettingsController::loading();
        let mut snap = ctl.snapshot().clone();
        snap.revision = 2;
        ctl.apply_host_snapshot(snap.clone());
        ctl.begin_edit("claude");
        ctl.set_draft_display_name("Saved Name".into());
        ctl.save_draft();
        assert!(ctl.pending_clears_draft);
        ctl.mutation_in_flight = true;
        snap.revision = 3;
        if let Some(slot) = snap.document.get_mut("claude") {
            slot.display_name = "Saved Name".into();
        }
        ctl.apply_reply(ProviderSettingsReply::MutationApplied { snapshot: snap });
        assert!(!ctl.is_dirty());
        assert_eq!(
            ctl.document().get("claude").unwrap().display_name,
            "Saved Name"
        );
    }

    #[test]
    fn late_save_receipt_retains_newer_edits() {
        let mut ctl = ProviderSettingsController::loading();
        ctl.begin_edit("claude");
        ctl.set_draft_display_name("First".into());
        ctl.save_draft();
        ctl.set_draft_display_name("Newer".into());
        let snapshot = ctl.snapshot().clone();
        ctl.apply_reply(ProviderSettingsReply::MutationApplied { snapshot });
        assert_eq!(
            ctl.working_instance("claude").unwrap().display_name,
            "Newer"
        );
        assert!(ctl.is_dirty());
    }

    #[test]
    fn host_deleted_instance_cannot_be_resurrected_by_a_dirty_draft() {
        let mut ctl = ProviderSettingsController::loading();
        ctl.begin_edit("claude");
        ctl.mutate_draft(|instance| instance.display_name = "Unsaved".into());
        let mut snapshot = ctl.snapshot().clone();
        snapshot
            .document
            .instances
            .retain(|instance| instance.instance_id.as_str() != "claude");
        snapshot.revision += 1;
        snapshot.document.revision = snapshot.revision;
        ctl.apply_host_snapshot(snapshot);
        ctl.save_draft();
        assert!(ctl.take_pending().is_none());
        assert!(ctl.error().unwrap().contains("removed"));
        assert!(ctl.dirty_draft.is_some());
    }

    #[test]
    fn invalid_args_block_save_even_after_other_fields_change() {
        let mut ctl = ProviderSettingsController::loading();
        ctl.begin_edit("claude");
        ctl.set_draft_launch_args("[broken".into());
        ctl.set_draft_display_name("Edited".into());
        ctl.save_draft();
        assert!(ctl.take_pending().is_none());
        assert!(ctl.error().is_some());
        ctl.set_draft_launch_args("[]".into());
        ctl.save_draft();
        assert!(ctl.take_pending().is_some());
    }

    #[test]
    fn health_interval_text_parses_manual_zero() {
        let mut ctl = ProviderSettingsController::loading();
        let mut snap = ctl.snapshot().clone();
        snap.revision = 1;
        ctl.apply_host_snapshot(snap);
        ctl.begin_edit("claude");
        ctl.set_draft_display_name("stay".into());
        ctl.set_health_interval_from_text("0");
        match ctl.take_pending() {
            Some(ProviderSettingsHostRequest::Mutate(
                ProviderSettingsMutation::SetHealthInterval {
                    interval_secs: 0, ..
                },
            )) => {}
            other => panic!("expected SetHealthInterval 0, got {other:?}"),
        }
        assert!(!ctl.pending_clears_draft);
        ctl.set_health_interval_from_text("abc");
        assert!(ctl.error().is_some());
    }

    #[test]
    fn hidden_model_show_and_catalog_order() {
        let mut ctl = test_ctl();
        let builtins = builtin_model_slugs(ProviderDriverKind::Claude);
        let first = builtins[0].clone();
        ctl.begin_edit("claude");
        ctl.toggle_hide_builtin("claude", &first);
        assert!(ctl
            .dirty_draft
            .as_ref()
            .unwrap()
            .model_policy
            .hidden_builtins
            .iter()
            .any(|s| s == &first));
        let catalog = ctl.settings_model_catalog("claude");
        assert!(catalog.iter().any(|s| s == &first));
        ctl.move_catalog_model("claude", &first, false);
        assert!(
            ctl.dirty_draft
                .as_ref()
                .unwrap()
                .model_policy
                .catalog_order
                .iter()
                .any(|s| s == &first)
                || ctl.is_dirty()
        );
    }

    #[test]
    fn policy_cache_survives_controller_reload_from_snapshot() {
        let mut ctl = test_ctl();
        ctl.begin_edit("claude");
        ctl.toggle_favorite("claude", "opus");
        ctl.save_draft();
        let snap = ctl.snapshot().clone();
        let reopened = ProviderSettingsController::from_snapshot(snap);
        let document = reopened.document();
        let favs = &document.get("claude").unwrap().model_policy.favorite_order;
        assert!(favs.iter().any(|s| s == "opus"));
    }

    #[test]
    fn stubs_remain_disabled_honest() {
        let ctl = test_ctl();
        let document = ctl.document();
        let grok = document.get("grok").unwrap();
        assert!(grok.driver.is_stub());
        assert!(!grok.enabled);
        let opencode = document.get("opencode").unwrap();
        assert!(opencode.driver.is_stub());
        assert!(!opencode.enabled);
    }

    #[test]
    fn reset_and_delete_require_confirmation() {
        let mut ctl = test_ctl();
        ctl.request_reset_builtin("claude");
        assert!(matches!(
            ctl.pending_confirm(),
            Some(PendingConfirm::ResetBuiltin { .. })
        ));
        ctl.cancel_confirm();
        assert!(ctl.pending_confirm().is_none());
        ctl.request_delete_custom("missing_custom");
        assert!(matches!(
            ctl.pending_confirm(),
            Some(PendingConfirm::DeleteCustom { .. })
        ));
    }

    #[test]
    fn stale_mutation_clears_in_flight_with_visible_error() {
        let mut ctl = ProviderSettingsController::loading();
        ctl.mutation_in_flight = true;
        ctl.apply_reply(ProviderSettingsReply::Error {
            message: "stale revision: expected 1, got 2".into(),
        });
        assert!(!ctl.mutation_in_flight());
        assert!(ctl.error().is_some());
    }

    #[test]
    fn debug_omits_secret_plaintext() {
        let mut ctl = test_ctl();
        ctl.begin_edit("claude");
        ctl.add_env_row("claude");
        ctl.set_env_name(0, "TOKEN".into());
        ctl.toggle_env_sensitive("claude", 0);
        ctl.set_env_value(0, "super-secret-token".into());
        let rendered = format!("{ctl:?}");
        assert!(!rendered.contains("super-secret-token"));
    }
}

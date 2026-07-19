use super::commands::verified_authenticated_local_project_root;
use super::replay_repair::{BrowserReplayLocatorSlot, BrowserReplayRecipeLocatorTarget};
use super::{BrowserError, BrowserLocator, BrowserViewport};
use serde::de::{DeserializeOwned, Error as _, MapAccess, SeqAccess, Visitor};
use serde::ser::{Error as _, SerializeStruct};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

pub const BROWSER_RECIPE_SCHEMA_VERSION: u32 = 1;
pub const MAX_BROWSER_RECIPE_WAIT_MS: u64 = 300_000;
const RECIPE_TEMP_PREFIX: &str = ".devmanager-browser-recipe.";
const MAX_RECIPE_TEMP_ENTRIES_TO_SCAN: usize = 1_024;
const MAX_RECIPE_TEMPS_TO_SCAVENGE: usize = 64;
const STALE_RECIPE_TEMP_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const BROWSER_RECIPE_DIGEST_DOMAIN: &[u8] = b"devmanager.browser-recipe-v1.sha256\0";
static RECIPE_WRITE_GATE: Mutex<()> = Mutex::new(());

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct BrowserRecipeDigestV1([u8; 32]);

impl BrowserRecipeDigestV1 {
    #[cfg(test)]
    fn bytes_for_test(&self) -> [u8; 32] {
        self.0
    }

    #[cfg(test)]
    pub(super) fn placeholder_for_test() -> Self {
        Self([0; 32])
    }
}

pub(crate) fn canonical_browser_recipe_digest(
    recipe: &BrowserRecipeV1,
) -> Result<BrowserRecipeDigestV1, BrowserError> {
    recipe.validate()?;
    let compact = serde_json::to_vec(recipe)
        .map_err(|error| invalid_recipe(format!("could not canonicalize recipe: {error}")))?;
    let mut digest = Sha256::new();
    digest.update(BROWSER_RECIPE_DIGEST_DOMAIN);
    digest.update(compact);
    Ok(BrowserRecipeDigestV1(digest.finalize().into()))
}

struct StrictJsonValue(Value);

impl<'de> Deserialize<'de> for StrictJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StrictValueVisitor;

        impl<'de> Visitor<'de> for StrictValueVisitor {
            type Value = StrictJsonValue;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON value without duplicate object members")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(StrictJsonValue(Value::Bool(value)))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(StrictJsonValue(Value::Number(value.into())))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(StrictJsonValue(Value::Number(value.into())))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                serde_json::Number::from_f64(value)
                    .map(Value::Number)
                    .map(StrictJsonValue)
                    .ok_or_else(|| E::custom("JSON number must be finite"))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                Ok(StrictJsonValue(Value::String(value.to_string())))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(StrictJsonValue(Value::String(value)))
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(StrictJsonValue(Value::Null))
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(StrictJsonValue(Value::Null))
            }

            fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
            where
                D: Deserializer<'de>,
            {
                StrictJsonValue::deserialize(deserializer)
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element::<StrictJsonValue>()? {
                    values.push(value.0);
                }
                Ok(StrictJsonValue(Value::Array(values)))
            }

            fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut values = serde_json::Map::new();
                while let Some(key) = object.next_key::<String>()? {
                    if values.contains_key(&key) {
                        let _ = object.next_value::<serde::de::IgnoredAny>()?;
                        return Err(A::Error::custom(format!("duplicate JSON member {key:?}")));
                    }
                    values.insert(key, object.next_value::<StrictJsonValue>()?.0);
                }
                Ok(StrictJsonValue(Value::Object(values)))
            }
        }

        deserializer.deserialize_any(StrictValueVisitor)
    }
}

fn deserialize_strict_document<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    let value = StrictJsonValue::deserialize(deserializer)?.0;
    serde_json::from_value(value).map_err(D::Error::custom)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserRecipeV1 {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub description: String,
    pub start_url: String,
    pub viewport: BrowserRecipeViewport,
    pub inputs: Vec<BrowserRecipeInput>,
    pub steps: Vec<BrowserRecipeStep>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct BrowserRecipeDocument {
    schema_version: u32,
    id: String,
    name: String,
    description: String,
    start_url: String,
    viewport: BrowserRecipeViewport,
    inputs: Vec<BrowserRecipeInput>,
    steps: Vec<BrowserRecipeStep>,
}

impl From<BrowserRecipeDocument> for BrowserRecipeV1 {
    fn from(document: BrowserRecipeDocument) -> Self {
        Self {
            schema_version: document.schema_version,
            id: document.id,
            name: document.name,
            description: document.description,
            start_url: document.start_url,
            viewport: document.viewport,
            inputs: document.inputs,
            steps: document.steps,
        }
    }
}

impl Serialize for BrowserRecipeV1 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(S::Error::custom)?;
        let mut state = serializer.serialize_struct("BrowserRecipeV1", 8)?;
        state.serialize_field("schemaVersion", &self.schema_version)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("name", &self.name)?;
        state.serialize_field("description", &self.description)?;
        state.serialize_field("startUrl", &self.start_url)?;
        state.serialize_field("viewport", &self.viewport)?;
        state.serialize_field("inputs", &self.inputs)?;
        state.serialize_field("steps", &self.steps)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for BrowserRecipeV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = StrictJsonValue::deserialize(deserializer)?.0;
        let version = schema_version_from_value(&value).map_err(D::Error::custom)?;
        if version != BROWSER_RECIPE_SCHEMA_VERSION {
            return Err(D::Error::custom(format!(
                "unsupported browser recipe schema version {version}; expected {BROWSER_RECIPE_SCHEMA_VERSION}"
            )));
        }
        let document: BrowserRecipeDocument =
            serde_json::from_value(value).map_err(D::Error::custom)?;
        let recipe = Self::from(document);
        recipe.validate().map_err(D::Error::custom)?;
        Ok(recipe)
    }
}

impl BrowserRecipeV1 {
    pub fn validate(&self) -> Result<(), BrowserError> {
        if self.schema_version != BROWSER_RECIPE_SCHEMA_VERSION {
            return Err(BrowserError::UnsupportedRecipeVersion {
                version: self.schema_version,
            });
        }
        if !is_safe_recipe_id(&self.id) {
            return Err(invalid_recipe("recipe id is not a safe slug"));
        }
        require_nonblank(&self.name, "recipe name")?;
        reject_obvious_secret(&self.name, "recipe name")?;
        reject_obvious_secret(&self.description, "recipe description")?;
        validate_safe_url(&self.start_url, "recipe start URL")?;
        self.viewport.validate()?;

        let mut input_names = HashSet::new();
        let mut inputs = HashMap::new();
        for input in &self.inputs {
            input.validate()?;
            if !input_names.insert(input.name.as_str()) {
                return Err(invalid_recipe("recipe input names must be unique"));
            }
            inputs.insert(input.name.as_str(), input.kind);
        }

        if self.steps.is_empty() {
            return Err(invalid_recipe("recipe requires at least one step"));
        }
        let mut step_ids = HashSet::new();
        for step in &self.steps {
            if !is_safe_recipe_id(&step.id) {
                return Err(invalid_recipe("recipe step id is not a safe slug"));
            }
            if !step_ids.insert(step.id.as_str()) {
                return Err(invalid_recipe("recipe step ids must be unique"));
            }
            step.validate(&inputs)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BrowserRecipeViewport {
    pub width: u32,
    pub height: u32,
    pub scale_percent: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct BrowserRecipeViewportDocument {
    width: u32,
    height: u32,
    scale_percent: u16,
}

impl<'de> Deserialize<'de> for BrowserRecipeViewport {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let document =
            deserialize_strict_document::<_, BrowserRecipeViewportDocument>(deserializer)?;
        let viewport = Self {
            width: document.width,
            height: document.height,
            scale_percent: document.scale_percent,
        };
        viewport.validate().map_err(D::Error::custom)?;
        Ok(viewport)
    }
}

impl BrowserRecipeViewport {
    fn validate(&self) -> Result<(), BrowserError> {
        if self.width == 0 || self.height == 0 || self.width > 16_384 || self.height > 16_384 {
            return Err(invalid_recipe(
                "recipe viewport dimensions are out of range",
            ));
        }
        if !(25..=500).contains(&self.scale_percent) {
            return Err(invalid_recipe("recipe viewport scale is out of range"));
        }
        Ok(())
    }
}

impl Default for BrowserRecipeViewport {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            scale_percent: 100,
        }
    }
}

impl From<BrowserViewport> for BrowserRecipeViewport {
    fn from(viewport: BrowserViewport) -> Self {
        Self {
            width: viewport.width,
            height: viewport.height,
            scale_percent: viewport.scale_percent,
        }
    }
}

impl From<BrowserRecipeViewport> for BrowserViewport {
    fn from(viewport: BrowserRecipeViewport) -> Self {
        Self {
            width: viewport.width,
            height: viewport.height,
            scale_percent: viewport.scale_percent,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserRecipeInput {
    pub name: String,
    pub kind: BrowserRecipeInputKind,
    pub default_value: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct BrowserRecipeInputDocument {
    name: String,
    kind: BrowserRecipeInputKind,
    #[serde(default)]
    default_value: Option<String>,
}

impl<'de> Deserialize<'de> for BrowserRecipeInput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let document = deserialize_strict_document::<_, BrowserRecipeInputDocument>(deserializer)?;
        let input = Self {
            name: document.name,
            kind: document.kind,
            default_value: document.default_value,
        };
        input.validate().map_err(D::Error::custom)?;
        Ok(input)
    }
}

impl BrowserRecipeInput {
    fn validate(&self) -> Result<(), BrowserError> {
        require_nonblank(&self.name, "recipe input name")?;
        if self.name.trim() != self.name {
            return Err(invalid_recipe(
                "recipe input names cannot have surrounding whitespace",
            ));
        }
        if self.default_value.is_some() {
            match self.kind {
                BrowserRecipeInputKind::Secret => {
                    return Err(invalid_recipe(
                        "secret input default values cannot be serialized",
                    ));
                }
                BrowserRecipeInputKind::File => {
                    return Err(invalid_recipe(
                        "file input default values cannot be serialized",
                    ));
                }
                BrowserRecipeInputKind::Text | BrowserRecipeInputKind::Url => {}
            }
        }
        if looks_sensitive_name(&self.name) && self.kind != BrowserRecipeInputKind::Secret {
            return Err(invalid_recipe(
                "credential-named recipe inputs must use the secret kind",
            ));
        }
        if let Some(default_value) = self.default_value.as_deref() {
            if self.kind == BrowserRecipeInputKind::Url {
                validate_safe_url(default_value, "URL input default")?;
            } else {
                reject_obvious_secret(default_value, "recipe input default")?;
            }
        }
        Ok(())
    }
}

impl Serialize for BrowserRecipeInput {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(S::Error::custom)?;
        let field_count = if self.default_value.is_some() { 3 } else { 2 };
        let mut state = serializer.serialize_struct("BrowserRecipeInput", field_count)?;
        state.serialize_field("name", &self.name)?;
        state.serialize_field("kind", &self.kind)?;
        if let Some(default_value) = &self.default_value {
            state.serialize_field("defaultValue", default_value)?;
        }
        state.end()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum BrowserRecipeInputKind {
    Text,
    Url,
    File,
    Secret,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BrowserRecipeLocator {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accessibility_role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accessibility_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub css_selectors: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct BrowserRecipeLocatorDocument {
    #[serde(default)]
    accessibility_role: Option<String>,
    #[serde(default)]
    accessibility_name: Option<String>,
    #[serde(default)]
    test_id: Option<String>,
    #[serde(default)]
    css_selectors: Vec<String>,
}

impl<'de> Deserialize<'de> for BrowserRecipeLocator {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let document =
            deserialize_strict_document::<_, BrowserRecipeLocatorDocument>(deserializer)?;
        let locator = Self {
            accessibility_role: document.accessibility_role,
            accessibility_name: document.accessibility_name,
            test_id: document.test_id,
            css_selectors: document.css_selectors,
        };
        locator.validate().map_err(D::Error::custom)?;
        Ok(locator)
    }
}

impl BrowserRecipeLocator {
    fn validate(&self) -> Result<(), BrowserError> {
        for value in [
            self.accessibility_role.as_deref(),
            self.accessibility_name.as_deref(),
            self.test_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            require_nonblank(value, "recipe locator fallback")?;
            reject_obvious_secret(value, "recipe locator fallback")?;
            if value.trim() != value {
                return Err(invalid_recipe(
                    "recipe locator fallbacks cannot have surrounding whitespace",
                ));
            }
        }
        if self.accessibility_role.is_some() != self.accessibility_name.is_some() {
            return Err(invalid_recipe(
                "recipe accessibility locator requires both role and name",
            ));
        }
        let mut selectors = HashSet::new();
        for selector in &self.css_selectors {
            require_nonblank(selector, "recipe CSS locator")?;
            reject_obvious_secret(selector, "recipe CSS locator")?;
            if selector.trim() != selector || !selectors.insert(selector.as_str()) {
                return Err(invalid_recipe(
                    "recipe CSS locators must be trimmed and unique",
                ));
            }
        }
        if self.test_id.is_none()
            && self.accessibility_role.is_none()
            && self.css_selectors.is_empty()
        {
            return Err(invalid_recipe(
                "recipe locator requires a semantic fallback",
            ));
        }
        Ok(())
    }

    fn looks_sensitive(&self) -> bool {
        self.accessibility_name
            .iter()
            .chain(self.test_id.iter())
            .chain(self.css_selectors.iter())
            .any(|value| looks_sensitive_name(value))
    }

    fn looks_file_input(&self) -> bool {
        self.accessibility_name
            .iter()
            .chain(self.test_id.iter())
            .any(|value| {
                let normalized = normalized_name(value);
                normalized.starts_with("upload")
                    || normalized.ends_with("upload")
                    || normalized.contains("fileupload")
                    || normalized.contains("uploadfile")
            })
            || self.css_selectors.iter().any(|selector| {
                let normalized = normalized_name(selector);
                normalized.contains("typefile")
            })
    }
}

impl From<BrowserLocator> for BrowserRecipeLocator {
    fn from(locator: BrowserLocator) -> Self {
        Self {
            accessibility_role: locator.accessibility_role,
            accessibility_name: locator.accessibility_name,
            test_id: locator.test_id,
            css_selectors: locator.css_selectors,
        }
    }
}

impl From<BrowserRecipeLocator> for BrowserLocator {
    fn from(locator: BrowserRecipeLocator) -> Self {
        Self {
            accessibility_role: locator.accessibility_role,
            accessibility_name: locator.accessibility_name,
            test_id: locator.test_id,
            css_selectors: locator.css_selectors,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(
    deny_unknown_fields,
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum BrowserRecipeValue {
    Literal { value: String },
    Input { name: String },
}

#[derive(Deserialize)]
#[serde(
    deny_unknown_fields,
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum BrowserRecipeValueDocument {
    Literal { value: String },
    Input { name: String },
}

impl<'de> Deserialize<'de> for BrowserRecipeValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let document = deserialize_strict_document::<_, BrowserRecipeValueDocument>(deserializer)?;
        let value = match document {
            BrowserRecipeValueDocument::Literal { value } => Self::Literal { value },
            BrowserRecipeValueDocument::Input { name } => Self::Input { name },
        };
        validate_value_context_free(&value, None).map_err(D::Error::custom)?;
        Ok(value)
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BrowserRecipeStep {
    #[serde(serialize_with = "serialize_recipe_step_id")]
    pub id: String,
    pub action: BrowserRecipeAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<BrowserRecipeWait>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assertions: Vec<BrowserRecipeAssertion>,
}

fn serialize_recipe_step_id<S>(id: &String, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if !is_safe_recipe_id(id) {
        return Err(S::Error::custom("recipe step id is not a safe slug"));
    }
    id.serialize(serializer)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct BrowserRecipeStepDocument {
    id: String,
    action: BrowserRecipeAction,
    #[serde(default)]
    wait: Option<BrowserRecipeWait>,
    #[serde(default)]
    assertions: Vec<BrowserRecipeAssertion>,
}

impl<'de> Deserialize<'de> for BrowserRecipeStep {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let document = deserialize_strict_document::<_, BrowserRecipeStepDocument>(deserializer)?;
        if !is_safe_recipe_id(&document.id) {
            return Err(D::Error::custom("recipe step id is not a safe slug"));
        }
        Ok(Self {
            id: document.id,
            action: document.action,
            wait: document.wait,
            assertions: document.assertions,
        })
    }
}

impl BrowserRecipeStep {
    fn validate(&self, inputs: &HashMap<&str, BrowserRecipeInputKind>) -> Result<(), BrowserError> {
        self.action.validate(inputs)?;
        if let Some(wait) = &self.wait {
            wait.validate(inputs)?;
        }
        for assertion in &self.assertions {
            assertion.validate(inputs)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(
    deny_unknown_fields,
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum BrowserRecipeAction {
    CreateTab {
        tab: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<BrowserRecipeValue>,
    },
    SelectTab {
        tab: String,
    },
    CloseTab {
        tab: String,
    },
    Back,
    Forward,
    Reload,
    SetViewport {
        viewport: BrowserRecipeViewport,
    },
    CdpMarker {
        method: String,
    },
    Navigate {
        url: BrowserRecipeValue,
    },
    Click {
        locator: BrowserRecipeLocator,
    },
    Hover {
        locator: BrowserRecipeLocator,
    },
    Focus {
        locator: BrowserRecipeLocator,
    },
    Type {
        locator: BrowserRecipeLocator,
        value: BrowserRecipeValue,
    },
    Clear {
        locator: BrowserRecipeLocator,
    },
    Select {
        locator: BrowserRecipeLocator,
        values: Vec<BrowserRecipeValue>,
    },
    Keypress {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        locator: Option<BrowserRecipeLocator>,
        key: BrowserRecipeValue,
    },
    Scroll {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        locator: Option<BrowserRecipeLocator>,
        delta_x: i32,
        delta_y: i32,
    },
    DragDrop {
        source: BrowserRecipeLocator,
        destination: BrowserRecipeLocator,
    },
    Upload {
        locator: BrowserRecipeLocator,
        file: BrowserRecipeValue,
    },
    Download {
        locator: BrowserRecipeLocator,
    },
    Wait {
        condition: BrowserRecipeWait,
    },
    Screenshot {
        full_page: bool,
    },
}

#[derive(Deserialize)]
#[serde(
    deny_unknown_fields,
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum BrowserRecipeActionDocument {
    CreateTab {
        tab: String,
        #[serde(default)]
        url: Option<BrowserRecipeValue>,
    },
    SelectTab {
        tab: String,
    },
    CloseTab {
        tab: String,
    },
    Back,
    Forward,
    Reload,
    SetViewport {
        viewport: BrowserRecipeViewport,
    },
    CdpMarker {
        method: String,
    },
    Navigate {
        url: BrowserRecipeValue,
    },
    Click {
        locator: BrowserRecipeLocator,
    },
    Hover {
        locator: BrowserRecipeLocator,
    },
    Focus {
        locator: BrowserRecipeLocator,
    },
    Type {
        locator: BrowserRecipeLocator,
        value: BrowserRecipeValue,
    },
    Clear {
        locator: BrowserRecipeLocator,
    },
    Select {
        locator: BrowserRecipeLocator,
        values: Vec<BrowserRecipeValue>,
    },
    Keypress {
        #[serde(default)]
        locator: Option<BrowserRecipeLocator>,
        key: BrowserRecipeValue,
    },
    Scroll {
        #[serde(default)]
        locator: Option<BrowserRecipeLocator>,
        delta_x: i32,
        delta_y: i32,
    },
    DragDrop {
        source: BrowserRecipeLocator,
        destination: BrowserRecipeLocator,
    },
    Upload {
        locator: BrowserRecipeLocator,
        file: BrowserRecipeValue,
    },
    Download {
        locator: BrowserRecipeLocator,
    },
    Wait {
        condition: BrowserRecipeWait,
    },
    Screenshot {
        full_page: bool,
    },
}

impl From<BrowserRecipeActionDocument> for BrowserRecipeAction {
    fn from(document: BrowserRecipeActionDocument) -> Self {
        match document {
            BrowserRecipeActionDocument::CreateTab { tab, url } => Self::CreateTab { tab, url },
            BrowserRecipeActionDocument::SelectTab { tab } => Self::SelectTab { tab },
            BrowserRecipeActionDocument::CloseTab { tab } => Self::CloseTab { tab },
            BrowserRecipeActionDocument::Back => Self::Back,
            BrowserRecipeActionDocument::Forward => Self::Forward,
            BrowserRecipeActionDocument::Reload => Self::Reload,
            BrowserRecipeActionDocument::SetViewport { viewport } => Self::SetViewport { viewport },
            BrowserRecipeActionDocument::CdpMarker { method } => Self::CdpMarker { method },
            BrowserRecipeActionDocument::Navigate { url } => Self::Navigate { url },
            BrowserRecipeActionDocument::Click { locator } => Self::Click { locator },
            BrowserRecipeActionDocument::Hover { locator } => Self::Hover { locator },
            BrowserRecipeActionDocument::Focus { locator } => Self::Focus { locator },
            BrowserRecipeActionDocument::Type { locator, value } => Self::Type { locator, value },
            BrowserRecipeActionDocument::Clear { locator } => Self::Clear { locator },
            BrowserRecipeActionDocument::Select { locator, values } => {
                Self::Select { locator, values }
            }
            BrowserRecipeActionDocument::Keypress { locator, key } => {
                Self::Keypress { locator, key }
            }
            BrowserRecipeActionDocument::Scroll {
                locator,
                delta_x,
                delta_y,
            } => Self::Scroll {
                locator,
                delta_x,
                delta_y,
            },
            BrowserRecipeActionDocument::DragDrop {
                source,
                destination,
            } => Self::DragDrop {
                source,
                destination,
            },
            BrowserRecipeActionDocument::Upload { locator, file } => Self::Upload { locator, file },
            BrowserRecipeActionDocument::Download { locator } => Self::Download { locator },
            BrowserRecipeActionDocument::Wait { condition } => Self::Wait { condition },
            BrowserRecipeActionDocument::Screenshot { full_page } => Self::Screenshot { full_page },
        }
    }
}

impl<'de> Deserialize<'de> for BrowserRecipeAction {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = StrictJsonValue::deserialize(deserializer)?.0;
        let document = serde_json::from_value::<BrowserRecipeActionDocument>(value)
            .map_err(D::Error::custom)?;
        let action = Self::from(document);
        action.validate_context_free().map_err(D::Error::custom)?;
        Ok(action)
    }
}

impl BrowserRecipeAction {
    fn validate_context_free(&self) -> Result<(), BrowserError> {
        match self {
            Self::CreateTab { tab, url } => {
                validate_recipe_tab_alias(tab)?;
                if let Some(url) = url {
                    validate_value_context_free(url, Some(LiteralKind::Url))?;
                }
                Ok(())
            }
            Self::SelectTab { tab } | Self::CloseTab { tab } => validate_recipe_tab_alias(tab),
            Self::Back | Self::Forward | Self::Reload => Ok(()),
            Self::SetViewport { viewport } => viewport.validate(),
            Self::CdpMarker { method } => validate_cdp_marker_method(method),
            Self::Navigate { url } => validate_value_context_free(url, Some(LiteralKind::Url)),
            Self::Click { locator }
            | Self::Hover { locator }
            | Self::Focus { locator }
            | Self::Clear { locator }
            | Self::Download { locator } => locator.validate(),
            Self::Type { locator, value } => {
                locator.validate()?;
                validate_value_context_free(value, Some(LiteralKind::Text))?;
                if locator.looks_file_input() {
                    return Err(invalid_recipe("file targets require a typed upload action"));
                }
                if locator.looks_sensitive() && !matches!(value, BrowserRecipeValue::Input { .. }) {
                    return Err(invalid_recipe(
                        "password targets require a typed secret input reference",
                    ));
                }
                Ok(())
            }
            Self::Select { locator, values } => {
                locator.validate()?;
                if values.is_empty() {
                    return Err(invalid_recipe("recipe select action requires values"));
                }
                for value in values {
                    validate_value_context_free(value, Some(LiteralKind::Text))?;
                }
                Ok(())
            }
            Self::Keypress { locator, key } => {
                if let Some(locator) = locator {
                    locator.validate()?;
                }
                validate_value_context_free(key, Some(LiteralKind::Text))
            }
            Self::Scroll {
                locator,
                delta_x,
                delta_y,
            } => {
                if let Some(locator) = locator {
                    locator.validate()?;
                }
                if *delta_x == 0 && *delta_y == 0 {
                    return Err(invalid_recipe("recipe scroll action requires a delta"));
                }
                Ok(())
            }
            Self::DragDrop {
                source,
                destination,
            } => {
                source.validate()?;
                destination.validate()
            }
            Self::Upload { locator, file } => {
                locator.validate()?;
                validate_value_context_free(file, None)?;
                if !matches!(file, BrowserRecipeValue::Input { .. }) {
                    return Err(invalid_recipe(
                        "recipe upload file must reference a typed File input",
                    ));
                }
                Ok(())
            }
            Self::Wait { condition } => condition.validate_context_free(),
            Self::Screenshot { .. } => Ok(()),
        }
    }

    fn validate(&self, inputs: &HashMap<&str, BrowserRecipeInputKind>) -> Result<(), BrowserError> {
        self.validate_context_free()?;
        match self {
            Self::CreateTab { url, .. } => {
                if let Some(url) = url {
                    validate_value(
                        url,
                        inputs,
                        &[BrowserRecipeInputKind::Url],
                        LiteralKind::Url,
                    )?;
                }
                Ok(())
            }
            Self::SelectTab { .. }
            | Self::CloseTab { .. }
            | Self::Back
            | Self::Forward
            | Self::Reload
            | Self::SetViewport { .. }
            | Self::CdpMarker { .. } => Ok(()),
            Self::Navigate { url } => validate_value(
                url,
                inputs,
                &[BrowserRecipeInputKind::Url],
                LiteralKind::Url,
            ),
            Self::Click { locator }
            | Self::Hover { locator }
            | Self::Focus { locator }
            | Self::Clear { locator }
            | Self::Download { locator } => locator.validate(),
            Self::Type { locator, value } => {
                locator.validate()?;
                if locator.looks_file_input() {
                    return Err(invalid_recipe("file targets require a typed upload action"));
                }
                if locator.looks_sensitive() {
                    validate_value(
                        value,
                        inputs,
                        &[BrowserRecipeInputKind::Secret],
                        LiteralKind::Forbidden,
                    )
                } else {
                    validate_value(
                        value,
                        inputs,
                        &[BrowserRecipeInputKind::Text, BrowserRecipeInputKind::Secret],
                        LiteralKind::Text,
                    )
                }
            }
            Self::Select { locator, values } => {
                locator.validate()?;
                if values.is_empty() {
                    return Err(invalid_recipe("recipe select action requires values"));
                }
                for value in values {
                    validate_value(
                        value,
                        inputs,
                        &[BrowserRecipeInputKind::Text],
                        LiteralKind::Text,
                    )?;
                }
                Ok(())
            }
            Self::Keypress { locator, key } => {
                if let Some(locator) = locator {
                    locator.validate()?;
                }
                validate_value(
                    key,
                    inputs,
                    &[BrowserRecipeInputKind::Text],
                    LiteralKind::Text,
                )
            }
            Self::Scroll {
                locator,
                delta_x,
                delta_y,
            } => {
                if let Some(locator) = locator {
                    locator.validate()?;
                }
                if *delta_x == 0 && *delta_y == 0 {
                    return Err(invalid_recipe("recipe scroll action requires a delta"));
                }
                Ok(())
            }
            Self::DragDrop {
                source,
                destination,
            } => {
                source.validate()?;
                destination.validate()
            }
            Self::Upload { locator, file } => {
                locator.validate()?;
                validate_value(
                    file,
                    inputs,
                    &[BrowserRecipeInputKind::File],
                    LiteralKind::Forbidden,
                )
            }
            Self::Wait { condition } => condition.validate(inputs),
            Self::Screenshot { .. } => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(
    deny_unknown_fields,
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum BrowserRecipeWait {
    Duration {
        duration_ms: u64,
    },
    Url {
        value: BrowserRecipeValue,
        exact: bool,
        timeout_ms: u64,
    },
    Load {
        timeout_ms: u64,
    },
    NetworkIdle {
        timeout_ms: u64,
    },
    ElementPresent {
        locator: BrowserRecipeLocator,
        timeout_ms: u64,
    },
    ElementVisible {
        locator: BrowserRecipeLocator,
        timeout_ms: u64,
    },
    ElementHidden {
        locator: BrowserRecipeLocator,
        timeout_ms: u64,
    },
    TextPresent {
        value: BrowserRecipeValue,
        timeout_ms: u64,
    },
    TextAbsent {
        value: BrowserRecipeValue,
        timeout_ms: u64,
    },
}

#[derive(Deserialize)]
#[serde(
    deny_unknown_fields,
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum BrowserRecipeWaitDocument {
    Duration {
        duration_ms: u64,
    },
    Url {
        value: BrowserRecipeValue,
        exact: bool,
        timeout_ms: u64,
    },
    Load {
        timeout_ms: u64,
    },
    NetworkIdle {
        timeout_ms: u64,
    },
    ElementPresent {
        locator: BrowserRecipeLocator,
        timeout_ms: u64,
    },
    ElementVisible {
        locator: BrowserRecipeLocator,
        timeout_ms: u64,
    },
    ElementHidden {
        locator: BrowserRecipeLocator,
        timeout_ms: u64,
    },
    TextPresent {
        value: BrowserRecipeValue,
        timeout_ms: u64,
    },
    TextAbsent {
        value: BrowserRecipeValue,
        timeout_ms: u64,
    },
}

impl From<BrowserRecipeWaitDocument> for BrowserRecipeWait {
    fn from(document: BrowserRecipeWaitDocument) -> Self {
        match document {
            BrowserRecipeWaitDocument::Duration { duration_ms } => Self::Duration { duration_ms },
            BrowserRecipeWaitDocument::Url {
                value,
                exact,
                timeout_ms,
            } => Self::Url {
                value,
                exact,
                timeout_ms,
            },
            BrowserRecipeWaitDocument::Load { timeout_ms } => Self::Load { timeout_ms },
            BrowserRecipeWaitDocument::NetworkIdle { timeout_ms } => {
                Self::NetworkIdle { timeout_ms }
            }
            BrowserRecipeWaitDocument::ElementPresent {
                locator,
                timeout_ms,
            } => Self::ElementPresent {
                locator,
                timeout_ms,
            },
            BrowserRecipeWaitDocument::ElementVisible {
                locator,
                timeout_ms,
            } => Self::ElementVisible {
                locator,
                timeout_ms,
            },
            BrowserRecipeWaitDocument::ElementHidden {
                locator,
                timeout_ms,
            } => Self::ElementHidden {
                locator,
                timeout_ms,
            },
            BrowserRecipeWaitDocument::TextPresent { value, timeout_ms } => {
                Self::TextPresent { value, timeout_ms }
            }
            BrowserRecipeWaitDocument::TextAbsent { value, timeout_ms } => {
                Self::TextAbsent { value, timeout_ms }
            }
        }
    }
}

impl<'de> Deserialize<'de> for BrowserRecipeWait {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let document = deserialize_strict_document::<_, BrowserRecipeWaitDocument>(deserializer)?;
        let wait = Self::from(document);
        wait.validate_context_free().map_err(D::Error::custom)?;
        Ok(wait)
    }
}

impl BrowserRecipeWait {
    fn validate_context_free(&self) -> Result<(), BrowserError> {
        match self {
            Self::Duration { duration_ms } => validate_wait_ms(*duration_ms, "wait duration"),
            Self::Url {
                value, timeout_ms, ..
            } => {
                validate_wait_ms(*timeout_ms, "wait timeout")?;
                validate_value_context_free(value, Some(LiteralKind::Url))
            }
            Self::Load { timeout_ms } | Self::NetworkIdle { timeout_ms } => {
                validate_wait_ms(*timeout_ms, "wait timeout")
            }
            Self::ElementPresent {
                locator,
                timeout_ms,
            }
            | Self::ElementVisible {
                locator,
                timeout_ms,
            }
            | Self::ElementHidden {
                locator,
                timeout_ms,
            } => {
                locator.validate()?;
                validate_wait_ms(*timeout_ms, "wait timeout")
            }
            Self::TextPresent {
                value, timeout_ms, ..
            }
            | Self::TextAbsent {
                value, timeout_ms, ..
            } => {
                validate_wait_ms(*timeout_ms, "wait timeout")?;
                validate_value_context_free(value, Some(LiteralKind::Text))
            }
        }
    }

    fn validate(&self, inputs: &HashMap<&str, BrowserRecipeInputKind>) -> Result<(), BrowserError> {
        self.validate_context_free()?;
        match self {
            Self::Duration { duration_ms } => validate_wait_ms(*duration_ms, "wait duration"),
            Self::Url {
                value, timeout_ms, ..
            } => {
                validate_wait_ms(*timeout_ms, "wait timeout")?;
                validate_value(
                    value,
                    inputs,
                    &[BrowserRecipeInputKind::Url],
                    LiteralKind::Url,
                )
            }
            Self::Load { timeout_ms } | Self::NetworkIdle { timeout_ms } => {
                validate_wait_ms(*timeout_ms, "wait timeout")
            }
            Self::ElementPresent {
                locator,
                timeout_ms,
            }
            | Self::ElementVisible {
                locator,
                timeout_ms,
            }
            | Self::ElementHidden {
                locator,
                timeout_ms,
            } => {
                locator.validate()?;
                validate_wait_ms(*timeout_ms, "wait timeout")
            }
            Self::TextPresent {
                value, timeout_ms, ..
            }
            | Self::TextAbsent {
                value, timeout_ms, ..
            } => {
                validate_wait_ms(*timeout_ms, "wait timeout")?;
                validate_value(
                    value,
                    inputs,
                    &[BrowserRecipeInputKind::Text],
                    LiteralKind::Text,
                )
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserRecipeElementState {
    Present,
    Absent,
    Visible,
    Hidden,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(
    deny_unknown_fields,
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum BrowserRecipeAssertion {
    Url {
        value: BrowserRecipeValue,
        exact: bool,
    },
    Title {
        value: BrowserRecipeValue,
        exact: bool,
    },
    Text {
        value: BrowserRecipeValue,
        present: bool,
    },
    Element {
        locator: BrowserRecipeLocator,
        state: BrowserRecipeElementState,
    },
    Value {
        locator: BrowserRecipeLocator,
        value: BrowserRecipeValue,
    },
}

#[derive(Deserialize)]
#[serde(
    deny_unknown_fields,
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum BrowserRecipeAssertionDocument {
    Url {
        value: BrowserRecipeValue,
        exact: bool,
    },
    Title {
        value: BrowserRecipeValue,
        exact: bool,
    },
    Text {
        value: BrowserRecipeValue,
        present: bool,
    },
    Element {
        locator: BrowserRecipeLocator,
        state: BrowserRecipeElementState,
    },
    Value {
        locator: BrowserRecipeLocator,
        value: BrowserRecipeValue,
    },
}

impl From<BrowserRecipeAssertionDocument> for BrowserRecipeAssertion {
    fn from(document: BrowserRecipeAssertionDocument) -> Self {
        match document {
            BrowserRecipeAssertionDocument::Url { value, exact } => Self::Url { value, exact },
            BrowserRecipeAssertionDocument::Title { value, exact } => Self::Title { value, exact },
            BrowserRecipeAssertionDocument::Text { value, present } => {
                Self::Text { value, present }
            }
            BrowserRecipeAssertionDocument::Element { locator, state } => {
                Self::Element { locator, state }
            }
            BrowserRecipeAssertionDocument::Value { locator, value } => {
                Self::Value { locator, value }
            }
        }
    }
}

impl<'de> Deserialize<'de> for BrowserRecipeAssertion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let document =
            deserialize_strict_document::<_, BrowserRecipeAssertionDocument>(deserializer)?;
        let assertion = Self::from(document);
        assertion
            .validate_context_free()
            .map_err(D::Error::custom)?;
        Ok(assertion)
    }
}

impl BrowserRecipeAssertion {
    fn validate_context_free(&self) -> Result<(), BrowserError> {
        match self {
            Self::Url { value, .. } => validate_value_context_free(value, Some(LiteralKind::Url)),
            Self::Title { value, .. } | Self::Text { value, .. } => {
                validate_value_context_free(value, Some(LiteralKind::Text))
            }
            Self::Element { locator, .. } => locator.validate(),
            Self::Value { locator, value } => {
                locator.validate()?;
                validate_value_context_free(value, Some(LiteralKind::Text))
            }
        }
    }

    fn validate(&self, inputs: &HashMap<&str, BrowserRecipeInputKind>) -> Result<(), BrowserError> {
        self.validate_context_free()?;
        match self {
            Self::Url { value, .. } => validate_value(
                value,
                inputs,
                &[BrowserRecipeInputKind::Url],
                LiteralKind::Url,
            ),
            Self::Title { value, .. } | Self::Text { value, .. } => validate_value(
                value,
                inputs,
                &[BrowserRecipeInputKind::Text],
                LiteralKind::Text,
            ),
            Self::Element { locator, .. } => locator.validate(),
            Self::Value { locator, value } => {
                locator.validate()?;
                validate_value(
                    value,
                    inputs,
                    &[BrowserRecipeInputKind::Text],
                    LiteralKind::Text,
                )
            }
        }
    }
}

#[derive(Clone, Copy)]
enum LiteralKind {
    Forbidden,
    Text,
    Url,
}

fn validate_value_context_free(
    value: &BrowserRecipeValue,
    literal_kind: Option<LiteralKind>,
) -> Result<(), BrowserError> {
    match value {
        BrowserRecipeValue::Input { name } => {
            require_nonblank(name, "recipe input reference")?;
            if name.trim() != name {
                return Err(invalid_recipe(
                    "recipe input references cannot have surrounding whitespace",
                ));
            }
            Ok(())
        }
        BrowserRecipeValue::Literal { value } => match literal_kind {
            Some(LiteralKind::Forbidden) => Err(invalid_recipe(
                "recipe value must reference a typed input and cannot be literal",
            )),
            Some(LiteralKind::Url) => validate_safe_url(value, "recipe URL value"),
            Some(LiteralKind::Text) | None => {
                require_nonblank(value, "recipe literal value")?;
                reject_obvious_secret(value, "recipe literal value")
            }
        },
    }
}

fn validate_value(
    value: &BrowserRecipeValue,
    inputs: &HashMap<&str, BrowserRecipeInputKind>,
    allowed_input_kinds: &[BrowserRecipeInputKind],
    literal_kind: LiteralKind,
) -> Result<(), BrowserError> {
    validate_value_context_free(value, Some(literal_kind))?;
    match value {
        BrowserRecipeValue::Literal { value } => match literal_kind {
            LiteralKind::Forbidden => Err(invalid_recipe(
                "recipe value must reference a typed input and cannot be literal",
            )),
            LiteralKind::Text => {
                require_nonblank(value, "recipe literal value")?;
                reject_obvious_secret(value, "recipe literal value")
            }
            LiteralKind::Url => validate_safe_url(value, "recipe URL value"),
        },
        BrowserRecipeValue::Input { name } => {
            require_nonblank(name, "recipe input reference")?;
            let Some(kind) = inputs.get(name.as_str()) else {
                return Err(invalid_recipe("recipe input reference does not exist"));
            };
            if !allowed_input_kinds.contains(kind) {
                return Err(invalid_recipe(
                    "recipe input reference has the wrong type for its use",
                ));
            }
            Ok(())
        }
    }
}

fn validate_wait_ms(value: u64, label: &str) -> Result<(), BrowserError> {
    if value == 0 || value > MAX_BROWSER_RECIPE_WAIT_MS {
        return Err(invalid_recipe(format!(
            "recipe {label} must be between 1 and {MAX_BROWSER_RECIPE_WAIT_MS} milliseconds"
        )));
    }
    Ok(())
}

fn require_nonblank(value: &str, label: &str) -> Result<(), BrowserError> {
    if value.trim().is_empty() {
        Err(invalid_recipe(format!("{label} cannot be blank")))
    } else {
        Ok(())
    }
}

fn validate_recipe_tab_alias(tab: &str) -> Result<(), BrowserError> {
    if !is_safe_recipe_id(tab) || tab.len() > 64 {
        return Err(invalid_recipe("recipe tab alias is not a safe slug"));
    }
    Ok(())
}

fn validate_cdp_marker_method(method: &str) -> Result<(), BrowserError> {
    if method.is_empty()
        || method.len() > 128
        || method.trim() != method
        || !method
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_'))
    {
        return Err(invalid_recipe("recipe CDP marker method is invalid"));
    }
    Ok(())
}

fn reject_obvious_secret(value: &str, label: &str) -> Result<(), BrowserError> {
    if super::automation::browser_text_contains_secret(value) {
        Err(invalid_recipe(format!(
            "{label} contains credential-like material"
        )))
    } else {
        Ok(())
    }
}

pub(crate) fn validate_safe_url(value: &str, label: &str) -> Result<(), BrowserError> {
    if super::validate_browser_url(value).is_err() {
        return Err(invalid_recipe(format!("{label} is not a safe browser URL")));
    }
    if value.eq_ignore_ascii_case("about:blank") {
        return Ok(());
    }
    let authority = value
        .split_once("://")
        .map(|(_, remainder)| remainder)
        .unwrap_or_default()
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if authority.contains('@') || super::automation::browser_text_contains_secret(value) {
        return Err(invalid_recipe(format!(
            "{label} cannot contain credential material"
        )));
    }
    if let Some((_, query_and_fragment)) = value.split_once('?') {
        let query = query_and_fragment.split('#').next().unwrap_or_default();
        if query.split('&').any(|pair| {
            let key = pair.split('=').next().unwrap_or_default();
            looks_sensitive_name(key)
        }) {
            return Err(invalid_recipe(format!(
                "{label} cannot contain credential query parameters"
            )));
        }
    }
    Ok(())
}

fn looks_sensitive_name(value: &str) -> bool {
    let normalized = normalized_name(value);
    [
        "password",
        "passwd",
        "secret",
        "token",
        "authorization",
        "apikey",
        "privatekey",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

fn normalized_name(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn invalid_recipe(message: impl Into<String>) -> BrowserError {
    BrowserError::InvalidRecipe {
        message: message.into(),
    }
}

fn schema_version_from_value(value: &Value) -> Result<u32, String> {
    let version = value
        .as_object()
        .and_then(|document| document.get("schemaVersion"))
        .and_then(Value::as_u64)
        .ok_or_else(|| "browser recipe schemaVersion must be an unsigned integer".to_string())?;
    u32::try_from(version)
        .map_err(|_| "browser recipe schemaVersion is outside the supported range".to_string())
}

fn is_safe_recipe_id(recipe_id: &str) -> bool {
    let Some(first) = recipe_id.chars().next() else {
        return false;
    };
    let Some(last) = recipe_id.chars().next_back() else {
        return false;
    };
    recipe_id.len() <= 128
        && first.is_ascii_alphanumeric()
        && last.is_ascii_alphanumeric()
        && recipe_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        && !super::automation::browser_text_contains_secret(recipe_id)
}

pub fn recipe_path(
    project_root: impl AsRef<Path>,
    recipe_id: &str,
) -> Result<PathBuf, BrowserError> {
    if !is_safe_recipe_id(recipe_id) {
        return Err(invalid_recipe("recipe id is not a safe slug"));
    }
    Ok(project_root
        .as_ref()
        .join(".devmanager")
        .join("browser-workflows")
        .join(format!("{recipe_id}.json")))
}

pub fn save_recipe(
    project_root: impl AsRef<Path>,
    recipe: &BrowserRecipeV1,
) -> Result<PathBuf, BrowserError> {
    save_recipe_with_overwrite_policy(project_root.as_ref(), recipe, true)
        .map(|(path, _overwrote_existing)| path)
}

pub(crate) fn recipe_exists(
    project_root: impl AsRef<Path>,
    recipe_id: &str,
) -> Result<bool, BrowserError> {
    let expected_path = recipe_path(&project_root, recipe_id)?;
    let Some(parent) = verified_workflow_directory(project_root.as_ref(), false)? else {
        return Ok(false);
    };
    let path = parent.join(format!("{recipe_id}.json"));
    if path != expected_path {
        return Err(BrowserError::OutsideWorkspace { path });
    }
    let boundary_verifier = OsRecipeBoundaryVerifier {
        project_root: project_root.as_ref(),
    };
    boundary_verifier.verify(RecipeIoBoundary::BeforeList, &parent, &parent)?;
    validate_existing_path(&path, RecipePathKind::RegularFile)
}

pub(crate) fn save_recipe_with_overwrite_policy(
    project_root: impl AsRef<Path>,
    recipe: &BrowserRecipeV1,
    allow_overwrite: bool,
) -> Result<(PathBuf, bool), BrowserError> {
    recipe.validate()?;
    let _write_guard = RECIPE_WRITE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    save_recipe_with_overwrite_policy_under_gate(project_root.as_ref(), recipe, allow_overwrite)
}

fn save_recipe_with_overwrite_policy_under_gate(
    project_root: &Path,
    recipe: &BrowserRecipeV1,
    allow_overwrite: bool,
) -> Result<(PathBuf, bool), BrowserError> {
    let parent = verified_workflow_directory(project_root, true)?
        .ok_or_else(|| invalid_recipe("recipe workflow directory could not be prepared"))?;
    let path = parent.join(format!("{}.json", recipe.id));
    let boundary_verifier = OsRecipeBoundaryVerifier { project_root };
    boundary_verifier.verify(RecipeIoBoundary::BeforeList, &parent, &parent)?;
    scavenge_stale_recipe_temps(&parent)?;
    let overwrote_existing = validate_existing_path(&path, RecipePathKind::RegularFile)?;
    if overwrote_existing && !allow_overwrite {
        return Err(invalid_recipe(
            "recording save requires destructive overwrite approval",
        ));
    }
    let mut json = serde_json::to_vec_pretty(recipe)
        .map_err(|error| invalid_recipe(format!("could not serialize recipe: {error}")))?;
    json.push(b'\n');
    if allow_overwrite {
        atomic_write_with_boundaries(&path, &json, &OsRecipeFileReplacer, &boundary_verifier)?;
    } else {
        atomic_write_with_boundaries(
            &path,
            &json,
            &NoClobberRecipeFileReplacer,
            &boundary_verifier,
        )?;
    }
    Ok((path, overwrote_existing))
}

pub fn load_recipe(
    project_root: impl AsRef<Path>,
    recipe_id: &str,
) -> Result<BrowserRecipeV1, BrowserError> {
    let expected_path = recipe_path(&project_root, recipe_id)?;
    let Some(parent) = verified_workflow_directory(project_root.as_ref(), false)? else {
        return Err(BrowserError::MissingFile {
            path: expected_path,
        });
    };
    let path = parent.join(format!("{recipe_id}.json"));
    let boundary_verifier = OsRecipeBoundaryVerifier {
        project_root: project_root.as_ref(),
    };
    load_recipe_path_with_boundary(&path, recipe_id, &boundary_verifier)
}

pub fn list_recipes(project_root: impl AsRef<Path>) -> Result<Vec<BrowserRecipeV1>, BrowserError> {
    let Some(parent) = verified_workflow_directory(project_root.as_ref(), false)? else {
        return Ok(Vec::new());
    };
    let boundary_verifier = OsRecipeBoundaryVerifier {
        project_root: project_root.as_ref(),
    };
    boundary_verifier.verify(RecipeIoBoundary::BeforeList, &parent, &parent)?;
    let entries = std::fs::read_dir(&parent)
        .map_err(|error| io_error("list recipe directory", &parent, error))?;
    let mut recipes = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| io_error("list recipe directory", &parent, error))?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Some(recipe_id) = file_name.strip_suffix(".json") else {
            continue;
        };
        if !is_safe_recipe_id(recipe_id) {
            continue;
        }
        let path = entry.path();
        recipes.push(load_recipe_path_with_boundary(
            &path,
            recipe_id,
            &boundary_verifier,
        )?);
    }
    recipes.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(recipes)
}

fn load_recipe_path_with_boundary(
    path: &Path,
    recipe_id: &str,
    boundary_verifier: &impl RecipeBoundaryVerifier,
) -> Result<BrowserRecipeV1, BrowserError> {
    load_recipe_path_with_digest(path, recipe_id, boundary_verifier).map(|loaded| loaded.recipe)
}

#[allow(dead_code)] // Task 7 calls this only after confirmation and authorization.
pub(crate) fn replace_recipe_locator_atomic(
    canonical_project_root: &Path,
    recipe_id: &str,
    expected_digest: &BrowserRecipeDigestV1,
    target: &BrowserReplayRecipeLocatorTarget,
    candidate: &BrowserRecipeLocator,
) -> Result<PathBuf, BrowserRecipeLocatorReplaceError> {
    let boundary_verifier = OsRecipeBoundaryVerifier {
        project_root: canonical_project_root,
    };
    replace_recipe_locator_atomic_with(
        canonical_project_root,
        recipe_id,
        expected_digest,
        target,
        candidate,
        &OsRecipeFileReplacer,
        &boundary_verifier,
        || {},
    )
}

#[allow(dead_code)] // Production/test injection seam for the Task 7 call path.
fn replace_recipe_locator_atomic_with(
    canonical_project_root: &Path,
    recipe_id: &str,
    expected_digest: &BrowserRecipeDigestV1,
    target: &BrowserReplayRecipeLocatorTarget,
    candidate: &BrowserRecipeLocator,
    replacer: &impl RecipeFileReplacer,
    boundary_verifier: &impl RecipeBoundaryVerifier,
    before_final_check: impl FnOnce(),
) -> Result<PathBuf, BrowserRecipeLocatorReplaceError> {
    candidate
        .validate()
        .map_err(|_| BrowserRecipeLocatorReplaceError::InvalidCandidate)?;
    if candidate == target.old_locator() {
        return Err(BrowserRecipeLocatorReplaceError::UnchangedCandidate);
    }
    let _write_guard = RECIPE_WRITE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    replace_recipe_locator_atomic_under_gate(
        canonical_project_root,
        recipe_id,
        expected_digest,
        target,
        candidate,
        replacer,
        boundary_verifier,
        before_final_check,
    )
}

#[allow(dead_code)] // Caller already owns RECIPE_WRITE_GATE; never re-enters it.
fn replace_recipe_locator_atomic_under_gate(
    canonical_project_root: &Path,
    recipe_id: &str,
    expected_digest: &BrowserRecipeDigestV1,
    target: &BrowserReplayRecipeLocatorTarget,
    candidate: &BrowserRecipeLocator,
    replacer: &impl RecipeFileReplacer,
    boundary_verifier: &impl RecipeBoundaryVerifier,
    before_final_check: impl FnOnce(),
) -> Result<PathBuf, BrowserRecipeLocatorReplaceError> {
    verified_authenticated_local_project_root(canonical_project_root)
        .map_err(BrowserRecipeLocatorReplaceError::Store)?;
    let expected_path = recipe_path(canonical_project_root, recipe_id)?;
    let parent = verified_workflow_directory(canonical_project_root, false)?.ok_or_else(|| {
        BrowserError::MissingFile {
            path: expected_path.clone(),
        }
    })?;
    let path = parent.join(format!("{recipe_id}.json"));
    if path != expected_path {
        return Err(BrowserRecipeLocatorReplaceError::Store(
            BrowserError::OutsideWorkspace { path },
        ));
    }
    boundary_verifier.verify(RecipeIoBoundary::BeforeList, &parent, &parent)?;
    scavenge_stale_recipe_temps(&parent)?;

    let loaded = load_recipe_path_with_digest(&path, recipe_id, boundary_verifier)?;
    verify_exact_recipe_locator_target(&loaded, expected_digest, target)?;
    let mut replacement_recipe = loaded.recipe.clone();
    replace_recipe_locator_at(&mut replacement_recipe, target, candidate)?;
    replacement_recipe
        .validate()
        .map_err(|_| BrowserRecipeLocatorReplaceError::InvalidCandidate)?;
    let mut json = serde_json::to_vec_pretty(&replacement_recipe)
        .map_err(|error| invalid_recipe(format!("could not serialize recipe: {error}")))?;
    json.push(b'\n');

    atomic_write_with_boundaries_and_final_check(
        &path,
        &json,
        replacer,
        boundary_verifier,
        || {
            before_final_check();
            let current = load_recipe_path_with_digest(&path, recipe_id, boundary_verifier)?;
            verify_exact_recipe_locator_target(&current, expected_digest, target)
        },
    )?;
    Ok(path)
}

#[allow(dead_code)] // Reused by both initial and final Task 7 comparisons.
fn verify_exact_recipe_locator_target(
    loaded: &LoadedBrowserRecipeV1,
    expected_digest: &BrowserRecipeDigestV1,
    target: &BrowserReplayRecipeLocatorTarget,
) -> Result<(), BrowserRecipeLocatorReplaceError> {
    if loaded.digest != *expected_digest {
        return Err(BrowserRecipeLocatorReplaceError::RecipeChanged);
    }
    match recipe_locator_at(
        &loaded.recipe,
        target.step_index(),
        target.step_id(),
        target.locator_slot(),
    )? {
        None => Err(BrowserRecipeLocatorReplaceError::TargetlessLocator),
        Some(locator) if locator != target.old_locator() => {
            Err(BrowserRecipeLocatorReplaceError::OldLocatorChanged)
        }
        Some(_) => Ok(()),
    }
}

// Cooperating DevManager save/apply operations serialize through RECIPE_WRITE_GATE. The only
// best-effort boundary is the narrow last-writer window for non-cooperating external writers
// after the final strict compare and before the atomic rename; this is not an OS-wide CAS.

#[allow(dead_code)] // Fixed private failure classes are mapped by Task 7.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BrowserRecipeLocatorReplaceError {
    Store(BrowserError),
    InvalidCandidate,
    UnchangedCandidate,
    RecipeChanged,
    StepIndexChanged,
    StepIdChanged,
    LocatorSlotChanged,
    TargetlessLocator,
    OldLocatorChanged,
}

impl From<BrowserError> for BrowserRecipeLocatorReplaceError {
    fn from(error: BrowserError) -> Self {
        Self::Store(error)
    }
}

#[allow(dead_code)] // Task 7 applies the retained exact target through this table.
pub(crate) fn recipe_locator_at<'a>(
    recipe: &'a BrowserRecipeV1,
    step_index: usize,
    step_id: &str,
    locator_slot: BrowserReplayLocatorSlot,
) -> Result<Option<&'a BrowserRecipeLocator>, BrowserRecipeLocatorReplaceError> {
    let step = recipe
        .steps
        .get(step_index)
        .ok_or(BrowserRecipeLocatorReplaceError::StepIndexChanged)?;
    if step.id != step_id {
        return Err(BrowserRecipeLocatorReplaceError::StepIdChanged);
    }
    recipe_step_locator_at(step, locator_slot)
}

macro_rules! with_recipe_locator {
    ($step:expr, $locator_slot:expr) => {{
        match $locator_slot {
            BrowserReplayLocatorSlot::PrimaryAction => match $step {
                BrowserRecipeStep {
                    action:
                        BrowserRecipeAction::Click { locator }
                        | BrowserRecipeAction::Hover { locator }
                        | BrowserRecipeAction::Focus { locator }
                        | BrowserRecipeAction::Type { locator, .. }
                        | BrowserRecipeAction::Clear { locator }
                        | BrowserRecipeAction::Select { locator, .. }
                        | BrowserRecipeAction::Upload { locator, .. }
                        | BrowserRecipeAction::Download { locator },
                    ..
                } => Ok(Some(locator)),
                _ => Err(BrowserRecipeLocatorReplaceError::LocatorSlotChanged),
            },
            BrowserReplayLocatorSlot::OptionalAction => match $step {
                BrowserRecipeStep {
                    action:
                        BrowserRecipeAction::Keypress {
                            locator: Some(locator),
                            ..
                        }
                        | BrowserRecipeAction::Scroll {
                            locator: Some(locator),
                            ..
                        },
                    ..
                } => Ok(Some(locator)),
                BrowserRecipeStep {
                    action:
                        BrowserRecipeAction::Keypress { locator: None, .. }
                        | BrowserRecipeAction::Scroll { locator: None, .. },
                    ..
                } => Ok(None),
                _ => Err(BrowserRecipeLocatorReplaceError::LocatorSlotChanged),
            },
            BrowserReplayLocatorSlot::DragSource => match $step {
                BrowserRecipeStep {
                    action: BrowserRecipeAction::DragDrop { source, .. },
                    ..
                } => Ok(Some(source)),
                _ => Err(BrowserRecipeLocatorReplaceError::LocatorSlotChanged),
            },
            BrowserReplayLocatorSlot::DragDestination => match $step {
                BrowserRecipeStep {
                    action: BrowserRecipeAction::DragDrop { destination, .. },
                    ..
                } => Ok(Some(destination)),
                _ => Err(BrowserRecipeLocatorReplaceError::LocatorSlotChanged),
            },
            BrowserReplayLocatorSlot::ActionWait => match $step {
                BrowserRecipeStep {
                    action:
                        BrowserRecipeAction::Wait {
                            condition:
                                BrowserRecipeWait::ElementPresent { locator, .. }
                                | BrowserRecipeWait::ElementVisible { locator, .. },
                        },
                    ..
                } => Ok(Some(locator)),
                _ => Err(BrowserRecipeLocatorReplaceError::LocatorSlotChanged),
            },
            BrowserReplayLocatorSlot::StepWait => match $step {
                BrowserRecipeStep {
                    wait:
                        Some(
                            BrowserRecipeWait::ElementPresent { locator, .. }
                            | BrowserRecipeWait::ElementVisible { locator, .. },
                        ),
                    ..
                } => Ok(Some(locator)),
                _ => Err(BrowserRecipeLocatorReplaceError::LocatorSlotChanged),
            },
            BrowserReplayLocatorSlot::Assertion { index } => {
                let assertion = match $step {
                    BrowserRecipeStep { assertions, .. } => assertions.into_iter().nth(index),
                };
                match assertion {
                    Some(BrowserRecipeAssertion::Element {
                        locator,
                        state:
                            BrowserRecipeElementState::Present | BrowserRecipeElementState::Visible,
                    })
                    | Some(BrowserRecipeAssertion::Value { locator, .. }) => Ok(Some(locator)),
                    _ => Err(BrowserRecipeLocatorReplaceError::LocatorSlotChanged),
                }
            }
        }
    }};
}

pub(crate) fn recipe_step_locator_at(
    step: &BrowserRecipeStep,
    locator_slot: BrowserReplayLocatorSlot,
) -> Result<Option<&BrowserRecipeLocator>, BrowserRecipeLocatorReplaceError> {
    with_recipe_locator!(step, locator_slot)
}

fn recipe_step_locator_mut(
    step: &mut BrowserRecipeStep,
    locator_slot: BrowserReplayLocatorSlot,
) -> Result<Option<&mut BrowserRecipeLocator>, BrowserRecipeLocatorReplaceError> {
    with_recipe_locator!(step, locator_slot)
}

#[allow(dead_code)] // Task 7 installs the authorized candidate through this helper.
pub(crate) fn replace_recipe_locator_at(
    recipe: &mut BrowserRecipeV1,
    target: &BrowserReplayRecipeLocatorTarget,
    candidate: &BrowserRecipeLocator,
) -> Result<(), BrowserRecipeLocatorReplaceError> {
    candidate
        .validate()
        .map_err(|_| BrowserRecipeLocatorReplaceError::InvalidCandidate)?;
    if candidate == target.old_locator() {
        return Err(BrowserRecipeLocatorReplaceError::UnchangedCandidate);
    }
    let step = recipe
        .steps
        .get_mut(target.step_index())
        .ok_or(BrowserRecipeLocatorReplaceError::StepIndexChanged)?;
    if step.id != target.step_id() {
        return Err(BrowserRecipeLocatorReplaceError::StepIdChanged);
    }
    match recipe_step_locator_mut(step, target.locator_slot())? {
        None => Err(BrowserRecipeLocatorReplaceError::TargetlessLocator),
        Some(locator) if locator != target.old_locator() => {
            Err(BrowserRecipeLocatorReplaceError::OldLocatorChanged)
        }
        Some(locator) => {
            *locator = candidate.clone();
            Ok(())
        }
    }
}

struct LoadedBrowserRecipeV1 {
    recipe: BrowserRecipeV1,
    #[allow(dead_code)] // The public load path needs only the recipe; repair apply compares both.
    digest: BrowserRecipeDigestV1,
}

fn load_recipe_path_with_digest(
    path: &Path,
    recipe_id: &str,
    boundary_verifier: &impl RecipeBoundaryVerifier,
) -> Result<LoadedBrowserRecipeV1, BrowserError> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid_recipe("recipe path has no parent"))?;
    boundary_verifier.verify(RecipeIoBoundary::BeforeRead, parent, path)?;
    let json = std::fs::read(&path).map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            BrowserError::MissingFile {
                path: path.to_path_buf(),
            }
        } else {
            io_error("read recipe", &path, error)
        }
    })?;
    let value = serde_json::from_slice::<StrictJsonValue>(&json)
        .map(|value| value.0)
        .map_err(|error| {
            invalid_recipe(format!(
                "could not read recipe schema {}: {error}",
                path.display()
            ))
        })?;
    let version = schema_version_from_value(&value).map_err(|message| invalid_recipe(message))?;
    if version != BROWSER_RECIPE_SCHEMA_VERSION {
        return Err(BrowserError::UnsupportedRecipeVersion { version });
    }
    let recipe: BrowserRecipeV1 = serde_json::from_value(value).map_err(|error| {
        invalid_recipe(format!(
            "could not parse recipe {}: {error}",
            path.display()
        ))
    })?;
    if recipe.id != recipe_id {
        return Err(invalid_recipe(
            "recipe document id must match its repository file name",
        ));
    }
    let digest = canonical_browser_recipe_digest(&recipe)?;
    Ok(LoadedBrowserRecipeV1 { recipe, digest })
}

fn verified_workflow_directory(
    project_root: &Path,
    create: bool,
) -> Result<Option<PathBuf>, BrowserError> {
    let project_metadata = std::fs::metadata(project_root)
        .map_err(|error| io_error("inspect recipe project root", project_root, error))?;
    if !project_metadata.is_dir() {
        return Err(BrowserError::OutsideWorkspace {
            path: project_root.to_path_buf(),
        });
    }
    let canonical_project = project_root
        .canonicalize()
        .map_err(|error| io_error("canonicalize recipe project root", project_root, error))?;
    let devmanager = project_root.join(".devmanager");
    let workflow = devmanager.join("browser-workflows");
    let devmanager_exists = validate_existing_path(&devmanager, RecipePathKind::Directory)?;
    if !devmanager_exists {
        if !create {
            return Ok(None);
        }
        create_recipe_directory(&devmanager)?;
    }
    ensure_direct_directory(&devmanager)?;
    let workflow_exists = validate_existing_path(&workflow, RecipePathKind::Directory)?;
    if !workflow_exists {
        if !create {
            return Ok(None);
        }
        ensure_direct_directory(&devmanager)?;
        create_recipe_directory(&workflow)?;
    }
    ensure_direct_directory(&devmanager)?;
    ensure_direct_directory(&workflow)?;
    let canonical_workflow = workflow
        .canonicalize()
        .map_err(|error| io_error("canonicalize recipe directory", &workflow, error))?;
    if !canonical_workflow.starts_with(&canonical_project) {
        return Err(BrowserError::OutsideWorkspace { path: workflow });
    }
    Ok(Some(workflow))
}

fn ensure_direct_directory(path: &Path) -> Result<(), BrowserError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| io_error("inspect recipe directory", path, error))?;
    validate_path_kind(path, recipe_path_kind(&metadata), RecipePathKind::Directory)
}

fn create_recipe_directory(path: &Path) -> Result<(), BrowserError> {
    match std::fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            if validate_existing_path(path, RecipePathKind::Directory)? {
                Ok(())
            } else {
                Err(io_error("create recipe directory", path, error))
            }
        }
        Err(error) => Err(io_error("create recipe directory", path, error)),
    }
}

fn ensure_direct_recipe_file(parent: &Path, path: &Path) -> Result<(), BrowserError> {
    if path.parent() != Some(parent) {
        return Err(BrowserError::OutsideWorkspace {
            path: path.to_path_buf(),
        });
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            BrowserError::MissingFile {
                path: path.to_path_buf(),
            }
        } else {
            io_error("inspect recipe file", path, error)
        }
    })?;
    validate_path_kind(
        path,
        recipe_path_kind(&metadata),
        RecipePathKind::RegularFile,
    )
}

fn reject_non_regular_destination(path: &Path) -> Result<(), BrowserError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => validate_path_kind(
            path,
            recipe_path_kind(&metadata),
            RecipePathKind::RegularFile,
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error("inspect recipe destination", path, error)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecipePathKind {
    Directory,
    RegularFile,
    Symlink,
    ReparsePoint,
    Other,
}

fn recipe_path_kind(metadata: &std::fs::Metadata) -> RecipePathKind {
    let file_type = metadata.file_type();
    recipe_path_kind_from_flags(
        file_type.is_symlink(),
        file_type.is_dir(),
        file_type.is_file(),
        metadata_is_reparse_point(metadata),
    )
}

fn recipe_path_kind_from_flags(
    is_symlink: bool,
    is_directory: bool,
    is_file: bool,
    is_reparse_point: bool,
) -> RecipePathKind {
    if is_symlink {
        RecipePathKind::Symlink
    } else if is_reparse_point {
        RecipePathKind::ReparsePoint
    } else if is_directory {
        RecipePathKind::Directory
    } else if is_file {
        RecipePathKind::RegularFile
    } else {
        RecipePathKind::Other
    }
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse_point(_metadata: &std::fs::Metadata) -> bool {
    false
}

fn validate_existing_path(path: &Path, expected: RecipePathKind) -> Result<bool, BrowserError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_path_kind(path, recipe_path_kind(&metadata), expected)?;
            Ok(true)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error("inspect recipe path", path, error)),
    }
}

fn validate_path_kind(
    path: &Path,
    actual: RecipePathKind,
    expected: RecipePathKind,
) -> Result<(), BrowserError> {
    if actual == expected {
        Ok(())
    } else {
        Err(BrowserError::OutsideWorkspace {
            path: path.to_path_buf(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecipeIoBoundary {
    BeforeList,
    BeforeRead,
    BeforeTempCreate,
    BeforeReplace,
}

trait RecipeBoundaryVerifier {
    fn verify(
        &self,
        boundary: RecipeIoBoundary,
        parent: &Path,
        path: &Path,
    ) -> Result<(), BrowserError>;
}

struct OsRecipeBoundaryVerifier<'a> {
    project_root: &'a Path,
}

impl RecipeBoundaryVerifier for OsRecipeBoundaryVerifier<'_> {
    fn verify(
        &self,
        boundary: RecipeIoBoundary,
        parent: &Path,
        path: &Path,
    ) -> Result<(), BrowserError> {
        revalidate_workflow_directory(self.project_root, parent)?;
        match boundary {
            RecipeIoBoundary::BeforeList => Ok(()),
            RecipeIoBoundary::BeforeRead => ensure_direct_recipe_file(parent, path),
            RecipeIoBoundary::BeforeTempCreate | RecipeIoBoundary::BeforeReplace => {
                if path.parent() != Some(parent) {
                    return Err(BrowserError::OutsideWorkspace {
                        path: path.to_path_buf(),
                    });
                }
                reject_non_regular_destination(path)
            }
        }
    }
}

fn revalidate_workflow_directory(
    project_root: &Path,
    expected_workflow: &Path,
) -> Result<(), BrowserError> {
    let Some(current_workflow) = verified_workflow_directory(project_root, false)? else {
        return Err(BrowserError::OutsideWorkspace {
            path: expected_workflow.to_path_buf(),
        });
    };
    if current_workflow != expected_workflow {
        return Err(BrowserError::OutsideWorkspace {
            path: expected_workflow.to_path_buf(),
        });
    }
    ensure_direct_directory(expected_workflow)
}

fn scavenge_stale_recipe_temps(parent: &Path) -> Result<usize, BrowserError> {
    scavenge_stale_recipe_temps_with(parent, |metadata| {
        metadata
            .modified()
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= STALE_RECIPE_TEMP_AGE)
    })
}

fn scavenge_stale_recipe_temps_with(
    parent: &Path,
    mut is_stale: impl FnMut(&std::fs::Metadata) -> bool,
) -> Result<usize, BrowserError> {
    ensure_direct_directory(parent)?;
    let entries = std::fs::read_dir(parent)
        .map_err(|error| io_error("list recipe temporary files", parent, error))?;
    let mut candidates = entries
        .take(MAX_RECIPE_TEMP_ENTRIES_TO_SCAN)
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let file_name = entry.file_name();
            let file_name = file_name.to_str()?;
            is_owned_recipe_temp_name(file_name).then(|| entry.path())
        })
        .collect::<Vec<_>>();
    candidates.sort();

    let mut removed = 0;
    for path in candidates {
        if removed == MAX_RECIPE_TEMPS_TO_SCAVENGE {
            break;
        }
        ensure_direct_directory(parent)?;
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(io_error("inspect recipe temporary file", &path, error)),
        };
        if recipe_path_kind(&metadata) != RecipePathKind::RegularFile || !is_stale(&metadata) {
            continue;
        }
        std::fs::remove_file(&path)
            .map_err(|error| io_error("remove stale recipe temporary file", &path, error))?;
        removed += 1;
    }
    Ok(removed)
}

fn is_owned_recipe_temp_name(file_name: &str) -> bool {
    let Some(body) = file_name
        .strip_prefix(RECIPE_TEMP_PREFIX)
        .and_then(|name| name.strip_suffix(".tmp"))
    else {
        return false;
    };
    let Some((recipe_file, nonce)) = body.rsplit_once('.') else {
        return false;
    };
    let Some(recipe_id) = recipe_file.strip_suffix(".json") else {
        return false;
    };
    is_safe_recipe_id(recipe_id)
        && nonce.len() == 32
        && nonce
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

trait RecipeFileReplacer {
    fn replace(&self, temporary: &Path, destination: &Path) -> std::io::Result<()>;
}

struct OsRecipeFileReplacer;

impl RecipeFileReplacer for OsRecipeFileReplacer {
    fn replace(&self, temporary: &Path, destination: &Path) -> std::io::Result<()> {
        replace_sibling_file(temporary, destination)
    }
}

struct NoClobberRecipeFileReplacer;

impl RecipeFileReplacer for NoClobberRecipeFileReplacer {
    fn replace(&self, temporary: &Path, destination: &Path) -> std::io::Result<()> {
        replace_sibling_file_without_overwrite(temporary, destination)
    }
}

#[cfg(test)]
fn atomic_write_with(
    path: &Path,
    bytes: &[u8],
    replacer: &impl RecipeFileReplacer,
) -> Result<(), BrowserError> {
    struct AllowAllBoundaries;

    impl RecipeBoundaryVerifier for AllowAllBoundaries {
        fn verify(
            &self,
            _boundary: RecipeIoBoundary,
            _parent: &Path,
            _path: &Path,
        ) -> Result<(), BrowserError> {
            Ok(())
        }
    }

    atomic_write_with_boundaries(path, bytes, replacer, &AllowAllBoundaries)
}

fn atomic_write_with_boundaries(
    path: &Path,
    bytes: &[u8],
    replacer: &impl RecipeFileReplacer,
    boundary_verifier: &impl RecipeBoundaryVerifier,
) -> Result<(), BrowserError> {
    atomic_write_with_boundaries_and_final_check(
        path,
        bytes,
        replacer,
        boundary_verifier,
        || Ok(()),
    )
}

fn atomic_write_with_boundaries_and_final_check<E>(
    path: &Path,
    bytes: &[u8],
    replacer: &impl RecipeFileReplacer,
    boundary_verifier: &impl RecipeBoundaryVerifier,
    final_check: impl FnOnce() -> Result<(), E>,
) -> Result<(), E>
where
    E: From<BrowserError>,
{
    let Some(parent) = path.parent() else {
        return Err(invalid_recipe("recipe path has no parent").into());
    };
    let (temporary_path, mut temporary_file) =
        create_sibling_temp(path, boundary_verifier).map_err(E::from)?;
    let cleanup = TempCleanup(temporary_path.clone());
    temporary_file
        .write_all(bytes)
        .and_then(|_| temporary_file.sync_all())
        .map_err(|error| io_error("write recipe temporary file", &temporary_path, error))
        .map_err(E::from)?;
    drop(temporary_file);
    final_check()?;
    boundary_verifier
        .verify(RecipeIoBoundary::BeforeReplace, parent, path)
        .map_err(E::from)?;
    replacer
        .replace(&temporary_path, path)
        .map_err(|error| io_error("replace recipe atomically", path, error))
        .map_err(E::from)?;
    drop(cleanup);
    // The rename has committed at this point. Directory sync is durability hardening, so a
    // failure cannot be reported as an ordinary pre-commit replacement failure.
    let _ = sync_parent_directory(parent);
    Ok(())
}

struct TempCleanup(PathBuf);

impl Drop for TempCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn create_sibling_temp(
    path: &Path,
    boundary_verifier: &impl RecipeBoundaryVerifier,
) -> Result<(PathBuf, std::fs::File), BrowserError> {
    let Some(parent) = path.parent() else {
        return Err(invalid_recipe("recipe path has no parent"));
    };
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid_recipe("recipe file name is not valid UTF-8"))?;
    for _ in 0..32 {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).map_err(|error| BrowserError::Io {
            operation: "generate recipe temporary path".to_string(),
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let temporary_path = parent.join(format!("{RECIPE_TEMP_PREFIX}{file_name}.{suffix}.tmp"));
        boundary_verifier.verify(RecipeIoBoundary::BeforeTempCreate, parent, path)?;
        match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary_path)
        {
            Ok(file) => return Ok((temporary_path, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(io_error(
                    "create recipe temporary file",
                    &temporary_path,
                    error,
                ))
            }
        }
    }
    Err(invalid_recipe(
        "could not allocate a unique recipe temporary file",
    ))
}

#[cfg(not(windows))]
fn replace_sibling_file(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(temporary, destination)
}

#[cfg(not(windows))]
fn replace_sibling_file_without_overwrite(
    temporary: &Path,
    destination: &Path,
) -> std::io::Result<()> {
    std::fs::hard_link(temporary, destination)
}

#[cfg(windows)]
fn replace_sibling_file_without_overwrite(
    temporary: &Path,
    destination: &Path,
) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};

    let temporary: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    unsafe {
        MoveFileExW(
            PCWSTR(temporary.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_WRITE_THROUGH,
        )
        .map_err(std::io::Error::from)
    }
}

#[cfg(windows)]
fn replace_sibling_file(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let temporary: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    unsafe {
        MoveFileExW(
            PCWSTR(temporary.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
        .map_err(std::io::Error::from)
    }
}

#[cfg(not(windows))]
fn sync_parent_directory(parent: &Path) -> Result<(), BrowserError> {
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| io_error("sync recipe directory", parent, error))
}

#[cfg(windows)]
fn sync_parent_directory(_parent: &Path) -> Result<(), BrowserError> {
    Ok(())
}

fn io_error(operation: &str, path: &Path, error: std::io::Error) -> BrowserError {
    BrowserError::Io {
        operation: operation.to_string(),
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "devmanager-recipe-unit-{label}-{}-{nanos:x}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct FailingReplacer;

    impl RecipeFileReplacer for FailingReplacer {
        fn replace(&self, temporary: &Path, destination: &Path) -> std::io::Result<()> {
            assert_eq!(temporary.parent(), destination.parent());
            assert!(temporary.is_file());
            Err(std::io::Error::other("injected atomic replace failure"))
        }
    }

    #[test]
    fn recipe_atomic_replace_failure_preserves_old_file_and_cleans_sibling_temp() {
        let temp = TestDir::new("atomic-failure");
        let destination = temp.0.join("workflow.json");
        std::fs::write(&destination, b"old-complete-file\n").expect("write old file");

        let error = atomic_write_with(&destination, b"new-complete-file\n", &FailingReplacer)
            .expect_err("replace failure must be returned");

        assert!(matches!(
            error,
            BrowserError::Io { ref operation, .. } if operation == "replace recipe atomically"
        ));
        assert_eq!(
            std::fs::read(&destination).expect("read preserved file"),
            b"old-complete-file\n"
        );
        let entries = std::fs::read_dir(&temp.0)
            .expect("list test directory")
            .map(|entry| entry.expect("directory entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, [std::ffi::OsString::from("workflow.json")]);
    }

    #[test]
    fn recipe_path_classification_rejects_symlinks_without_platform_privileges() {
        let path = Path::new("project/.devmanager/browser-workflows/workflow.json");
        assert!(matches!(
            validate_path_kind(path, RecipePathKind::Symlink, RecipePathKind::RegularFile),
            Err(BrowserError::OutsideWorkspace { .. })
        ));
        assert!(matches!(
            validate_path_kind(path, RecipePathKind::Symlink, RecipePathKind::Directory),
            Err(BrowserError::OutsideWorkspace { .. })
        ));
    }

    #[test]
    fn recipe_path_classification_rejects_windows_reparse_attributes() {
        let path = Path::new("project/.devmanager/browser-workflows/workflow.json");
        assert_eq!(
            recipe_path_kind_from_flags(false, true, false, true),
            RecipePathKind::ReparsePoint
        );
        assert!(matches!(
            validate_path_kind(
                path,
                RecipePathKind::ReparsePoint,
                RecipePathKind::Directory
            ),
            Err(BrowserError::OutsideWorkspace { .. })
        ));
    }

    struct BoundaryReparse {
        fail_at: RecipeIoBoundary,
    }

    impl RecipeBoundaryVerifier for BoundaryReparse {
        fn verify(
            &self,
            boundary: RecipeIoBoundary,
            _parent: &Path,
            path: &Path,
        ) -> Result<(), BrowserError> {
            if boundary == self.fail_at {
                Err(BrowserError::OutsideWorkspace {
                    path: path.to_path_buf(),
                })
            } else {
                Ok(())
            }
        }
    }

    struct PanickingReplacer;

    impl RecipeFileReplacer for PanickingReplacer {
        fn replace(&self, _temporary: &Path, _destination: &Path) -> std::io::Result<()> {
            panic!("replacement must not run after a failed boundary revalidation")
        }
    }

    #[test]
    fn injected_reparse_swap_blocks_read_temp_open_and_replace_boundaries() {
        let temp = TestDir::new("reparse-boundary");
        let destination = temp.0.join("workflow.json");
        std::fs::write(&destination, b"old-complete-file\n").expect("write old file");

        let read_error = load_recipe_path_with_boundary(
            &destination,
            "workflow",
            &BoundaryReparse {
                fail_at: RecipeIoBoundary::BeforeRead,
            },
        )
        .expect_err("reparse swap before read must be rejected");
        assert!(matches!(read_error, BrowserError::OutsideWorkspace { .. }));

        let create_error = atomic_write_with_boundaries(
            &destination,
            b"new-complete-file\n",
            &PanickingReplacer,
            &BoundaryReparse {
                fail_at: RecipeIoBoundary::BeforeTempCreate,
            },
        )
        .expect_err("reparse swap before temporary open must be rejected");
        assert!(matches!(
            create_error,
            BrowserError::OutsideWorkspace { .. }
        ));
        assert_eq!(
            std::fs::read_dir(&temp.0)
                .expect("list after blocked temporary open")
                .count(),
            1,
            "temporary open must not run after boundary validation fails"
        );

        let replace_error = atomic_write_with_boundaries(
            &destination,
            b"new-complete-file\n",
            &PanickingReplacer,
            &BoundaryReparse {
                fail_at: RecipeIoBoundary::BeforeReplace,
            },
        )
        .expect_err("reparse swap before replace must be rejected");
        assert!(matches!(
            replace_error,
            BrowserError::OutsideWorkspace { .. }
        ));
        assert_eq!(
            std::fs::read(&destination).expect("read preserved destination"),
            b"old-complete-file\n"
        );
        assert_eq!(
            std::fs::read_dir(&temp.0)
                .expect("list test directory")
                .count(),
            1,
            "temporary file must be cleaned when boundary validation fails"
        );
    }

    #[test]
    fn stale_temp_scavenger_is_bounded_and_removes_only_owned_regular_files() {
        let temp = TestDir::new("stale-scavenge");
        let total_owned = MAX_RECIPE_TEMPS_TO_SCAVENGE + 3;
        for index in 0..total_owned {
            let suffix = format!("{index:032x}");
            std::fs::write(
                temp.0.join(format!(
                    ".devmanager-browser-recipe.workflow.json.{suffix}.tmp"
                )),
                b"stale",
            )
            .expect("write owned stale temp");
        }
        let lookalikes = [
            ".workflow.json.00000000000000000000000000000000.tmp",
            ".devmanager-browser-recipe.workflow.json.short.tmp",
            ".devmanager-browser-recipe.workflow.json.0000000000000000000000000000000G.tmp",
            ".devmanager-browser-recipe.unsafe.name.json.00000000000000000000000000000000.tmp",
        ];
        for name in lookalikes {
            std::fs::write(temp.0.join(name), b"user data").expect("write lookalike");
        }
        let owned_directory = temp
            .0
            .join(".devmanager-browser-recipe.directory.json.ffffffffffffffffffffffffffffffff.tmp");
        std::fs::create_dir(&owned_directory).expect("create matching directory");

        assert_eq!(
            scavenge_stale_recipe_temps(&temp.0).expect("inspect fresh owned temps"),
            0,
            "fresh owned temporary files must not be scavenged"
        );

        let removed = scavenge_stale_recipe_temps_with(&temp.0, |_| true)
            .expect("scavenge injected stale temps");

        assert_eq!(removed, MAX_RECIPE_TEMPS_TO_SCAVENGE);
        let remaining_owned = std::fs::read_dir(&temp.0)
            .expect("list test directory")
            .filter_map(Result::ok)
            .filter(|entry| {
                is_owned_recipe_temp_name(&entry.file_name().to_string_lossy())
                    && entry.file_type().is_ok_and(|kind| kind.is_file())
            })
            .count();
        assert_eq!(remaining_owned, total_owned - MAX_RECIPE_TEMPS_TO_SCAVENGE);
        for name in lookalikes {
            assert_eq!(
                std::fs::read(temp.0.join(name)).expect("lookalike must survive"),
                b"user data"
            );
        }
        assert!(owned_directory.is_dir());
    }

    fn repair_test_locator(label: &str) -> BrowserRecipeLocator {
        BrowserRecipeLocator {
            test_id: Some(label.to_string()),
            ..BrowserRecipeLocator::default()
        }
    }

    fn repair_test_literal(value: &str) -> BrowserRecipeValue {
        BrowserRecipeValue::Literal {
            value: value.to_string(),
        }
    }

    fn repair_test_step(action: BrowserRecipeAction) -> BrowserRecipeStep {
        BrowserRecipeStep {
            id: "case".to_string(),
            action,
            wait: None,
            assertions: Vec::new(),
        }
    }

    fn repair_test_recipe_with_step(step: BrowserRecipeStep) -> BrowserRecipeV1 {
        let mut recipe = repair_test_recipe();
        recipe.inputs = vec![BrowserRecipeInput {
            name: "fixture_file".to_string(),
            kind: BrowserRecipeInputKind::File,
            default_value: None,
        }];
        recipe.steps = vec![step];
        recipe
    }

    fn assert_only_locator_changed(
        before: &BrowserRecipeV1,
        after: &BrowserRecipeV1,
        old_locator: &BrowserRecipeLocator,
        candidate: &BrowserRecipeLocator,
    ) {
        fn replace_exact_value(
            value: &mut Value,
            old_value: &Value,
            new_value: &Value,
            replacements: &mut usize,
        ) {
            if value == old_value {
                *value = new_value.clone();
                *replacements += 1;
                return;
            }
            match value {
                Value::Array(values) => {
                    for value in values {
                        replace_exact_value(value, old_value, new_value, replacements);
                    }
                }
                Value::Object(values) => {
                    for value in values.values_mut() {
                        replace_exact_value(value, old_value, new_value, replacements);
                    }
                }
                _ => {}
            }
        }

        let mut expected = serde_json::to_value(before).unwrap();
        let old_value = serde_json::to_value(old_locator).unwrap();
        let new_value = serde_json::to_value(candidate).unwrap();
        let mut replacements = 0;
        replace_exact_value(&mut expected, &old_value, &new_value, &mut replacements);
        assert_eq!(replacements, 1, "old locator must occur exactly once");
        assert_eq!(serde_json::to_value(after).unwrap(), expected);
    }

    fn repair_test_recipe() -> BrowserRecipeV1 {
        let primary = repair_test_locator("primary");
        let optional = repair_test_locator("optional");
        let source = repair_test_locator("source");
        let destination = repair_test_locator("destination");
        let action_wait = repair_test_locator("action-wait");
        let step_wait = repair_test_locator("step-wait");
        let assertion = repair_test_locator("assertion");
        BrowserRecipeV1 {
            schema_version: BROWSER_RECIPE_SCHEMA_VERSION,
            id: "repair-test".to_string(),
            name: "Repair test".to_string(),
            description: "Exact locator replacement".to_string(),
            start_url: "https://example.test/".to_string(),
            viewport: BrowserRecipeViewport {
                width: 1280,
                height: 720,
                scale_percent: 100,
            },
            inputs: Vec::new(),
            steps: vec![
                BrowserRecipeStep {
                    id: "primary".to_string(),
                    action: BrowserRecipeAction::Click { locator: primary },
                    wait: None,
                    assertions: Vec::new(),
                },
                BrowserRecipeStep {
                    id: "optional".to_string(),
                    action: BrowserRecipeAction::Keypress {
                        locator: Some(optional),
                        key: BrowserRecipeValue::Literal {
                            value: "Enter".to_string(),
                        },
                    },
                    wait: None,
                    assertions: Vec::new(),
                },
                BrowserRecipeStep {
                    id: "targetless".to_string(),
                    action: BrowserRecipeAction::Scroll {
                        locator: None,
                        delta_x: 0,
                        delta_y: 10,
                    },
                    wait: None,
                    assertions: Vec::new(),
                },
                BrowserRecipeStep {
                    id: "drag".to_string(),
                    action: BrowserRecipeAction::DragDrop {
                        source,
                        destination,
                    },
                    wait: None,
                    assertions: Vec::new(),
                },
                BrowserRecipeStep {
                    id: "action-wait".to_string(),
                    action: BrowserRecipeAction::Wait {
                        condition: BrowserRecipeWait::ElementPresent {
                            locator: action_wait,
                            timeout_ms: 1_000,
                        },
                    },
                    wait: None,
                    assertions: Vec::new(),
                },
                BrowserRecipeStep {
                    id: "step-wait".to_string(),
                    action: BrowserRecipeAction::Screenshot { full_page: false },
                    wait: Some(BrowserRecipeWait::ElementVisible {
                        locator: step_wait,
                        timeout_ms: 1_000,
                    }),
                    assertions: Vec::new(),
                },
                BrowserRecipeStep {
                    id: "assertion".to_string(),
                    action: BrowserRecipeAction::Screenshot { full_page: false },
                    wait: None,
                    assertions: vec![BrowserRecipeAssertion::Element {
                        locator: assertion,
                        state: BrowserRecipeElementState::Visible,
                    }],
                },
            ],
        }
    }

    #[test]
    fn canonical_recipe_digest_is_strict_compact_and_domain_separated() {
        use sha2::{Digest, Sha256};

        let temp = TestDir::new("canonical-digest");
        let recipe = repair_test_recipe();
        let path = save_recipe(&temp.0, &recipe).expect("save canonical recipe");
        let loaded = load_recipe_path_with_digest(
            &path,
            &recipe.id,
            &OsRecipeBoundaryVerifier {
                project_root: &temp.0,
            },
        )
        .expect("strict load with digest");
        let compact = serde_json::to_vec(&recipe).expect("compact validated recipe");
        let mut expected = Sha256::new();
        expected.update(b"devmanager.browser-recipe-v1.sha256\0");
        expected.update(&compact);
        let expected: [u8; 32] = expected.finalize().into();
        assert_eq!(
            loaded
                .digest
                .bytes_for_test()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            "3e9581a08ef1f46627e89375535288563e9e526dc648f9109dbdad07e35911e8"
        );
        assert_eq!(loaded.digest.bytes_for_test(), expected);
        let no_domain: [u8; 32] = Sha256::digest(&compact).into();
        assert_ne!(loaded.digest.bytes_for_test(), no_domain);
        let mut alternate_domain = Sha256::new();
        alternate_domain.update(b"devmanager.browser-recipe-v2.sha256\0");
        alternate_domain.update(&compact);
        let alternate_domain: [u8; 32] = alternate_domain.finalize().into();
        assert_ne!(loaded.digest.bytes_for_test(), alternate_domain);

        let reversed = format!(
            "{{\"steps\":{},\"inputs\":{},\"viewport\":{},\"startUrl\":{},\"description\":{},\"name\":{},\"id\":{},\"schemaVersion\":1}}",
            serde_json::to_string(&recipe.steps).unwrap(),
            serde_json::to_string(&recipe.inputs).unwrap(),
            serde_json::to_string(&recipe.viewport).unwrap(),
            serde_json::to_string(&recipe.start_url).unwrap(),
            serde_json::to_string(&recipe.description).unwrap(),
            serde_json::to_string(&recipe.name).unwrap(),
            serde_json::to_string(&recipe.id).unwrap(),
        );
        std::fs::write(&path, format!("\n  {reversed}\n")).expect("write reordered JSON");
        let reordered = load_recipe_path_with_digest(
            &path,
            &recipe.id,
            &OsRecipeBoundaryVerifier {
                project_root: &temp.0,
            },
        )
        .expect("strict load reordered JSON");
        assert!(loaded.digest == reordered.digest);

        let mut semantic_change = recipe.clone();
        semantic_change.description.push_str(" changed");
        assert!(canonical_browser_recipe_digest(&semantic_change).unwrap() != reordered.digest);

        let unknown = reversed.replacen("{\"steps\"", "{\"future\":true,\"steps\"", 1);
        std::fs::write(&path, unknown).expect("write unknown schema member");
        assert!(matches!(
            load_recipe_path_with_digest(
                &path,
                &recipe.id,
                &OsRecipeBoundaryVerifier {
                    project_root: &temp.0,
                },
            ),
            Err(BrowserError::InvalidRecipe { .. })
        ));
    }

    #[test]
    fn exact_locator_helpers_cover_every_slot_and_reject_each_identity_mismatch() {
        use crate::browser::replay_repair::BrowserReplayRecipeLocatorTarget;
        use crate::browser::BrowserReplayLocatorSlot;

        let mut recipe = repair_test_recipe();
        let cases = [
            (
                0,
                "primary",
                BrowserReplayLocatorSlot::PrimaryAction,
                "primary",
            ),
            (
                1,
                "optional",
                BrowserReplayLocatorSlot::OptionalAction,
                "optional",
            ),
            (3, "drag", BrowserReplayLocatorSlot::DragSource, "source"),
            (
                3,
                "drag",
                BrowserReplayLocatorSlot::DragDestination,
                "destination",
            ),
            (
                4,
                "action-wait",
                BrowserReplayLocatorSlot::ActionWait,
                "action-wait",
            ),
            (
                5,
                "step-wait",
                BrowserReplayLocatorSlot::StepWait,
                "step-wait",
            ),
            (
                6,
                "assertion",
                BrowserReplayLocatorSlot::Assertion { index: 0 },
                "assertion",
            ),
        ];
        for (index, id, slot, label) in cases {
            assert_eq!(
                recipe_locator_at(&recipe, index, id, slot)
                    .unwrap()
                    .and_then(|locator| locator.test_id.as_deref()),
                Some(label)
            );
            let mut changed = repair_test_recipe();
            let old = recipe_locator_at(&changed, index, id, slot)
                .unwrap()
                .unwrap()
                .clone();
            let target = BrowserReplayRecipeLocatorTarget::new(index, id.to_string(), slot, old);
            let replacement = repair_test_locator(&format!("new-{label}"));
            replace_recipe_locator_at(&mut changed, &target, &replacement).unwrap();
            assert_eq!(
                recipe_locator_at(&changed, index, id, slot)
                    .unwrap()
                    .unwrap(),
                &replacement
            );
            changed.validate().unwrap();
        }
        assert_eq!(
            recipe_locator_at(
                &recipe,
                2,
                "targetless",
                BrowserReplayLocatorSlot::OptionalAction,
            )
            .unwrap(),
            None
        );

        let old_source = repair_test_locator("source");
        let target = BrowserReplayRecipeLocatorTarget::new(
            3,
            "drag".to_string(),
            BrowserReplayLocatorSlot::DragSource,
            old_source.clone(),
        );
        let replacement = repair_test_locator("new-source");
        replace_recipe_locator_at(&mut recipe, &target, &replacement).unwrap();
        assert_eq!(
            recipe_locator_at(&recipe, 3, "drag", BrowserReplayLocatorSlot::DragSource)
                .unwrap()
                .unwrap(),
            &replacement
        );
        assert_eq!(
            recipe_locator_at(
                &recipe,
                3,
                "drag",
                BrowserReplayLocatorSlot::DragDestination,
            )
            .unwrap()
            .unwrap(),
            &repair_test_locator("destination")
        );
        assert!(matches!(
            recipe_locator_at(&recipe, 99, "drag", BrowserReplayLocatorSlot::DragSource),
            Err(BrowserRecipeLocatorReplaceError::StepIndexChanged)
        ));
        assert!(matches!(
            recipe_locator_at(&recipe, 3, "wrong-id", BrowserReplayLocatorSlot::DragSource),
            Err(BrowserRecipeLocatorReplaceError::StepIdChanged)
        ));
        assert!(matches!(
            recipe_locator_at(&recipe, 0, "primary", BrowserReplayLocatorSlot::DragSource),
            Err(BrowserRecipeLocatorReplaceError::LocatorSlotChanged)
        ));
        let wrong_old = BrowserReplayRecipeLocatorTarget::new(
            0,
            "primary".to_string(),
            BrowserReplayLocatorSlot::PrimaryAction,
            repair_test_locator("wrong-old"),
        );
        assert!(matches!(
            replace_recipe_locator_at(&mut recipe, &wrong_old, &replacement),
            Err(BrowserRecipeLocatorReplaceError::OldLocatorChanged)
        ));
        let targetless = BrowserReplayRecipeLocatorTarget::new(
            2,
            "targetless".to_string(),
            BrowserReplayLocatorSlot::OptionalAction,
            repair_test_locator("never-present"),
        );
        assert!(matches!(
            replace_recipe_locator_at(&mut recipe, &targetless, &replacement),
            Err(BrowserRecipeLocatorReplaceError::TargetlessLocator)
        ));
        let invalid = BrowserRecipeLocator::default();
        assert!(matches!(
            replace_recipe_locator_at(&mut recipe, &target, &invalid),
            Err(BrowserRecipeLocatorReplaceError::InvalidCandidate)
        ));
    }

    #[test]
    fn locator_read_and_replace_share_one_structural_dispatch_source() {
        let source = include_str!("recipes.rs");
        assert_eq!(source.matches("fn recipe_step_locator_mut(").count(), 2);
        assert_eq!(source.matches("fn replace_recipe_step_locator(").count(), 1);
        assert_eq!(source.matches("fn recipe_wait_locator_at(").count(), 1);
    }

    #[test]
    fn locator_dispatch_exhaustively_changes_only_the_supported_target() {
        use crate::browser::replay_repair::BrowserReplayRecipeLocatorTarget;
        use crate::browser::BrowserReplayLocatorSlot;

        struct SupportedCase {
            name: &'static str,
            step: BrowserRecipeStep,
            slot: BrowserReplayLocatorSlot,
        }

        let supported = vec![
            SupportedCase {
                name: "click",
                step: repair_test_step(BrowserRecipeAction::Click {
                    locator: repair_test_locator("click"),
                }),
                slot: BrowserReplayLocatorSlot::PrimaryAction,
            },
            SupportedCase {
                name: "hover",
                step: repair_test_step(BrowserRecipeAction::Hover {
                    locator: repair_test_locator("hover"),
                }),
                slot: BrowserReplayLocatorSlot::PrimaryAction,
            },
            SupportedCase {
                name: "focus",
                step: repair_test_step(BrowserRecipeAction::Focus {
                    locator: repair_test_locator("focus"),
                }),
                slot: BrowserReplayLocatorSlot::PrimaryAction,
            },
            SupportedCase {
                name: "type",
                step: repair_test_step(BrowserRecipeAction::Type {
                    locator: repair_test_locator("type"),
                    value: repair_test_literal("text"),
                }),
                slot: BrowserReplayLocatorSlot::PrimaryAction,
            },
            SupportedCase {
                name: "clear",
                step: repair_test_step(BrowserRecipeAction::Clear {
                    locator: repair_test_locator("clear"),
                }),
                slot: BrowserReplayLocatorSlot::PrimaryAction,
            },
            SupportedCase {
                name: "select",
                step: repair_test_step(BrowserRecipeAction::Select {
                    locator: repair_test_locator("select"),
                    values: vec![repair_test_literal("choice")],
                }),
                slot: BrowserReplayLocatorSlot::PrimaryAction,
            },
            SupportedCase {
                name: "upload",
                step: repair_test_step(BrowserRecipeAction::Upload {
                    locator: repair_test_locator("upload"),
                    file: BrowserRecipeValue::Input {
                        name: "fixture_file".to_string(),
                    },
                }),
                slot: BrowserReplayLocatorSlot::PrimaryAction,
            },
            SupportedCase {
                name: "download",
                step: repair_test_step(BrowserRecipeAction::Download {
                    locator: repair_test_locator("download"),
                }),
                slot: BrowserReplayLocatorSlot::PrimaryAction,
            },
            SupportedCase {
                name: "keypress-some",
                step: repair_test_step(BrowserRecipeAction::Keypress {
                    locator: Some(repair_test_locator("keypress-some")),
                    key: repair_test_literal("Enter"),
                }),
                slot: BrowserReplayLocatorSlot::OptionalAction,
            },
            SupportedCase {
                name: "scroll-some",
                step: repair_test_step(BrowserRecipeAction::Scroll {
                    locator: Some(repair_test_locator("scroll-some")),
                    delta_x: 0,
                    delta_y: 10,
                }),
                slot: BrowserReplayLocatorSlot::OptionalAction,
            },
            SupportedCase {
                name: "drag-source",
                step: repair_test_step(BrowserRecipeAction::DragDrop {
                    source: repair_test_locator("drag-source"),
                    destination: repair_test_locator("drag-source-neighbor"),
                }),
                slot: BrowserReplayLocatorSlot::DragSource,
            },
            SupportedCase {
                name: "drag-destination",
                step: repair_test_step(BrowserRecipeAction::DragDrop {
                    source: repair_test_locator("drag-destination-neighbor"),
                    destination: repair_test_locator("drag-destination"),
                }),
                slot: BrowserReplayLocatorSlot::DragDestination,
            },
            SupportedCase {
                name: "action-wait-present",
                step: repair_test_step(BrowserRecipeAction::Wait {
                    condition: BrowserRecipeWait::ElementPresent {
                        locator: repair_test_locator("action-wait-present"),
                        timeout_ms: 1_000,
                    },
                }),
                slot: BrowserReplayLocatorSlot::ActionWait,
            },
            SupportedCase {
                name: "action-wait-visible",
                step: repair_test_step(BrowserRecipeAction::Wait {
                    condition: BrowserRecipeWait::ElementVisible {
                        locator: repair_test_locator("action-wait-visible"),
                        timeout_ms: 1_000,
                    },
                }),
                slot: BrowserReplayLocatorSlot::ActionWait,
            },
            SupportedCase {
                name: "step-wait-present",
                step: BrowserRecipeStep {
                    wait: Some(BrowserRecipeWait::ElementPresent {
                        locator: repair_test_locator("step-wait-present"),
                        timeout_ms: 1_000,
                    }),
                    ..repair_test_step(BrowserRecipeAction::Screenshot { full_page: false })
                },
                slot: BrowserReplayLocatorSlot::StepWait,
            },
            SupportedCase {
                name: "step-wait-visible",
                step: BrowserRecipeStep {
                    wait: Some(BrowserRecipeWait::ElementVisible {
                        locator: repair_test_locator("step-wait-visible"),
                        timeout_ms: 1_000,
                    }),
                    ..repair_test_step(BrowserRecipeAction::Screenshot { full_page: false })
                },
                slot: BrowserReplayLocatorSlot::StepWait,
            },
            SupportedCase {
                name: "assertion-present",
                step: BrowserRecipeStep {
                    assertions: vec![
                        BrowserRecipeAssertion::Title {
                            value: repair_test_literal("prefix"),
                            exact: true,
                        },
                        BrowserRecipeAssertion::Element {
                            locator: repair_test_locator("assertion-present"),
                            state: BrowserRecipeElementState::Present,
                        },
                    ],
                    ..repair_test_step(BrowserRecipeAction::Screenshot { full_page: false })
                },
                slot: BrowserReplayLocatorSlot::Assertion { index: 1 },
            },
            SupportedCase {
                name: "assertion-visible",
                step: BrowserRecipeStep {
                    assertions: vec![
                        BrowserRecipeAssertion::Title {
                            value: repair_test_literal("prefix"),
                            exact: true,
                        },
                        BrowserRecipeAssertion::Element {
                            locator: repair_test_locator("assertion-visible"),
                            state: BrowserRecipeElementState::Visible,
                        },
                    ],
                    ..repair_test_step(BrowserRecipeAction::Screenshot { full_page: false })
                },
                slot: BrowserReplayLocatorSlot::Assertion { index: 1 },
            },
            SupportedCase {
                name: "assertion-value",
                step: BrowserRecipeStep {
                    assertions: vec![
                        BrowserRecipeAssertion::Title {
                            value: repair_test_literal("prefix"),
                            exact: true,
                        },
                        BrowserRecipeAssertion::Value {
                            locator: repair_test_locator("assertion-value"),
                            value: repair_test_literal("expected"),
                        },
                    ],
                    ..repair_test_step(BrowserRecipeAction::Screenshot { full_page: false })
                },
                slot: BrowserReplayLocatorSlot::Assertion { index: 1 },
            },
        ];

        for case in supported {
            let mut recipe = repair_test_recipe_with_step(case.step);
            recipe.validate().unwrap();
            let before = recipe.clone();
            let old_locator = recipe_step_locator_at(&recipe.steps[0], case.slot)
                .unwrap()
                .unwrap()
                .clone();
            assert_eq!(old_locator.test_id.as_deref(), Some(case.name));
            let candidate = repair_test_locator(&format!("replacement-{}", case.name));
            let target = BrowserReplayRecipeLocatorTarget::new(
                0,
                "case".to_string(),
                case.slot,
                old_locator.clone(),
            );

            replace_recipe_locator_at(&mut recipe, &target, &candidate).unwrap();

            recipe.validate().unwrap();
            assert_eq!(
                recipe_step_locator_at(&recipe.steps[0], case.slot).unwrap(),
                Some(&candidate),
                "{} must update its exact locator",
                case.name
            );
            assert_only_locator_changed(&before, &recipe, &old_locator, &candidate);
        }
    }

    #[test]
    fn locator_dispatch_explicitly_rejects_targetless_and_unsupported_structures_untouched() {
        use crate::browser::replay_repair::BrowserReplayRecipeLocatorTarget;
        use crate::browser::BrowserReplayLocatorSlot;

        struct RejectedCase {
            name: &'static str,
            step: BrowserRecipeStep,
            slot: BrowserReplayLocatorSlot,
            targetless: bool,
        }

        let rejected = vec![
            RejectedCase {
                name: "keypress-none",
                step: repair_test_step(BrowserRecipeAction::Keypress {
                    locator: None,
                    key: repair_test_literal("Enter"),
                }),
                slot: BrowserReplayLocatorSlot::OptionalAction,
                targetless: true,
            },
            RejectedCase {
                name: "scroll-none",
                step: repair_test_step(BrowserRecipeAction::Scroll {
                    locator: None,
                    delta_x: 0,
                    delta_y: 10,
                }),
                slot: BrowserReplayLocatorSlot::OptionalAction,
                targetless: true,
            },
            RejectedCase {
                name: "unsupported-action",
                step: repair_test_step(BrowserRecipeAction::Screenshot { full_page: false }),
                slot: BrowserReplayLocatorSlot::PrimaryAction,
                targetless: false,
            },
            RejectedCase {
                name: "action-wait-hidden",
                step: repair_test_step(BrowserRecipeAction::Wait {
                    condition: BrowserRecipeWait::ElementHidden {
                        locator: repair_test_locator("hidden"),
                        timeout_ms: 1_000,
                    },
                }),
                slot: BrowserReplayLocatorSlot::ActionWait,
                targetless: false,
            },
            RejectedCase {
                name: "action-wait-absent",
                step: repair_test_step(BrowserRecipeAction::Wait {
                    condition: BrowserRecipeWait::TextAbsent {
                        value: repair_test_literal("gone"),
                        timeout_ms: 1_000,
                    },
                }),
                slot: BrowserReplayLocatorSlot::ActionWait,
                targetless: false,
            },
            RejectedCase {
                name: "step-wait-hidden",
                step: BrowserRecipeStep {
                    wait: Some(BrowserRecipeWait::ElementHidden {
                        locator: repair_test_locator("hidden"),
                        timeout_ms: 1_000,
                    }),
                    ..repair_test_step(BrowserRecipeAction::Screenshot { full_page: false })
                },
                slot: BrowserReplayLocatorSlot::StepWait,
                targetless: false,
            },
            RejectedCase {
                name: "step-wait-absent",
                step: BrowserRecipeStep {
                    wait: Some(BrowserRecipeWait::TextAbsent {
                        value: repair_test_literal("gone"),
                        timeout_ms: 1_000,
                    }),
                    ..repair_test_step(BrowserRecipeAction::Screenshot { full_page: false })
                },
                slot: BrowserReplayLocatorSlot::StepWait,
                targetless: false,
            },
            RejectedCase {
                name: "assertion-hidden",
                step: BrowserRecipeStep {
                    assertions: vec![BrowserRecipeAssertion::Element {
                        locator: repair_test_locator("hidden"),
                        state: BrowserRecipeElementState::Hidden,
                    }],
                    ..repair_test_step(BrowserRecipeAction::Screenshot { full_page: false })
                },
                slot: BrowserReplayLocatorSlot::Assertion { index: 0 },
                targetless: false,
            },
            RejectedCase {
                name: "assertion-absent",
                step: BrowserRecipeStep {
                    assertions: vec![BrowserRecipeAssertion::Element {
                        locator: repair_test_locator("absent"),
                        state: BrowserRecipeElementState::Absent,
                    }],
                    ..repair_test_step(BrowserRecipeAction::Screenshot { full_page: false })
                },
                slot: BrowserReplayLocatorSlot::Assertion { index: 0 },
                targetless: false,
            },
        ];

        for case in rejected {
            let mut recipe = repair_test_recipe_with_step(case.step);
            recipe.validate().unwrap();
            let before = recipe.clone();
            let lookup = recipe_step_locator_at(&recipe.steps[0], case.slot);
            if case.targetless {
                assert_eq!(lookup.unwrap(), None, "{} lookup", case.name);
            } else {
                assert_eq!(
                    lookup,
                    Err(BrowserRecipeLocatorReplaceError::LocatorSlotChanged),
                    "{} lookup",
                    case.name
                );
            }
            let target = BrowserReplayRecipeLocatorTarget::new(
                0,
                "case".to_string(),
                case.slot,
                repair_test_locator("retained-old"),
            );
            let expected = if case.targetless {
                BrowserRecipeLocatorReplaceError::TargetlessLocator
            } else {
                BrowserRecipeLocatorReplaceError::LocatorSlotChanged
            };

            assert_eq!(
                replace_recipe_locator_at(
                    &mut recipe,
                    &target,
                    &repair_test_locator("replacement")
                ),
                Err(expected),
                "{} replacement",
                case.name
            );
            assert_eq!(recipe, before, "{} must stay untouched", case.name);
        }
    }

    #[test]
    fn atomic_locator_replace_rejects_unchanged_candidate_without_touching_repository() {
        use crate::browser::replay_repair::BrowserReplayRecipeLocatorTarget;
        use crate::browser::BrowserReplayLocatorSlot;

        let temp = TestDir::new("locator-unchanged");
        let canonical_root = temp.0.canonicalize().unwrap();
        let recipe = repair_test_recipe();
        let path = save_recipe(&canonical_root, &recipe).unwrap();
        let before = std::fs::read(&path).unwrap();
        let old_locator = repair_test_locator("primary");
        let target = BrowserReplayRecipeLocatorTarget::new(
            0,
            "primary".to_string(),
            BrowserReplayLocatorSlot::PrimaryAction,
            old_locator.clone(),
        );

        let error = replace_recipe_locator_atomic(
            &canonical_root,
            &recipe.id,
            &canonical_browser_recipe_digest(&recipe).unwrap(),
            &target,
            &old_locator,
        )
        .expect_err("an unchanged candidate must be rejected");

        assert_eq!(error, BrowserRecipeLocatorReplaceError::UnchangedCandidate);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );

        let unavailable_root = canonical_root.join("does-not-exist");
        assert_eq!(
            replace_recipe_locator_atomic(
                &unavailable_root,
                &recipe.id,
                &canonical_browser_recipe_digest(&recipe).unwrap(),
                &target,
                &old_locator,
            ),
            Err(BrowserRecipeLocatorReplaceError::UnchangedCandidate),
            "unchanged rejection must happen before root authentication or repository I/O"
        );
        assert!(!unavailable_root.exists());
    }

    #[test]
    fn atomic_locator_replace_maps_context_invalid_candidate_to_invalid_candidate() {
        use crate::browser::replay_repair::BrowserReplayRecipeLocatorTarget;
        use crate::browser::BrowserReplayLocatorSlot;

        let temp = TestDir::new("locator-context-invalid");
        let canonical_root = temp.0.canonicalize().unwrap();
        let mut recipe = repair_test_recipe();
        let old_locator = repair_test_locator("primary");
        recipe.steps[0].action = BrowserRecipeAction::Type {
            locator: old_locator.clone(),
            value: BrowserRecipeValue::Literal {
                value: "hello".to_string(),
            },
        };
        recipe.validate().unwrap();
        let path = save_recipe(&canonical_root, &recipe).unwrap();
        let before = std::fs::read(&path).unwrap();
        let target = BrowserReplayRecipeLocatorTarget::new(
            0,
            "primary".to_string(),
            BrowserReplayLocatorSlot::PrimaryAction,
            old_locator,
        );

        let error = replace_recipe_locator_atomic(
            &canonical_root,
            &recipe.id,
            &canonical_browser_recipe_digest(&recipe).unwrap(),
            &target,
            &repair_test_locator("file-upload"),
        )
        .expect_err("a file-upload-looking Type target must be rejected");

        assert_eq!(error, BrowserRecipeLocatorReplaceError::InvalidCandidate);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn atomic_locator_replace_preserves_missing_destination_without_temps() {
        use crate::browser::replay_repair::BrowserReplayRecipeLocatorTarget;
        use crate::browser::BrowserReplayLocatorSlot;

        let temp = TestDir::new("locator-missing-file");
        let canonical_root = temp.0.canonicalize().unwrap();
        let recipe = repair_test_recipe();
        let path = save_recipe(&canonical_root, &recipe).unwrap();
        let parent = path.parent().unwrap().to_path_buf();
        std::fs::remove_file(&path).unwrap();
        let target = BrowserReplayRecipeLocatorTarget::new(
            0,
            "primary".to_string(),
            BrowserReplayLocatorSlot::PrimaryAction,
            repair_test_locator("primary"),
        );

        let error = replace_recipe_locator_atomic(
            &canonical_root,
            &recipe.id,
            &canonical_browser_recipe_digest(&recipe).unwrap(),
            &target,
            &repair_test_locator("replacement"),
        )
        .expect_err("a missing recipe file must not be recreated by repair");

        match error {
            BrowserRecipeLocatorReplaceError::Store(BrowserError::MissingFile {
                path: missing,
            }) => assert_eq!(missing, path),
            other => panic!("unexpected missing-file result: {other:?}"),
        }
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(parent).unwrap().count(), 0);
    }

    #[test]
    fn atomic_locator_replace_preserves_filename_document_id_mismatch_without_temps() {
        use crate::browser::replay_repair::BrowserReplayRecipeLocatorTarget;
        use crate::browser::BrowserReplayLocatorSlot;

        let temp = TestDir::new("locator-id-mismatch");
        let canonical_root = temp.0.canonicalize().unwrap();
        let recipe = repair_test_recipe();
        let path = save_recipe(&canonical_root, &recipe).unwrap();
        let mut mismatched = recipe.clone();
        mismatched.id = "different-document-id".to_string();
        let mut mismatch_bytes = serde_json::to_vec_pretty(&mismatched).unwrap();
        mismatch_bytes.push(b'\n');
        std::fs::write(&path, &mismatch_bytes).unwrap();
        let target = BrowserReplayRecipeLocatorTarget::new(
            0,
            "primary".to_string(),
            BrowserReplayLocatorSlot::PrimaryAction,
            repair_test_locator("primary"),
        );

        let error = replace_recipe_locator_atomic(
            &canonical_root,
            &recipe.id,
            &canonical_browser_recipe_digest(&recipe).unwrap(),
            &target,
            &repair_test_locator("replacement"),
        )
        .expect_err("a filename/document-ID mismatch must be rejected");

        assert!(matches!(
            error,
            BrowserRecipeLocatorReplaceError::Store(BrowserError::InvalidRecipe { .. })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), mismatch_bytes);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn atomic_locator_replace_rechecks_after_temp_sync_and_preserves_complete_files() {
        use crate::browser::replay_repair::BrowserReplayRecipeLocatorTarget;
        use crate::browser::BrowserReplayLocatorSlot;

        let temp = TestDir::new("locator-atomic");
        let recipe = repair_test_recipe();
        let path = save_recipe(&temp.0, &recipe).unwrap();
        let canonical_root = temp.0.canonicalize().unwrap();
        let original = std::fs::read(&path).unwrap();
        let digest = canonical_browser_recipe_digest(&recipe).unwrap();
        let target = BrowserReplayRecipeLocatorTarget::new(
            0,
            "primary".to_string(),
            BrowserReplayLocatorSlot::PrimaryAction,
            repair_test_locator("primary"),
        );
        let replacement = repair_test_locator("replacement");
        let verifier = OsRecipeBoundaryVerifier {
            project_root: &canonical_root,
        };
        let error = replace_recipe_locator_atomic_with(
            &canonical_root,
            &recipe.id,
            &digest,
            &target,
            &replacement,
            &FailingReplacer,
            &verifier,
            || {},
        )
        .expect_err("injected replace failure");
        assert!(
            matches!(
                &error,
                BrowserRecipeLocatorReplaceError::Store(BrowserError::Io { ref operation, .. })
                    if operation == "replace recipe atomically"
            ),
            "actual error: {error:?}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );

        let mut external = recipe.clone();
        external.description = "complete external writer".to_string();
        let mut external_bytes = serde_json::to_vec_pretty(&external).unwrap();
        external_bytes.push(b'\n');
        let external_for_hook = external_bytes.clone();
        let error = replace_recipe_locator_atomic_with(
            &canonical_root,
            &recipe.id,
            &digest,
            &target,
            &replacement,
            &OsRecipeFileReplacer,
            &verifier,
            || std::fs::write(&path, &external_for_hook).unwrap(),
        )
        .expect_err("external edit before final compare");
        assert!(matches!(
            error,
            BrowserRecipeLocatorReplaceError::RecipeChanged
        ));
        assert_eq!(std::fs::read(&path).unwrap(), external_bytes);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn cooperating_save_and_locator_apply_serialize_in_both_gate_orders() {
        use crate::browser::replay_repair::BrowserReplayRecipeLocatorTarget;
        use crate::browser::BrowserReplayLocatorSlot;
        use std::sync::mpsc;

        let temp = TestDir::new("cooperating-writers");
        let canonical_root = temp.0.canonicalize().unwrap();
        let original = repair_test_recipe();
        save_recipe(&canonical_root, &original).unwrap();
        let target = BrowserReplayRecipeLocatorTarget::new(
            0,
            "primary".to_string(),
            BrowserReplayLocatorSlot::PrimaryAction,
            repair_test_locator("primary"),
        );
        let candidate = repair_test_locator("cooperating-replacement");

        let digest = canonical_browser_recipe_digest(&original).unwrap();
        let mut later_save = original.clone();
        later_save.description = "save after apply".to_string();
        let gate = RECIPE_WRITE_GATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (started_tx, started_rx) = mpsc::channel();
        let save_root = canonical_root.clone();
        let save_thread = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            save_recipe(&save_root, &later_save)
        });
        started_rx.recv().unwrap();
        let verifier = OsRecipeBoundaryVerifier {
            project_root: &canonical_root,
        };
        replace_recipe_locator_atomic_under_gate(
            &canonical_root,
            &original.id,
            &digest,
            &target,
            &candidate,
            &OsRecipeFileReplacer,
            &verifier,
            || {},
        )
        .expect("apply wins the first gate order");
        drop(gate);
        save_thread
            .join()
            .unwrap()
            .expect("save runs after apply releases the gate");
        let after_apply_then_save = load_recipe(&canonical_root, &original.id).unwrap();
        assert_eq!(after_apply_then_save.description, "save after apply");
        assert_eq!(
            recipe_locator_at(
                &after_apply_then_save,
                0,
                "primary",
                BrowserReplayLocatorSlot::PrimaryAction,
            )
            .unwrap()
            .unwrap(),
            &repair_test_locator("primary")
        );

        save_recipe(&canonical_root, &original).unwrap();
        let digest = canonical_browser_recipe_digest(&original).unwrap();
        let mut winning_save = original.clone();
        winning_save.description = "save before apply".to_string();
        let gate = RECIPE_WRITE_GATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (started_tx, started_rx) = mpsc::channel();
        let apply_root = canonical_root.clone();
        let apply_id = original.id.clone();
        let apply_target = target.clone();
        let apply_candidate = candidate.clone();
        let apply_thread = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            replace_recipe_locator_atomic(
                &apply_root,
                &apply_id,
                &digest,
                &apply_target,
                &apply_candidate,
            )
        });
        started_rx.recv().unwrap();
        save_recipe_with_overwrite_policy_under_gate(&canonical_root, &winning_save, true)
            .expect("save wins the second gate order");
        drop(gate);
        assert!(matches!(
            apply_thread.join().unwrap(),
            Err(BrowserRecipeLocatorReplaceError::RecipeChanged)
        ));
        let after_save_then_apply = load_recipe(&canonical_root, &original.id).unwrap();
        assert_eq!(after_save_then_apply, winning_save);
        let workflow = canonical_root.join(".devmanager").join("browser-workflows");
        assert_eq!(std::fs::read_dir(workflow).unwrap().count(), 1);
    }

    struct FailSecondReadBoundary {
        reads: std::sync::atomic::AtomicUsize,
    }

    impl RecipeBoundaryVerifier for FailSecondReadBoundary {
        fn verify(
            &self,
            boundary: RecipeIoBoundary,
            _parent: &Path,
            path: &Path,
        ) -> Result<(), BrowserError> {
            if boundary == RecipeIoBoundary::BeforeRead
                && self.reads.fetch_add(1, std::sync::atomic::Ordering::AcqRel) == 1
            {
                return Err(BrowserError::OutsideWorkspace {
                    path: path.to_path_buf(),
                });
            }
            Ok(())
        }
    }

    #[test]
    fn atomic_locator_replace_revalidates_path_before_the_final_read() {
        use crate::browser::replay_repair::BrowserReplayRecipeLocatorTarget;
        use crate::browser::BrowserReplayLocatorSlot;

        let temp = TestDir::new("locator-final-reparse");
        let canonical_root = temp.0.canonicalize().unwrap();
        let recipe = repair_test_recipe();
        let path = save_recipe(&canonical_root, &recipe).unwrap();
        let old_bytes = std::fs::read(&path).unwrap();
        let verifier = FailSecondReadBoundary {
            reads: std::sync::atomic::AtomicUsize::new(0),
        };
        let error = replace_recipe_locator_atomic_with(
            &canonical_root,
            &recipe.id,
            &canonical_browser_recipe_digest(&recipe).unwrap(),
            &BrowserReplayRecipeLocatorTarget::new(
                0,
                "primary".to_string(),
                BrowserReplayLocatorSlot::PrimaryAction,
                repair_test_locator("primary"),
            ),
            &repair_test_locator("replacement"),
            &PanickingReplacer,
            &verifier,
            || {},
        )
        .expect_err("final path revalidation must stop replacement");
        assert!(matches!(
            error,
            BrowserRecipeLocatorReplaceError::Store(BrowserError::OutsideWorkspace { .. })
        ));
        assert_eq!(verifier.reads.load(std::sync::atomic::Ordering::Acquire), 2);
        assert_eq!(std::fs::read(&path).unwrap(), old_bytes);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn atomic_locator_replace_accepts_reformatting_and_rejects_every_drift_class() {
        use crate::browser::replay_repair::BrowserReplayRecipeLocatorTarget;
        use crate::browser::BrowserReplayLocatorSlot;

        let temp = TestDir::new("locator-drift");
        let canonical_root = temp.0.canonicalize().unwrap();
        let recipe = repair_test_recipe();
        let path = save_recipe(&canonical_root, &recipe).unwrap();
        let digest = canonical_browser_recipe_digest(&recipe).unwrap();
        let exact_target = BrowserReplayRecipeLocatorTarget::new(
            0,
            "primary".to_string(),
            BrowserReplayLocatorSlot::PrimaryAction,
            repair_test_locator("primary"),
        );
        let replacement = repair_test_locator("replacement");
        let reordered = format!(
            "{{\"steps\":{},\"inputs\":{},\"viewport\":{},\"startUrl\":{},\"description\":{},\"name\":{},\"id\":{},\"schemaVersion\":1}}",
            serde_json::to_string(&recipe.steps).unwrap(),
            serde_json::to_string(&recipe.inputs).unwrap(),
            serde_json::to_string(&recipe.viewport).unwrap(),
            serde_json::to_string(&recipe.start_url).unwrap(),
            serde_json::to_string(&recipe.description).unwrap(),
            serde_json::to_string(&recipe.name).unwrap(),
            serde_json::to_string(&recipe.id).unwrap(),
        );
        std::fs::write(&path, format!("\n{reordered}\n")).unwrap();
        replace_recipe_locator_atomic(
            &canonical_root,
            &recipe.id,
            &digest,
            &exact_target,
            &replacement,
        )
        .expect("format and key order are not semantic drift");
        assert_eq!(
            recipe_locator_at(
                &load_recipe(&canonical_root, &recipe.id).unwrap(),
                0,
                "primary",
                BrowserReplayLocatorSlot::PrimaryAction,
            )
            .unwrap()
            .unwrap(),
            &replacement
        );

        let drift_cases = [
            (
                BrowserReplayRecipeLocatorTarget::new(
                    99,
                    "primary".to_string(),
                    BrowserReplayLocatorSlot::PrimaryAction,
                    repair_test_locator("primary"),
                ),
                BrowserRecipeLocatorReplaceError::StepIndexChanged,
            ),
            (
                BrowserReplayRecipeLocatorTarget::new(
                    0,
                    "wrong-id".to_string(),
                    BrowserReplayLocatorSlot::PrimaryAction,
                    repair_test_locator("primary"),
                ),
                BrowserRecipeLocatorReplaceError::StepIdChanged,
            ),
            (
                BrowserReplayRecipeLocatorTarget::new(
                    0,
                    "primary".to_string(),
                    BrowserReplayLocatorSlot::DragSource,
                    repair_test_locator("primary"),
                ),
                BrowserRecipeLocatorReplaceError::LocatorSlotChanged,
            ),
            (
                BrowserReplayRecipeLocatorTarget::new(
                    0,
                    "primary".to_string(),
                    BrowserReplayLocatorSlot::PrimaryAction,
                    repair_test_locator("wrong-old"),
                ),
                BrowserRecipeLocatorReplaceError::OldLocatorChanged,
            ),
        ];
        for (wrong_target, expected_error) in drift_cases {
            save_recipe(&canonical_root, &recipe).unwrap();
            let before = std::fs::read(&path).unwrap();
            let error = replace_recipe_locator_atomic(
                &canonical_root,
                &recipe.id,
                &digest,
                &wrong_target,
                &replacement,
            )
            .expect_err("wrong exact identity must fail");
            assert_eq!(error, expected_error);
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }

        let before = std::fs::read(&path).unwrap();
        assert_eq!(
            replace_recipe_locator_atomic(
                &canonical_root,
                &recipe.id,
                &digest,
                &exact_target,
                &BrowserRecipeLocator::default(),
            ),
            Err(BrowserRecipeLocatorReplaceError::InvalidCandidate)
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);

        let mut semantic_change = recipe.clone();
        semantic_change.description = "semantic repository change".to_string();
        save_recipe(&canonical_root, &semantic_change).unwrap();
        let changed_bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            replace_recipe_locator_atomic(
                &canonical_root,
                &recipe.id,
                &digest,
                &exact_target,
                &replacement,
            ),
            Err(BrowserRecipeLocatorReplaceError::RecipeChanged)
        );
        assert_eq!(std::fs::read(&path).unwrap(), changed_bytes);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }
}

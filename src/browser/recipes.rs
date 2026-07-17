use super::{BrowserError, BrowserLocator, BrowserViewport};
use serde::de::{DeserializeOwned, Error as _, MapAccess, SeqAccess, Visitor};
use serde::ser::{Error as _, SerializeStruct};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

pub const BROWSER_RECIPE_SCHEMA_VERSION: u32 = 1;
const MAX_RECIPE_WAIT_MS: u64 = 300_000;
const RECIPE_TEMP_PREFIX: &str = ".devmanager-browser-recipe.";
const MAX_RECIPE_TEMP_ENTRIES_TO_SCAN: usize = 1_024;
const MAX_RECIPE_TEMPS_TO_SCAVENGE: usize = 64;
const STALE_RECIPE_TEMP_AGE: Duration = Duration::from_secs(24 * 60 * 60);
static RECIPE_WRITE_GATE: Mutex<()> = Mutex::new(());

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
    pub id: String,
    pub action: BrowserRecipeAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<BrowserRecipeWait>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assertions: Vec<BrowserRecipeAssertion>,
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
    if value == 0 || value > MAX_RECIPE_WAIT_MS {
        return Err(invalid_recipe(format!(
            "recipe {label} must be between 1 and {MAX_RECIPE_WAIT_MS} milliseconds"
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

fn reject_obvious_secret(value: &str, label: &str) -> Result<(), BrowserError> {
    if super::redact_browser_text(value) != value {
        Err(invalid_recipe(format!(
            "{label} contains credential-like material"
        )))
    } else {
        Ok(())
    }
}

fn validate_safe_url(value: &str, label: &str) -> Result<(), BrowserError> {
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
    if authority.contains('@') || super::redact_browser_text(value) != value {
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
    recipe.validate()?;
    let _write_guard = RECIPE_WRITE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let project_root = project_root.as_ref();
    let parent = verified_workflow_directory(project_root, true)?
        .ok_or_else(|| invalid_recipe("recipe workflow directory could not be prepared"))?;
    let path = parent.join(format!("{}.json", recipe.id));
    let boundary_verifier = OsRecipeBoundaryVerifier { project_root };
    boundary_verifier.verify(RecipeIoBoundary::BeforeList, &parent, &parent)?;
    scavenge_stale_recipe_temps(&parent)?;
    let mut json = serde_json::to_vec_pretty(recipe)
        .map_err(|error| invalid_recipe(format!("could not serialize recipe: {error}")))?;
    json.push(b'\n');
    atomic_write_with_boundaries(&path, &json, &OsRecipeFileReplacer, &boundary_verifier)?;
    Ok(path)
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
    Ok(recipe)
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
    let Some(parent) = path.parent() else {
        return Err(invalid_recipe("recipe path has no parent"));
    };
    let (temporary_path, mut temporary_file) = create_sibling_temp(path, boundary_verifier)?;
    let cleanup = TempCleanup(temporary_path.clone());
    temporary_file
        .write_all(bytes)
        .and_then(|_| temporary_file.sync_all())
        .map_err(|error| io_error("write recipe temporary file", &temporary_path, error))?;
    drop(temporary_file);
    boundary_verifier.verify(RecipeIoBoundary::BeforeReplace, parent, path)?;
    replacer
        .replace(&temporary_path, path)
        .map_err(|error| io_error("replace recipe atomically", path, error))?;
    drop(cleanup);
    sync_parent_directory(parent)?;
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
}

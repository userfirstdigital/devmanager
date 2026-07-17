use super::{BrowserError, BrowserLocator, BrowserViewport};
use serde::de::Error as _;
use serde::ser::{Error as _, SerializeStruct};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub const BROWSER_RECIPE_SCHEMA_VERSION: u32 = 1;
const MAX_RECIPE_WAIT_MS: u64 = 300_000;
static RECIPE_WRITE_GATE: Mutex<()> = Mutex::new(());

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
        let value = Value::deserialize(deserializer)?;
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BrowserRecipeViewport {
    pub width: u32,
    pub height: u32,
    pub scale_percent: u16,
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
        let document = BrowserRecipeInputDocument::deserialize(deserializer)?;
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct BrowserRecipeStep {
    pub id: String,
    pub action: BrowserRecipeAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<BrowserRecipeWait>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assertions: Vec<BrowserRecipeAssertion>,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

impl BrowserRecipeAction {
    fn validate(&self, inputs: &HashMap<&str, BrowserRecipeInputKind>) -> Result<(), BrowserError> {
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

impl BrowserRecipeWait {
    fn validate(&self, inputs: &HashMap<&str, BrowserRecipeInputKind>) -> Result<(), BrowserError> {
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

impl BrowserRecipeAssertion {
    fn validate(&self, inputs: &HashMap<&str, BrowserRecipeInputKind>) -> Result<(), BrowserError> {
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

fn validate_value(
    value: &BrowserRecipeValue,
    inputs: &HashMap<&str, BrowserRecipeInputKind>,
    allowed_input_kinds: &[BrowserRecipeInputKind],
    literal_kind: LiteralKind,
) -> Result<(), BrowserError> {
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
    reject_non_regular_destination(&path)?;
    let mut json = serde_json::to_vec_pretty(recipe)
        .map_err(|error| invalid_recipe(format!("could not serialize recipe: {error}")))?;
    json.push(b'\n');
    atomic_write(&path, &json)?;
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
    ensure_direct_recipe_file(&parent, &path)?;
    load_recipe_path(&path, recipe_id)
}

pub fn list_recipes(project_root: impl AsRef<Path>) -> Result<Vec<BrowserRecipeV1>, BrowserError> {
    let Some(parent) = verified_workflow_directory(project_root.as_ref(), false)? else {
        return Ok(Vec::new());
    };
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
        let file_type = entry
            .file_type()
            .map_err(|error| io_error("inspect recipe entry", &path, error))?;
        if file_type.is_symlink() {
            return Err(BrowserError::OutsideWorkspace { path });
        }
        if !file_type.is_file() {
            continue;
        }
        ensure_direct_recipe_file(&parent, &path)?;
        recipes.push(load_recipe_path(&path, recipe_id)?);
    }
    recipes.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(recipes)
}

fn load_recipe_path(path: &Path, recipe_id: &str) -> Result<BrowserRecipeV1, BrowserError> {
    let json = std::fs::read(&path).map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            BrowserError::MissingFile {
                path: path.to_path_buf(),
            }
        } else {
            io_error("read recipe", &path, error)
        }
    })?;
    let value: Value = serde_json::from_slice(&json).map_err(|error| {
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
    let workflow_exists = validate_existing_path(&workflow, RecipePathKind::Directory)?;
    if !workflow_exists {
        if !create {
            return Ok(None);
        }
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
    Other,
}

fn recipe_path_kind(metadata: &std::fs::Metadata) -> RecipePathKind {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        RecipePathKind::Symlink
    } else if file_type.is_dir() {
        RecipePathKind::Directory
    } else if file_type.is_file() {
        RecipePathKind::RegularFile
    } else {
        RecipePathKind::Other
    }
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

trait RecipeFileReplacer {
    fn replace(&self, temporary: &Path, destination: &Path) -> std::io::Result<()>;
}

struct OsRecipeFileReplacer;

impl RecipeFileReplacer for OsRecipeFileReplacer {
    fn replace(&self, temporary: &Path, destination: &Path) -> std::io::Result<()> {
        replace_sibling_file(temporary, destination)
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), BrowserError> {
    atomic_write_with(path, bytes, &OsRecipeFileReplacer)
}

fn atomic_write_with(
    path: &Path,
    bytes: &[u8],
    replacer: &impl RecipeFileReplacer,
) -> Result<(), BrowserError> {
    let Some(parent) = path.parent() else {
        return Err(invalid_recipe("recipe path has no parent"));
    };
    let (temporary_path, mut temporary_file) = create_sibling_temp(path)?;
    let cleanup = TempCleanup(temporary_path.clone());
    temporary_file
        .write_all(bytes)
        .and_then(|_| temporary_file.sync_all())
        .map_err(|error| io_error("write recipe temporary file", &temporary_path, error))?;
    drop(temporary_file);
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

fn create_sibling_temp(path: &Path) -> Result<(PathBuf, std::fs::File), BrowserError> {
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
        let temporary_path = parent.join(format!(".{file_name}.{suffix}.tmp"));
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
}

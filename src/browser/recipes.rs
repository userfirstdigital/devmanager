use super::{BrowserError, BrowserLocator, BrowserViewport};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

pub const BROWSER_RECIPE_SCHEMA_VERSION: u32 = 1;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrowserRecipeHeader {
    schema_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserRecipeV1 {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub description: String,
    pub start_url: String,
    pub viewport: BrowserViewport,
    pub inputs: Vec<BrowserRecipeInput>,
    pub steps: Vec<BrowserRecipeStep>,
}

impl BrowserRecipeV1 {
    pub fn validate(&self) -> Result<(), BrowserError> {
        if self.schema_version != BROWSER_RECIPE_SCHEMA_VERSION {
            return Err(BrowserError::UnsupportedRecipeVersion {
                version: self.schema_version,
            });
        }
        if !is_safe_recipe_id(&self.id) {
            return Err(BrowserError::InvalidRecipe {
                message: format!("recipe id {:?} is not a safe slug", self.id),
            });
        }
        if self.name.trim().is_empty() {
            return Err(BrowserError::InvalidRecipe {
                message: "recipe name cannot be blank".to_string(),
            });
        }

        let mut input_names = HashSet::new();
        for input in &self.inputs {
            if !input_names.insert(input.name.as_str()) {
                return Err(BrowserError::InvalidRecipe {
                    message: format!("duplicate recipe input name {:?}", input.name),
                });
            }
            if input.kind == BrowserRecipeInputKind::Secret && input.default_value.is_some() {
                return Err(BrowserError::InvalidRecipe {
                    message: format!("secret recipe input {:?} cannot have a default", input.name),
                });
            }
        }

        Ok(())
    }
}

fn is_safe_recipe_id(recipe_id: &str) -> bool {
    let Some(first) = recipe_id.chars().next() else {
        return false;
    };
    let Some(last) = recipe_id.chars().next_back() else {
        return false;
    };

    first.is_ascii_alphanumeric()
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
        return Err(BrowserError::InvalidRecipe {
            message: format!("recipe id {recipe_id:?} is not a safe slug"),
        });
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
    let path = recipe_path(project_root, &recipe.id)?;
    let Some(parent) = path.parent() else {
        return Err(BrowserError::InvalidRecipe {
            message: format!("recipe path has no parent: {}", path.display()),
        });
    };
    std::fs::create_dir_all(parent).map_err(|error| BrowserError::Io {
        operation: "create recipe directory".to_string(),
        path: parent.to_path_buf(),
        message: error.to_string(),
    })?;

    let mut json =
        serde_json::to_string_pretty(recipe).map_err(|error| BrowserError::InvalidRecipe {
            message: format!("could not serialize recipe: {error}"),
        })?;
    json.push('\n');
    std::fs::write(&path, json).map_err(|error| BrowserError::Io {
        operation: "write recipe".to_string(),
        path: path.clone(),
        message: error.to_string(),
    })?;
    Ok(path)
}

pub fn load_recipe(
    project_root: impl AsRef<Path>,
    recipe_id: &str,
) -> Result<BrowserRecipeV1, BrowserError> {
    let path = recipe_path(project_root, recipe_id)?;
    let json = std::fs::read_to_string(&path).map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            BrowserError::MissingFile { path: path.clone() }
        } else {
            BrowserError::Io {
                operation: "read recipe".to_string(),
                path: path.clone(),
                message: error.to_string(),
            }
        }
    })?;
    let header: BrowserRecipeHeader =
        serde_json::from_str(&json).map_err(|error| BrowserError::InvalidRecipe {
            message: format!("could not read recipe schema {}: {error}", path.display()),
        })?;
    if header.schema_version != BROWSER_RECIPE_SCHEMA_VERSION {
        return Err(BrowserError::UnsupportedRecipeVersion {
            version: header.schema_version,
        });
    }
    let recipe: BrowserRecipeV1 =
        serde_json::from_str(&json).map_err(|error| BrowserError::InvalidRecipe {
            message: format!("could not parse recipe {}: {error}", path.display()),
        })?;
    recipe.validate()?;
    Ok(recipe)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserRecipeInput {
    pub name: String,
    pub kind: BrowserRecipeInputKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_value: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserRecipeInputKind {
    Text,
    Url,
    File,
    Secret,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserRecipeStep {
    pub id: String,
    pub action: BrowserRecipeAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locator: Option<BrowserLocator>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_condition: Option<String>,
    #[serde(default)]
    pub assertions: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BrowserRecipeAction {
    Navigate,
    Click,
    Hover,
    Focus,
    Type,
    Clear,
    Select,
    Keypress,
    Scroll,
    DragDrop,
    Wait,
    Screenshot,
    Cdp,
}

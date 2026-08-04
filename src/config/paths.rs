use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};

const APP_CONFIG_DIR: &str = "com.userfirst.devmanager";
const CONFIG_FILE_NAME: &str = "config.json";
const REMOTE_FILE_NAME: &str = "remote.json";
const DATABASE_FILE_NAME: &str = "kernel.sqlite3";
const BROWSER_DIR_NAME: &str = "browser";
const LOGS_DIR_NAME: &str = "logs";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppProfile {
    Production,
    Named(String),
    UnitTest(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildKind {
    Debug,
    Release,
    Test,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAppPaths {
    pub root: PathBuf,
    pub config: PathBuf,
    pub remote: PathBuf,
    pub database: PathBuf,
    pub browser_root: PathBuf,
    pub logs: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    InvalidProfileName(String),
    BuildKindProfileMismatch {
        build_kind: BuildKind,
        profile: String,
    },
    ProductionDebugForbidden,
}

impl Display for PathError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidProfileName(name) => {
                write!(f, "invalid app profile name: {name:?}")
            }
            Self::BuildKindProfileMismatch {
                build_kind,
                profile,
            } => {
                write!(
                    f,
                    "build kind {build_kind:?} is incompatible with profile {profile}"
                )
            }
            Self::ProductionDebugForbidden => {
                write!(
                    f,
                    "debug builds cannot use the production profile without an explicit test seam"
                )
            }
        }
    }
}

impl std::error::Error for PathError {}

impl AppProfile {
    pub fn named(raw: &str) -> Result<Self, PathError> {
        Ok(Self::Named(validate_profile_segment(raw)?))
    }

    fn describe(&self) -> String {
        match self {
            Self::Production => "production".to_string(),
            Self::Named(name) => format!("named:{name}"),
            Self::UnitTest(name) => format!("unit-test:{name}"),
        }
    }
}

pub fn resolve_app_paths(
    base: &Path,
    profile: AppProfile,
    build_kind: BuildKind,
) -> Result<ResolvedAppPaths, PathError> {
    resolve_app_paths_inner(base, profile, build_kind, false)
}

#[cfg(test)]
pub fn resolve_app_paths_allowing_production_debug(
    base: &Path,
    profile: AppProfile,
    build_kind: BuildKind,
) -> Result<ResolvedAppPaths, PathError> {
    resolve_app_paths_inner(base, profile, build_kind, true)
}

fn resolve_app_paths_inner(
    base: &Path,
    profile: AppProfile,
    build_kind: BuildKind,
    allow_production_debug: bool,
) -> Result<ResolvedAppPaths, PathError> {
    validate_build_kind_profile(build_kind, &profile, allow_production_debug)?;
    let profile = normalize_profile_payload(profile)?;

    let root = base.join(directory_name(&profile));
    Ok(ResolvedAppPaths {
        config: root.join(CONFIG_FILE_NAME),
        remote: root.join(REMOTE_FILE_NAME),
        database: root.join(DATABASE_FILE_NAME),
        browser_root: root.join(BROWSER_DIR_NAME),
        logs: root.join(LOGS_DIR_NAME),
        root,
    })
}

fn validate_build_kind_profile(
    build_kind: BuildKind,
    profile: &AppProfile,
    allow_production_debug: bool,
) -> Result<(), PathError> {
    match (build_kind, profile) {
        (BuildKind::Test, AppProfile::UnitTest(_)) => Ok(()),
        (BuildKind::Test, _) => Err(PathError::BuildKindProfileMismatch {
            build_kind,
            profile: profile.describe(),
        }),
        (BuildKind::Debug, AppProfile::Production) if allow_production_debug => Ok(()),
        (BuildKind::Debug, AppProfile::Production) => Err(PathError::ProductionDebugForbidden),
        (BuildKind::Debug, AppProfile::Named(_)) => Ok(()),
        (BuildKind::Debug, AppProfile::UnitTest(_)) => Err(PathError::BuildKindProfileMismatch {
            build_kind,
            profile: profile.describe(),
        }),
        (BuildKind::Release, AppProfile::Production | AppProfile::Named(_)) => Ok(()),
        (BuildKind::Release, AppProfile::UnitTest(_)) => Err(PathError::BuildKindProfileMismatch {
            build_kind,
            profile: profile.describe(),
        }),
    }
}

fn normalize_profile_payload(profile: AppProfile) -> Result<AppProfile, PathError> {
    match profile {
        AppProfile::Production => Ok(AppProfile::Production),
        AppProfile::Named(name) => Ok(AppProfile::Named(validate_profile_segment(&name)?)),
        AppProfile::UnitTest(name) if name.is_empty() => Ok(AppProfile::UnitTest(name)),
        AppProfile::UnitTest(name) => Ok(AppProfile::UnitTest(validate_profile_segment(&name)?)),
    }
}

fn validate_profile_segment(raw: &str) -> Result<String, PathError> {
    if raw.is_empty()
        || !raw
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    {
        return Err(PathError::InvalidProfileName(raw.to_string()));
    }

    Ok(raw.to_ascii_lowercase())
}

fn directory_name(profile: &AppProfile) -> String {
    match profile {
        AppProfile::Production => APP_CONFIG_DIR.to_string(),
        AppProfile::Named(name) => format!("{APP_CONFIG_DIR}-{name}"),
        AppProfile::UnitTest(name) if name.is_empty() => APP_CONFIG_DIR.to_string(),
        AppProfile::UnitTest(name) => format!("{APP_CONFIG_DIR}-{name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_lowercases_valid_profiles() {
        assert_eq!(
            AppProfile::named("Native-Next_Dev").unwrap(),
            AppProfile::Named("native-next_dev".to_string())
        );
    }

    #[test]
    fn test_build_rejects_non_unit_test_profiles() {
        let base = Path::new(r"C:\tmp");
        assert!(matches!(
            resolve_app_paths(base, AppProfile::Production, BuildKind::Test),
            Err(PathError::BuildKindProfileMismatch { .. })
        ));
        assert!(matches!(
            resolve_app_paths(base, AppProfile::Named("dev".to_string()), BuildKind::Test),
            Err(PathError::BuildKindProfileMismatch { .. })
        ));
    }

    #[test]
    fn debug_rejects_production_without_test_seam() {
        let base = Path::new(r"C:\tmp");
        assert_eq!(
            resolve_app_paths(base, AppProfile::Production, BuildKind::Debug),
            Err(PathError::ProductionDebugForbidden)
        );
        let allowed = resolve_app_paths_allowing_production_debug(
            base,
            AppProfile::Production,
            BuildKind::Debug,
        )
        .unwrap();
        assert_eq!(allowed.root, base.join(APP_CONFIG_DIR));
    }

    #[test]
    fn unit_test_empty_keeps_production_directory_spelling() {
        let base = Path::new("config-root");
        let paths =
            resolve_app_paths(base, AppProfile::UnitTest(String::new()), BuildKind::Test).unwrap();
        assert_eq!(paths.root, base.join(APP_CONFIG_DIR));
        assert_eq!(paths.config, paths.root.join(CONFIG_FILE_NAME));
        assert_eq!(paths.remote, paths.root.join(REMOTE_FILE_NAME));
        assert_eq!(paths.logs, paths.root.join(LOGS_DIR_NAME));
    }

    #[test]
    fn debug_rejects_unit_test_profiles() {
        let base = Path::new(r"C:\tmp");
        assert!(matches!(
            resolve_app_paths(
                base,
                AppProfile::UnitTest("native-next-dev".to_string()),
                BuildKind::Debug
            ),
            Err(PathError::BuildKindProfileMismatch { .. })
        ));
        assert!(matches!(
            resolve_app_paths(base, AppProfile::UnitTest(String::new()), BuildKind::Debug),
            Err(PathError::BuildKindProfileMismatch { .. })
        ));
    }

    #[test]
    fn resolve_rejects_path_shaped_named_or_unit_test_payloads() {
        let base = Path::new(r"C:\Users\tester\AppData\Roaming");
        for invalid in ["", "..", r"a\b", "a/b", "native next", "../.."] {
            assert!(
                matches!(
                    resolve_app_paths(
                        base,
                        AppProfile::Named(invalid.to_string()),
                        BuildKind::Debug
                    ),
                    Err(PathError::InvalidProfileName(_))
                ),
                "Named accepted {invalid:?}"
            );
            if invalid.is_empty() {
                continue;
            }
            assert!(
                matches!(
                    resolve_app_paths(
                        base,
                        AppProfile::UnitTest(invalid.to_string()),
                        BuildKind::Test
                    ),
                    Err(PathError::InvalidProfileName(_))
                ),
                "UnitTest accepted {invalid:?}"
            );
        }
    }
}

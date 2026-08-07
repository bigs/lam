//! Agent / batteries configuration from `~/.lam/config.toml`.
//!
//! Separate from inference providers (`~/.lam/providers.toml`). Missing file
//! means empty defaults (no batteries unless keys resolve elsewhere).

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use lam_batteries::{ExaConfig, ExaFunction, ParallelConfig, ParallelFunction};
use serde::Deserialize;
use thiserror::Error;

const CONFIG_DIR: &str = ".lam";
const CONFIG_FILE: &str = "config.toml";

/// Loaded agent configuration used when building optional batteries packs.
#[derive(Clone, Debug, Default)]
pub(crate) struct AgentConfig {
    /// Path that was loaded, when known (for diagnostics).
    #[allow(dead_code)]
    pub(crate) path: Option<PathBuf>,
    pub(crate) exa: Option<ResolvedExa>,
    pub(crate) parallel: Option<ResolvedParallel>,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedExa {
    pub(crate) config: ExaConfig,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedParallel {
    pub(crate) config: ParallelConfig,
}

#[derive(Debug, Deserialize)]
struct FileConfig {
    #[serde(default)]
    exa: Option<ProviderSection>,
    #[serde(default)]
    parallel: Option<ProviderSection>,
}

#[derive(Clone, Debug, Deserialize)]
struct ProviderSection {
    /// When false, the provider is not installed even if a key is present.
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    api_key_env: Option<String>,
    /// Optional subset of functions; omit for the default must+should set.
    #[serde(default)]
    functions: Option<Vec<String>>,
    #[serde(default)]
    base_url: Option<String>,
}

fn default_exa_section() -> ProviderSection {
    ProviderSection {
        enabled: true,
        api_key: None,
        api_key_env: Some("EXA_API_KEY".to_owned()),
        functions: None,
        base_url: None,
    }
}

fn default_parallel_section() -> ProviderSection {
    ProviderSection {
        enabled: true,
        api_key: None,
        api_key_env: Some("PARALLEL_API_KEY".to_owned()),
        functions: None,
        base_url: None,
    }
}

fn default_true() -> bool {
    true
}

impl AgentConfig {
    /// Loads `~/.lam/config.toml`, or the explicit path when provided.
    ///
    /// A missing file is not an error: returns empty defaults. Soft-skips
    /// providers whose keys are missing so boot still succeeds.
    pub(crate) fn load(explicit: Option<&Path>) -> Result<(Self, Vec<String>), AgentConfigError> {
        let path = match explicit {
            Some(path) => path.to_path_buf(),
            None => default_path()?,
        };
        let mut warnings = Vec::new();
        let file = match fs::read_to_string(&path) {
            Ok(source) => Some(toml::from_str::<FileConfig>(&source).map_err(|source| {
                AgentConfigError::Parse {
                    path: path.clone(),
                    source,
                }
            })?),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(AgentConfigError::Read {
                    path: path.clone(),
                    source,
                });
            }
        };

        // Omitted sections still try the default env keys so zero-config works.
        // Explicit `enabled = false` turns a provider off entirely.
        let exa_section = file
            .as_ref()
            .and_then(|file| file.exa.clone())
            .unwrap_or_else(default_exa_section);
        let parallel_section = file
            .as_ref()
            .and_then(|file| file.parallel.clone())
            .unwrap_or_else(default_parallel_section);

        let exa = if exa_section.enabled {
            match resolve_exa(&exa_section) {
                Ok(Some(resolved)) => Some(resolved),
                Ok(None) => {
                    warnings.push(
                        "Exa search not configured (set [exa] api_key_env / EXA_API_KEY)."
                            .to_owned(),
                    );
                    None
                }
                Err(error) => {
                    warnings.push(format!("Exa search skipped: {error}"));
                    None
                }
            }
        } else {
            None
        };

        let parallel = if parallel_section.enabled {
            match resolve_parallel(&parallel_section) {
                Ok(Some(resolved)) => Some(resolved),
                Ok(None) => {
                    warnings.push(
                        "Parallel search not configured (set [parallel] api_key_env / PARALLEL_API_KEY)."
                            .to_owned(),
                    );
                    None
                }
                Err(error) => {
                    warnings.push(format!("Parallel search skipped: {error}"));
                    None
                }
            }
        } else {
            None
        };

        Ok((
            Self {
                path: Some(path),
                exa,
                parallel,
            },
            warnings,
        ))
    }

    /// True when at least one search provider will be installed.
    pub(crate) fn has_search(&self) -> bool {
        self.exa.is_some() || self.parallel.is_some()
    }
}

fn resolve_exa(section: &ProviderSection) -> Result<Option<ResolvedExa>, AgentConfigError> {
    let Some(api_key) = resolve_api_key("exa", section, "EXA_API_KEY")? else {
        return Ok(None);
    };
    let functions = parse_exa_functions(section.functions.as_deref())?;
    let mut config = ExaConfig::from_api_key(api_key).functions(functions);
    if let Some(base_url) = &section.base_url {
        config.base_url = base_url.clone();
    }
    Ok(Some(ResolvedExa { config }))
}

fn resolve_parallel(
    section: &ProviderSection,
) -> Result<Option<ResolvedParallel>, AgentConfigError> {
    let Some(api_key) = resolve_api_key("parallel", section, "PARALLEL_API_KEY")? else {
        return Ok(None);
    };
    let functions = parse_parallel_functions(section.functions.as_deref())?;
    let mut config = ParallelConfig::from_api_key(api_key).functions(functions);
    if let Some(base_url) = &section.base_url {
        config.base_url = base_url.clone();
    }
    Ok(Some(ResolvedParallel { config }))
}

fn resolve_api_key(
    name: &str,
    section: &ProviderSection,
    default_env: &str,
) -> Result<Option<String>, AgentConfigError> {
    match (&section.api_key, &section.api_key_env) {
        (Some(_), Some(_)) => Err(AgentConfigError::Invalid {
            message: format!("[{name}] must set only one of api_key or api_key_env"),
        }),
        (Some(key), None) if key.trim().is_empty() => Err(AgentConfigError::Invalid {
            message: format!("[{name}] has an empty api_key"),
        }),
        (Some(key), None) => Ok(Some(key.clone())),
        (None, Some(variable)) if variable.trim().is_empty() => Err(AgentConfigError::Invalid {
            message: format!("[{name}] has an empty api_key_env"),
        }),
        (None, Some(variable)) => match env::var(variable) {
            Ok(value) if !value.trim().is_empty() => Ok(Some(value)),
            _ => Ok(None),
        },
        // Bare `[exa]` / `[parallel]` tables still consult the conventional env var.
        (None, None) => match env::var(default_env) {
            Ok(value) if !value.trim().is_empty() => Ok(Some(value)),
            _ => Ok(None),
        },
    }
}

fn parse_exa_functions(
    names: Option<&[String]>,
) -> Result<BTreeSet<ExaFunction>, AgentConfigError> {
    let Some(names) = names else {
        return Ok(ExaFunction::DEFAULTS.iter().copied().collect());
    };
    if names.is_empty() {
        return Ok(BTreeSet::new());
    }
    let mut out = BTreeSet::new();
    for name in names {
        let Some(function) = ExaFunction::parse(name) else {
            return Err(AgentConfigError::Invalid {
                message: format!(
                    "unknown exa function `{name}`; expected one of search, contents, context, answer, findSimilar"
                ),
            });
        };
        out.insert(function);
    }
    Ok(out)
}

fn parse_parallel_functions(
    names: Option<&[String]>,
) -> Result<BTreeSet<ParallelFunction>, AgentConfigError> {
    let Some(names) = names else {
        return Ok(ParallelFunction::DEFAULTS.iter().copied().collect());
    };
    if names.is_empty() {
        return Ok(BTreeSet::new());
    }
    let mut out = BTreeSet::new();
    for name in names {
        let Some(function) = ParallelFunction::parse(name) else {
            return Err(AgentConfigError::Invalid {
                message: format!(
                    "unknown parallel function `{name}`; expected one of search, extract"
                ),
            });
        };
        out.insert(function);
    }
    Ok(out)
}

fn default_path() -> Result<PathBuf, AgentConfigError> {
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .ok_or(AgentConfigError::HomeUnavailable)?;
    Ok(PathBuf::from(home).join(CONFIG_DIR).join(CONFIG_FILE))
}

#[derive(Debug, Error)]
pub(crate) enum AgentConfigError {
    #[error("could not determine the home directory for ~/.lam/config.toml")]
    HomeUnavailable,
    #[error("could not read agent configuration at `{path}`: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not parse agent configuration at `{path}`: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("invalid agent configuration: {message}")]
    Invalid { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn missing_file_soft_skips_when_using_inline_disabled_sections() {
        // Avoid mutating process env (workspace forbids unsafe_code). An
        // explicit empty file with enabled=false exercises soft-skip without
        // depending on ambient EXA_API_KEY / PARALLEL_API_KEY.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            r#"
[exa]
enabled = false

[parallel]
enabled = false
"#,
        )
        .unwrap();
        let (config, warnings) = AgentConfig::load(Some(&path)).unwrap();
        assert!(config.exa.is_none());
        assert!(config.parallel.is_none());
        assert!(warnings.is_empty());
    }

    #[test]
    fn parses_keys_and_function_allowlist() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            r#"
[exa]
api_key = "exa-test"
functions = ["search", "context"]

[parallel]
enabled = false
api_key = "parallel-test"
"#,
        )
        .unwrap();

        let (config, warnings) = AgentConfig::load(Some(&path)).unwrap();
        assert!(warnings.is_empty());
        let exa = config.exa.expect("exa configured");
        assert_eq!(exa.config.api_key, "exa-test");
        assert!(exa.config.functions.contains(&ExaFunction::Search));
        assert!(exa.config.functions.contains(&ExaFunction::Context));
        assert!(!exa.config.functions.contains(&ExaFunction::Answer));
        assert!(config.parallel.is_none());
    }

    #[test]
    fn rejects_both_api_key_fields() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            r#"
[exa]
api_key = "a"
api_key_env = "B"
"#,
        )
        .unwrap();
        let (config, warnings) = AgentConfig::load(Some(&path)).unwrap();
        assert!(config.exa.is_none());
        assert!(warnings.iter().any(|w| w.contains("only one of")));
    }

    #[test]
    fn missing_env_key_soft_skips_provider() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            r#"
[exa]
api_key_env = "LAM_TEST_MISSING_EXA_KEY_XYZ_NEVER_SET"

[parallel]
enabled = false
"#,
        )
        .unwrap();
        let (config, warnings) = AgentConfig::load(Some(&path)).unwrap();
        assert!(config.exa.is_none());
        assert!(warnings.iter().any(|w| w.contains("Exa")));
    }
}

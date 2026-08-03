use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

const CONFIG_DIR: &str = ".lam";
const CONFIG_FILE: &str = "providers.toml";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ProviderProtocol {
    #[serde(alias = "openai_responses")]
    OpenaiResponses,
    #[serde(alias = "openai_chat_completions")]
    OpenaiChatCompletions,
}

#[derive(Deserialize)]
pub(crate) struct ProvidersConfig {
    pub(crate) default_model: String,
    pub(crate) providers: Vec<ProviderConfig>,
}

#[derive(Deserialize)]
pub(crate) struct ProviderConfig {
    pub(crate) name: String,
    #[serde(rename = "type")]
    pub(crate) protocol: ProviderProtocol,
    #[serde(default, alias = "base_url")]
    pub(crate) api_base: Option<String>,
    #[serde(default)]
    pub(crate) api_key: Option<String>,
    #[serde(default)]
    pub(crate) api_key_env: Option<String>,
    #[serde(default)]
    pub(crate) effort_path: Option<String>,
    pub(crate) models: Vec<ModelConfig>,
}

#[derive(Deserialize)]
pub(crate) struct ModelConfig {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) context_window: u64,
    pub(crate) efforts: Vec<String>,
    #[serde(default)]
    pub(crate) extra_body: toml::Table,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ModelChoice {
    pub(crate) registry_id: String,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) display_name: String,
    pub(crate) context_window: u64,
    pub(crate) efforts: Vec<String>,
}

pub(crate) struct LoadedConfig {
    pub(crate) path: PathBuf,
    pub(crate) config: ProvidersConfig,
    pub(crate) models: Vec<ModelChoice>,
    pub(crate) default_index: usize,
}

impl LoadedConfig {
    pub(crate) fn load(explicit: Option<&Path>) -> Result<Self, ConfigError> {
        let path = match explicit {
            Some(path) => path.to_path_buf(),
            None => default_path()?,
        };
        let source = fs::read_to_string(&path).map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
        let config: ProvidersConfig =
            toml::from_str(&source).map_err(|source| ConfigError::Parse {
                path: path.clone(),
                source,
            })?;
        let (models, default_index) = validate(&config)?;
        Ok(Self {
            path,
            config,
            models,
            default_index,
        })
    }
}

impl ProviderConfig {
    pub(crate) fn resolved_api_key(&self) -> Result<Option<String>, ConfigError> {
        match (&self.api_key, &self.api_key_env) {
            (Some(_), Some(_)) => Err(ConfigError::Invalid {
                message: format!(
                    "provider `{}` must set only one of api_key or api_key_env",
                    self.name
                ),
            }),
            (Some(key), None) if key.trim().is_empty() => Err(ConfigError::Invalid {
                message: format!("provider `{}` has an empty api_key", self.name),
            }),
            (Some(key), None) => Ok(Some(key.clone())),
            (None, Some(variable)) if variable.trim().is_empty() => Err(ConfigError::Invalid {
                message: format!("provider `{}` has an empty api_key_env", self.name),
            }),
            (None, Some(variable)) => {
                env::var(variable)
                    .map(Some)
                    .map_err(|_| ConfigError::MissingEnvironmentKey {
                        provider: self.name.clone(),
                        variable: variable.clone(),
                    })
            }
            (None, None) => Ok(None),
        }
    }

    pub(crate) fn resolved_effort_path(&self) -> Result<Vec<String>, ConfigError> {
        let path = self.effort_path.as_deref().unwrap_or(match self.protocol {
            ProviderProtocol::OpenaiResponses => "reasoning.effort",
            ProviderProtocol::OpenaiChatCompletions => "reasoning_effort",
        });
        let segments = path.split('.').map(str::to_owned).collect::<Vec<_>>();
        if segments.is_empty()
            || segments
                .iter()
                .any(|segment| segment.trim().is_empty() || segment.trim() != segment)
        {
            return Err(invalid(format!(
                "provider `{}` has invalid effort_path `{path}`",
                self.name
            )));
        }
        Ok(segments)
    }
}

fn default_path() -> Result<PathBuf, ConfigError> {
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .ok_or(ConfigError::HomeUnavailable)?;
    Ok(PathBuf::from(home).join(CONFIG_DIR).join(CONFIG_FILE))
}

fn validate(config: &ProvidersConfig) -> Result<(Vec<ModelChoice>, usize), ConfigError> {
    if config.providers.is_empty() {
        return Err(invalid("at least one provider is required"));
    }

    let mut provider_names = BTreeSet::new();
    let mut registry_ids = BTreeSet::new();
    let mut models = Vec::new();
    for provider in &config.providers {
        validate_provider_name(&provider.name)?;
        if !provider_names.insert(provider.name.as_str()) {
            return Err(invalid(format!(
                "provider `{}` is configured more than once",
                provider.name
            )));
        }
        if provider.models.is_empty() {
            return Err(invalid(format!(
                "provider `{}` must configure at least one model",
                provider.name
            )));
        }
        provider.resolved_api_key()?;
        let effort_path = provider.resolved_effort_path()?;

        let mut provider_models = BTreeSet::new();
        for model in &provider.models {
            validate_model_id(&model.id)?;
            if model.name.trim().is_empty() {
                return Err(invalid(format!(
                    "model `{}/{}` has an empty display name",
                    provider.name, model.id
                )));
            }
            if model.context_window == 0 {
                return Err(invalid(format!(
                    "model `{}/{}` has a zero context_window",
                    provider.name, model.id
                )));
            }
            validate_efforts(&provider.name, model)?;
            validate_effort_slot(&provider.name, model, &effort_path)?;
            if !provider_models.insert(model.id.as_str()) {
                return Err(invalid(format!(
                    "model `{}/{}` is configured more than once",
                    provider.name, model.id
                )));
            }
            let registry_id = format!("{}/{}", provider.name, model.id);
            debug_assert!(registry_ids.insert(registry_id.clone()));
            models.push(ModelChoice {
                registry_id,
                provider: provider.name.clone(),
                model: model.id.clone(),
                display_name: model.name.clone(),
                context_window: model.context_window,
                efforts: model.efforts.clone(),
            });
        }
    }

    let default_index = models
        .iter()
        .position(|model| model.registry_id == config.default_model)
        .ok_or_else(|| {
            invalid(format!(
                "default_model `{}` is not present in the provider model list",
                config.default_model
            ))
        })?;
    Ok((models, default_index))
}

fn validate_efforts(provider: &str, model: &ModelConfig) -> Result<(), ConfigError> {
    if model.efforts.is_empty() {
        return Err(invalid(format!(
            "model `{provider}/{}` must configure at least one effort",
            model.id
        )));
    }
    let mut efforts = BTreeSet::new();
    for effort in &model.efforts {
        if effort.trim().is_empty() || effort.trim() != effort {
            return Err(invalid(format!(
                "model `{provider}/{}` has an empty or padded effort",
                model.id
            )));
        }
        if !efforts.insert(effort.as_str()) {
            return Err(invalid(format!(
                "model `{provider}/{}` configures effort `{effort}` more than once",
                model.id
            )));
        }
    }
    Ok(())
}

fn validate_effort_slot(
    provider: &str,
    model: &ModelConfig,
    effort_path: &[String],
) -> Result<(), ConfigError> {
    let mut table = &model.extra_body;
    for (index, segment) in effort_path.iter().enumerate() {
        let Some(value) = table.get(segment) else {
            return Ok(());
        };
        if index + 1 == effort_path.len() {
            return Err(invalid(format!(
                "model `{provider}/{}` configures `{}` in extra_body; use its efforts list instead",
                model.id,
                effort_path.join(".")
            )));
        }
        let Some(next) = value.as_table() else {
            return Err(invalid(format!(
                "model `{provider}/{}` cannot apply effort_path `{}` through a non-table extra_body field",
                model.id,
                effort_path.join(".")
            )));
        };
        table = next;
    }
    Ok(())
}

fn validate_provider_name(value: &str) -> Result<(), ConfigError> {
    if value.trim().is_empty() || value.contains('/') {
        return Err(invalid(format!(
            "provider name `{value}` must be nonempty and must not contain `/`"
        )));
    }
    Ok(())
}

fn validate_model_id(value: &str) -> Result<(), ConfigError> {
    if value.trim().is_empty() || value.trim() != value {
        return Err(invalid(format!(
            "model id `{value}` must be nonempty and must not have surrounding whitespace"
        )));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        message: message.into(),
    }
}

#[derive(Debug, Error)]
pub(crate) enum ConfigError {
    #[error("could not determine the home directory for ~/.lam/providers.toml")]
    HomeUnavailable,
    #[error("could not read provider configuration at `{path}`: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not parse provider configuration at `{path}`: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("invalid provider configuration: {message}")]
    Invalid { message: String },
    #[error("provider `{provider}` expects API key environment variable `{variable}`")]
    MissingEnvironmentKey { provider: String, variable: String },
}

#[cfg(test)]
mod tests {
    use super::{ProvidersConfig, validate};

    const VALID: &str = r#"
default_model = "openai/gpt-5"

[[providers]]
name = "openai"
type = "openai-responses"
api_key = "test-key"

[[providers.models]]
id = "gpt-5"
name = "GPT-5"
context_window = 400000
efforts = ["none", "low", "medium", "high", "xhigh", "max"]

[[providers.models]]
id = "gpt-5-mini"
name = "GPT-5 mini"
context_window = 128000
efforts = ["low", "medium", "high"]
"#;

    #[test]
    fn parses_grouped_models_and_finds_default() {
        let config: ProvidersConfig = toml::from_str(VALID).unwrap();
        let (models, default) = validate(&config).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[default].registry_id, "openai/gpt-5");
        assert_eq!(models[1].display_name, "GPT-5 mini");
    }

    #[test]
    fn distributed_provider_example_matches_the_config_schema() {
        let config: ProvidersConfig =
            toml::from_str(include_str!("../providers.example.toml")).unwrap();
        let model_ids = config
            .providers
            .iter()
            .flat_map(|provider| {
                provider
                    .models
                    .iter()
                    .map(move |model| (provider.name.as_str(), model.id.as_str()))
            })
            .collect::<Vec<_>>();

        assert!(model_ids.contains(&("openai", "gpt-5.6-sol")));
        assert!(model_ids.contains(&("openai", "gpt-5.6-terra")));
        assert!(model_ids.contains(&("synthetic", "syn:large:text")));
        assert!(model_ids.contains(&("synthetic", "syn:small:vision")));
    }

    #[test]
    fn parses_model_specific_request_options() {
        let source = VALID.replace(
            "efforts = [\"none\", \"low\", \"medium\", \"high\", \"xhigh\", \"max\"]",
            "efforts = [\"none\", \"low\", \"medium\", \"high\", \"xhigh\", \"max\"]\n\n[providers.models.extra_body]\nreasoning_history = \"interleaved\"",
        );
        let config: ProvidersConfig = toml::from_str(&source).unwrap();
        assert_eq!(
            config.providers[0].models[0].extra_body["reasoning_history"].as_str(),
            Some("interleaved")
        );
    }

    #[test]
    fn defaults_effort_path_by_protocol_and_rejects_legacy_effort_body() {
        let config: ProvidersConfig = toml::from_str(VALID).unwrap();
        assert_eq!(
            config.providers[0].resolved_effort_path().unwrap(),
            ["reasoning", "effort"]
        );

        let source = VALID.replace(
            "efforts = [\"none\", \"low\", \"medium\", \"high\", \"xhigh\", \"max\"]",
            "efforts = [\"none\", \"low\", \"medium\", \"high\", \"xhigh\", \"max\"]\n\n[providers.models.extra_body.reasoning]\neffort = \"high\"",
        );
        let config: ProvidersConfig = toml::from_str(&source).unwrap();
        assert!(
            validate(&config)
                .unwrap_err()
                .to_string()
                .contains("use its efforts list instead")
        );
    }

    #[test]
    fn rejects_empty_or_duplicate_efforts() {
        for efforts in ["[]", "[\"high\", \"high\"]"] {
            let source = VALID.replacen(
                "[\"none\", \"low\", \"medium\", \"high\", \"xhigh\", \"max\"]",
                efforts,
                1,
            );
            let config: ProvidersConfig = toml::from_str(&source).unwrap();
            assert!(validate(&config).is_err());
        }
    }

    #[test]
    fn accepts_provider_native_model_paths() {
        let source = VALID.replace(
            "gpt-5\"\nname = \"GPT-5\"",
            "accounts/fireworks/models/deepseek-v4-flash-0731\"\nname = \"GPT-5\"",
        );
        let source = source.replace(
            "default_model = \"openai/gpt-5\"",
            "default_model = \"openai/accounts/fireworks/models/deepseek-v4-flash-0731\"",
        );
        let config: ProvidersConfig = toml::from_str(&source).unwrap();
        let (models, default) = validate(&config).unwrap();
        assert_eq!(
            models[default].model,
            "accounts/fireworks/models/deepseek-v4-flash-0731"
        );
    }

    #[test]
    fn rejects_unknown_default_model() {
        let source = VALID.replace("openai/gpt-5\"", "openai/missing\"");
        let config: ProvidersConfig = toml::from_str(&source).unwrap();
        assert!(
            validate(&config)
                .unwrap_err()
                .to_string()
                .contains("is not present")
        );
    }

    #[test]
    fn rejects_duplicate_provider_names() {
        let duplicate = r#"
[[providers]]
name = "openai"
type = "openai-chat-completions"

[[providers.models]]
id = "other"
name = "Other"
context_window = 1000
efforts = ["high"]
"#;
        let source = format!("{VALID}\n{duplicate}");
        let config: ProvidersConfig = toml::from_str(&source).unwrap();
        assert!(
            validate(&config)
                .unwrap_err()
                .to_string()
                .contains("configured more than once")
        );
    }
}

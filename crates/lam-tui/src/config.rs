use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

use crate::codex::CodexCredentialStore;

const CONFIG_DIR: &str = ".lam";
const CONFIG_FILE: &str = "providers.toml";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ProviderProtocol {
    #[serde(alias = "openai_responses")]
    OpenaiResponses,
    /// OpenAI Codex subscription through the official Codex login cache.
    #[serde(
        alias = "openai_codex",
        alias = "codex",
        alias = "chatgpt-subscription"
    )]
    OpenaiCodex,
    #[serde(alias = "openai_chat_completions")]
    OpenaiChatCompletions,
    /// SuperGrok / X Premium subscription via OAuth and the Grok CLI chat proxy.
    #[serde(alias = "xai_supergrok", alias = "supergrok")]
    XaiSupergrok,
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
        let use_builtin_codex = explicit.is_none()
            && CodexCredentialStore::default_store().is_ok_and(|store| store.credentials_present());
        Self::load_path(path, use_builtin_codex)
    }

    fn load_path(path: PathBuf, use_builtin_codex: bool) -> Result<Self, ConfigError> {
        let mut config = match fs::read_to_string(&path) {
            Ok(source) => toml::from_str(&source).map_err(|source| ConfigError::Parse {
                path: path.clone(),
                source,
            })?,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound && use_builtin_codex => {
                codex_only_config()
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.clone(),
                    source,
                });
            }
        };
        if use_builtin_codex {
            add_builtin_codex_provider(&mut config);
        }
        let (models, default_index) = validate(&config)?;
        Ok(Self {
            path,
            config,
            models,
            default_index,
        })
    }
}

fn codex_only_config() -> ProvidersConfig {
    ProvidersConfig {
        default_model: "codex/gpt-5.6-terra".to_owned(),
        providers: vec![builtin_codex_provider("codex")],
    }
}

fn add_builtin_codex_provider(config: &mut ProvidersConfig) {
    if config
        .providers
        .iter()
        .any(|provider| provider.protocol == ProviderProtocol::OpenaiCodex)
    {
        return;
    }
    let name = unique_provider_name(config, "codex");
    config.providers.push(builtin_codex_provider(&name));
}

fn unique_provider_name(config: &ProvidersConfig, preferred: &str) -> String {
    if config
        .providers
        .iter()
        .all(|provider| provider.name != preferred)
    {
        return preferred.to_owned();
    }
    let base = format!("{preferred}-subscription");
    if config
        .providers
        .iter()
        .all(|provider| provider.name != base)
    {
        return base;
    }
    (2..)
        .map(|suffix| format!("{base}-{suffix}"))
        .find(|candidate| {
            config
                .providers
                .iter()
                .all(|provider| provider.name != *candidate)
        })
        .expect("the provider suffix space is not bounded")
}

fn builtin_codex_provider(name: &str) -> ProviderConfig {
    ProviderConfig {
        name: name.to_owned(),
        protocol: ProviderProtocol::OpenaiCodex,
        api_base: None,
        api_key: None,
        api_key_env: None,
        effort_path: None,
        models: [
            ("gpt-5.6-luna", "GPT-5.6 Luna"),
            ("gpt-5.6-sol", "GPT-5.6 Sol"),
            ("gpt-5.6-terra", "GPT-5.6 Terra"),
        ]
        .into_iter()
        .map(|(id, name)| ModelConfig {
            id: id.to_owned(),
            name: name.to_owned(),
            context_window: 1_050_000,
            efforts: ["none", "low", "medium", "high", "xhigh", "max"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            extra_body: reasoning_summary_body(),
        })
        .collect(),
    }
}

fn reasoning_summary_body() -> toml::Table {
    let mut reasoning = toml::Table::new();
    reasoning.insert("summary".to_owned(), toml::Value::String("auto".to_owned()));
    let mut body = toml::Table::new();
    body.insert("reasoning".to_owned(), toml::Value::Table(reasoning));
    body
}

impl ProviderConfig {
    /// Structural check only: rejects contradictory or empty key fields.
    /// Missing environment variables are not an error here — runtime soft-skips.
    pub(crate) fn validate_api_key_fields(&self) -> Result<(), ConfigError> {
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
            (None, Some(variable)) if variable.trim().is_empty() => Err(ConfigError::Invalid {
                message: format!("provider `{}` has an empty api_key_env", self.name),
            }),
            _ => Ok(()),
        }
    }

    /// Resolve the provider API key for runtime use.
    ///
    /// Returns `Ok(None)` when no key is configured. Returns
    /// `Err(MissingEnvironmentKey)` when `api_key_env` is set but absent so
    /// the caller can soft-skip that provider with a clear warning.
    pub(crate) fn resolved_api_key(&self) -> Result<Option<String>, ConfigError> {
        self.validate_api_key_fields()?;
        match (&self.api_key, &self.api_key_env) {
            (Some(key), None) => Ok(Some(key.clone())),
            (None, Some(variable)) => match env::var(variable) {
                Ok(value) if !value.trim().is_empty() => Ok(Some(value)),
                Ok(_) | Err(_) => Err(ConfigError::MissingEnvironmentKey {
                    provider: self.name.clone(),
                    variable: variable.clone(),
                }),
            },
            (None, None) => Ok(None),
            (Some(_), Some(_)) => unreachable!("validated above"),
        }
    }

    pub(crate) fn resolved_effort_path(&self) -> Result<Vec<String>, ConfigError> {
        let path = self.effort_path.as_deref().unwrap_or(match self.protocol {
            ProviderProtocol::OpenaiResponses
            | ProviderProtocol::OpenaiCodex
            | ProviderProtocol::XaiSupergrok => "reasoning.effort",
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
        match provider.protocol {
            ProviderProtocol::OpenaiCodex
                if provider.api_key.is_some() || provider.api_key_env.is_some() =>
            {
                return Err(invalid(format!(
                    "provider `{}` uses Codex subscription credentials and must not set api_key or api_key_env",
                    provider.name
                )));
            }
            ProviderProtocol::OpenaiCodex if provider.api_base.is_some() => {
                return Err(invalid(format!(
                    "provider `{}` must not override api_base because Codex credentials are restricted to the ChatGPT Codex endpoint",
                    provider.name
                )));
            }
            ProviderProtocol::XaiSupergrok | ProviderProtocol::OpenaiCodex => {}
            ProviderProtocol::OpenaiResponses | ProviderProtocol::OpenaiChatCompletions => {
                provider.validate_api_key_fields()?;
            }
        }
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
    use std::fs;

    use super::{LoadedConfig, ProvidersConfig, add_builtin_codex_provider, validate};

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
    fn signed_in_codex_is_added_to_the_default_provider_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("providers.toml");
        fs::write(&path, VALID).unwrap();

        let loaded = LoadedConfig::load_path(path, true).unwrap();

        assert_eq!(loaded.config.providers.len(), 2);
        assert!(
            loaded
                .models
                .iter()
                .any(|model| model.registry_id == "codex/gpt-5.6-sol")
        );
        assert!(
            loaded
                .models
                .iter()
                .any(|model| model.registry_id == "codex/gpt-5.6-terra")
        );
        assert_eq!(
            loaded.models[loaded.default_index].registry_id,
            "openai/gpt-5"
        );
    }

    #[test]
    fn signed_in_codex_can_start_without_a_provider_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("providers.toml");

        let loaded = LoadedConfig::load_path(path, true).unwrap();

        assert_eq!(loaded.config.providers.len(), 1);
        assert_eq!(loaded.models.len(), 3);
        assert_eq!(
            loaded.models[loaded.default_index].registry_id,
            "codex/gpt-5.6-terra"
        );
    }

    #[test]
    fn configured_codex_provider_is_not_added_twice() {
        let mut config: ProvidersConfig = toml::from_str(
            &VALID
                .replace("type = \"openai-responses\"", "type = \"openai-codex\"")
                .replace("api_key = \"test-key\"\n", ""),
        )
        .unwrap();

        add_builtin_codex_provider(&mut config);

        assert_eq!(config.providers.len(), 1);
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
        assert!(model_ids.contains(&("codex", "gpt-5.6-sol")));
        assert!(model_ids.contains(&("codex", "gpt-5.6-terra")));
        assert!(model_ids.contains(&("synthetic", "syn:large:text")));
        assert!(model_ids.contains(&("synthetic", "syn:small:vision")));
        assert!(model_ids.contains(&("xai", "grok-4.5")));
        assert!(model_ids.contains(&("xai", "grok-build-0.1")));
        let openai = config
            .providers
            .iter()
            .find(|provider| provider.name == "openai")
            .expect("example configures openai");
        for model in &openai.models {
            let summary = model
                .extra_body
                .get("reasoning")
                .and_then(|value| value.get("summary"))
                .and_then(|value| value.as_str());
            assert_eq!(
                summary,
                Some("auto"),
                "openai model {} should opt into reasoning summaries",
                model.id
            );
        }
        assert_eq!(
            config
                .providers
                .iter()
                .find(|provider| provider.name == "xai")
                .map(|provider| provider.protocol),
            Some(super::ProviderProtocol::XaiSupergrok)
        );
        assert_eq!(
            config
                .providers
                .iter()
                .find(|provider| provider.name == "codex")
                .map(|provider| provider.protocol),
            Some(super::ProviderProtocol::OpenaiCodex)
        );
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

    #[test]
    fn missing_api_key_env_is_not_a_config_error() {
        let source = VALID.replace(
            r#"api_key = "test-key""#,
            r#"api_key_env = "LAM_TEST_MISSING_PROVIDER_KEY_XYZ""#,
        );
        let config: ProvidersConfig = toml::from_str(&source).unwrap();
        // Structural validation succeeds; runtime soft-skips the provider.
        validate(&config).expect("missing env key must not fail config load");
        let err = config.providers[0].resolved_api_key().unwrap_err();
        assert!(matches!(
            err,
            super::ConfigError::MissingEnvironmentKey { .. }
        ));
    }

    #[test]
    fn codex_subscription_rejects_api_keys() {
        let source = VALID
            .replace("type = \"openai-responses\"", "type = \"openai-codex\"")
            .replace("api_key = \"test-key\"", "api_key = \"must-not-be-here\"");
        let config: ProvidersConfig = toml::from_str(&source).unwrap();
        assert!(
            validate(&config)
                .unwrap_err()
                .to_string()
                .contains("must not set api_key")
        );
    }
}

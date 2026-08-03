use std::path::{Path, PathBuf};

use lam::{Lam, LamBuilder, MemStore, Model, ModelDescriptor};
use lam_agents::{Agent, AgentSystem, AgentSystemEvents, SubagentConfig, SubagentConfigBuilder};
use lam_code::{CodingPack, FilesystemAccess, LocalCommandRunner};
use lam_openai::chat_completions::{
    ChatCompletions, ChatCompletionsCodec, ChatCompletionsProvider,
};
use lam_openai::responses::{Responses, ResponsesCodec, ResponsesProvider};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::config::{LoadedConfig, ModelChoice, ModelConfig, ProviderConfig, ProviderProtocol};

pub(crate) struct Runtime {
    pub(crate) system: AgentSystem<MemStore>,
    pub(crate) root: Agent<MemStore>,
    pub(crate) events: AgentSystemEvents,
    pub(crate) models: Vec<ModelChoice>,
    pub(crate) selected_model: usize,
}

#[derive(Clone, Debug)]
pub(crate) enum Command {
    Call(String),
    Compact,
    SwitchModel { index: usize, registry_id: String },
}

#[derive(Debug)]
pub(crate) enum CommandResult {
    Call(Result<String, String>),
    Compact(Result<String, String>),
    SwitchModel {
        index: usize,
        result: Result<String, String>,
    },
}

impl Runtime {
    pub(crate) async fn build(config: LoadedConfig, cwd: PathBuf) -> Result<Self, RuntimeError> {
        let models = configured_models(&config)?;
        let initial = models
            .get(config.default_index)
            .expect("validated default model index");
        let coding = CodingPack::builder(&cwd)
            .filesystem_access(FilesystemAccess::ReadWrite)
            .shell(LocalCommandRunner::default())
            .build()
            .map_err(|error| RuntimeError::CodingPack(error.to_string()))?;
        let system = AgentSystem::builder(MemStore::new())
            .worker_threads(default_worker_threads())
            .max_agents(64)
            .build()
            .map_err(|error| RuntimeError::AgentSystem(error.to_string()))?;
        let events = system
            .take_events()
            .expect("a new agent system owns its event receiver");
        let instruction = coding_instruction(&cwd);

        let mut root_builder = initial
            .model
            .lam_builder()
            .initial_model_id(initial.choice.registry_id.clone())
            .state_store(system.state_store())
            .namespaces(coding.namespaces())
            .context_window_tokens(smallest_context_window(&config.models))
            .annotate_system_prompt(&instruction);
        for configured in &models {
            if configured.choice.registry_id != initial.choice.registry_id {
                root_builder = configured
                    .model
                    .register_lam(root_builder, configured.choice.registry_id.clone());
            }
        }

        let mut child_builder: SubagentConfigBuilder<MemStore> = initial.model.subagent_builder();
        for configured in &models {
            if configured.choice.registry_id != initial.choice.registry_id {
                child_builder = configured.model.register_subagent(child_builder);
            }
        }
        let children: SubagentConfig<MemStore> = child_builder
            .namespaces(coding.namespaces())
            .required_instructions(instruction)
            .build()
            .map_err(|error| RuntimeError::AgentSystem(error.to_string()))?;
        let root = system
            .host_with_subagents(root_builder.build().actor("/root"), children)
            .await
            .map_err(|error| RuntimeError::AgentSystem(error.to_string()))?;

        Ok(Self {
            system,
            root,
            events,
            models: config.models,
            selected_model: config.default_index,
        })
    }

    pub(crate) fn execute(&self, command: Command, output: mpsc::UnboundedSender<CommandResult>) {
        let root = self.root.clone();
        tokio::spawn(async move {
            let result = match command {
                Command::Call(input) => {
                    CommandResult::Call(root.call(input).await.map_err(|error| error.to_string()))
                }
                Command::Compact => {
                    let result = root.compact().await.map(|receipt| match receipt {
                        Some(receipt) => format!(
                            "Compacted context through sequence {}.",
                            receipt.covers_through.get()
                        ),
                        None => "Context is already compact.".to_owned(),
                    });
                    CommandResult::Compact(result.map_err(|error| error.to_string()))
                }
                Command::SwitchModel { index, registry_id } => {
                    let result = root
                        .switch_model(registry_id)
                        .await
                        .map(|receipt| format!("Switched to {}.", receipt.selected_model_id));
                    CommandResult::SwitchModel {
                        index,
                        result: result.map_err(|error| error.to_string()),
                    }
                }
            };
            let _ = output.send(result);
        });
    }
}

struct ConfiguredModel {
    choice: ModelChoice,
    model: AnyModel,
}

enum AnyModel {
    Responses(Model<ResponsesProvider, ResponsesCodec>),
    ChatCompletions(Model<ChatCompletionsProvider, ChatCompletionsCodec>),
}

impl AnyModel {
    fn lam_builder(&self) -> LamBuilder<MemStore> {
        match self {
            Self::Responses(model) => Lam::builder(model.clone()),
            Self::ChatCompletions(model) => Lam::builder(model.clone()),
        }
    }

    fn register_lam<S>(&self, builder: LamBuilder<S>, id: String) -> LamBuilder<S> {
        match self {
            Self::Responses(model) => builder.model(id, model.clone()),
            Self::ChatCompletions(model) => builder.model(id, model.clone()),
        }
    }

    fn subagent_builder(&self) -> SubagentConfigBuilder<MemStore> {
        match self {
            Self::Responses(model) => SubagentConfig::builder(model.clone()),
            Self::ChatCompletions(model) => SubagentConfig::builder(model.clone()),
        }
    }

    fn register_subagent(
        &self,
        builder: SubagentConfigBuilder<MemStore>,
    ) -> SubagentConfigBuilder<MemStore> {
        match self {
            Self::Responses(model) => builder.model(model.clone()),
            Self::ChatCompletions(model) => builder.model(model.clone()),
        }
    }
}

fn configured_models(config: &LoadedConfig) -> Result<Vec<ConfiguredModel>, RuntimeError> {
    let mut configured = Vec::with_capacity(config.models.len());
    for (provider, model, choice) in config.config.providers.iter().flat_map(|provider| {
        provider.models.iter().map(move |model| {
            let registry_id = format!("{}/{}", provider.name, model.id);
            let choice = config
                .models
                .iter()
                .find(|choice| choice.registry_id == registry_id)
                .expect("validated model list");
            (provider, model, choice)
        })
    }) {
        configured.push(ConfiguredModel {
            choice: choice.clone(),
            model: build_model(provider, model, choice)?,
        });
    }
    Ok(configured)
}

fn build_model(
    provider: &ProviderConfig,
    model_config: &ModelConfig,
    choice: &ModelChoice,
) -> Result<AnyModel, RuntimeError> {
    let api_key = provider
        .resolved_api_key()
        .map_err(|error| RuntimeError::Model(error.to_string()))?;
    let descriptor = |codec| {
        ModelDescriptor::new(&provider.name, &choice.model, codec)
            .expect("configuration validation rejects empty names")
    };
    let extra_body = serde_json::to_value(&model_config.extra_body)
        .map_err(|error| RuntimeError::Model(error.to_string()))?;
    match provider.protocol {
        ProviderProtocol::OpenaiResponses => {
            let key = api_key.ok_or_else(|| {
                RuntimeError::Model(format!(
                    "provider `{}` requires api_key or api_key_env for OpenAI Responses",
                    provider.name
                ))
            })?;
            let mut builder = Responses::builder(&choice.model)
                .api_key(key)
                .extra_body(extra_body);
            if let Some(base) = &provider.api_base {
                builder = builder.base_url(base);
            }
            let model = builder
                .build()
                .map_err(|error| RuntimeError::Model(error.to_string()))?
                .with_descriptor(descriptor("openai/responses"));
            Ok(AnyModel::Responses(model))
        }
        ProviderProtocol::OpenaiChatCompletions => {
            let mut builder = ChatCompletions::builder(&choice.model).extra_body(extra_body);
            if let Some(key) = api_key {
                builder = builder.api_key(key);
            }
            if let Some(base) = &provider.api_base {
                builder = builder.base_url(base);
            }
            let model = builder
                .build()
                .map_err(|error| RuntimeError::Model(error.to_string()))?
                .with_descriptor(descriptor("openai/chat-completions"));
            Ok(AnyModel::ChatCompletions(model))
        }
    }
}

fn smallest_context_window(models: &[ModelChoice]) -> u64 {
    models
        .iter()
        .map(|model| model.context_window)
        .min()
        .expect("configuration validation requires a model")
}

fn default_worker_threads() -> usize {
    std::thread::available_parallelism()
        .map_or(2, usize::from)
        .clamp(1, 4)
}

fn coding_instruction(cwd: &Path) -> String {
    format!(
        "You are a coding agent operating in `{}`. Work within this directory unless the user explicitly directs you otherwise. Use the installed coding and multi-agent namespaces when they help complete the task.",
        cwd.display()
    )
}

#[derive(Debug, Error)]
pub(crate) enum RuntimeError {
    #[error("could not configure a model: {0}")]
    Model(String),
    #[error("could not configure coding capabilities: {0}")]
    CodingPack(String),
    #[error("could not start the agent runtime: {0}")]
    AgentSystem(String),
}

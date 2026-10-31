use futures::future::BoxFuture;
use maki_agent::headless::SessionPlugins;
use maki_agent::prompt::ResolvedPrompts;
use maki_agent::{AgentEvent, CancelToken, Envelope};
use maki_lua::agent_autocmd::HostEventKind;
use maki_lua::{AgentHandle, Component, CoreHandle, PluginHost};
use maki_storage::id::MakiId;
use maki_types::MoveOutcome;
use serde_json::{Value, json};
use std::env;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use color_eyre::Result;
use color_eyre::eyre::{Context, ContextCompat, eyre};

use maki_acp::AcpParams;
use maki_config::Config;
use maki_config::load_env_files;
use maki_config::project::{self, ProjectDecision, Runtime, TrustMode, trust_question_lines};
use maki_lua::{InitFiles, Interaction};
use maki_lua::{PackCommand, PackPreparation};
use maki_providers::Model;
use maki_storage::{StateDir, logging};
use maki_wire::plugins::{PluginOperation, PluginReply, PluginRequest};

use crate::interrupt::Interrupt;
use crate::setup;
use crate::start::{
    BuiltinFailure, config_or_fallback, load_plugins, report_warnings, stop_plugins,
};
use crate::{AcpArgs, HostFlags, provider_scripts};

pub fn run(args: AcpArgs, flags: &HostFlags, trust_mode: TrustMode) -> Result<()> {
    let (no_plugins, no_jit) = (flags.no_plugins, flags.no_jit);
    let storage = StateDir::resolve().context("resolve data directory")?;
    maki_providers::model_registry::load_from_storage(&storage);

    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    let trust = project::resolve(&storage, &cwd, trust_mode, Runtime::Agent);
    load_env_files(&trust.project_config);

    let (plugin_host, mut config, mut warnings) = load_agent(no_plugins, no_jit, &trust, None)?;
    warnings.extend(provider_scripts::startup_warning());
    report_warnings(warnings);

    let (model, warning) = setup::resolve_model(args.model.as_deref(), &config.provider, &storage)?;
    report_warnings(Vec::from_iter(warning));
    setup::remember_thinking(&mut config.session_defaults, &storage);

    logging::init_logging(
        config.storage.max_log_bytes,
        config.storage.max_log_files,
        trust.project_config.config_root(),
    );
    setup::init_telemetry(&config.telemetry);
    logging::install_panic_log_hook();
    setup::warn_ignored_provider_fields();

    let mut params = parameters(
        &plugin_host,
        &config,
        model,
        cwd,
        trust_mode,
        storage,
        args.yolo || config.always_yolo,
    );
    params.background = args.background_timeout != Some(0);
    let interrupt = Interrupt::install();
    params.stop = interrupt.stop();
    let runtime = Mutex::new(Some((plugin_host, config)));
    params.administration = Some(Arc::new(move |request, params| {
        let mut runtime = runtime.lock().map_err(|error| error.to_string())?;
        administer(&mut runtime, no_plugins, no_jit, &args, request, params)
            .map_err(|error| format!("{error:#}"))
    }));
    maki_acp::run(params)?;
    interrupt.exit_if_caught();
    Ok(())
}

pub fn parameters(
    host: &PluginHost,
    config: &Config,
    model: Model,
    cwd: PathBuf,
    trust_mode: TrustMode,
    storage: StateDir,
    yolo: bool,
) -> AcpParams {
    let core = host.core_handle();
    AcpParams {
        initial: None,
        administration: None,
        plugins: Some(session_plugins(host)),
        model,
        config: config.agent.clone(),
        timeouts: maki_providers::Timeouts::from(&config.provider),
        initial_wd: cwd,
        prompt_slots: Arc::new(host.agent_handle().collect_prompt_slots()),
        yolo,
        defaults: config.session_defaults,
        model_policy: Arc::new(config.provider.model_policy.clone()),
        plugin_rules: host.plugin_rules(),
        trust_mode,
        trust_policy: Arc::new(config.trust.clone()),
        on_session_end: Some(Arc::new(move |id, reason| {
            let core = core.clone();
            Box::pin(async move { core.end_session_async(id, reason).await })
        })),
        storage,
        background: true,
        stop: CancelToken::none(),
    }
}

fn load_agent(
    no_plugins: bool,
    no_jit: bool,
    trust: &ProjectDecision,
    fallback: Option<Config>,
) -> Result<(PluginHost, Config, Vec<String>)> {
    load_plugins(
        no_jit,
        no_plugins,
        BuiltinFailure::Fatal,
        Interaction::None,
        |host, names, warnings| {
            warnings.extend(trust.warning.clone());
            let config = (|| {
                let config = host
                    .load_init_files(
                        InitFiles::resolve(&trust.project_config, no_plugins),
                        warnings,
                    )
                    .context("load init.lua files")?
                    .unwrap_or_default()
                    .into_config(&names(host)?)
                    .context("invalid config")?;
                config.validate()?;
                Ok(config)
            })();
            config_or_fallback(config, fallback, warnings)
        },
    )
}

fn administer(
    runtime: &mut Option<(PluginHost, Config)>,
    no_plugins: bool,
    no_jit: bool,
    args: &AcpArgs,
    request: PluginRequest,
    mut params: AcpParams,
) -> Result<(AcpParams, PluginReply)> {
    let trust = project::resolve(
        &params.storage,
        &params.initial_wd,
        params.trust_mode,
        Runtime::Agent,
    );
    let load_env = request.operation == PluginOperation::Trust && trust.state.question().is_some();
    let mut plan = None;
    if let Some((name, args, bang)) = request.operation.command() {
        let command = PackCommand::parse(name, args, bang).map_err(|error| eyre!(error))?;
        let host = &runtime.as_ref().context("reload the agent first")?.0;
        let context = host
            .core_handle()
            .package_context()
            .map_err(|error| eyre!(error))?;
        match maki_lua::prepare_pack_command(
            &command,
            &context,
            &[Component::Agent],
            maki_lua::bundled(),
        ) {
            PackPreparation::Complete(report) => {
                return Ok((params, PluginReply::Complete(report.message())));
            }
            PackPreparation::Review { prompt, .. } if request.review.as_ref() != Some(&prompt) => {
                return Ok((params, PluginReply::Review(prompt)));
            }
            PackPreparation::Review { plan: prepared, .. } | PackPreparation::Ready(prepared) => {
                plan = Some(prepared)
            }
        }
    }
    if request.operation == PluginOperation::Trust
        && let Some(question) = trust.state.question()
    {
        let prompt = trust_question_lines(question).join("\n");
        if request.review.as_ref() != Some(&prompt) {
            return Ok((params, PluginReply::Review(prompt)));
        }
        project::grant(&params.storage, question).map_err(|error| eyre!(error))?;
    }
    let fallback = runtime.take().map(|(mut host, config)| {
        stop_plugins(&mut host);
        drop(host);
        config
    });
    let message = plan
        .map(maki_lua::apply_pack_plan)
        .map(|report| report.message())
        .unwrap_or_default();
    let trust = project::resolve(
        &params.storage,
        &params.initial_wd,
        params.trust_mode,
        Runtime::Agent,
    );
    if load_env {
        load_env_files(&trust.project_config);
    }
    let (host, config, mut warnings) = load_agent(no_plugins, no_jit, &trust, fallback)?;
    if !message.is_empty() {
        warnings.push(message);
    }
    let model = match setup::resolve_model(args.model.as_deref(), &config.provider, &params.storage)
    {
        Ok((model, warning)) => {
            warnings.extend(warning);
            model
        }
        Err(error) => {
            warnings.push(format!(
                "model reload failed, using previous model: {error:#}"
            ));
            params.model
        }
    };
    let administration = params.administration.take();
    let stop = params.stop.clone();
    params = parameters(
        &host,
        &config,
        model,
        params.initial_wd,
        params.trust_mode,
        params.storage,
        args.yolo || config.always_yolo,
    );
    params.background = args.background_timeout != Some(0);
    params.administration = administration;
    params.stop = stop;
    *runtime = Some((host, config));
    Ok((params, PluginReply::Complete(warnings.join("; "))))
}

pub fn session_plugins(host: &PluginHost) -> Arc<dyn SessionPlugins> {
    Arc::new(LuaSession {
        core: host.core_handle(),
        agent: host.agent_handle(),
    })
}

struct LuaSession {
    core: CoreHandle,
    agent: AgentHandle,
}

impl SessionPlugins for LuaSession {
    fn prompts(&self) -> BoxFuture<'static, ResolvedPrompts> {
        let agent = self.agent.clone();
        Box::pin(async move { agent.collect_prompt_slots_async().await })
    }

    fn observe(&self, session: MakiId, envelope: &Envelope) {
        let envelope = match &envelope.event {
            AgentEvent::PluginTask { envelope, .. } => envelope,
            _ => envelope,
        };
        self.core
            .dispatch(&session, envelope, envelope.subagent.is_some());
    }

    fn fire(&self, session: MakiId, event: &str, mut data: Value) -> Result<(), String> {
        HostEventKind::from_str(event).map_err(|error| error.to_string())?;
        if !data.is_object() {
            return Err("event data must be an object".into());
        }
        data["session_id"] = json!(session);
        self.core.fire_autocmd(event, data);
        Ok(())
    }

    fn move_tool(&self, id: String) -> BoxFuture<'static, MoveOutcome> {
        let response = self.agent.request_background(id);
        Box::pin(async move { response.recv_async().await.unwrap_or(MoveOutcome::NoHook) })
    }

    fn stop_task(&self, id: &str) {
        self.agent.cancel_plugin_session(id);
    }
}

#[cfg(test)]
mod tests {
    use maki_agent::tools::ToolRegistry;
    use maki_providers::test_support::isolated_home;
    use maki_wire::client::{END_OF_INPUT, Link};
    use maki_wire::{extensions, plugins};
    use std::fs;
    use test_case::test_case;

    use super::*;

    const REQUEST_ID: u64 = 1;
    const COMPLETE_STATUS: &str = "complete";
    const MODEL: &str = "anthropic/claude-sonnet-4-20250514";
    const UPDATE_REFUSED: &str = "missing: not a package declared with maki.pack.add";
    const DELETE_REFUSED: &str = "missing: not an installed package";

    async fn request(link: &Link, method: &str, params: Value) -> Value {
        link.tx
            .send(
                json!({ "jsonrpc": "2.0", "id": REQUEST_ID, "method": method, "params": params })
                    .to_string(),
            )
            .unwrap();
        loop {
            let reply: Value = serde_json::from_str(&link.rx.recv_async().await.unwrap()).unwrap();
            if reply["id"] == REQUEST_ID {
                assert!(reply.get("error").is_none(), "{reply}");
                return reply["result"].clone();
            }
        }
    }

    #[test_case(PluginOperation::Reload, None ; "reload")]
    #[test_case(PluginOperation::Trust, None ; "trust")]
    #[test_case(PluginOperation::Update { args: "missing".into(), bang: false }, Some(UPDATE_REFUSED) ; "update")]
    #[test_case(PluginOperation::Delete { args: "missing".into(), bang: false }, Some(DELETE_REFUSED) ; "delete")]
    fn administration_uses_the_native_paths(operation: PluginOperation, refusal: Option<&str>) {
        let root = isolated_home();
        let project = root.path().join("project");
        fs::create_dir_all(project.join(".git")).unwrap();
        fs::create_dir_all(project.join(".maki")).unwrap();
        fs::write(
            project.join(".maki/init.lua"),
            "maki.setup({ always_fast = true })",
        )
        .unwrap();
        let storage = StateDir::from_path(root.path().join("state"));
        let host = PluginHost::new(Arc::clone(ToolRegistry::global_arc())).unwrap();
        let config = maki_config::RawConfig::default().into_config(&[]).unwrap();
        let mut params = parameters(
            &host,
            &config,
            Model::from_spec(MODEL).unwrap(),
            project,
            if operation == PluginOperation::Trust {
                TrustMode::Consult
            } else {
                TrustMode::Session
            },
            storage,
            false,
        );
        let runtime = Arc::new(Mutex::new(Some((host, config))));
        let args = AcpArgs {
            model: Some(MODEL.into()),
            yolo: false,
            login: false,
            background_timeout: None,
        };
        params.administration = Some({
            let runtime = Arc::clone(&runtime);
            Arc::new(move |request, params| {
                administer(
                    &mut runtime.lock().unwrap(),
                    false,
                    true,
                    &args,
                    request,
                    params,
                )
                .map_err(|error| format!("{error:#}"))
            })
        });
        let storage = params.storage.clone();
        let project = params.initial_wd.clone();
        let (link, server) = maki_acp::server::memory_link();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let serving = tokio::spawn(maki_acp::server::serve_channels(
                params,
                server.input,
                server.output,
            ));
            let initialized = request(&link, "initialize", json!({ "protocolVersion": 1, "clientCapabilities": {}, "_meta": extensions::request_meta() })).await;
            assert_eq!(
                initialized["_meta"][extensions::META_KEY][plugins::KEY],
                plugins::VERSION
            );
            let mut operation = PluginRequest {
                operation,
                review: None,
            };
            let reply: PluginReply = serde_json::from_value(
                request(
                    &link,
                    plugins::METHOD,
                    serde_json::to_value(&operation).unwrap(),
                )
                .await,
            )
            .unwrap();
            if let Some(refusal) = refusal {
                assert!(matches!(reply, PluginReply::Complete(message) if message == refusal));
            } else {
                if operation.operation == PluginOperation::Trust {
                    let PluginReply::Review(prompt) = reply else {
                        panic!("expected a trust review");
                    };
                    assert!(prompt.contains("init.lua") && !prompt.contains("ui.lua"));
                    operation.review = Some(prompt);
                    let reply = request(
                        &link,
                        plugins::METHOD,
                        serde_json::to_value(operation).unwrap(),
                    )
                    .await;
                    assert_eq!(reply["status"], COMPLETE_STATUS);
                    assert!(
                        project::resolve(&storage, &project, TrustMode::Consult, Runtime::Agent)
                            .project_config
                            .is_trusted()
                    );
                }
                assert!(
                    runtime
                        .lock()
                        .unwrap()
                        .as_ref()
                        .unwrap()
                        .1
                        .session_defaults
                        .fast
                );
            }
            link.tx.send(END_OF_INPUT.into()).unwrap();
            serving.await.unwrap();
        });
    }
}

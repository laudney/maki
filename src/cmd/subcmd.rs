use std::env;
use std::path::Path;

use color_eyre::Result;
use color_eyre::eyre::{Context, bail};

use maki_agent::mcp::{config as mcp_config, oauth as mcp_oauth};
use maki_agent::tools::ToolRegistry;
use maki_config::project::{self, Runtime, TrustMode};
use maki_host::start::{cli_stack, report_warnings};

use maki_providers::provider::fetch_all_models;
use maki_storage::StateDir;
use tokio::runtime::Handle;

const PLAN_PREVIEW_PATH: &str = "plan.md";

pub fn models(no_plugins: bool, no_jit: bool, refresh: bool, trust_mode: TrustMode) -> Result<()> {
    // Model listing calls plugin hooks, so the host outlives the fetch.
    let (_host, config) = cli_stack(no_plugins, no_jit, trust_mode)?;

    let mut refresh_failure = None;
    if refresh {
        match maki_providers::refresh_catalog() {
            Ok(()) => eprintln!("models.dev catalog has been refreshed"),
            Err(e) => refresh_failure = Some(e),
        }
    }

    Handle::current().block_on(fetch_all_models(
        &config.provider.model_policy,
        |batch| {
            for model in batch.models {
                println!("{model}");
            }
            for warning in batch.warnings {
                eprintln!("warning: {warning}");
            }
        },
        None,
    ));

    if let Some(e) = refresh_failure {
        bail!("catalog refresh failed, keeping existing cache: {e}");
    }
    Ok(())
}

pub fn index(path: &str, no_plugins: bool, no_jit: bool, trust_mode: TrustMode) -> Result<()> {
    // Load-bearing binding: `index` is a Lua builtin, and dropping the host
    // stops the Lua thread the tool runs on.
    let (_host, _config) = cli_stack(no_plugins, no_jit, trust_mode)?;

    let abs_path = Path::new(path)
        .canonicalize()
        .unwrap_or_else(|_| Path::new(path).to_path_buf());
    let input = serde_json::json!({"path": abs_path.to_str().unwrap_or(path)});
    let reg = ToolRegistry::global_arc();
    let entry = reg
        .get("index")
        .ok_or_else(|| color_eyre::eyre::eyre!("index tool not registered"))?;
    let inv = entry
        .tool
        .parse(&input)
        .map_err(|e| color_eyre::eyre::eyre!("parse index input: {e}"))?;
    let ctx = maki_agent::tools::cli_tool_ctx();
    let result = Handle::current().block_on(inv.execute(&ctx));
    match result.output {
        Ok(output) => print!("{}", output.as_text()),
        Err(e) => bail!("index failed: {e}"),
    }
    Ok(())
}

pub fn mcp_auth(server: &str, storage: &StateDir, trust_mode: TrustMode) -> Result<()> {
    Handle::current().block_on(async {
        let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
        let trust = project::resolve(storage, &cwd, trust_mode, Runtime::Agent);
        report_warnings(trust.notices());
        let (config, _) = mcp_config::load_config(&cwd, trust.project_config);
        let raw = config
            .mcp
            .get(server)
            .ok_or_else(|| color_eyre::eyre::eyre!("unknown MCP server: {server}"))?;
        let origin = config.origins.get(server).cloned().unwrap_or_default();
        let (url, oauth, ca_file) =
            match mcp_config::parse_server(server.to_owned(), raw.clone(), &origin)?.transport {
                mcp_config::Transport::Http {
                    url,
                    oauth,
                    ca_file,
                    ..
                } => (url, oauth, ca_file),
                _ => color_eyre::eyre::bail!("server '{server}' is not an HTTP transport"),
            };
        mcp_oauth::authenticate(
            server,
            &url,
            None,
            storage,
            mcp_oauth::Interaction::Cli,
            oauth,
            ca_file.as_deref(),
        )
        .await?;
        eprintln!("Successfully authenticated with MCP server '{server}'");
        Ok(())
    })
}

pub fn mcp_logout(server: &str, storage: &StateDir) -> Result<()> {
    let deleted = maki_storage::auth::delete_mcp_auth(storage, server)?;
    if deleted {
        eprintln!("Removed OAuth credentials for MCP server '{server}'");
    } else {
        eprintln!("No stored credentials for MCP server '{server}'");
    }
    Ok(())
}

pub fn prompt(
    variant: &crate::cli::PromptVariant,
    plan: bool,
    tools: bool,
    names: bool,
    no_plugins: bool,
    no_jit: bool,
    trust_mode: TrustMode,
) -> Result<()> {
    use crate::cli::PromptVariant;
    use maki_agent::agent::{build_system_prompt, load_instruction_text, plan_mode_update};
    use maki_agent::prompt::PromptId;
    use maki_agent::template;
    use maki_agent::tools::{DescriptionContext, ToolAudience, ToolFilter, ToolRegistry};
    use maki_providers::Model;

    if plan && !matches!(variant, PromptVariant::System) {
        bail!("--plan can only be used with the 'system' prompt variant");
    }

    let (host, config) = cli_stack(no_plugins, no_jit, trust_mode)?;

    let vars = template::env_vars();
    let reg = ToolRegistry::global_arc();

    if tools {
        let ctx = DescriptionContext {
            filter: &ToolFilter::All,
            audience: ToolAudience::MAIN,
            workflow: false,
            mcp: false,
        };
        let defs = reg.definitions(&vars, &ctx, true);
        if names {
            for name in defs
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|d| d["name"].as_str())
            {
                println!("{name}");
            }
        } else {
            println!("{}", serde_json::to_string_pretty(&defs)?);
        }
        return Ok(());
    }

    let cwd = env::current_dir().unwrap_or_else(|_| ".".into());
    let instructions = load_instruction_text(&cwd.to_string_lossy(), &config.agent.instructions);
    let slots = host.agent_handle().collect_prompt_slots();

    let output = match variant {
        PromptVariant::System => {
            let model_spec = config
                .provider
                .default_model
                .as_deref()
                .unwrap_or("anthropic/claude-sonnet-4-20250514");
            let model = Model::from_spec(model_spec).context("invalid default model")?;
            let prompt = build_system_prompt(&vars, &instructions, &slots, &model);
            // Plan mode reaches the model as a context update after the
            // prompt, so this shows that message too.
            if plan {
                format!(
                    "{prompt}\n\n{}",
                    plan_mode_update(Path::new(PLAN_PREVIEW_PATH), &slots)
                )
            } else {
                prompt
            }
        }
        PromptVariant::Research => slots.render(PromptId::Research, &vars, &instructions),
        PromptVariant::General => slots.render(PromptId::General, &vars, &instructions),
    };

    print!("{output}");
    Ok(())
}

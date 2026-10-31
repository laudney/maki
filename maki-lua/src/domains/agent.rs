//! Tools, hooks, prompt hints, providers, the interpreter, `maki.fs`,
//! `maki.agent` and background sessions.

use std::collections::HashMap;
use std::sync::Arc;

use flume::Sender;
use maki_agent::permissions::PluginRuleStore;
use maki_agent::tools::{RegistryError, Tool, ToolRegistry};
use maki_lua_core::handle::CoreHandle;
use maki_providers::plugin::DeclAuthority;
use maki_storage::id::MakiId;
use mlua::{Lua, RegistryKey, Result as LuaResult, Table};

use super::ToolRecords;
use crate::api::fs::publish_walks;
use crate::api::provider::Owner;
use crate::api::slot::LayeredTools;
use crate::api::tool::{
    self, LuaTool, PendingRules, PendingTool, PendingTools, ToolPermission, resolve_rules,
};
use crate::api::{agent, interpreter, provider};
use crate::background::BackgroundTasks;
use crate::docs::DocEntry;
use crate::domain::{Domain, LoadScope};
use crate::error::PluginError;
use crate::hook::SlotHook;
use crate::plugin_permissions::Permission;
use crate::runtime::{ProviderRequests, Request};

mod chains;
pub(crate) mod moves;
mod prompt_hints;
mod requests;
mod tools;

pub(crate) use chains::{HookRun, PromptRun};
pub(crate) use prompt_hints::{
    HintContent, PromptHintCallbacks, PromptHintRegistration, PromptTarget,
};
pub(crate) use requests::{
    CallProviderHook, CallTool, CollectPromptSlots, ComputePermissionScopes, ComputeSummary,
    Describe, RunHook, RunPrompt,
};
use tools::run_describe;

const NAME: &str = "agent";
pub(super) const DOCS: &[DocEntry] = &[
    DocEntry::new(20, &tool::DOCS),
    DocEntry::new(22, &tool::PROMPT_HINT_DOCS),
    DocEntry::new(24, &tool::PROMPT_DOCS),
    DocEntry::new(70, &agent::DOCS),
    DocEntry::new(80, &agent::SESSION_DOCS),
    DocEntry::new(190, &interpreter::DOCS),
    DocEntry::new(270, &provider::DOCS),
    DocEntry::new(280, &provider::AUTH_DOCS),
];
const NO_GRANT_ERR: &str = "agent domain: the load carries no agent grant";

/// What one load of a plugin may do as an agent: the authority and provider
/// slugs the trust layer granted, and where its permission rules wait until
/// the load commits.
pub(crate) struct AgentGrant {
    /// Whether the code being loaded ships inside the binary, which is what
    /// lets it declare a provider under a built-in slug. Everything else is
    /// third party however the user installed it, so every path but
    /// `PluginHost::load_builtins` and the config files grants nothing.
    authority: DeclAuthority,
    /// The slugs this load may register: the manifest's `providers` list
    /// intersected with the user's approval. Everything but a package loads
    /// with none, so every third-party slug stays refused.
    granted_providers: Arc<[String]>,
    /// Scoped to this load so a failed load simply drops its rules; only a
    /// successful load commits them to the store.
    pending_rules: PendingRules,
}

impl AgentGrant {
    pub(crate) fn new(authority: DeclAuthority, granted_providers: Arc<[String]>) -> Self {
        Self {
            authority,
            granted_providers,
            pending_rules: PendingRules::default(),
        }
    }

    /// A load that may register no third-party slug.
    pub(crate) fn third_party() -> Self {
        Self::new(DeclAuthority::ThirdParty, Arc::from([]))
    }
}

/// The agent domain's handles a host keeps outside the VM thread.
#[derive(Default)]
pub(crate) struct AgentHandles {
    /// Published by the Lua thread and read without touching it: whether
    /// asking a chain can change the answer at all.
    pub layered: Arc<LayeredTools>,
    /// Shared with the Lua thread too: Esc on a plugin task closes its
    /// session from the event loop, with no Lua-thread hop.
    pub background: BackgroundTasks,
}

/// The callbacks of one tool that the agent calls.
pub(crate) struct AgentToolKeys {
    pub handler: RegistryKey,
    pub permission_scopes: Option<RegistryKey>,
    pub describe: Option<RegistryKey>,
    pub summary: Option<RegistryKey>,
}

pub(crate) struct AgentDomain {
    pub pending: PendingTools,
    pub registry: Arc<ToolRegistry>,
    pub plugin_rules: Arc<PluginRuleStore>,
    pub tx: Sender<Request>,
    pub prio_tx: Sender<Request>,
    pub layered: Arc<LayeredTools>,
    pub background: BackgroundTasks,
}

impl Domain for AgentDomain {
    fn start(&self, lua: &Lua) {
        moves::start(lua);
    }

    fn name(&self) -> &'static str {
        NAME
    }

    fn docs(&self) -> &'static [DocEntry] {
        DOCS
    }

    fn install(&mut self, lua: &Lua) -> LuaResult<()> {
        lua.set_app_data(PromptHintCallbacks::default());
        self.registry.set_hook(SlotHook {
            tx: self.tx.clone(),
            layered: Arc::clone(&self.layered),
        });
        self.registry.set_agent_hook(SlotHook {
            tx: self.tx.clone(),
            layered: Arc::clone(&self.layered),
        });
        lua.set_app_data(Arc::clone(&self.registry));
        lua.set_app_data(self.background.clone());
        lua.set_app_data(ProviderRequests(self.prio_tx.clone()));
        lua.set_app_data(ToolRecords::<AgentToolKeys>::default());
        {
            let lua = lua.clone();
            tool::set_local_describe(move |plugin, tool, dctx| {
                run_describe(&lua, plugin, tool, dctx)
            });
        }
        // The file index outlives any one plugin host, so the walks a host
        // rebuilt by `/reload` is waiting on are the ones the old host started.
        publish_walks(vec![CoreHandle::new(self.tx.clone(), self.prio_tx.clone())]);
        Ok(())
    }

    fn extend(&self, lua: &Lua, maki: &Table, load: &LoadScope<'_>) -> LuaResult<()> {
        let grant = load
            .grant
            .downcast_ref::<AgentGrant>()
            .ok_or_else(|| mlua::Error::runtime(NO_GRANT_ERR))?;
        let plugin = load.plugin;
        let permissions = load.reach.permissions();
        tool::add_agent_fns(
            &maki.get::<Table>("api")?,
            lua,
            Arc::clone(&self.pending),
            Arc::clone(&grant.pending_rules),
            load.reach.clone(),
            Arc::clone(plugin),
        )?;
        let owner = Owner {
            name: Arc::clone(plugin),
            authority: grant.authority,
            granted_providers: Arc::clone(&grant.granted_providers),
        };
        // The egress `maki.net` holds, so a provider registered through
        // `maki.provider` is reachable from `maki.net` without the manifest
        // naming an origin only maki resolves.
        maki.set(
            "provider",
            provider::create_provider_namespace(
                lua,
                permissions,
                owner,
                load.reach.egress().clone(),
            )?,
        )?;
        maki.set(
            "interpreter",
            interpreter::create_interpreter_table(
                lua,
                permissions,
                permissions.is_allowed(Permission::FsRead),
            )?,
        )?;
        maki.set(
            "agent",
            agent::create_agent_table(lua, Arc::clone(plugin), permissions.clone())?,
        )?;
        maki_lua_core::facade::extend(lua, maki, load, &super::facade::facade_ops())?;
        crate::api::task::extend_agent(lua, &maki.get::<Table>("task")?)
    }

    fn reload(&self, lua: &Lua, plugin: &str) {
        let stale = self.drain_pending();
        debug_assert!(
            stale.is_empty(),
            "leftover pending tools from previous load"
        );
        discard_pending(lua, stale);
        drop_plugin_keys(lua, plugin);
    }

    fn commit_load(&self, lua: &Lua, load: &LoadScope<'_>) -> Result<(), PluginError> {
        let name = load.plugin;
        let grant = load
            .grant
            .downcast_ref::<AgentGrant>()
            .ok_or_else(|| PluginError::Lua {
                plugin: name.to_string(),
                source: mlua::Error::runtime(NO_GRANT_ERR),
            })?;
        let pending = self.drain_pending();

        let registry_entries: Vec<Arc<dyn Tool>> = pending
            .iter()
            .map(|t| {
                let tool: Arc<dyn Tool> = Arc::new(LuaTool {
                    name: Arc::clone(&t.name),
                    description: t.description.clone(),
                    schema: t.schema,
                    audience: t.audience,
                    kind: t.kind.clone(),
                    tx: self.tx.clone(),
                    plugin: Arc::clone(name),
                    permission: t.permission.as_ref().map(ToolPermission::kind),
                    mutable_path_field: t.mutable_path_field.clone(),
                    timeout: t.timeout,
                    start_annotation: t.start_annotation.clone(),
                    examples: t.examples.clone(),
                    has_describe_fn: t.describe_key.is_some(),
                    has_summary_fn: t.summary_key.is_some(),
                    background_only: t.background_only.clone(),
                });
                tool
            })
            .collect();

        if let Err(e) = self.registry.replace_plugin(name, registry_entries) {
            discard_pending(lua, pending);
            return Err(match e {
                RegistryError::NameConflict { name: n, .. } => PluginError::NameConflict {
                    plugin: name.to_string(),
                    tool: n,
                },
            });
        }

        // After `replace_plugin`, so a rule may name a tool this very load
        // registered.
        let declared = std::mem::take(
            &mut *grant
                .pending_rules
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        let rules = resolve_rules(&self.registry, name, load.reach.permissions(), declared);

        let keys: HashMap<Arc<str>, AgentToolKeys> = pending
            .into_iter()
            .map(|t| {
                (
                    t.name,
                    AgentToolKeys {
                        handler: t.handler_key,
                        permission_scopes: t.permission.and_then(|p| p.scopes?.callback_key()),
                        describe: t.describe_key,
                        summary: t.summary_key,
                    },
                )
            })
            .collect();
        self.plugin_rules.replace(name, rules);
        if let Some(mut records) = lua.app_data_mut::<ToolRecords<AgentToolKeys>>() {
            records.0.insert(Arc::clone(name), keys);
        }
        Ok(())
    }

    fn rollback_load(&self, lua: &Lua, _plugin: &str) {
        discard_pending(lua, self.drain_pending());
    }

    fn clear_plugin(&self, lua: &Lua, plugin: &str) {
        self.registry.clear_plugin(plugin);
        maki_providers::plugin::discard(plugin);
        self.plugin_rules.remove(plugin);
        drop_plugin_keys(lua, plugin);
    }

    /// The one place a package's grant is released.
    fn unload(&self, _lua: &Lua, plugin: &str) {
        maki_providers::plugin::release_granted(plugin);
    }

    fn end_session(&self, lua: &Lua, session: MakiId) {
        BackgroundTasks::of(lua).end_session(session);
    }

    fn end_task(&self, lua: &Lua, task: u64) {
        BackgroundTasks::of(lua).end_task(task);
    }
}

impl AgentDomain {
    fn drain_pending(&self) -> Vec<PendingTool> {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect()
    }
}

fn discard_pending(lua: &Lua, tools: Vec<PendingTool>) {
    for key in tools.into_iter().flat_map(PendingTool::registry_keys) {
        if let Err(e) = lua.remove_registry_value(key) {
            tracing::warn!(error = %e, "failed to drop lua registry key on rollback");
        }
    }
}

fn drop_plugin_keys(lua: &Lua, name: &str) {
    BackgroundTasks::of(lua).end_plugin(name);
    let tools = lua
        .app_data_mut::<ToolRecords<AgentToolKeys>>()
        .and_then(|mut records| records.0.remove(name));
    for (_, tk) in tools.into_iter().flatten() {
        if let Err(e) = lua.remove_registry_value(tk.handler) {
            tracing::warn!(plugin = name, error = %e, "failed to drop lua handler key");
        }
        if let Some(sk) = tk.permission_scopes
            && let Err(e) = lua.remove_registry_value(sk)
        {
            tracing::warn!(plugin = name, error = %e, "failed to drop lua permission_scopes key");
        }
        if let Some(sk) = tk.describe
            && let Err(e) = lua.remove_registry_value(sk)
        {
            tracing::warn!(plugin = name, error = %e, "failed to drop lua describe key");
        }
        if let Some(sk) = tk.summary
            && let Err(e) = lua.remove_registry_value(sk)
        {
            tracing::warn!(plugin = name, error = %e, "failed to drop lua summary key");
        }
    }
    if let Some(mut hints) = lua.app_data_mut::<PromptHintCallbacks>()
        && let Some(regs) = hints.remove(name)
    {
        for reg in regs {
            if let HintContent::Callback(key) = reg.content
                && let Err(e) = lua.remove_registry_value(key)
            {
                tracing::warn!(plugin = name, error = %e, "failed to drop prompt hint key");
            }
        }
    }
}

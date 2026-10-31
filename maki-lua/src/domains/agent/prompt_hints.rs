//! The hints plugins add to the agent's prompts, and how a request resolves
//! them into slots.

use std::collections::BTreeMap;
use std::sync::Arc;

use maki_agent::prompt::{PromptId, ResolvedPrompts, Slot, SlotEntry};
use mlua::{Function, Lua, RegistryKey, Value as LuaValue};

use crate::runtime::run_detached;

pub(crate) enum HintContent {
    Static(String),
    Callback(RegistryKey),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptTarget {
    Slot(Slot),
    Section(Slot),
    Template,
}

pub(crate) struct PromptHintRegistration {
    pub(crate) prompts: Vec<PromptId>,
    pub(crate) explicit: bool,
    pub(crate) target: PromptTarget,
    pub(crate) content: HintContent,
}

pub(crate) type PromptHintCallbacks = BTreeMap<Arc<str>, Vec<PromptHintRegistration>>;

async fn run_hint_callback(lua: &Lua, plugin: &str, func: Function) -> Option<String> {
    let result: mlua::Result<LuaValue> = run_detached(lua, async {
        let thread = lua.create_thread(func)?;
        thread.into_async::<LuaValue>(())?.await
    })
    .await;
    match result {
        Ok(LuaValue::String(s)) => Some(s.to_string_lossy()),
        Ok(LuaValue::Nil) => None,
        Ok(_) => {
            tracing::warn!(plugin, "prompt hint callback returned non-string");
            None
        }
        Err(e) => {
            tracing::warn!(plugin, error = %e, "prompt hint callback failed");
            None
        }
    }
}

pub(crate) async fn collect_prompt_slots(lua: &Lua) -> ResolvedPrompts {
    struct Pending {
        plugin: Arc<str>,
        prompts: Vec<PromptId>,
        explicit: bool,
        target: PromptTarget,
        content: PendingContent,
    }
    enum PendingContent {
        Static(String),
        Callback(Function),
    }

    let pending: Vec<Pending> = {
        let Some(map) = lua.app_data_ref::<PromptHintCallbacks>() else {
            return ResolvedPrompts::default();
        };
        map.iter()
            .flat_map(|(plugin, regs)| {
                regs.iter().filter_map(move |r| {
                    let content = match &r.content {
                        HintContent::Static(s) => PendingContent::Static(s.clone()),
                        HintContent::Callback(key) => match lua.registry_value(key) {
                            Ok(func) => PendingContent::Callback(func),
                            Err(e) => {
                                tracing::warn!(plugin = %plugin, error = %e, "failed to read prompt hint callback");
                                return None;
                            }
                        },
                    };
                    Some(Pending {
                        plugin: Arc::clone(plugin),
                        prompts: r.prompts.clone(),
                        explicit: r.explicit,
                        target: r.target,
                        content,
                    })
                })
            })
            .collect()
    };

    let mut slots = ResolvedPrompts::default();
    for item in &pending {
        if item.target == PromptTarget::Template
            && let PendingContent::Static(content) = &item.content
        {
            for &pid in &item.prompts {
                slots.set_template(pid, content.clone());
            }
        }
    }
    for item in pending {
        let slot = match item.target {
            PromptTarget::Slot(slot) | PromptTarget::Section(slot) => slot,
            PromptTarget::Template => continue,
        };
        let content = match item.content {
            PendingContent::Static(s) => Some(s),
            PendingContent::Callback(func) => run_hint_callback(lua, &item.plugin, func).await,
        };
        let Some(content) = content else { continue };
        for pid in item.prompts {
            if !slots.has_slot(pid, slot) {
                if item.explicit {
                    tracing::warn!(
                        plugin = %item.plugin,
                        slot = ?slot,
                        prompt = ?pid,
                        "prompt hint targets a prompt that has no such slot; ignoring"
                    );
                }
                continue;
            }
            if matches!(item.target, PromptTarget::Section(_)) {
                slots.set_section(pid, slot, content.clone());
            } else {
                slots.insert(
                    pid,
                    slot,
                    SlotEntry {
                        plugin: Arc::clone(&item.plugin),
                        content: content.clone(),
                    },
                );
            }
        }
    }
    slots
}

//! What the UI asks of its plugins on the VM thread: commands, keybinds,
//! and the plan and question forms.

use std::any::Any;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use flume::{Receiver, Sender};
use futures::future::LocalBoxFuture;
use maki_types::cancel::CancelToken;
use mlua::{Function, Value as LuaValue};
use serde_json::Value;

use super::keybind::spawn_keybind_callback;
use super::plan_form::{PLAN_ROW_HANDLER_DEADLINE, open_plan_form};
use super::plan_rows::{PlanRowHandlers, clear_menu_generation, row_handler_opts};
use crate::api::keymap::KeybindTicket;
use crate::api::util::command::{CommandHandlerMap, PlanActionOutcome, PlanFormRow, PlanMenu};
use crate::domain::{DomainRequest, Vm};
use crate::plugin_permissions::PluginPermissions;
use crate::question_form::{QuestionFormAnswer, question_forms};
use crate::runtime::{TaskCell, run_awaited, run_command_scoped, strip_traceback};

const PLAN_HANDLERS_MISSING_ERR: &str = "plan row handlers not initialized";
const PLAN_HANDLER_GONE_ERR: &str = "plan row handler was reaped";

/// Fires when the user picks a plugin row on the plan form. The row is
/// named by its position in the menu the chain built for that session,
/// plus the generation that menu was published with, so a pick only ever
/// reaches the handlers of the menu the user was looking at.
pub(crate) struct RunPlanAction {
    pub session: String,
    pub generation: u64,
    pub row: usize,
    /// Absolute plan path.
    pub path: String,
    /// Value of the form's "parallel" checkbox when the user picked the row.
    pub parallel: bool,
    /// Whether the row's built-in action still runs, and if not, whether
    /// that was the handler's decision or the host failing to take the
    /// pick at all.
    pub reply: Sender<PlanActionOutcome>,
}

impl DomainRequest for RunPlanAction {
    fn serve<'a>(self: Box<Self>, vm: &'a Vm<'a>) -> LocalBoxFuture<'a, ()> {
        let Self {
            session,
            generation,
            row,
            path,
            parallel,
            reply,
        } = *self;
        Box::pin(async move {
            let found = vm
                .rt
                .lua
                .app_data_ref::<PlanRowHandlers>()
                .ok_or(PLAN_HANDLERS_MISSING_ERR)
                .and_then(|handlers| {
                    let handler = handlers.handler(&session, generation, row)?;
                    let func = vm
                        .rt
                        .lua
                        .registry_value::<Function>(&handler.key)
                        .map_err(|_| PLAN_HANDLER_GONE_ERR)?;
                    Ok((Arc::clone(&handler.plugin), func))
                });
            match found {
                Ok((plugin, func)) => {
                    let lua = vm.rt.lua.clone();
                    let gate = Rc::clone(vm.gate);
                    vm.ex
                        .spawn_local(async move {
                            let run = async {
                                let opts = row_handler_opts(&lua, &session, &path, parallel)?;
                                let thread = lua.create_thread(func)?;
                                thread.into_async::<LuaValue>(opts)?.await
                            };
                            // Handler-then-action: the row's built-in outcome
                            // follows unless the handler answered `false` or
                            // failed.
                            let deadline = Instant::now() + PLAN_ROW_HANDLER_DEADLINE;
                            let outcome =
                                match run_awaited(&lua, &gate, TaskCell::new(CancelToken::none(), Some(deadline)), run)
                                    .await
                                {
                                    Ok(Ok(LuaValue::Boolean(false))) => PlanActionOutcome::Vetoed,
                                    Ok(Ok(_)) => PlanActionOutcome::Proceed,
                                    Ok(Err(e)) => {
                                        tracing::warn!(plugin = %plugin, row, error = %strip_traceback(&e), "plan form row handler failed");
                                        PlanActionOutcome::Failed
                                    }
                                    Err(reason) => {
                                        tracing::warn!(plugin = %plugin, row, reason, "plan form row handler abandoned");
                                        PlanActionOutcome::Failed
                                    }
                                };
                            let _ = reply.send(outcome);
                        });
                }
                Err(reason) => {
                    tracing::warn!(
                        session,
                        generation,
                        row,
                        reason,
                        "plan form row pick reached no handler"
                    );
                    let _ = reply.send(PlanActionOutcome::Failed);
                }
            }
        })
    }
}

/// Opens the bundled question form outside any tool call, see
/// [`UiHandle::open_question_form`](crate::UiHandle::open_question_form).
pub(crate) struct OpenQuestionForm {
    pub questions: Value,
    pub permissions: PluginPermissions,
    /// The grant the composition's domains read for a bundled load.
    pub grant: Box<dyn Any + Send>,
    pub reply: Sender<QuestionFormAnswer>,
    pub cancel: Receiver<()>,
}

impl DomainRequest for OpenQuestionForm {
    fn serve<'a>(self: Box<Self>, vm: &'a Vm<'a>) -> LocalBoxFuture<'a, ()> {
        let Self {
            questions,
            permissions,
            grant,
            reply,
            cancel,
        } = *self;
        Box::pin(async move {
            match question_forms(vm.rt, &permissions, &*grant) {
                Ok(forms) => {
                    let lua = vm.rt.lua.clone();
                    // Spawned: the form parks until the user answers.
                    vm.ex.spawn_local(async move {
                        if let Some(answer) = forms.run(&lua, questions, cancel).await {
                            let _ = reply.send(answer);
                        }
                    });
                }
                Err(error) => tracing::warn!(
                    error = %error,
                    "cannot build the question form's environment"
                ),
            }
        })
    }
}

/// Fires the two `ui.plan_form*` chains for a session whose plan just
/// landed: whether the built-in form opens, and with which rows.
pub(crate) struct OpenPlanForm {
    pub path: String,
    pub session: String,
    /// The rows the host proposes, i.e. the bottom of the actions chain.
    pub rows: Vec<PlanFormRow>,
    /// The menu to draw, or `None` when a layer took the form over.
    pub reply: Sender<Option<PlanMenu>>,
}

impl DomainRequest for OpenPlanForm {
    fn serve<'a>(self: Box<Self>, vm: &'a Vm<'a>) -> LocalBoxFuture<'a, ()> {
        let Self {
            path,
            session,
            rows,
            reply,
        } = *self;
        Box::pin(async move {
            // Spawned, not awaited: a layer may park, and every other session
            // is waiting on this request loop.
            let lua = vm.rt.lua.clone();
            let plugins = Rc::clone(&vm.rt.plugins);
            let gate = Rc::clone(vm.gate);
            vm.ex.spawn_local(async move {
                let menu = open_plan_form(&lua, &plugins, &gate, path, &session, rows).await;
                let generation = menu.as_ref().map_or(0, |m| m.generation);
                if reply.send(menu).is_err() {
                    // The form gave up on this answer, or the session it
                    // was asked about is gone. The handlers stashed for a
                    // menu nobody will ever draw go with it.
                    clear_menu_generation(&lua, &session, generation);
                }
            });
        })
    }
}

pub(crate) struct RunCommand {
    pub plugin: Arc<str>,
    pub command: Arc<str>,
    pub args: String,
    /// How many `maki.api.run_command` hops led here. It seeds the
    /// handler's [`Lineage::command_depth`], so an alias cycle runs out.
    ///
    /// [`Lineage::command_depth`]: crate::runtime::Lineage::command_depth
    pub depth: u8,
}

impl DomainRequest for RunCommand {
    fn serve<'a>(self: Box<Self>, vm: &'a Vm<'a>) -> LocalBoxFuture<'a, ()> {
        let Self {
            plugin,
            command,
            args,
            depth,
        } = *self;
        Box::pin(async move {
            let handler_fn = vm.rt.lua.app_data_ref::<CommandHandlerMap>().and_then(|m| {
                let entry = m.get(&plugin)?.get(&command)?;
                vm.rt.lua.registry_value::<Function>(&entry.handler).ok()
            });
            if let Some(func) = handler_fn {
                let lua = vm.rt.lua.clone();
                vm.ex
                    .spawn_local(async move {
                        let run = async {
                            let opts = lua.create_table()?;
                            opts.set("fargs", lua.create_sequence_from(args.split_whitespace())?)?;
                            opts.set("args", args)?;
                            let thread = lua.create_thread(func)?;
                            thread.into_async::<()>(opts)?.await
                        };
                        if let Err(e) = run_command_scoped(&lua, depth, run).await {
                            tracing::warn!(plugin = %plugin, command = %command, error = %e, "command handler failed");
                        }
                    });
            }
        })
    }
}

pub(crate) struct RunKeybindCallback {
    pub ticket: KeybindTicket,
}

impl DomainRequest for RunKeybindCallback {
    fn serve<'a>(self: Box<Self>, vm: &'a Vm<'a>) -> LocalBoxFuture<'a, ()> {
        let Self { ticket } = *self;
        Box::pin(async move {
            spawn_keybind_callback(&vm.rt.lua, vm.ex, vm.gate, ticket);
        })
    }
}

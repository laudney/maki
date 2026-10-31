//! What a UI holds of a host running the UI domain: the handle its requests
//! go through and the readers of what plugins publish.

use std::sync::Arc;

use maki_lua_core::api::slot::{LayeredTools, PLAN_FORM_ACTIONS_SLOT, PLAN_FORM_SLOT};
use maki_lua_core::handle::CoreHandle;
use maki_lua_core::runtime::Request;
use serde_json::Value;

use crate::api::keymap::{KeybindTicket, KeymapReader};
use crate::api::ui::marks::InputRulesReader;
use crate::api::ui::renderer::RendererRoutes;
use crate::api::util::command::{
    HintReader, LuaCommandReader, PlanActionOutcome, PlanFormRow, PlanMenu, UiAction, UiAttachment,
};
use crate::domains::ui::{
    ClickTool, OpenPlanForm, OpenQuestionForm, RenderCall, RenderCtx, RenderReply, RenderTool,
    RunCommand, RunKeybindCallback, RunPlanAction,
};
use crate::question_form::{QUESTION_PLUGIN, QuestionFormAnswer};

#[derive(Clone)]
pub struct UiHandle {
    core: CoreHandle,
    /// Published by the Lua thread and read without touching it: whether
    /// asking a chain can change the answer at all.
    layered: Arc<LayeredTools>,
    renderer_routes: Arc<RendererRoutes>,
}

impl UiHandle {
    pub fn new(core: CoreHandle, layered: Arc<LayeredTools>) -> Self {
        Self {
            core,
            layered,
            renderer_routes: Arc::default(),
        }
    }

    /// Tools with renderers, read without the Lua
    /// thread, so a call is routed in the frame it arrives.
    pub fn renderer_routes(&self) -> &RendererRoutes {
        &self.renderer_routes
    }

    /// Draws {call} with the renderer its tool registered, for {ctx}. The
    /// receiver answers [`RenderReply::Default`] when no renderer is
    /// registered or the renderer raised, and disconnects when the host
    /// is gone, which leaves the caller the default renderer as well.
    pub fn render(&self, call: RenderCall, ctx: RenderCtx) -> flume::Receiver<RenderReply> {
        self.redraw(call, ctx, Vec::new())
    }

    /// [`Self::render`], then {clicks} in order as [`Self::click`] replays
    /// earlier ones, so a call drawn again from new data, for a new theme or
    /// on a new route keeps the state its clicks made.
    pub fn redraw(
        &self,
        call: RenderCall,
        ctx: RenderCtx,
        clicks: Vec<usize>,
    ) -> flume::Receiver<RenderReply> {
        let (reply, rx) = flume::bounded(1);
        self.core.send(Request::domain(RenderTool {
            call,
            ctx,
            clicks,
            reply,
        }));
        rx
    }

    /// Hands a click on line {row} of {call}'s body (1-based, 0 outside it)
    /// to its renderer's `click`, which changes the state on the call's data,
    /// and answers with the call drawn again from it, as [`Self::render`]
    /// does. A call drawn recently keeps its data. Any other is drawn, then
    /// given its {earlier} clicks in order before this one, each followed by
    /// a draw, so the answer is the same either way. The draws and clicks of
    /// one call take turns.
    pub fn click(
        &self,
        call: RenderCall,
        ctx: RenderCtx,
        row: usize,
        earlier: Vec<usize>,
    ) -> flume::Receiver<RenderReply> {
        let (reply, rx) = flume::bounded(1);
        self.core.send(Request::domain(ClickTool {
            call,
            ctx,
            row,
            earlier,
            reply,
        }));
        rx
    }

    /// Whether a plugin is layering either of the plan form slots. False on a
    /// stock install, where the form opens in the frame the plan lands in: a
    /// request loop busy with a `/reload` or a long tool call would otherwise
    /// keep the user waiting for an answer that cannot differ.
    pub fn plan_form_layered(&self) -> bool {
        let layered = &self.layered;
        layered.layers_surface(PLAN_FORM_SLOT) || layered.layers_surface(PLAN_FORM_ACTIONS_SLOT)
    }

    /// Runs the handler behind a plugin row of {session}'s plan form, named
    /// by its position in the menu [`Self::open_plan_form`] built and the
    /// {generation} that menu was published with.
    ///
    /// The receiver answers whether the row's built-in action still runs, and
    /// disconnects when the pick never reached the host, so the caller can
    /// tell the user instead of going quiet.
    pub fn run_plan_action(
        &self,
        session: String,
        generation: u64,
        row: usize,
        path: String,
        parallel: bool,
    ) -> flume::Receiver<PlanActionOutcome> {
        let (reply, rx) = flume::bounded(1);
        self.core.send_prio(Request::domain(RunPlanAction {
            session,
            generation,
            row,
            path,
            parallel,
            reply,
        }));
        rx
    }

    /// Asks the `ui.plan_form*` chains what to draw for a draft that just
    /// landed, starting from the {rows} the host proposes. The receiver
    /// answers `None` when a layer took the form over, and disconnects when
    /// the host is gone, which leaves the caller the built-in form.
    pub fn open_plan_form(
        &self,
        path: String,
        session: String,
        rows: Vec<PlanFormRow>,
    ) -> flume::Receiver<Option<PlanMenu>> {
        let (reply, rx) = flume::bounded(1);
        self.core.send_prio(Request::domain(OpenPlanForm {
            path,
            session,
            rows,
            reply,
        }));
        rx
    }

    /// Opens the bundled `question` form for {questions}, the `questions` of
    /// a question tool input, outside any tool call, as a client does for a
    /// remote agent's ask. A send on the returned sender, or dropping it,
    /// closes the form. The receiver then disconnects without an answer, as
    /// it does when the form failed or no host is running.
    pub fn open_question_form(
        &self,
        questions: Value,
    ) -> (flume::Receiver<QuestionFormAnswer>, flume::Sender<()>) {
        let (reply, answer) = flume::bounded(1);
        let (cancel, cancelled) = flume::bounded(1);
        match crate::ui_host::bundled_grant(QUESTION_PLUGIN) {
            Ok((permissions, grant)) => {
                self.core.send_prio(Request::domain(OpenQuestionForm {
                    questions,
                    permissions,
                    grant,
                    reply,
                    cancel: cancelled,
                }));
            }
            Err(error) => tracing::warn!(%error, "cannot open the question form"),
        }
        (answer, cancel)
    }

    pub fn run_command(&self, plugin: Arc<str>, command: Arc<str>, args: String, depth: u8) {
        self.core.send_prio(Request::domain(RunCommand {
            plugin,
            command,
            args,
            depth,
        }));
    }

    /// Whether the keystroke reached the Lua thread's queue. `false` says the
    /// host is gone, which the caller answers by running the built-in binding
    /// for the key in the same keystroke.
    pub fn run_keybind_callback(&self, ticket: KeybindTicket) -> bool {
        self.core
            .send_prio(Request::domain(RunKeybindCallback { ticket }))
    }
}

impl crate::UiHost {
    pub fn ui_handle(&self) -> UiHandle {
        UiHandle {
            core: self.core_handle(),
            layered: Arc::clone(&self.layered),
            renderer_routes: Arc::clone(&self.handles.renderer_routes),
        }
    }

    pub fn command_reader(&self) -> LuaCommandReader {
        self.handles.command_reader.clone()
    }

    pub fn keymap_reader(&self) -> KeymapReader {
        self.handles.keymap_reader.clone()
    }

    pub fn hint_reader(&self) -> HintReader {
        self.handles.hint_reader.clone()
    }

    pub fn input_rules_reader(&self) -> InputRulesReader {
        self.handles.input_rules_reader.clone()
    }

    pub fn ui_action_rx(&self) -> flume::Receiver<UiAction> {
        self.handles.ui_action_rx.clone()
    }

    /// Wakes when a plugin changes a window or its content.
    pub fn ui_wake_rx(&self) -> flume::Receiver<()> {
        self.handles.ui_wake_rx.clone()
    }

    /// Attach while draining UI requests, then detach before session teardown
    /// so a handler cannot wait on a receiver the event loop no longer reads.
    pub fn ui_attachment(&self) -> UiAttachment {
        self.handles.ui_attachment.clone()
    }
}

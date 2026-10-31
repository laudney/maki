//! The bundled `question` form, opened outside any tool call: a client draws
//! it for a remote agent's ask, the same form a local tool call draws.

use std::any::Any;
use std::sync::Arc;

use futures_lite::future;
use mlua::{Function, Lua, Table, Value as LuaValue};
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::warn;

use crate::api::ui::WinStore;
use crate::api::util::convert::json_to_lua;
use crate::plugin_permissions::{PluginPermissions, PluginReach};
use crate::runtime::{LuaRuntime, run_command_scoped};

/// The owner the form's windows are stamped with, so a form that ends
/// without an answer can close them without waiting for Lua to collect them.
pub(crate) const QUESTION_PLUGIN: &str = "question";
const CHUNK_NAME: &str = "=question_form";
const CHUNK: &str = r#"
local questions = ...
local QuestionHelpers = require("question_helpers")
return require("question_form").open(QuestionHelpers.normalize(questions))
"#;
/// The forms live in the UI domain, so a host without it has none to open.
const NO_UI_DOMAIN_ERR: &str = "the question form needs the UI domain";
const SUBMIT: &str = "submit";
const DISMISS: &str = "dismiss";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionFormAnswer {
    Dismissed,
    /// One entry per question, empty for a skipped one.
    Submitted(Vec<Vec<String>>),
}

/// The forms the UI domain keeps once the first ask built them.
#[derive(Default)]
pub(crate) struct QuestionFormCache(pub Option<Arc<QuestionForms>>);

/// One environment for every form, since a `require` loader and its
/// environment hold each other and would outlive a form built per ask; and
/// one form at a time, so a form that ends without an answer closes only its
/// own window, never the next one's.
pub(crate) struct QuestionForms {
    env: Table,
    turn: Mutex<()>,
}

impl QuestionForms {
    pub(crate) fn new(env: Table) -> Self {
        Self {
            env,
            turn: Mutex::new(()),
        }
    }

    /// `None` when {cancel} fired or its sender went first, or the form
    /// failed; either way its window is closed.
    pub(crate) async fn run(
        &self,
        lua: &Lua,
        questions: Value,
        cancel: flume::Receiver<()>,
    ) -> Option<QuestionFormAnswer> {
        // A form cancelled while it waits for its turn never opens.
        let turn = future::or(async { Some(self.turn.lock().await) }, async {
            let _ = cancel.recv_async().await;
            None
        })
        .await;
        let _turn = turn?;
        run(lua, self.env.clone(), questions, cancel).await
    }
}

/// Built on the first ask in the bundled `question` plugin's
/// environment, without loading the plugin: what its form needs, and no
/// tool or rule it registers. {grant} is the composition's grant for a
/// bundled load.
pub(crate) fn question_forms(
    rt: &LuaRuntime,
    permissions: &PluginPermissions,
    grant: &dyn Any,
) -> Result<Arc<QuestionForms>, mlua::Error> {
    let cached = rt
        .lua
        .app_data_ref::<QuestionFormCache>()
        .ok_or_else(|| mlua::Error::runtime(NO_UI_DOMAIN_ERR))?
        .0
        .clone();
    if let Some(forms) = cached {
        return Ok(forms);
    }
    let env = rt.plugin_env(
        Arc::from(QUESTION_PLUGIN),
        &PluginReach::new(permissions.clone()),
        grant,
    )?;
    let forms = Arc::new(QuestionForms::new(env));
    if let Some(mut cache) = rt.lua.app_data_mut::<QuestionFormCache>() {
        cache.0 = Some(Arc::clone(&forms));
    }
    Ok(forms)
}

async fn run(
    lua: &Lua,
    env: Table,
    questions: Value,
    cancel: flume::Receiver<()>,
) -> Option<QuestionFormAnswer> {
    let count = questions.as_array().map_or(0, Vec::len);
    let form = async {
        let open: Function = lua
            .load(CHUNK)
            .set_name(CHUNK_NAME)
            .set_environment(env)
            .into_function()?;
        let input = json_to_lua(lua, &questions)?;
        let result: Table = lua.create_thread(open)?.into_async(input)?.await?;
        answer(&result, count)
    };
    let cancelled = async {
        let _ = cancel.recv_async().await;
        None
    };
    let outcome =
        run_command_scoped(lua, 0, future::or(async { Some(form.await) }, cancelled)).await;
    match outcome {
        Some(Ok(answer)) => return Some(answer),
        Some(Err(error)) => warn!(%error, "the question form failed"),
        None => {}
    }
    if let Some(mut store) = lua.app_data_mut::<WinStore>() {
        store.close_plugin(QUESTION_PLUGIN);
    }
    None
}

/// The form leaves a hole for a skipped question, so the answers are read
/// by position, one per question.
fn answer(result: &Table, count: usize) -> mlua::Result<QuestionFormAnswer> {
    match result.get::<String>("type")?.as_str() {
        DISMISS => Ok(QuestionFormAnswer::Dismissed),
        SUBMIT => {
            let answers: Table = result.get("answers")?;
            (1..=count)
                .map(|index| match answers.raw_get::<LuaValue>(index)? {
                    LuaValue::Table(picks) => picks.sequence_values::<String>().collect(),
                    _ => Ok(Vec::new()),
                })
                .collect::<mlua::Result<_>>()
                .map(QuestionFormAnswer::Submitted)
        }
        other => Err(mlua::Error::runtime(format!(
            "the question form returned an unknown result type {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use test_case::test_case;

    use super::*;
    use crate::api::util::command::{UiAction, WinCommand, WinEvent};
    use crate::key::Key;
    use crate::ui_host::UiHost;

    /// A bound on a wait that a working host answers at once, never a pause.
    const WAIT: Duration = Duration::from_secs(10);
    const CUSTOM: &str = "hi";

    struct Form {
        answer: flume::Receiver<QuestionFormAnswer>,
        cancel: flume::Sender<()>,
        keys: flume::Sender<WinEvent>,
        commands: flume::Receiver<WinCommand>,
    }

    type Asked = (flume::Receiver<QuestionFormAnswer>, flume::Sender<()>);

    impl Form {
        fn open(host: &UiHost, questions: Value) -> Self {
            Self::shown(host, host.ui_handle().open_question_form(questions))
        }

        /// Waits for the window of an ask, which opens once the form before
        /// it is gone.
        fn shown(host: &UiHost, (answer, cancel): Asked) -> Self {
            let (keys, commands) = loop {
                match host.ui_action_rx().recv_timeout(WAIT).unwrap() {
                    UiAction::OpenWin {
                        event_tx, cmd_rx, ..
                    } => break (event_tx, cmd_rx),
                    _ => continue,
                }
            };
            Self {
                answer,
                cancel,
                keys,
                commands,
            }
        }

        fn press(&self, keys: &[&str]) {
            for key in keys {
                let key = Key::parse(key).unwrap();
                self.keys.send(WinEvent::Key { key }).unwrap();
            }
        }

        fn answer(&self) -> Result<QuestionFormAnswer, flume::RecvTimeoutError> {
            self.answer.recv_timeout(WAIT)
        }

        fn closed(&self) -> bool {
            loop {
                match self.commands.recv_timeout(WAIT) {
                    Ok(WinCommand::Close) => return true,
                    Ok(_) => continue,
                    Err(_) => return false,
                }
            }
        }
    }

    fn host() -> UiHost {
        UiHost::bare()
    }

    fn picks(answers: &[&[&str]]) -> QuestionFormAnswer {
        QuestionFormAnswer::Submitted(
            answers
                .iter()
                .map(|picks| picks.iter().map(|pick| pick.to_string()).collect())
                .collect(),
        )
    }

    fn three_questions() -> Value {
        json!([
            { "question": "First?", "header": "One", "options": [{ "label": "A" }] },
            { "question": "Second?", "header": "Two", "options": [{ "label": "B" }] },
            { "question": "Third?", "header": "Three", "options": [{ "label": "C" }] },
        ])
    }

    #[test]
    fn a_picked_option_is_submitted() {
        let host = host();
        let form = Form::open(
            &host,
            json!([{ "question": "Pick?", "options": [{ "label": "A" }, { "label": "B" }] }]),
        );
        form.press(&["<Down>", "<CR>"]);
        assert_eq!(form.answer().unwrap(), picks(&[&["B"]]));
    }

    #[test]
    fn a_typed_answer_is_submitted_as_typed() {
        let host = host();
        let form = Form::open(
            &host,
            json!([{ "question": "Pick?", "options": [{ "label": "A" }] }]),
        );
        form.press(&["<Down>", "<CR>", "h", "i", "<CR>"]);
        assert_eq!(form.answer().unwrap(), picks(&[&[CUSTOM]]));
    }

    /// The form leaves a hole for a skipped question; every one still gets
    /// its own entry.
    #[test_case(&["<Tab>", "<CR>", "<Tab>", "<CR>"], &[&[], &["B"], &[]] ; "first_and_last_skipped")]
    #[test_case(&["<Tab>", "<Tab>", "<Tab>", "<CR>"], &[&[], &[], &[]] ; "every_one_skipped")]
    fn skipped_questions_keep_their_place(keys: &[&str], expected: &[&[&str]]) {
        let host = host();
        let form = Form::open(&host, three_questions());
        form.press(keys);
        assert_eq!(form.answer().unwrap(), picks(expected));
    }

    #[test]
    fn escape_dismisses() {
        let host = host();
        let form = Form::open(&host, three_questions());
        form.press(&["<Esc>"]);
        assert_eq!(form.answer().unwrap(), QuestionFormAnswer::Dismissed);
    }

    #[test]
    fn a_cancel_closes_the_form_without_an_answer() {
        let host = host();
        let form = Form::open(&host, three_questions());
        form.cancel.send(()).unwrap();
        assert!(form.closed());
        assert!(matches!(
            form.answer(),
            Err(flume::RecvTimeoutError::Disconnected)
        ));
    }

    /// The agent sends its next ask once the last one was answered, which can
    /// be before the cancelled form's cleanup ran.
    #[test]
    fn a_cancelled_form_leaves_the_next_one_open() {
        let host = host();
        let first = Form::open(&host, three_questions());
        let next = host
            .ui_handle()
            .open_question_form(json!([{ "question": "Pick?", "options": [{ "label": "A" }] }]));
        first.cancel.send(()).unwrap();
        assert!(first.closed());
        // The reply goes after the cleanup, so every Close it sent is in.
        assert!(matches!(
            first.answer(),
            Err(flume::RecvTimeoutError::Disconnected)
        ));
        let second = Form::shown(&host, next);
        assert!(
            !second
                .commands
                .try_iter()
                .any(|command| matches!(command, WinCommand::Close)),
            "the cancelled form closed the next one"
        );
        second.press(&["<CR>"]);
        assert_eq!(second.answer().unwrap(), picks(&[&["A"]]));
    }

    #[test]
    fn a_form_cancelled_in_its_wait_leaves_the_open_one_alone() {
        let host = host();
        let open = Form::open(
            &host,
            json!([{ "question": "Pick?", "options": [{ "label": "A" }] }]),
        );
        let (waiting, cancel) = host.ui_handle().open_question_form(three_questions());
        cancel.send(()).unwrap();
        assert!(matches!(
            waiting.recv_timeout(WAIT),
            Err(flume::RecvTimeoutError::Disconnected)
        ));
        open.press(&["<CR>"]);
        assert_eq!(open.answer().unwrap(), picks(&[&["A"]]));
    }

    /// An option without a label breaks the form's first draw, after its
    /// window opened.
    #[test]
    fn a_failing_form_closes_its_window() {
        let host = host();
        let form = Form::open(&host, json!([{ "question": "Pick?", "options": [{}] }]));
        assert!(form.closed());
        assert!(matches!(
            form.answer(),
            Err(flume::RecvTimeoutError::Disconnected)
        ));
    }
}

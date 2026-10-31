//! A remote agent's plan file. The agent owns it, so the tab learns of a
//! write from that call's completion, and reads and saves the plan over the
//! link. A path is only a name here, never a file this machine opens.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_lite::future;
use maki_wire::client::ClientError;
use maki_wire::plan::{PlanText, PlanWritten};
use serde_json::Value;
use tempfile::NamedTempFile;

use super::RemoteSession;
use super::capabilities::SESSION_OPENING;

/// A read or save the user waits on, as on a local file, so it is bounded.
const PLAN_WAIT: Duration = Duration::from_secs(10);
const PLAN_TIMEOUT: &str = "The agent did not answer about the plan in time";
pub(crate) const PLAN_SAVED: &str = "Plan saved on the agent";
pub(crate) const PLAN_UNCHANGED: &str = "Plan unchanged";
const PLAN_KEPT: &str = "your edit is kept at";
const COPY_PREFIX: &str = "maki-plan-";
const COPY_SUFFIX: &str = ".md";
const EDITOR_OK: i32 = 0;
const EDITOR_FAILED: &str = "The editor exited with code";

/// Plan writes by call id, kept by the loop before it emits the call's
/// `ToolDone`, and taken by the App when it handles that event.
pub(crate) type PlanWrites = Arc<Mutex<HashMap<String, PlanWritten>>>;

#[derive(Clone)]
pub(crate) struct RemotePlan {
    pub(crate) writes: PlanWrites,
    pub(crate) link: RemoteSession,
}

impl RemotePlan {
    pub(crate) fn take(&self, call: &str) -> Option<PlanWritten> {
        self.writes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(call)
    }

    pub(crate) fn read(&self) -> Result<PlanText, String> {
        let session = self.link.session().ok_or(SESSION_OPENING)?;
        bounded(self.link.client.plan_read(session))
    }

    /// Edits the plan `read` gave as a copy here, with `editor` run on the
    /// copy's path, and saves it at the revision it was read at. The copy is
    /// kept and named whenever the edit does not reach the agent: a failed
    /// editor, which may have written half an edit, is never saved.
    pub(crate) fn edit(
        &self,
        read: PlanText,
        editor: impl FnOnce(&Path) -> Result<i32, String>,
    ) -> String {
        let copy = match copy_of(read.text.as_deref().unwrap_or_default()) {
            Ok(copy) => copy,
            Err(error) => return error,
        };
        let edited = match editor(copy.path()) {
            Ok(EDITOR_OK) => fs::read_to_string(copy.path()).map_err(|e| e.to_string()),
            Ok(code) => Err(format!("{EDITOR_FAILED} {code}")),
            Err(error) => Err(error),
        };
        let edited = match edited {
            Ok(edited) => edited,
            Err(error) => return kept(error, copy),
        };
        if edited == read.text.unwrap_or_default() {
            return PLAN_UNCHANGED.to_owned();
        }
        let saved = self
            .link
            .session()
            .ok_or_else(|| SESSION_OPENING.to_owned())
            .and_then(|session| {
                bounded(self.link.client.plan_write(session, edited, read.revision))
            });
        match saved {
            Ok(()) => PLAN_SAVED.to_owned(),
            Err(error) => kept(error, copy),
        }
    }
}

fn bounded<T>(call: impl Future<Output = Result<T, ClientError>>) -> Result<T, String> {
    tokio::runtime::Handle::current().block_on(future::or(
        async { call.await.map_err(reason) },
        async {
            tokio::time::sleep(PLAN_WAIT).await;
            Err(PLAN_TIMEOUT.to_owned())
        },
    ))
}

/// The agent names why it refused, such as a busy session or a changed
/// plan, in the error's data.
fn reason(error: ClientError) -> String {
    match &error {
        ClientError::Rpc(rpc) => match rpc.data.as_ref().and_then(Value::as_str) {
            Some(data) => data.to_owned(),
            None => error.to_string(),
        },
        _ => error.to_string(),
    }
}

fn copy_of(text: &str) -> Result<NamedTempFile, String> {
    let copy = tempfile::Builder::new()
        .prefix(COPY_PREFIX)
        .suffix(COPY_SUFFIX)
        .tempfile()
        .map_err(|e| format!("cannot copy the plan: {e}"))?;
    fs::write(copy.path(), text).map_err(|e| format!("cannot copy the plan: {e}"))?;
    Ok(copy)
}

/// The copy stays where the editor wrote it.
fn kept(error: String, mut copy: NamedTempFile) -> String {
    copy.disable_cleanup(true);
    format!("{error}: {PLAN_KEPT} {}", copy.path().display())
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::thread;

    use maki_wire::client::{AcpClient, Link};
    use maki_wire::methods;
    use serde_json::json;

    use super::*;

    pub(crate) const SESSION: &str = "plan-session";
    const CWD: &str = "/agent/workspace";

    /// A plan link bound to [`SESSION`], whose agent answers every request
    /// after `session/new` with `answer`, and hands each one to the
    /// receiver.
    pub(crate) fn answering(
        answer: impl Fn(&Value) -> Result<Value, Value> + Send + 'static,
    ) -> (RemotePlan, flume::Receiver<Value>) {
        let _runtime = crate::test_runtime().enter();
        let (client_tx, from_client) = flume::unbounded::<String>();
        let (to_client, client_rx) = flume::unbounded::<String>();
        let (client, _events) = AcpClient::new(Link::new(client_tx, client_rx));
        let (seen_tx, seen) = flume::unbounded();
        thread::spawn(move || {
            for line in from_client.iter() {
                let request: Value = serde_json::from_str(&line).unwrap();
                let reply = if request["method"] == "session/new" {
                    Ok(json!(methods::new_session_response(SESSION)))
                } else {
                    let reply = answer(&request);
                    let _ = seen_tx.send(request.clone());
                    reply
                };
                let message = match reply {
                    Ok(result) => json!({"jsonrpc": "2.0", "id": request["id"], "result": result}),
                    Err(error) => json!({"jsonrpc": "2.0", "id": request["id"], "error": error}),
                };
                let _ = to_client.send(message.to_string());
            }
        });
        crate::test_block_on(client.new_session(CWD.into())).unwrap();
        let plan = RemotePlan {
            writes: PlanWrites::default(),
            link: RemoteSession {
                rewind_tx: None,
                client: Arc::new(client),
                binding: Arc::default(),
            },
        };
        (plan, seen)
    }
}

#[cfg(test)]
mod tests {
    use maki_wire::plan::{BUSY, CONFLICT, WRITE_METHOD, revision};
    use serde_json::json;
    use test_case::test_case;

    use std::path::PathBuf;

    use super::test_support::answering;
    use super::*;

    const PATH: &str = "/agent/plans/plan.md";
    const TEXT: &str = "# Plan";
    const EDITED: &str = "# Plan\n- more";

    fn read_at(text: Option<&str>) -> PlanText {
        PlanText {
            path: PATH.into(),
            text: text.map(str::to_owned),
            revision: text.map(revision),
        }
    }

    /// An editor that writes `text` over the copy and exits with `code`.
    fn writing(text: &'static str, code: i32) -> impl FnOnce(&Path) -> Result<i32, String> {
        move |path| {
            fs::write(path, text).unwrap();
            Ok(code)
        }
    }

    fn copy_named(outcome: &str) -> PathBuf {
        PathBuf::from(outcome.rsplit(' ').next().unwrap())
    }

    #[test]
    fn an_unchanged_plan_is_not_written() {
        let _runtime = crate::test_runtime().enter();
        let (plan, seen) = answering(|_| Ok(json!({})));
        assert_eq!(
            plan.edit(read_at(Some(TEXT)), writing(TEXT, 0)),
            PLAN_UNCHANGED
        );
        assert!(seen.is_empty());
    }

    /// It may have written half an edit, which must not reach the agent.
    #[test]
    fn a_failed_editor_saves_nothing_and_keeps_the_copy() {
        let _runtime = crate::test_runtime().enter();
        let (plan, seen) = answering(|_| Ok(json!({})));
        let outcome = plan.edit(read_at(Some(TEXT)), writing(EDITED, 1));
        assert!(outcome.starts_with(EDITOR_FAILED), "{outcome}");
        let copy = copy_named(&outcome);
        assert_eq!(fs::read_to_string(&copy).unwrap(), EDITED);
        fs::remove_file(copy).unwrap();
        assert!(seen.is_empty());
    }

    #[test]
    fn an_edit_is_written_at_the_revision_it_was_read_at() {
        let _runtime = crate::test_runtime().enter();
        let (plan, seen) = answering(|_| Ok(json!({})));
        assert_eq!(
            plan.edit(read_at(Some(TEXT)), writing(EDITED, 0)),
            PLAN_SAVED
        );
        let write = seen.recv().unwrap();
        assert_eq!(write["method"], WRITE_METHOD);
        assert_eq!(write["params"]["text"], EDITED);
        assert_eq!(write["params"]["revision"], revision(TEXT));
    }

    /// The agent changed the plan, or is running a prompt: the edit goes to
    /// a file of its own, which the flash names.
    #[test_case(CONFLICT ; "conflict")]
    #[test_case(BUSY ; "busy")]
    fn a_refused_edit_is_kept_and_named(refusal: &'static str) {
        let _runtime = crate::test_runtime().enter();
        let (plan, _seen) = answering(move |_| {
            Err(json!({"code": -32603, "message": "Internal error", "data": refusal}))
        });
        let outcome = plan.edit(read_at(Some(TEXT)), writing(EDITED, 0));
        assert!(outcome.starts_with(refusal), "{outcome}");
        let copy = copy_named(&outcome);
        assert_eq!(fs::read_to_string(&copy).unwrap(), EDITED);
        fs::remove_file(copy).unwrap();
    }

    #[test]
    fn a_plan_is_read_from_the_agent() {
        let _runtime = crate::test_runtime().enter();
        let (plan, _seen) =
            answering(|_| Ok(json!({"path": PATH, "text": TEXT, "revision": revision(TEXT)})));
        assert_eq!(plan.read().unwrap(), read_at(Some(TEXT)));
    }

    #[test]
    fn nothing_is_read_while_a_new_session_opens() {
        let _runtime = crate::test_runtime().enter();
        let (plan, seen) = answering(|_| Ok(json!({})));
        plan.link.binding.request();
        assert_eq!(plan.read(), Err(SESSION_OPENING.to_owned()));
        assert!(seen.is_empty());
    }
}

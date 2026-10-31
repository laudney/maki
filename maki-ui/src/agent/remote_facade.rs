//! What a remote tab answers the Lua facade with, from the agent it is
//! connected to: its stored sessions and its model and settings menus. The
//! values stay the agent's own: nothing here resolves one locally.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol_schema::v1::SessionInfo;
use jiff::Timestamp;
use maki_lua_ui::UiReply;
use maki_storage::sessions::DEFAULT_TITLE;
use maki_wire::client::AcpClient;
use serde_json::{Value, json};

use super::capabilities::{CONNECTION_LOST, UNAVAILABLE_REMOTE};
use super::remote_settings::{RemoteSettings, Setting, unoffered};

/// `maki.session.list`: every page of the agent's sessions, in the rows a
/// local list returns.
pub(crate) async fn list_sessions(client: Arc<AcpClient>, cwd: PathBuf) -> UiReply {
    let sessions = client
        .list_all_sessions(cwd)
        .await
        .map_err(|e| e.to_string())?;
    Ok(Value::Array(
        sessions.into_iter().map(session_row).collect(),
    ))
}

/// `updatedAt` becomes the epoch seconds the `sessions` picker subtracts,
/// 0 when it is missing or unreadable, as a local store writes an unknown
/// time.
fn session_row(info: SessionInfo) -> Value {
    let updated_at = info
        .updated_at
        .as_deref()
        .and_then(|at| at.parse::<Timestamp>().ok())
        .map_or(0, |at| at.as_second().max(0));
    let title = info
        .title
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_TITLE.to_owned());
    json!({
        "id": info.session_id.to_string(),
        "title": title,
        "cwd": info.cwd,
        "updated_at": updated_at,
    })
}

/// `maki.model.get`: the model the agent names and the settings it holds.
/// Thinking and fast can be set only under the settings extension, so only
/// then are they offered.
pub(crate) fn model_state(
    settings: Option<&RemoteSettings>,
    takes_settings: bool,
    fallback_spec: &str,
) -> Value {
    let model = settings.and_then(|settings| settings.models.load_full());
    let menu = settings.map(|settings| settings.menu.load_full());
    let thinking = menu.as_ref().and_then(|menu| menu.thinking.as_ref());
    let fast = menu.as_ref().and_then(|menu| menu.fast);
    let spec = model
        .as_ref()
        .and_then(|model| model.current.clone())
        .unwrap_or_else(|| fallback_spec.to_owned());
    let provider = spec.split_once('/').map_or("", |(provider, _)| provider);
    let fallback_id = spec.split_once('/').map_or(spec.as_str(), |(_, id)| id);
    let options: Vec<_> = thinking
        .filter(|_| takes_settings)
        .iter()
        .flat_map(|thinking| &thinking.choices)
        .map(|(value, _)| json!({ "name": value }))
        .collect();
    json!({
        "id": model.as_ref().and_then(|model| model.current_name()).unwrap_or(fallback_id),
        "provider": provider,
        "spec": spec,
        "thinking": thinking.map(|thinking| &thinking.current),
        "thinking_shown": thinking.map(|thinking| {
            menu.as_ref()
                .and_then(|menu| menu.thinking_label(&thinking.current))
                .unwrap_or(&thinking.current)
        }),
        "thinking_options": options,
        "fast": fast.unwrap_or_default(),
        "supports_thinking": takes_settings && thinking.is_some(),
        "supports_fast": takes_settings && fast.is_some(),
    })
}

/// `maki.model.available`: the values the agent's model menu offers.
pub(crate) fn model_choices(models: Option<&RemoteSettings>) -> Value {
    let menu = models.and_then(|models| models.models.load_full());
    let choices = menu.iter().flat_map(|menu| &menu.choices);
    Value::Array(choices.map(|choice| json!(choice.value)).collect())
}

/// `maki.model.set` of thinking and fast. A setting the agent does not
/// offer, or one that rides on the prompt without the settings extension,
/// fails at once and nothing changes. Otherwise every change is made, and
/// the future answers the confirmed state once the agent has answered each.
pub(crate) fn set_settings(
    settings: Option<&RemoteSettings>,
    takes_settings: bool,
    changes: Vec<(Setting, Value)>,
) -> Result<impl Future<Output = UiReply> + use<>, String> {
    let settings = settings.ok_or(UNAVAILABLE_REMOTE)?.clone();
    let menu = settings.menu.load_full();
    let refused = changes.iter().find(|(setting, _)| {
        let offered = menu.value(*setting).is_some();
        !offered || (setting.rides_on_the_prompt() && !takes_settings)
    });
    if let Some((setting, _)) = refused {
        return Err(unoffered(*setting));
    }
    let replies: Vec<_> = changes
        .into_iter()
        .map(|(setting, value)| settings.change(setting, value))
        .collect();
    Ok(async move {
        for reply in replies {
            reply
                .recv_async()
                .await
                .unwrap_or_else(|_| Err(CONNECTION_LOST.to_owned()))?;
        }
        // The reply can wake before the loop publishes the UI menus.
        let _confirmed = settings.lock();
        Ok(model_state(Some(&settings), takes_settings, ""))
    })
}

#[cfg(test)]
mod tests {
    use std::pin::pin;

    use agent_client_protocol_schema::v1::SessionConfigOption;
    use futures_lite::future;
    use maki_wire::extensions::Agreed;
    use test_case::test_case;

    use super::*;
    use crate::agent::AgentHandles;
    use crate::agent::fake_agent::{CWD, Peer, config_update, remote, remote_settled};
    use crate::agent::remote_loop::SETTING_NOT_SET;
    use crate::agent::remote_settings::{SESSION_REPLACED, SETTING_SUPERSEDED};
    use crate::app::tests::test_app;

    const OFF: &str = "off";
    const ADAPTIVE: &str = "adaptive";
    const HIGH: &str = "high";
    const FIRST: &str = "first-session";
    const SECOND: &str = "second-session";
    const TITLE: &str = "fix the parser";
    const UPDATED: &str = "2026-01-01T00:00:00Z";
    const UPDATED_SECS: i64 = 1_767_225_600;
    const PAGE_TWO: &str = "page-2";
    const REFUSED: &str = "no such level";
    const SET_CONFIG: &str = "session/set_config_option";
    const INVALID_PARAMS: i32 = -32602;

    fn options(thinking: &str, yolo: bool) -> Value {
        json!([
            {"id": "thinking", "name": "Thinking", "type": "select", "category": "thought_level",
             "currentValue": thinking, "options": [
                {"value": OFF, "name": OFF}, {"value": ADAPTIVE, "name": ADAPTIVE},
                {"value": HIGH, "name": HIGH}]},
            {"id": "workflow", "name": "Workflow", "type": "boolean", "currentValue": false},
            {"id": "yolo", "name": "Yolo", "type": "boolean", "currentValue": yolo},
        ])
    }

    fn with_fast(fast: bool) -> Value {
        let mut options = options(OFF, false);
        options
            .as_array_mut()
            .unwrap()
            .push(json!({"id": "fast", "name": "Fast", "type": "boolean", "currentValue": fast}));
        options
    }

    /// A remote tab under the settings extension, thinking `off`, offering
    /// no fast.
    fn agent() -> (AgentHandles, Peer, RemoteSettings) {
        agent_offering(options(OFF, false))
    }

    fn agent_offering(options: Value) -> (AgentHandles, Peer, RemoteSettings) {
        let options: Vec<SessionConfigOption> = serde_json::from_value(options).unwrap();
        let settings = RemoteSettings::from_options(&options);
        let extensions = Agreed {
            settings: true,
            ..Agreed::default()
        };
        let (handles, peer) = remote_settled(false, None, Some(settings.clone()), extensions);
        (handles, peer, settings)
    }

    fn thinking(value: &str) -> Vec<(Setting, Value)> {
        vec![(Setting::Thinking, Value::String(value.into()))]
    }

    fn set(settings: &RemoteSettings, value: &str) -> impl Future<Output = UiReply> + use<> {
        set_settings(Some(settings), true, thinking(value)).unwrap()
    }

    fn expect(peer: &Peer, id: &str, value: Value) -> Value {
        let request = peer.recv();
        assert_eq!(request["method"], SET_CONFIG);
        assert_eq!(request["params"]["configId"], id);
        assert_eq!(request["params"]["value"], value);
        request
    }

    fn answer(peer: &Peer, request: &Value, thinking: &str, yolo: bool) {
        peer.respond(request, json!({"configOptions": options(thinking, yolo)}));
    }

    fn thinking_of(state: UiReply) -> String {
        state.unwrap()["thinking"].as_str().unwrap().to_owned()
    }

    fn page(sessions: Value, next: Option<&str>) -> Value {
        json!({"sessions": sessions, "nextCursor": next})
    }

    /// The `sessions` picker subtracts `updated_at` from the clock and names
    /// an untitled row, as it does a local one.
    #[test]
    fn list_follows_the_pages_into_local_rows() {
        let _runtime = crate::test_runtime().enter();
        let (handles, peer) = remote();
        let client = Arc::clone(&handles.remote_link.client);
        let listed = tokio::spawn(list_sessions(client, CWD.into()));
        let first = peer.recv();
        assert_eq!(first["method"], "session/list");
        peer.respond(
            &first,
            page(
                json!([{"sessionId": FIRST, "cwd": CWD, "title": TITLE, "updatedAt": UPDATED}]),
                Some(PAGE_TWO),
            ),
        );
        let second = peer.recv();
        assert_eq!(second["params"]["cursor"], PAGE_TWO);
        peer.respond(
            &second,
            page(json!([{"sessionId": SECOND, "cwd": CWD}]), None),
        );

        let rows = crate::test_block_on(listed).unwrap().unwrap();
        assert_eq!(
            rows,
            json!([
                {"id": FIRST, "title": TITLE, "cwd": CWD, "updated_at": UPDATED_SECS},
                {"id": SECOND, "title": DEFAULT_TITLE, "cwd": CWD, "updated_at": 0},
            ])
        );
    }

    #[test]
    fn set_returns_the_state_the_agent_confirmed() {
        let _runtime = crate::test_runtime().enter();
        let (_handles, peer, settings) = agent();
        let answered = set(&settings, HIGH);
        let request = expect(&peer, Setting::Thinking.id(), json!(HIGH));
        answer(&peer, &request, HIGH, false);

        let state = crate::test_block_on(answered).unwrap();
        assert_eq!(state["thinking"], HIGH);
        assert_eq!(
            state["thinking_options"],
            json!([{"name": OFF}, {"name": ADAPTIVE}, {"name": HIGH}])
        );
    }

    /// Nothing is sent for it: the next request out is the next change.
    #[test]
    fn set_to_the_held_value_returns_at_once() {
        let _runtime = crate::test_runtime().enter();
        let (_handles, peer, settings) = agent();
        let answered = future::block_on(future::poll_once(set(&settings, OFF)));
        assert_eq!(thinking_of(answered.unwrap()), OFF);

        drop(set(&settings, HIGH));
        expect(&peer, Setting::Thinking.id(), json!(HIGH));
    }

    #[test_case(Setting::Fast, Value::Bool(true), true ; "not_offered")]
    #[test_case(Setting::Thinking, Value::String(HIGH.into()), false ; "without_the_settings_extension")]
    fn set_of_a_setting_the_agent_does_not_take_fails_at_once(
        setting: Setting,
        value: Value,
        takes_settings: bool,
    ) {
        let _runtime = crate::test_runtime().enter();
        let (_handles, _peer, settings) = agent();
        let refused = set_settings(Some(&settings), takes_settings, vec![(setting, value)]);
        assert_eq!(refused.err(), Some(unoffered(setting)));
        assert_eq!(settings.lock().revision(&setting.key()), 0);
    }

    #[test]
    fn set_fails_when_the_agent_refuses() {
        let _runtime = crate::test_runtime().enter();
        let (_handles, peer, settings) = agent();
        let answered = set(&settings, HIGH);
        let request = expect(&peer, Setting::Thinking.id(), json!(HIGH));
        peer.send(json!({"jsonrpc": "2.0", "id": request["id"],
            "error": {"code": INVALID_PARAMS, "message": REFUSED}}));

        let error = crate::test_block_on(answered).unwrap_err();
        assert_eq!(
            error,
            format!("{SETTING_NOT_SET}: {}: {REFUSED}", Setting::Thinking.id())
        );
    }

    #[test]
    fn set_fails_when_the_link_drops() {
        let _runtime = crate::test_runtime().enter();
        let (_handles, peer, settings) = agent();
        let answered = set(&settings, HIGH);
        expect(&peer, Setting::Thinking.id(), json!(HIGH));
        drop(peer);

        assert_eq!(
            crate::test_block_on(answered),
            Err(CONNECTION_LOST.to_owned())
        );
    }

    #[test]
    fn set_fails_when_the_session_is_replaced_first() {
        let _runtime = crate::test_runtime().enter();
        let (mut handles, peer, settings) = agent();
        let answered = set(&settings, HIGH);
        expect(&peer, Setting::Thinking.id(), json!(HIGH));
        handles.rebind(&mut test_app());

        assert_eq!(
            crate::test_block_on(answered),
            Err(SESSION_REPLACED.to_owned())
        );
    }

    /// Yolo is in flight, so the first thinking change waits unsent and the
    /// second replaces it.
    #[test]
    fn a_superseded_set_fails_and_the_newer_one_gets_the_answer() {
        let _runtime = crate::test_runtime().enter();
        let (_handles, peer, settings) = agent();
        let _ = settings.change(Setting::Yolo, Value::Bool(true));
        let yolo = expect(&peer, Setting::Yolo.id(), json!(true));
        let older = set(&settings, HIGH);
        let newer = set(&settings, ADAPTIVE);

        let superseded = future::block_on(future::poll_once(older));
        assert_eq!(
            superseded,
            Some(Err(format!(
                "{SETTING_SUPERSEDED}: {} = {ADAPTIVE}",
                Setting::Thinking.id()
            )))
        );
        answer(&peer, &yolo, OFF, true);
        let request = expect(&peer, Setting::Thinking.id(), json!(ADAPTIVE));
        answer(&peer, &request, ADAPTIVE, true);
        assert_eq!(thinking_of(crate::test_block_on(newer)), ADAPTIVE);
    }

    /// The agent holds `off` while `high` is out, so `off` is a change of
    /// its own and waits for the agent to come back to it.
    #[test]
    fn set_to_the_held_value_waits_while_another_value_is_out() {
        let _runtime = crate::test_runtime().enter();
        let (_handles, peer, settings) = agent();
        let high = set(&settings, HIGH);
        let request = expect(&peer, Setting::Thinking.id(), json!(HIGH));
        let mut off = pin!(set(&settings, OFF));
        assert_eq!(future::block_on(future::poll_once(&mut off)), None);

        answer(&peer, &request, HIGH, false);
        assert_eq!(thinking_of(crate::test_block_on(high)), HIGH);
        let request = expect(&peer, Setting::Thinking.id(), json!(OFF));
        answer(&peer, &request, OFF, false);
        assert_eq!(thinking_of(crate::test_block_on(off)), OFF);
    }

    /// Yolo is in flight, so fast waits unsent when the agent drops it.
    #[test_case(options(OFF, false) ; "removed")]
    #[test_case(json!([{"id": "fast", "name": "Fast", "type": "select", "currentValue": "off", "options": []}]) ; "changed_type")]
    fn set_fails_when_the_agent_stops_offering_the_setting(published: Value) {
        let _runtime = crate::test_runtime().enter();
        let (_handles, peer, settings) = agent_offering(with_fast(false));
        let _ = settings.change(Setting::Yolo, Value::Bool(true));
        expect(&peer, Setting::Yolo.id(), json!(true));
        let fast = vec![(Setting::Fast, Value::Bool(true))];
        let answered = set_settings(Some(&settings), true, fast).unwrap();
        peer.send(config_update(published));

        assert_eq!(
            crate::test_block_on(answered),
            Err(unoffered(Setting::Fast))
        );
    }
}

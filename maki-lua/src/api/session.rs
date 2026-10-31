//! Tests of `maki.session` over the agent's backends and a UI channel. The
//! namespace is the core's.

pub(crate) use maki_lua_core::api::session::{not_live, required_session};

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::Arc;

    use arc_swap::ArcSwap;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use maki_agent::SessionMailbox;
    use maki_agent::agent::{
        History, HistorySnapshot, SessionExport, SharedHeader, SharedMessages,
        publish_live_history, publish_live_session,
    };
    use maki_agent::session::{NOTHING_TO_STORE, SessionHeader, StoredSession, store_new};
    use maki_lua_core::api::session::{
        BLANK_NOTIFY_ERR, CONTENT_KEY, EMPTY_ARRAY_ERR, MESSAGES_KEY, NO_FOCUSED_ERR,
        SESSION_REQUIRED_ERR,
    };
    use maki_providers::{ContentBlock, ImageMediaType, ImageSource, Message, MessageKind, Role};
    use maki_storage::StateDir;
    use maki_storage::frame::{FactsUpdate, PromptFacts, StoredFrame};
    use maki_storage::id::MakiId;
    use maki_storage::sessions::{StoredMode, StoredThinking};
    use mlua::{Lua, Result as LuaResult, Table};
    use serde_json::Value as JsonValue;
    use tempfile::TempDir;

    use crate::api::image::{LuaImage, create_image_table};
    use crate::api::test_support::{
        eval, granted as lua_granted, lua as lua_with_session, replying_ui, serving_ui,
    };
    use crate::api::util::convert::lua_to_json;
    use crate::domains::facade::{SessionSnapshotFn, SessionSnapshotSlot};
    use crate::plugin_permissions::{Permission, PluginPermissions, denied_error};
    use crate::session_snapshot::{MODE_PLAN, STATUS_IDLE, STATUS_STORED};
    use maki_lua_core::api::base64::create_base64_table;
    use maki_lua_core::api::json::create_json_table;
    use maki_lua_core::api::top::add_top_methods;
    use maki_lua_core::facade::{NO_UI_ERR, SessionRequest};
    use maki_lua_ui::UiAction;
    use mlua::Value;
    use serde_json::json;
    use test_case::test_case;

    const FOCUSED_TEXT: &str = "focused";
    const TOOL_ID: &str = "toolu_1";
    const TOOL_NAME: &str = "read";
    const TOOL_PATH: &str = "src/main.rs";
    const TOOL_OUTPUT: &str = "no such file";
    const SIGNATURE: &str = "sig";
    const THOUGHT: &str = "let me check";
    const IMAGE_DATA: &str = "iVBORw0KGgo=";
    const TEXT: &str = "hello";
    const TITLE: &str = "ported";
    const MODEL: &str = "anthropic/claude-sonnet-4-5";
    const THINKING: &str = "high";
    const PLUGIN: &str = "test";
    const FACTS_CWD: &str = "/origin";
    const ONE_MESSAGE: &str = "{ { role = 'user', content = { { type = 'text', text = 'hi' } } } }";

    #[test]
    fn live_without_ui_returns_error_pair() {
        let lua = lua_with_session(None);
        let (val, err): (Value, Option<String>) = eval(&lua, "return session.live()");
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(NO_UI_ERR));
    }

    #[test_case("return session.prompt('hi', { session = 'abc' })", Some("abc") ; "explicit_session_id")]
    #[test_case("return session.prompt('hi')", None ; "defaults_to_focused")]
    fn prompt_forwards_text_and_session_id(code: &str, expected_id: Option<&str>) {
        let (lua, requests) = replying_ui(Ok(json!("queued")));
        let (val, err): (String, Option<String>) = eval(&lua, code);
        let UiAction::Session {
            req: SessionRequest::Prompt { id, text },
            ..
        } = requests.try_recv().unwrap()
        else {
            panic!("expected prompt request");
        };
        assert_eq!(id.as_deref(), expected_id);
        assert_eq!(text, "hi");
        assert_eq!(err, None);
        assert_eq!(val, "queued");
    }

    /// Mode is per session like the plan it drives, so a row handler firing
    /// for a background tab has to be able to name it.
    #[test_case(r#"return session.set_mode('build', { session = 'abc' })"#, Some("abc"), "build" ; "explicit_session_id")]
    #[test_case("return session.set_mode('plan')", None, "plan" ; "defaults_to_focused")]
    fn set_mode_forwards_the_mode_and_session_id(
        code: &str,
        expected_id: Option<&str>,
        expected_mode: &str,
    ) {
        let (lua, requests) = replying_ui(Ok(json!(true)));
        let (val, err): (bool, Option<String>) = eval(&lua, code);
        let UiAction::Session {
            req: SessionRequest::SetMode { id, mode },
            ..
        } = requests.try_recv().unwrap()
        else {
            panic!("expected set_mode request");
        };
        assert_eq!(id.as_deref(), expected_id);
        assert_eq!(mode, expected_mode);
        assert_eq!(err, None);
        assert!(val);
    }

    #[test]
    fn notify_is_synchronous_and_queues_an_observation() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let lua = lua_with_session(None);
        lua.globals().set("session_id", id.to_string()).unwrap();

        let (value, error): (bool, Option<String>) = lua
            .load("return session.notify('built', { session = session_id })")
            .eval()
            .unwrap();

        assert!(value);
        assert_eq!(error, None);
        let messages = mailbox.drain();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].is_observation());
        assert_eq!(messages[0].first_text_content(), Some("built"));
        assert_eq!(messages[0].display_text, None);
    }

    #[test_case(" agent-1 finished ", Some("agent-1 finished"); "trimmed")]
    #[test_case("agent-1\n finished", Some("agent-1 finished"); "single_line")]
    #[test_case("   ", None; "blank")]
    fn notify_normalizes_display_notes_without_changing_model_text(
        display: &str,
        expected: Option<&str>,
    ) {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let lua = lua_with_session(None);
        lua.globals().set("session_id", id.to_string()).unwrap();
        lua.globals().set("notice_text", TEXT).unwrap();
        lua.globals().set("display_note", display).unwrap();

        let (value, error): (bool, Option<String>) = lua
            .load(concat!(
                "return session.notify(notice_text, { session = session_id, ",
                "wake = true, display = display_note })"
            ))
            .eval()
            .unwrap();

        assert!(value);
        assert_eq!(error, None);
        let messages = mailbox.claim_wake();
        assert_eq!(messages[0].display_text.as_deref(), expected);
        assert_eq!(messages[0].first_text_content(), Some(TEXT));
    }

    #[test]
    fn waking_notify_sets_the_mailbox_wake_flag() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let lua = lua_with_session(None);
        lua.globals().set("session_id", id.to_string()).unwrap();

        let (value, error): (bool, Option<String>) = lua
            .load("return session.notify('failed', { session = session_id, wake = true })")
            .eval()
            .unwrap();

        assert!(value);
        assert_eq!(error, None);
        assert_eq!(mailbox.claim_wake().len(), 1);
    }

    #[test]
    fn notify_rejects_missing_and_non_live_sessions() {
        let lua = lua_with_session(None);
        let (_, missing): (Value, Option<String>) =
            lua.load("return session.notify('built')").eval().unwrap();
        assert_eq!(missing.as_deref(), Some(SESSION_REQUIRED_ERR));

        let id = MakiId::generate();
        lua.globals().set("session_id", id.to_string()).unwrap();
        let (_, not_live): (Value, Option<String>) = lua
            .load("return session.notify('built', { session = session_id })")
            .eval()
            .unwrap();
        assert_eq!(not_live, Some(format!("session not live: {id}")));
    }

    #[test]
    fn notify_rejects_blank_text_and_invalid_session_ids() {
        let lua = lua_with_session(None);
        let (_, blank): (Value, Option<String>) = lua
            .load("return session.notify(' ', { session = 'invalid' })")
            .eval()
            .unwrap();
        assert_eq!(blank.as_deref(), Some(BLANK_NOTIFY_ERR));

        let (_, invalid): (Value, Option<String>) = lua
            .load("return session.notify('built', { session = 'invalid' })")
            .eval()
            .unwrap();
        assert!(invalid.is_some_and(|error| error.contains("invalid base58")));
    }

    /// Keep the returned history alive for as long as the session should be.
    fn live(id: MakiId, messages: Vec<Message>) -> History {
        let mirror: SharedMessages = Arc::new(ArcSwap::from_pointee(HistorySnapshot::default()));
        publish_live_history(id, &mirror);
        History::new(messages).with_mirror(mirror)
    }

    #[test]
    fn messages_reads_a_live_transcript() {
        const FIRST: &str = "first";
        const ANSWER: &str = "done";
        let id = MakiId::generate();
        let _history = live(
            id,
            vec![
                Message::user(FIRST.into()),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: ANSWER.into(),
                    }],
                    ..Default::default()
                },
            ],
        );
        let lua = lua_with_session(None);

        let code = format!("return session.messages({{ session = '{id}', last = 1 }})");
        let (msgs, err): (Table, Option<String>) = eval(&lua, &code);
        assert_eq!(err, None);
        assert_eq!(msgs.raw_len(), 1);
        let last: Table = msgs.get(1).unwrap();
        assert_eq!(last.get::<String>("role").unwrap(), "assistant");
        let block: Table = last.get::<Table>("content").unwrap().get(1).unwrap();
        assert_eq!(block.get::<String>("text").unwrap(), ANSWER);
    }

    #[test_case("messages" ; "messages")]
    #[test_case("export" ; "export")]
    fn reading_a_session_that_does_not_exist_is_an_error_pair(call: &str) {
        let (lua, _dir) = headless();
        let id = MakiId::generate();
        let code = format!("return session.{call}({{ session = '{id}' }})");
        let (value, err): (Value, Option<String>) = eval(&lua, &code);
        assert!(value.is_nil());
        assert!(err.is_some_and(|err| err.contains(&id.to_string())));
    }

    fn headless_focused_on(id: MakiId) -> Lua {
        let lua = lua_with_session(None);
        let snapshot = json!({ "id": id.to_string(), "title": FOCUSED_TEXT });
        let provider: SessionSnapshotFn = Box::new(move |requested| {
            assert_eq!(requested, None);
            Ok(snapshot.clone())
        });
        lua.set_app_data(SessionSnapshotSlot(provider));
        lua
    }

    /// The UI answers `Current` with the bare id, not a snapshot.
    fn ui_focused_on(id: MakiId) -> Lua {
        serving_ui(move |action| match action {
            UiAction::Session {
                req: SessionRequest::Current,
                reply_tx,
            } => {
                reply_tx.send(Ok(json!(id.to_string()))).unwrap();
            }
            UiAction::ExportSession { reply_tx, .. } => {
                let mut stored = StoredSession::new(MODEL, FACTS_CWD);
                stored.push_message(Message::user(FOCUSED_TEXT.into()));
                reply_tx.send(Ok(SessionExport::stored(stored))).unwrap();
            }
            _ => panic!("unexpected UI request"),
        })
    }

    #[test_case(headless_focused_on ; "headless_snapshot_id")]
    #[test_case(ui_focused_on ; "ui_current_bare_id")]
    fn current_names_the_focused_session(focused_on: fn(MakiId) -> Lua) {
        let id = MakiId::generate();
        let lua = focused_on(id);
        let (got, err): (Option<String>, Option<String>) = eval(&lua, "return session.current()");
        assert_eq!(err, None);
        assert_eq!(got, Some(id.to_string()));
    }

    #[test_case(headless_focused_on ; "headless_snapshot_id")]
    #[test_case(ui_focused_on ; "ui_current_bare_id")]
    fn messages_defaults_to_the_focused_session(focused_on: fn(MakiId) -> Lua) {
        let id = MakiId::generate();
        let _history = live(id, vec![Message::user(FOCUSED_TEXT.into())]);
        let lua = focused_on(id);

        let (msgs, err): (Table, Option<String>) = eval(&lua, "return session.messages()");
        assert_eq!(err, None);
        assert_eq!(msgs.raw_len(), 1);
        let block: Table = msgs
            .get::<Table>(1)
            .unwrap()
            .get::<Table>("content")
            .unwrap()
            .get(1)
            .unwrap();
        assert_eq!(block.get::<String>("text").unwrap(), FOCUSED_TEXT);
    }

    #[test]
    fn messages_without_a_focused_session_is_an_error_pair() {
        let (lua, requests) = replying_ui(Ok(JsonValue::Null));
        let (_, err): (Value, Option<String>) = eval(&lua, "return session.messages()");
        assert_eq!(err.as_deref(), Some(NO_FOCUSED_ERR));
        assert!(matches!(
            requests.try_recv().unwrap(),
            UiAction::Session {
                req: SessionRequest::Current,
                ..
            }
        ));
    }

    /// `session` plus the `maki` pieces a plugin round trips with, and
    /// `one_message`, the smallest history `new` stores.
    fn plugin_lua(tx: Option<flume::Sender<UiAction>>) -> Lua {
        with_plugin_globals(lua_with_session(tx))
    }

    fn with_plugin_globals(lua: Lua) -> Lua {
        let maki = lua.create_table().unwrap();
        maki.set("json", create_json_table(&lua).unwrap()).unwrap();
        maki.set("image", create_image_table(&lua).unwrap())
            .unwrap();
        maki.set("base64", create_base64_table(&lua).unwrap())
            .unwrap();
        maki.set("NIL", Value::NULL).unwrap();
        add_top_methods(&maki, &lua, PLUGIN.into()).unwrap();
        lua.globals().set("maki", maki).unwrap();
        lua.load(format!("one_message = {ONE_MESSAGE}"))
            .exec()
            .unwrap();
        lua
    }

    /// No UI, and stored sessions in a temp dir.
    fn headless() -> (Lua, TempDir) {
        headless_granted(PluginPermissions::trusted())
    }

    fn headless_granted(grants: PluginPermissions) -> (Lua, TempDir) {
        let dir = TempDir::new().unwrap();
        let lua = with_plugin_globals(lua_granted(None, grants));
        lua.set_app_data(StateDir::from_path(dir.path().to_path_buf()));
        (lua, dir)
    }

    fn storage(lua: &Lua) -> StateDir {
        lua.app_data_ref::<StateDir>().unwrap().clone()
    }

    fn assistant(content: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::Assistant,
            content,
            ..Default::default()
        }
    }

    fn user(content: Vec<ContentBlock>) -> Message {
        Message {
            content,
            ..Default::default()
        }
    }

    fn image() -> ContentBlock {
        ContentBlock::Image {
            source: ImageSource::new(ImageMediaType::Png, IMAGE_DATA.into()),
        }
    }

    /// Something of every shape a stored transcript holds, the opaque fields
    /// and a null inside a tool input included.
    fn rich_history() -> Vec<Message> {
        vec![
            Message::user(TEXT.into()),
            assistant(vec![
                ContentBlock::Thinking {
                    thinking: THOUGHT.into(),
                    signature: Some(SIGNATURE.into()),
                },
                ContentBlock::RedactedThinking {
                    data: SIGNATURE.into(),
                },
                ContentBlock::ToolUse {
                    id: TOOL_ID.into(),
                    name: TOOL_NAME.into(),
                    input: json!({ "path": TOOL_PATH, "offset": null, "files": [] }),
                    thought_signature: Some(SIGNATURE.into()),
                },
            ]),
            user(vec![ContentBlock::ToolResult {
                tool_use_id: TOOL_ID.into(),
                content: TOOL_OUTPUT.into(),
                is_error: true,
                loaded_tools: vec![TOOL_NAME.into()],
            }]),
            Message {
                kind: MessageKind::ContextUpdate(Arc::new(FactsUpdate {
                    cwd: Some(FACTS_CWD.into()),
                    ..Default::default()
                })),
                content: vec![ContentBlock::Text { text: TEXT.into() }],
                ..Default::default()
            },
            Message {
                kind: MessageKind::Observation,
                display_text: Some(String::new()),
                content: vec![
                    image(),
                    ContentBlock::Unknown(json!({ "type": "audio", "id": 1 })),
                ],
                ..Default::default()
            },
            assistant(vec![ContentBlock::Text { text: TEXT.into() }]),
        ]
    }

    fn stored(lua: &Lua, messages: Vec<Message>) -> MakiId {
        let mut session = StoredSession::new(MODEL, FACTS_CWD);
        session.replace_messages(messages);
        session.set_title(TITLE.into());
        session.meta.mode = Some(StoredMode::Plan);
        session.meta.thinking = Some(StoredThinking::parse_setting(THINKING).unwrap());
        session.meta.fast = true;
        session.set_frame(Some(Arc::new(StoredFrame::facts_only(PromptFacts {
            cwd: FACTS_CWD.into(),
            ..Default::default()
        }))));
        store_new(session, &storage(lua)).unwrap()
    }

    fn json_of(lua: &Lua, value: Value) -> JsonValue {
        lua_to_json(lua, &value).unwrap()
    }

    /// What `new` drops on purpose: thinking is bound to the origin's prefix.
    fn without_thinking(mut doc: JsonValue) -> JsonValue {
        for message in doc[MESSAGES_KEY].as_array_mut().unwrap() {
            let blocks = message[CONTENT_KEY].as_array_mut().unwrap();
            blocks.retain(|block| {
                !matches!(
                    block["type"].as_str(),
                    Some("thinking" | "redacted_thinking")
                )
            });
            for block in blocks {
                block.as_object_mut().unwrap().remove("thought_signature");
            }
        }
        doc
    }

    #[test]
    fn messages_are_the_serde_shape_with_nulls_kept() {
        let id = MakiId::generate();
        let history = rich_history();
        let _live = live(id, history.clone());
        let (lua, _dir) = headless();

        let msgs: Table = eval(
            &lua,
            &format!("return session.messages({{ session = '{id}' }})"),
        );

        assert_eq!(
            json_of(&lua, Value::Table(msgs)),
            serde_json::to_value(&history).unwrap()
        );
    }

    #[test]
    fn an_image_source_is_a_handle_sharing_the_payload() {
        let id = MakiId::generate();
        let block = image();
        let ContentBlock::Image { source } = &block else {
            unreachable!()
        };
        let payload = Arc::clone(&source.data);
        let _live = live(id, vec![user(vec![block])]);
        let (lua, _dir) = headless();

        let handle: mlua::AnyUserData = eval(
            &lua,
            &format!("return session.messages({{ session = '{id}' }})[1].content[1].source"),
        );

        let shared = handle.borrow::<LuaImage>().unwrap().to_source();
        assert!(Arc::ptr_eq(&shared.data, &payload));
    }

    /// The document goes out and comes back as one table, through
    /// `maki.deepcopy` like a redactor would copy it, and only what `new`
    /// drops on purpose is gone.
    #[test]
    fn export_new_export_round_trips_minus_thinking() {
        let (lua, _dir) = headless();
        let id = stored(&lua, rich_history());
        lua.globals().set("origin", id.to_string()).unwrap();

        let (before, after): (Value, Value) = eval(
            &lua,
            r#"
            local doc = assert(session.export({ session = origin }))
            local copy = assert(session.new(maki.deepcopy(doc)))
            return doc, assert(session.export({ session = copy }))
            "#,
        );

        let after = json_of(&lua, after);
        assert_eq!(after, without_thinking(json_of(&lua, before)));
        assert_eq!(after["facts"]["cwd"], FACTS_CWD);
    }

    #[test]
    fn messages_reads_the_tail_of_a_stored_transcript() {
        let (lua, _dir) = headless();
        let history = rich_history();
        let id = stored(&lua, history.clone());

        let msgs: Value = eval(
            &lua,
            &format!("return session.messages({{ session = '{id}', last = 2 }})"),
        );

        assert_eq!(
            json_of(&lua, msgs),
            serde_json::to_value(&history[history.len() - 2..]).unwrap()
        );
    }

    #[test]
    fn read_of_a_stored_session_reports_its_settings() {
        let (lua, _dir) = headless();
        let id = stored(&lua, vec![Message::user(TEXT.into())]);

        let snapshot: Table = eval(
            &lua,
            &format!("return session.read({{ session = '{id}' }})"),
        );

        assert_eq!(snapshot.get::<String>("status").unwrap(), STATUS_STORED);
        assert_eq!(snapshot.get::<String>("thinking").unwrap(), THINKING);
        assert!(snapshot.get::<bool>("fast").unwrap());
        assert_eq!(snapshot.get::<String>("mode").unwrap(), MODE_PLAN);
        assert_eq!(snapshot.get::<String>("model").unwrap(), MODEL);
        assert_eq!(snapshot.get::<String>("title").unwrap(), TITLE);
    }

    /// A failed call leaves nothing behind. A headless driver has no tab to
    /// open, so `prompt` and `focus` fail there before anything is stored.
    #[test_case("{ messages = {} }", format!("{MESSAGES_KEY}: {EMPTY_ARRAY_ERR}") ; "untagged_messages")]
    #[test_case(
        "{ messages = { one_message[1], { role = 'user', content = {} } } }",
        format!("{MESSAGES_KEY}[1].{CONTENT_KEY}: {EMPTY_ARRAY_ERR}")
        ; "untagged_content_is_located"
    )]
    #[test_case("{}", NOTHING_TO_STORE.to_owned() ; "nothing_to_store")]
    #[test_case("{ title = 'x', messages = maki.json.decode('[]') }", NOTHING_TO_STORE.to_owned() ; "tagged_empty_history")]
    #[test_case(r#"{ messages = maki.json.decode('[{"role":"user","content":[]}]') }"#, NOTHING_TO_STORE.to_owned() ; "empty_after_repair")]
    #[test_case("{ prompt = 'go', messages = one_message }", NO_UI_ERR.to_owned() ; "headless_prompt")]
    #[test_case("{ focus = true, messages = one_message }", NO_UI_ERR.to_owned() ; "headless_focus")]
    fn new_refuses(opts: &str, expected: String) {
        let (lua, _dir) = headless();
        let (id, err): (Value, Option<String>) = eval(&lua, &format!("return session.new({opts})"));
        assert!(id.is_nil());
        assert_eq!(err, Some(expected));
        assert!(StoredSession::list_all(&storage(&lua)).unwrap().is_empty());
    }

    /// Rewind brings a turn that runs back as written by a plugin that
    /// could send prompts, so importing one takes `agent` as well.
    #[test_case("hi" => true ; "a_plain_turn")]
    #[test_case("/compact" => false ; "a_command")]
    #[test_case("!ls" => false ; "a_shell_line")]
    #[test_case(" exit " => false ; "the_exit_word")]
    fn new_with_session_write_alone_imports(text: &str) -> bool {
        let (lua, _dir) = headless_granted(PluginPermissions::from_approved(["session_write"]));
        let code = format!(
            "return session.new({{ messages = {{ {{ role = 'user', content = {{ {{ type = 'text', text = '{text}' }} }} }} }} }})"
        );
        match crate::test_support::runtime()
            .block_on(lua.load(code).eval_async::<(Value, Option<String>)>())
        {
            Ok((id, err)) => {
                assert_eq!(err, None);
                !id.is_nil()
            }
            Err(e) => {
                let expected = denied_error(Permission::Agent).to_string();
                assert!(e.to_string().contains(&expected), "{e}");
                assert!(StoredSession::list_all(&storage(&lua)).unwrap().is_empty());
                false
            }
        }
    }

    fn image_history(source: &str) -> String {
        format!(
            "{{ {{ role = 'user', content = {{ {{ type = 'image', source = {source} }} }} }} }}"
        )
    }

    #[test_case("{ type = 'base64', media_type = 'image/png', data = image_data }" ; "base64_table")]
    #[test_case("maki.image.decode(maki.base64.decode(image_data))" ; "handle")]
    fn headless_new_stores_an_image_given_either_way(source: &str) {
        let (lua, _dir) = headless();
        lua.globals().set("image_data", tiny_png()).unwrap();
        let code = format!(
            "return assert(session.new({{ messages = {} }}))",
            image_history(source)
        );
        let id: String = eval(&lua, &code);

        let session = StoredSession::read_only(id.parse().unwrap(), &storage(&lua)).unwrap();
        let [Message { content, .. }] = session.messages() else {
            panic!("expected one message");
        };
        let [ContentBlock::Image { source }] = content.as_slice() else {
            panic!("expected one image block");
        };
        assert_eq!(source.media_type, ImageMediaType::Png);
        assert_eq!(&*source.data, tiny_png());
    }

    fn tiny_png() -> String {
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(1, 1)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        STANDARD.encode(bytes.into_inner())
    }

    #[test]
    fn importing_with_a_closed_ui_does_not_fall_back_to_local_storage() {
        const CODE: &str = "return session.new({ messages = one_message })";
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = plugin_lua(Some(tx));
        drop(rx);
        let (value, error): (Value, Option<String>) = eval(&lua, CODE);
        assert_eq!(value, Value::Nil);
        assert_eq!(error.as_deref(), Some(NO_UI_ERR));
    }

    /// Validation is the UI's: it lays the transcript over the focused tab's
    /// blank session, so the transcript arrives as parsed.
    #[test]
    fn new_with_a_ui_hands_it_the_parsed_transcript() {
        let id = MakiId::generate();
        let (lua, requests) = replying_ui(Ok(json!(id)));
        let lua = with_plugin_globals(lua);
        let code = format!(
            "return session.new({{ title = '{TITLE}', focus = true, messages = one_message }})"
        );
        let (got, err): (String, Option<String>) = eval(&lua, &code);
        let UiAction::Session {
            req:
                SessionRequest::New {
                    transcript,
                    prompt,
                    focus,
                },
            ..
        } = requests.try_recv().unwrap()
        else {
            panic!("expected new request");
        };
        assert_eq!(transcript.title.as_deref(), Some(TITLE));
        assert_eq!(transcript.messages.len(), 1);
        assert_eq!(transcript.fast, None);
        assert_eq!(prompt, None);
        assert!(focus);
        assert_eq!(err, None);
        assert_eq!(got, id.to_string());
    }

    #[test]
    fn export_with_a_ui_is_answered_by_it() {
        let id = MakiId::generate();
        let lua = serving_ui(move |request| {
            let UiAction::ExportSession {
                id: asked,
                reply_tx,
            } = request
            else {
                panic!("expected export request");
            };
            assert_eq!(asked, id.to_string());
            let mut session = StoredSession::new(MODEL, FACTS_CWD);
            session.set_title(TITLE.into());
            session.replace_messages(vec![Message::user(TEXT.into())]);
            reply_tx.send(Ok(SessionExport::stored(session))).unwrap();
        });

        let doc: Value = eval(
            &lua,
            &format!("return session.export({{ session = '{id}' }})"),
        );

        let doc = json_of(&lua, doc);
        assert_eq!(doc["title"], TITLE);
        assert_eq!(doc[MESSAGES_KEY][0]["content"][0]["text"], TEXT);
    }

    /// A headless driver's session before its first save: `maki -p`'s
    /// `TurnEnd` hook, ACP's first prompt. Keep the returned pair alive for
    /// as long as the session should be.
    fn live_unsaved(id: MakiId, messages: Vec<Message>) -> (History, SharedHeader) {
        let mut session = StoredSession::new(MODEL, FACTS_CWD);
        session.set_title(TITLE.into());
        session.meta.fast = true;
        let header: SharedHeader = Arc::new(ArcSwap::from_pointee(SessionHeader::of(&session)));
        let mirror: SharedMessages = Arc::new(ArcSwap::from_pointee(HistorySnapshot::default()));
        publish_live_session(id, &mirror, &header);
        (History::new(messages).with_mirror(mirror), header)
    }

    #[test]
    fn headless_reads_of_a_live_unsaved_session_need_no_file() {
        let (lua, _dir) = headless();
        let id = MakiId::generate();
        let history = vec![Message::user(TEXT.into())];
        let _live = live_unsaved(id, history.clone());
        lua.globals().set("id", id.to_string()).unwrap();

        let (doc, snapshot): (Value, Table) = eval(
            &lua,
            "return assert(session.export({ session = id })), assert(session.read({ session = id }))",
        );

        let doc = json_of(&lua, doc);
        assert_eq!(doc["title"], TITLE);
        assert_eq!(doc["model"], MODEL);
        assert_eq!(doc["fast"], true);
        assert_eq!(doc[MESSAGES_KEY], serde_json::to_value(&history).unwrap());
        assert_eq!(snapshot.get::<String>("status").unwrap(), STATUS_IDLE);
        assert_eq!(snapshot.get::<String>("cwd").unwrap(), FACTS_CWD);
    }

    /// A stored-only session lands in the project of the session the driver
    /// runs, not wherever the process started.
    #[test]
    fn headless_new_lands_in_the_driver_session_cwd() {
        const DRIVER_CWD: &str = "/driver/project";
        let (lua, _dir) = headless();
        let provider: SessionSnapshotFn =
            Box::new(|_| Ok(json!({ "id": MakiId::generate().to_string(), "cwd": DRIVER_CWD })));
        lua.set_app_data(SessionSnapshotSlot(provider));

        let id: String = eval(
            &lua,
            "return assert(session.new({ messages = one_message }))",
        );

        let session = StoredSession::read_only(id.parse().unwrap(), &storage(&lua)).unwrap();
        assert_eq!(session.cwd, DRIVER_CWD);
    }

    /// ACP installs no slot. The one session it happens to have open must not
    /// decide where an import lands, or a second open session would move it.
    #[test]
    fn headless_new_without_a_driver_session_lands_where_maki_runs() {
        let (lua, _dir) = headless();
        let _live = live_unsaved(MakiId::generate(), Vec::new());

        let id: String = eval(
            &lua,
            "return assert(session.new({ messages = one_message }))",
        );

        let session = StoredSession::read_only(id.parse().unwrap(), &storage(&lua)).unwrap();
        let here = std::env::current_dir().unwrap();
        assert_eq!(session.cwd, here.to_string_lossy());
    }

    #[test]
    fn set_title_with_wrong_type_throws() {
        let lua = lua_with_session(None);
        let result: LuaResult<Value> = crate::test_support::runtime()
            .block_on(lua.load("return session.set_title('oops')").eval_async());
        assert!(result.unwrap_err().to_string().contains("table"));
    }
}

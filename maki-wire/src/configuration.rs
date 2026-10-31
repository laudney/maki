//! Desired configuration and the confirmations that settle its revisions.

use std::collections::BTreeMap;
use std::mem;

use agent_client_protocol_schema::v1::{
    SessionConfigOption, SessionConfigOptionValue, SetSessionConfigOptionResponse,
    SetSessionModeResponse,
};
use flume::{Receiver, Sender};
use serde_json::Value;

use crate::client::{Confirmed, Sequenced};

pub const NOT_SET: &str = "The remote agent did not take the setting";
pub const UNOFFERED: &str = "The remote agent does not offer the setting";
pub const KEPT: &str = "the agent kept another value";
pub const SUPERSEDED: &str = "A newer change replaced it before it was sent";
pub const REPLACED: &str = "The session was replaced before the agent answered";
pub const CLOSED: &str = "The connection to the remote agent was lost";
pub type Reply = Receiver<Result<(), String>>;
type Replies = Vec<Sender<Result<(), String>>>;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Key {
    Option(String),
    Mode,
}

impl Key {
    pub fn name(&self) -> &str {
        match self {
            Self::Option(id) => id,
            Self::Mode => "mode",
        }
    }
}

#[derive(Clone)]
pub struct Change {
    pub key: Key,
    pub value: Value,
    pub revision: u64,
}

struct Desired {
    change: Change,
    deferred: bool,
    replies: Replies,
}
pub struct Flight {
    pub change: Change,
    replies: Replies,
}

#[derive(Default)]
pub struct Configuration {
    pub confirmed: Confirmed,
    options_seq: u64,
    mode_seq: u64,
    desired: BTreeMap<Key, Desired>,
    pub flights: BTreeMap<u64, Flight>,
    revision: u64,
    closed: bool,
}

impl Configuration {
    pub fn new(confirmed: Confirmed, seq: u64) -> Self {
        Self {
            confirmed,
            options_seq: seq,
            mode_seq: seq,
            ..Self::default()
        }
    }

    pub fn open(&mut self, mode: Option<String>, seq: u64) {
        self.confirmed.mode = mode;
        self.mode_seq = seq;
        self.options_seq = seq;
    }

    pub fn sequence(&self) -> u64 {
        self.options_seq.max(self.mode_seq)
    }
    pub fn value(&self, key: &Key) -> Option<Value> {
        held(&self.confirmed, key)
    }
    pub fn revision(&self, key: &Key) -> u64 {
        self.desired
            .get(key)
            .map_or(0, |desired| desired.change.revision)
    }
    pub fn wanted(&self, key: &Key) -> Option<&Value> {
        self.desired.get(key).map(|desired| &desired.change.value)
    }
    pub fn deferred_change(&self) -> Option<Change> {
        self.desired
            .values()
            .find(|desired| desired.deferred)
            .map(|desired| desired.change.clone())
    }

    pub fn seed(&mut self, key: Key, value: Value) {
        self.desired.insert(
            key.clone(),
            Desired {
                change: Change {
                    key,
                    value,
                    revision: 0,
                },
                deferred: false,
                replies: Vec::new(),
            },
        );
    }

    pub fn desire(&mut self, key: Key, value: Value, deferred: bool) -> Change {
        self.revision += 1;
        let change = Change {
            key: key.clone(),
            value,
            revision: self.revision,
        };
        if let Some(mut old) = self.desired.remove(&key) {
            let value = change
                .value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| change.value.to_string());
            answer(
                &mut old.replies,
                Err(format!("{SUPERSEDED}: {} = {value}", key.name())),
            );
        }
        self.desired.insert(
            key,
            Desired {
                change: change.clone(),
                deferred,
                replies: Vec::new(),
            },
        );
        change
    }

    pub fn request(&mut self, key: Key, value: Value) -> Reply {
        let (reply, received) = flume::bounded(1);
        let change = self.desire(key.clone(), value, false);
        if self.closed {
            let _ = reply.send(Err(CLOSED.into()));
        } else if self.value(&key).as_ref() == Some(&change.value) && !self.pending(&key) {
            let _ = reply.send(Ok(()));
        } else {
            self.desired.get_mut(&key).unwrap().replies.push(reply);
        }
        received
    }

    pub fn take_deferred(&mut self, key: &Key) -> Option<Change> {
        if !self.desired.get(key)?.deferred {
            return None;
        }
        self.desired.remove(key).map(|desired| desired.change)
    }

    pub fn next(&self, keys: &[Key], offered: impl Fn(&Key) -> bool) -> Option<Change> {
        if self
            .flights
            .values()
            .any(|flight| keys.contains(&flight.change.key))
        {
            return None;
        }
        keys.iter().find_map(|key| {
            let desired = self.desired.get(key)?;
            (offered(key) && self.value(key)? != desired.change.value)
                .then(|| desired.change.clone())
        })
    }

    pub fn pending(&self, key: &Key) -> bool {
        self.flights
            .values()
            .any(|flight| &flight.change.key == key)
    }

    pub fn start(&mut self, tag: u64, change: Change) {
        let replies = self
            .desired
            .get_mut(&change.key)
            .filter(|desired| desired.change.revision == change.revision)
            .map(|desired| mem::take(&mut desired.replies))
            .unwrap_or_default();
        self.flights.insert(tag, Flight { change, replies });
    }

    pub fn publish_options(&mut self, options: Vec<SessionConfigOption>, seq: u64) {
        if seq < self.options_seq {
            return;
        }
        self.confirmed.config_options = options;
        self.options_seq = seq;
    }

    pub fn publish_mode(&mut self, mode: String, seq: u64) {
        if seq >= self.mode_seq {
            self.confirmed.mode = Some(mode);
            self.mode_seq = seq;
        }
    }

    pub fn finish(
        &mut self,
        tag: u64,
        response: Result<Sequenced<Value>, String>,
    ) -> Option<(Change, Result<(), String>, bool)> {
        let mut flight = self.flights.remove(&tag)?;
        let (result, current) = self.confirm(&flight.change, response);
        answer(
            &mut flight.replies,
            result
                .clone()
                .map_err(|reason| format!("{NOT_SET}: {}: {reason}", flight.change.key.name())),
        );
        Some((flight.change, result, current))
    }

    pub fn confirm(
        &mut self,
        change: &Change,
        response: Result<Sequenced<Value>, String>,
    ) -> (Result<(), String>, bool) {
        let result = response.and_then(|response| {
            let held = match change.key {
                Key::Option(_) => {
                    let set =
                        serde_json::from_value::<SetSessionConfigOptionResponse>(response.value)
                            .map_err(|error| error.to_string())?;
                    let value = set
                        .config_options
                        .iter()
                        .find(|option| option.id.to_string() == change.key.name())
                        .and_then(current_value);
                    self.publish_options(set.config_options, response.seq);
                    value
                }
                Key::Mode => {
                    serde_json::from_value::<SetSessionModeResponse>(response.value)
                        .map_err(|error| error.to_string())?;
                    self.publish_mode(
                        change.value.as_str().unwrap_or_default().to_owned(),
                        response.seq,
                    );
                    Some(change.value.clone())
                }
            };
            if held.as_ref() == Some(&change.value) {
                Ok(())
            } else {
                Err(KEPT.into())
            }
        });
        let current = self.revision(&change.key) == change.revision;
        if result.is_err()
            && current
            && let Some(value) = self.value(&change.key)
        {
            self.desired.get_mut(&change.key).unwrap().change.value = value;
        }
        (result, current)
    }

    pub fn settle_held(&mut self, offered: impl Fn(&Key) -> bool) {
        for (key, desired) in &mut self.desired {
            if !offered(key) {
                answer(
                    &mut desired.replies,
                    Err(format!("{UNOFFERED}: {}", key.name())),
                );
            } else if !self
                .flights
                .values()
                .any(|flight| &flight.change.key == key)
                && held(&self.confirmed, key).as_ref() == Some(&desired.change.value)
            {
                answer(&mut desired.replies, Ok(()));
            }
        }
    }

    pub fn reset(&mut self, reason: &str) {
        for flight in self.flights.values_mut() {
            answer(&mut flight.replies, Err(reason.into()));
        }
        self.flights.clear();
        for desired in self.desired.values_mut() {
            answer(&mut desired.replies, Err(reason.into()));
        }
    }

    pub fn close(&mut self) {
        self.closed = true;
        self.reset(CLOSED);
    }
}

fn answer(replies: &mut Replies, result: Result<(), String>) {
    for reply in replies.drain(..) {
        let _ = reply.send(result.clone());
    }
}

fn held(confirmed: &Confirmed, key: &Key) -> Option<Value> {
    match key {
        Key::Mode => confirmed.mode.clone().map(Value::String),
        Key::Option(id) => confirmed
            .config_options
            .iter()
            .find(|option| option.id.to_string() == *id)
            .and_then(current_value),
    }
}

pub fn current_value(option: &SessionConfigOption) -> Option<Value> {
    serde_json::to_value(option)
        .ok()?
        .get("currentValue")
        .cloned()
}

pub fn wire_value(value: &Value) -> SessionConfigOptionValue {
    match value {
        Value::Bool(on) => SessionConfigOptionValue::boolean(*on),
        _ => SessionConfigOptionValue::value_id(value.as_str().unwrap_or_default().to_owned()),
    }
}

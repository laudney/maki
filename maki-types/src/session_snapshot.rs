//! The one shape `maki.session.read()` returns. The UI and the headless
//! drivers all serialize this struct, so a plugin sees the same keys wherever
//! it runs and a new field is one edit instead of three `json!` blobs.

use crate::session::{SessionHeader, StoredSession};
use crate::usage::TokenUsage;
use maki_storage::id::MakiId;
use maki_storage::sessions::StoredMode;
use serde::Serialize;

pub const MODE_BUILD: &str = "build";
pub const MODE_PLAN: &str = "plan";
pub const STATUS_IDLE: &str = "idle";
pub const STATUS_WORKING: &str = "working";
pub const STATUS_NEEDS_INPUT: &str = "needs_input";
/// A session no runtime here has open, read from disk.
pub const STATUS_STORED: &str = "stored";

#[derive(Serialize)]
pub struct SessionSnapshot {
    pub id: String,
    pub cwd: String,
    /// Left out when the host has no title to give. Never filled with the
    /// session id, so a plugin can tell "no title" from a real one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub model: String,
    pub mode: &'static str,
    /// Spelled like the thinking setting. Left out by hosts that do not
    /// track it (the headless drivers).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fast: Option<bool>,
    pub status: &'static str,
    /// Whether this is the session the user is looking at. Always true
    /// for the single-session headless drivers.
    pub focused: bool,
    /// Unix seconds.
    pub updated_at: u64,
    /// Omitted by hosts that hold no queue of their own. An sdk client can
    /// pipeline messages that wait outside the driver's sight, so leaving
    /// this out beats reporting a zero the plugin would trust.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue: Option<SessionQueueSnapshot>,
    /// Subagent turns included, their tokens are real spend on this session
    /// too.
    pub usage: TokenUsage,
    /// Main session only. A subagent fills its own window, not ours.
    pub context_size: u32,
    /// Left out for a stored session: the window belongs to a model that is
    /// only resolved when the session opens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    /// What the session bills, `None` on an unpriced model. No list price
    /// twin here, because `cost` is re-settled from stored usage when a
    /// session resumes and list price is not stored. Per turn list price
    /// lives on the `TurnEnd` autocmd instead.
    pub cost: Option<f64>,
}

impl SessionSnapshot {
    /// What the file says. Nothing runs it here, so no cost: that is settled
    /// against the model's rates when the session opens.
    pub fn stored(session: &StoredSession) -> Self {
        Self::unhosted(session.id, SessionHeader::of(session), STATUS_STORED)
    }

    /// A session a headless driver runs, answered from what it publishes
    /// rather than from a file it may not have written yet.
    pub fn live_headless(id: MakiId, header: SessionHeader) -> Self {
        let status = if header.working {
            STATUS_WORKING
        } else {
            STATUS_IDLE
        };
        Self::unhosted(id, header, status)
    }

    /// A session this host holds no runtime for: no window, cost or queue.
    fn unhosted(id: MakiId, header: SessionHeader, status: &'static str) -> Self {
        Self {
            id: id.to_string(),
            cwd: header.cwd,
            title: Some(header.title),
            model: header.model,
            mode: match header.mode {
                Some(StoredMode::Plan) => MODE_PLAN,
                _ => MODE_BUILD,
            },
            thinking: header.thinking.map(|thinking| thinking.to_string()),
            fast: Some(header.fast),
            status,
            focused: false,
            updated_at: header.updated_at,
            queue: None,
            usage: header.usage,
            context_size: header.context_size,
            context_window: None,
            cost: None,
        }
    }
}

#[derive(Serialize)]
pub struct SessionQueueSnapshot {
    /// Pending user text only. A queued `/compact` is housekeeping, not human
    /// input a supervisor should yield to.
    pub count: usize,
}

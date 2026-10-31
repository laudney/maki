//! A remote tab's own state, under `remote/<agent>/` in the user's state
//! dir: its input history per workspace, and its unsent draft and queued
//! messages per session. The agent owns the sessions; this is only what the
//! client typed and has not handed over.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::lock::{Lock, LockError};
use crate::paths::fnv1a_64;
use crate::sessions::StoredWriters;
use crate::{StateDir, StorageError, atomic_write};

const REMOTE_DIR: &str = "remote";
const HISTORY_PREFIX: &str = "history-";
const RECORD_EXT: &str = "json";
const LOCK_EXT: &str = "lock";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteDraft {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_draft: Option<String>,
    /// Who wrote the draft, as a local session's meta keeps it, so a line a
    /// plugin wrote does not come back as the user's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_draft_writers: Option<StoredWriters>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_messages: Vec<String>,
}

impl RemoteDraft {
    pub fn is_empty(&self) -> bool {
        self.input_draft.is_none() && self.queued_messages.is_empty()
    }
}

/// One agent's corner of the store. `agent` names the agent and how it was
/// launched; it is hashed, so any string is safe.
#[derive(Debug, Clone)]
pub struct RemoteStore {
    dir: PathBuf,
}

impl RemoteStore {
    pub fn open(state: &StateDir, agent: &str) -> Self {
        Self {
            dir: state.path().join(REMOTE_DIR).join(key(agent)),
        }
    }

    /// The input history file of a workspace, a path on the agent's side
    /// that this client never resolves.
    pub fn history_file(&self, workspace: &str) -> PathBuf {
        self.dir
            .join(format!("{HISTORY_PREFIX}{}.{RECORD_EXT}", key(workspace)))
    }

    /// The record of `session`, an opaque id from the agent, for as long as
    /// the claim lives. `None` when another client holds it.
    pub fn claim(&self, session: &str) -> Result<Option<DraftClaim>, StorageError> {
        let name = key(session);
        // A sibling that is never renamed or removed, so replacing or
        // removing the record never moves the lock to another file.
        let lock = match Lock::acquire(&self.dir.join(format!("{name}.{LOCK_EXT}"))) {
            Ok(lock) => lock,
            Err(LockError::Held { .. }) => return Ok(None),
            Err(LockError::Io { source, .. }) => return Err(source.into()),
        };
        Ok(Some(DraftClaim {
            _lock: lock,
            record: self.dir.join(format!("{name}.{RECORD_EXT}")),
        }))
    }

    /// Moves what was typed in session `from` to session `to`, which took
    /// its place. Nothing moves while another client holds either record.
    pub fn carry(&self, from: &str, to: &str) -> Result<(), StorageError> {
        let (Some(from), Some(to)) = (self.claim(from)?, self.claim(to)?) else {
            return Ok(());
        };
        let draft = from.load();
        if draft.is_empty() {
            return Ok(());
        }
        to.save(&draft)?;
        from.save(&RemoteDraft::default())
    }
}

#[derive(Debug)]
pub struct DraftClaim {
    _lock: Lock,
    record: PathBuf,
}

impl DraftClaim {
    /// An unreadable record is as good as none: it only held unsent input.
    pub fn load(&self) -> RemoteDraft {
        self.load_or(RemoteDraft::default())
    }

    pub fn load_or(&self, fallback: RemoteDraft) -> RemoteDraft {
        fs::read(&self.record)
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or(fallback)
    }

    pub fn save(&self, draft: &RemoteDraft) -> Result<(), StorageError> {
        atomic_write(&self.record, &serde_json::to_vec(draft)?)
    }
}

fn key(text: &str) -> String {
    format!("{:016x}", fnv1a_64(text.as_bytes()))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const AGENT: &str = "maki acp --yolo @ /home/user/project";
    const OTHER_AGENT: &str = "wss://agent.example";
    const SESSION: &str = "session-1";
    const OTHER_SESSION: &str = "session-2";
    const DRAFT: &str = "half a prompt";
    const QUEUED: &str = "and run the tests";

    fn store(tmp: &TempDir, agent: &str) -> RemoteStore {
        RemoteStore::open(&StateDir::from_path(tmp.path().to_path_buf()), agent)
    }

    fn draft() -> RemoteDraft {
        RemoteDraft {
            input_draft: Some(DRAFT.into()),
            input_draft_writers: Some(StoredWriters::Unknown),
            queued_messages: vec![QUEUED.into()],
        }
    }

    #[test]
    fn a_draft_round_trips_per_agent_and_session() {
        let tmp = TempDir::new().unwrap();
        let claim = store(&tmp, AGENT).claim(SESSION).unwrap().unwrap();
        claim.save(&draft()).unwrap();
        drop(claim);

        let again = store(&tmp, AGENT).claim(SESSION).unwrap().unwrap();
        assert_eq!(again.load(), draft());
        let other = store(&tmp, OTHER_AGENT).claim(SESSION).unwrap().unwrap();
        assert_eq!(other.load(), RemoteDraft::default(), "agents never share");
    }

    /// The id comes from the agent, so it must never become a path.
    #[test_case("../../escape" ; "parent")]
    #[test_case("/etc/passwd" ; "absolute")]
    #[test_case("a\\b" ; "backslash")]
    fn a_hostile_session_id_stays_in_the_store(session: &str) {
        let tmp = TempDir::new().unwrap();
        let claim = store(&tmp, AGENT).claim(session).unwrap().unwrap();
        claim.save(&draft()).unwrap();
        assert!(claim.record.starts_with(tmp.path().join(REMOTE_DIR)));
        assert_eq!(
            claim.record.parent(),
            Some(store(&tmp, AGENT).dir.as_path())
        );
    }

    #[test]
    fn clearing_a_draft_does_not_restore_an_old_session_draft() {
        let tmp = TempDir::new().unwrap();
        let claim = store(&tmp, AGENT).claim(SESSION).unwrap().unwrap();
        claim.save(&draft()).unwrap();
        claim.save(&RemoteDraft::default()).unwrap();
        assert_eq!(claim.load_or(draft()), RemoteDraft::default());
        claim.save(&RemoteDraft::default()).unwrap();
    }

    /// The lock is a sibling of the record, so a replaced or removed record
    /// keeps a second client out all the same.
    #[test]
    fn a_held_record_refuses_a_second_client() {
        let tmp = TempDir::new().unwrap();
        let first = store(&tmp, AGENT).claim(SESSION).unwrap().unwrap();
        first.save(&draft()).unwrap();
        assert!(store(&tmp, AGENT).claim(SESSION).unwrap().is_none());
        first.save(&RemoteDraft::default()).unwrap();
        assert!(store(&tmp, AGENT).claim(SESSION).unwrap().is_none());
        drop(first);
        assert!(store(&tmp, AGENT).claim(SESSION).unwrap().is_some());
    }

    #[test]
    fn a_carried_draft_moves_to_the_session_that_took_its_place() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp, AGENT);
        store
            .claim(SESSION)
            .unwrap()
            .unwrap()
            .save(&draft())
            .unwrap();

        store.carry(SESSION, OTHER_SESSION).unwrap();

        assert_eq!(store.claim(OTHER_SESSION).unwrap().unwrap().load(), draft());
        assert_eq!(
            store.claim(SESSION).unwrap().unwrap().load(),
            RemoteDraft::default()
        );
    }

    #[test]
    fn a_held_draft_is_not_carried() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp, AGENT);
        let held = store.claim(SESSION).unwrap().unwrap();
        held.save(&draft()).unwrap();

        store.carry(SESSION, OTHER_SESSION).unwrap();

        assert_eq!(held.load(), draft());
        assert_eq!(
            store.claim(OTHER_SESSION).unwrap().unwrap().load(),
            RemoteDraft::default()
        );
    }

    #[test]
    fn each_workspace_has_its_own_history() {
        let tmp = TempDir::new().unwrap();
        let store = store(&tmp, AGENT);
        assert_ne!(
            store.history_file("/agent/a"),
            store.history_file("/agent/b")
        );
    }
}

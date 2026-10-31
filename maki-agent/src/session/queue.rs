use maki_config::SessionDefaults;
use maki_storage::id::MakiId;
use maki_storage::sessions::{QueueReceipt, QueueStatus, StoredMode};
use maki_types::input::{SessionInput, merge_inputs};
use thiserror::Error;

use super::{SessionControl, SessionError};
use crate::{AgentInput, AgentMode, CancelToken, InputSource};

const UNKNOWN_ITEM: &str = "queued item not found";
const NOT_QUEUED: &str = "item has already left the queue";

#[derive(Debug, Error)]
pub enum QueueError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Invalid(&'static str),
}

fn queued(input: &SessionInput, id: MakiId) -> Result<QueueReceipt, serde_json::Error> {
    Ok(QueueReceipt {
        id,
        status: QueueStatus::Queued,
        error: None,
        input: Some(serde_json::to_value(input)?),
    })
}

impl SessionControl {
    pub fn enqueue(&self, input: &SessionInput) -> Result<QueueReceipt, QueueError> {
        self.enqueue_with_id(input, MakiId::generate())
    }

    pub fn enqueue_with_id(
        &self,
        input: &SessionInput,
        id: MakiId,
    ) -> Result<QueueReceipt, QueueError> {
        let mut store = self.lock();
        if let Some(work) = store
            .session
            .meta
            .accepted_work
            .iter()
            .find(|work| work.id == id)
        {
            return Ok(work.clone());
        }
        let work = queued(input, id)?;
        let receipt = work.clone();
        let before = store.session.clone();
        store.session.meta.accepted_work.push(work);
        store.commit(before)?;
        let _ = store.queue_ready.0.try_send(());
        Ok(receipt)
    }

    pub fn queue(&self) -> Vec<QueueReceipt> {
        self.lock().session.meta.accepted_work.clone()
    }

    pub fn cancel_queued(&self, id: MakiId) -> Result<QueueReceipt, QueueError> {
        let mut store = self.lock();
        let pos = store
            .session
            .meta
            .accepted_work
            .iter()
            .position(|work| work.id == id)
            .ok_or(QueueError::Invalid(UNKNOWN_ITEM))?;
        if store.session.meta.accepted_work[pos].status != QueueStatus::Queued {
            return Err(QueueError::Invalid(NOT_QUEUED));
        }
        let before = store.session.clone();
        let work = &mut store.session.meta.accepted_work[pos];
        work.status = QueueStatus::Cancelled;
        work.input = None;
        let receipt = work.clone();
        store.commit(before)?;
        Ok(receipt)
    }

    pub(crate) async fn queue_ready(&self) {
        let rx = self.lock().queue_ready.1.clone();
        let _ = rx.recv_async().await;
    }

    pub(crate) fn recover_queue(
        &self,
        defaults: SessionDefaults,
    ) -> Result<Vec<QueueReceipt>, QueueError> {
        let mut store = self.lock();
        let mut recovered: Vec<_> = store
            .session
            .meta
            .accepted_work
            .iter()
            .filter(|work| work.status == QueueStatus::Queued)
            .cloned()
            .collect();
        if store.session.meta.queued_messages.is_empty()
            && !store
                .session
                .meta
                .accepted_work
                .iter()
                .any(|work| work.status == QueueStatus::Running)
        {
            return Ok(recovered);
        }
        let defaults = if store.persisted.is_some() {
            SessionDefaults {
                thinking: store.session.meta.thinking,
                fast: store.session.meta.fast,
                workflow: store.session.meta.workflow,
            }
        } else {
            defaults
        };
        let mode = match store.session.meta.mode {
            Some(StoredMode::Plan) => AgentMode::Plan(match &store.session.meta.plan_path {
                Some(path) => path.into(),
                None => {
                    maki_storage::plans::new_plan_path(&store.dir).map_err(SessionError::from)?
                }
            }),
            _ => AgentMode::Build,
        };
        let before = store.session.clone();
        for work in &mut store.session.meta.accepted_work {
            if work.status == QueueStatus::Running {
                work.status = QueueStatus::Interrupted;
                recovered.push(work.clone());
            }
        }
        for text in std::mem::take(&mut store.session.meta.queued_messages) {
            let input = SessionInput::Prompt(AgentInput::from_defaults(
                text,
                mode.clone(),
                Vec::new(),
                defaults,
                InputSource::Tui,
            ));
            let work = queued(&input, MakiId::generate())?;
            recovered.push(work.clone());
            store.session.meta.accepted_work.push(work);
        }
        store.commit(before)?;
        Ok(recovered)
    }

    pub(crate) fn take_queued(
        &self,
        mode: Option<&AgentMode>,
        cancel: Option<&CancelToken>,
    ) -> Result<Option<(SessionInput, Vec<QueueReceipt>)>, QueueError> {
        let mut store = self.lock();
        if cancel.is_some_and(CancelToken::is_cancelled) {
            return Ok(None);
        }
        let mut selected = Vec::new();
        let mut inputs = Vec::new();
        let mut compact = None;
        let mut key = None;
        for (pos, work) in store.session.meta.accepted_work.iter().enumerate() {
            if work.status != QueueStatus::Queued {
                continue;
            }
            let input: SessionInput =
                serde_json::from_value(work.input.clone().ok_or(QueueError::Invalid(NOT_QUEUED))?)?;
            match input {
                SessionInput::Compact(input) => {
                    if inputs.is_empty() {
                        compact = Some(input);
                        selected.push(pos);
                    }
                    break;
                }
                SessionInput::Prompt(input) => {
                    if mode.is_some_and(|mode| *mode != input.mode || input.prompt.is_some())
                        || (!inputs.is_empty() && (key.is_none() || input.batch_key() != key))
                    {
                        break;
                    }
                    key = input.batch_key();
                    inputs.push(input);
                    selected.push(pos);
                }
            }
        }
        let input = match compact {
            Some(compact) => SessionInput::Compact(compact),
            None => match merge_inputs(inputs) {
                Some(input) => SessionInput::Prompt(input),
                None => return Ok(None),
            },
        };
        let before = store.session.clone();
        let mut receipts = Vec::new();
        for pos in selected {
            let work = &mut store.session.meta.accepted_work[pos];
            work.status = QueueStatus::Running;
            receipts.push(work.clone());
        }
        store.commit(before)?;
        Ok(Some((input, receipts)))
    }

    pub(crate) fn finish_queue(
        &self,
        status: QueueStatus,
        error: Option<String>,
    ) -> Result<Vec<QueueReceipt>, QueueError> {
        let mut store = self.lock();
        if !store
            .session
            .meta
            .accepted_work
            .iter()
            .any(|work| work.status == QueueStatus::Running)
        {
            return Ok(Vec::new());
        }
        let before = store.session.clone();
        let mut receipts = Vec::new();
        for work in &mut store.session.meta.accepted_work {
            if work.status == QueueStatus::Running
                || (status == QueueStatus::Error && work.status == QueueStatus::Queued)
            {
                work.status = if work.status == QueueStatus::Queued {
                    QueueStatus::Interrupted
                } else {
                    status.clone()
                };
                work.error = error.clone();
                if matches!(status, QueueStatus::Done | QueueStatus::Cancelled) {
                    work.input = None;
                }
                receipts.push(work.clone());
            }
        }
        store.commit(before)?;
        Ok(receipts)
    }
}

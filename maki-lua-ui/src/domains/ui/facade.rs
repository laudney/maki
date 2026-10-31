//! The facade's UI backend: each request is a `UiAction` round trip.

use std::sync::Arc;

use flume::Sender;
use futures::future::BoxFuture;
use maki_lua_core::facade::{FacadeRequest, UiOps, UiReply};
use maki_types::transcript::SessionExport;

use crate::api::util::command::{UiAction, ui_roundtrip};

struct UiFacade(Sender<UiAction>);

impl UiOps for UiFacade {
    fn request(&self, req: FacadeRequest) -> BoxFuture<'_, Result<UiReply, &'static str>> {
        Box::pin(ui_roundtrip(Some(&self.0), |reply_tx| match req {
            FacadeRequest::Session(req) => UiAction::Session { req, reply_tx },
            FacadeRequest::Model(req) => UiAction::Model { req, reply_tx },
            FacadeRequest::Plan(req) => UiAction::Plan { req, reply_tx },
            FacadeRequest::Task(req) => UiAction::Task { req, reply_tx },
        }))
    }

    fn export(
        &self,
        id: String,
    ) -> BoxFuture<'_, Result<Result<SessionExport, String>, &'static str>> {
        Box::pin(ui_roundtrip(Some(&self.0), move |reply_tx| {
            UiAction::ExportSession { id, reply_tx }
        }))
    }
}

/// The facade's UI backend over {tx}, none without a UI to send to.
pub fn ui_ops(tx: Option<Sender<UiAction>>) -> Option<Arc<dyn UiOps>> {
    tx.map(|tx| Arc::new(UiFacade(tx)) as Arc<dyn UiOps>)
}

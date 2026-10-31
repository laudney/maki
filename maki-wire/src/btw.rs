//! The btw extension: `/btw` asked of a remote agent. The request
//! `_maki/btw` is answered `{}` once the side answer ends, and each delta
//! of the answer comes before that as a `_maki/btw/chunk` notification,
//! which names the request by its JSON-RPC id.

use agent_client_protocol_schema::v1::{RequestId, SessionId};
use maki_types::btw::SideAnswer;
use serde::{Deserialize, Serialize};

pub const BTW_KEY: &str = "btw";
pub const BTW_VERSION: u64 = 1;
pub const BTW_METHOD: &str = "_maki/btw";
pub const CHUNK_METHOD: &str = "_maki/btw/chunk";

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BtwRequest {
    pub session_id: SessionId,
    pub question: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BtwChunk {
    pub request_id: RequestId,
    #[serde(flatten)]
    pub delta: SideAnswer,
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    const DELTA: &str = "it is embedded";
    const REQUEST_ID: i64 = 7;

    #[test_case(SideAnswer::Text(DELTA.into()), json!({"requestId": REQUEST_ID, "text": DELTA}) ; "text")]
    #[test_case(SideAnswer::Thinking(DELTA.into()), json!({"requestId": REQUEST_ID, "thinking": DELTA}) ; "thinking")]
    fn a_chunk_names_its_request_and_one_delta(delta: SideAnswer, wire: serde_json::Value) {
        let chunk = BtwChunk {
            request_id: RequestId::Number(REQUEST_ID),
            delta,
        };
        let sent = serde_json::to_value(&chunk).unwrap();
        assert_eq!(sent, wire);
        let read: BtwChunk = serde_json::from_value(sent).unwrap();
        assert_eq!(read.delta, chunk.delta);
    }
}

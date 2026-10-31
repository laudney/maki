//! One streamed piece of a `/btw` side answer.

use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SideAnswer {
    Text(String),
    Thinking(String),
}

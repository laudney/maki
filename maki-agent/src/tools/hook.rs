//! One interception point for tool calls, shared by every route dispatch
//! knows: registry tools, MCP tools, and host (ACP client) tools alike.
//!
//! It lives on the [`ToolRegistry`](super::registry::ToolRegistry) rather than
//! on the [`Tool`](super::Tool) trait, so a tool is hookable because dispatch
//! reached it, not because whoever wrote it remembered to ask.

use std::future::Future;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::time::timeout_at;

use super::registry::{BoxFuture, ToolReach};
use super::{CallOrigin, ToolContext, ToolOutputMode};
use crate::cancel::CancelToken;
use crate::permissions::LayerAnswer;
pub use maki_types::hook::{Authority, HookStage, OUTPUT_IS_ERROR, OUTPUT_TEXT};

pub fn authority_of(reach: Option<ToolReach>) -> Authority {
    reach.map_or(Authority::Unbounded, |reach| {
        Authority::Capability(reach.permission)
    })
}

pub struct HookCall<'a> {
    pub working_dir: &'a Path,
    pub tool: &'a str,
    pub tool_id: &'a str,
    /// The group the tool files itself under (`read`, `edit`, `execute`,
    /// ...), so a layer wrapping every tool can skip the harmless ones.
    pub tool_kind: Option<&'a str>,
    /// Only set for [`HookStage::Output`], so a layer judging a result never
    /// has to stash the call that produced it.
    pub input: Option<&'a Value>,
    pub session_id: Option<&'a str>,
    pub origin: CallOrigin,
    pub output_mode: ToolOutputMode,
    pub authority: Authority,
    /// [`HookStage::Output`] on the receipt of a call that moved to the
    /// background. Its final result fires the stage again, with this unset.
    pub receipt: bool,
    /// The call's own cancellation, so a hook that runs elsewhere (the Lua
    /// thread) dies with the call rather than outliving the reply channel.
    pub cancel: &'a CancelToken,
    /// When the chain has to be dead by, whatever it is waiting on.
    pub deadline: Instant,
}

/// How a hook answered. `Unchanged` also covers every way a hook can fail: a
/// hook is an opinion about a call, never a precondition for making it, so a
/// broken one costs exactly what no hook costs.
#[derive(Debug)]
pub enum Verdict {
    Unchanged,
    Replaced(Value),
    Denied(String),
    /// Show the call to the user whatever the rules and yolo say, with
    /// `reason` on the prompt. It can only make things stricter, so a deny
    /// rule still wins. `input` is a rewrite that came along, if any. Only
    /// [`HookStage::Input`] may ask, and everywhere else this reads as
    /// [`Verdict::Unchanged`].
    Ask {
        reason: String,
        input: Option<Value>,
    },
}

/// Why a chain ended without a verdict.
#[derive(Debug, PartialEq, Eq)]
pub enum ChainCut {
    Cancelled,
    Lapsed,
}

/// The one wait every hook chain takes. The host on the other end enforces
/// `deadline` and has the last word there: a chain it already ran deserves
/// its verdict however late it arrives. This only ends the wait on a host
/// that never answers (one that is gone or mid-reload), `grace` past the
/// deadline, or past now for a deadline that had already lapsed.
pub async fn bounded<T>(
    cancel: &CancelToken,
    deadline: Instant,
    grace: Duration,
    chain: impl Future<Output = T>,
) -> Result<T, ChainCut> {
    let latest = deadline.max(Instant::now()) + grace;
    match timeout_at(latest.into(), cancel.race(chain)).await {
        Ok(answered) => answered.map_err(|_| ChainCut::Cancelled),
        Err(_) => Err(ChainCut::Lapsed),
    }
}

pub trait ToolHook: Send + Sync + 'static {
    /// Synchronous and allocation free, because dispatch asks it for every
    /// call: a stage nobody wrapped never leaves the calling thread.
    fn wraps(&self, tool: &str, stage: HookStage) -> bool;

    /// How long past a chain's deadline this host may still answer, see
    /// [`bounded`].
    fn answer_grace(&self) -> Duration;

    /// `value` is the call's input at [`HookStage::Input`], and
    /// `{ [OUTPUT_TEXT]: string, [OUTPUT_IS_ERROR]: bool }` at
    /// [`HookStage::Output`].
    fn run<'a>(
        &'a self,
        stage: HookStage,
        value: Value,
        call: &'a HookCall<'a>,
    ) -> BoxFuture<'a, Verdict>;

    /// Asked where the permission prompt would show, with the scopes it would
    /// ask about. `None` leaves the call to the prompt.
    fn prompt<'a>(
        &'a self,
        input: &'a Value,
        scopes: &'a [String],
        call: &'a HookCall<'a>,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Option<LayerAnswer>>;
}

#[cfg(test)]
mod tests {
    use std::future::pending;

    use test_case::test_case;
    use tokio::task::yield_now;

    use super::*;

    const SHORT_GRACE: Duration = Duration::from_millis(20);
    const LONG_GRACE: Duration = Duration::from_secs(60);
    const VERDICT: &str = "late but heard";

    /// A reply crossing threads is pending on the first poll even when the
    /// host already ran the chain, so a lapsed deadline alone must not end
    /// the wait: the host decides what a late chain is worth. The answering
    /// case gets a grace it never reaches, so a slow machine cannot fail it.
    #[test_case(true,  LONG_GRACE,  Ok(VERDICT)           ; "a_late_answer_is_heard")]
    #[test_case(false, SHORT_GRACE, Err(ChainCut::Lapsed) ; "silence_ends_after_the_grace")]
    #[tokio::test]
    async fn a_lapsed_deadline_waits_on_the_host(
        answers: bool,
        grace: Duration,
        expected: Result<&str, ChainCut>,
    ) {
        let started = Instant::now();
        let chain = async {
            if !answers {
                pending::<()>().await;
            }
            yield_now().await;
            VERDICT
        };

        let got = bounded(&CancelToken::none(), started, grace, chain).await;

        assert_eq!(got, expected);
        if !answers {
            assert!(started.elapsed() >= grace, "gave up inside the grace");
        }
    }
}

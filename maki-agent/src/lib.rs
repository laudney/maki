//! Async agent loop with tools.

pub mod agent;
pub mod btw;
pub mod call_scope;
pub mod cancel;
pub mod child_guard;
pub use child_guard::ChildGuard;
pub mod file_index;
pub use file_index::{
    FILE_MATCH_CONFIG, FileIndex, FileMatch, FileQuery, FileReader, Ranked, WalkEnd,
    byte_highlights, cancel_walks, file_haystack, file_haystack_owned, file_index, file_pattern,
    invalidate_for, on_walk_end, resolved_index,
};
pub mod headless;
pub mod live_session;
pub mod mcp;
pub use mcp::config::{McpConfigError, McpConfigErrors, McpServerInfo, McpServerStatus};
pub use mcp::transport::PromptRole;
pub use mcp::{
    McpCommand, McpHandle, McpPromptArg, McpPromptInfo, McpSession, McpSnapshot, McpSnapshotReader,
    ToolDeferral,
};
pub mod inbox;
pub mod session;
pub use agent::{
    Agent, AgentParams, AgentRunParams, DisplayItem, History, HistorySnapshot, Instructions,
    LoadedInstructions, ModelSlot, RunContext, RunContextBuilder, SharedMessages, Transcript,
    UNAVAILABLE_RESULT, close_dangling_tool_calls, display_order, find_subdirectory_instructions,
    is_instruction_file, observation_detail, observation_line,
};
pub use call_scope::{AlreadyMoved, CallGuard, CallScope, SessionStub, TaskHome};
pub use cancel::{CancelHandle, CancelMap, CancelToken, CancelTrigger};
pub use inbox::SubagentInbox;
pub use live_session::{
    AgentSeed, BackgroundNotice, MailboxError, SessionMailbox, TaskLabel, receipt_tag,
    result_notice,
};
pub use maki_config::{AgentConfig, PermissionsConfig, SessionDefaults, ToolOutputLines};
pub use maki_types::diff;
pub use maki_types::input::{
    AgentInput, AgentMode, EarlierInput, InputSource, McpPromptRef, merge_inputs,
};
pub mod command;
pub mod permissions;
pub mod prompt;
pub mod template;
pub mod tools;
pub use tools::ToolFilter;
pub mod types;

pub use maki_providers::AgentError;
use maki_providers::provider::BoxFuture;
pub use maki_providers::{
    EMPTY_RESPONSE_MARKER, ImageMediaType, ImageSource, TaskEnd, ThinkingConfig,
};
pub use types::{
    AgentEvent, BufferSnapshot, CallRecord, ChildStatus, CostTally, DEFAULT_DATA_VERSION,
    DoneReason, Envelope, EventSender, EventStreamGuard, GrepFileEntry, GrepLine, GrepMatchGroup,
    InstructionBlock, NO_FILES_FOUND, PartStatus, PermissionChoice, RunLedger, RunTotals,
    SessionEndReason, SessionEvents, SharedBuf, SnapshotLine, SnapshotSpan, SpanColor, SpanStyle,
    SteerKind, SubagentInfo, TextOutput, ToolChild, ToolData, ToolDoneEvent, ToolInput, ToolOutput,
    ToolPart, ToolProgressEvent, ToolStartEvent, TurnCompleteEvent, UiWaker, error_event,
    event_stream,
};

pub enum ExtractedCommand {
    /// Every message the user queued back to back, so one turn answers them
    /// all. A source must stop there and hand anything else over on its own,
    /// since a command like `/compact` rewrites the history the later messages
    /// land in.
    Interrupt(Vec<AgentInput>),
    /// Carries the guidance typed as `/compact <instructions>`, for this one
    /// summary.
    Compact(Option<String>),
}

/// Polled once at every turn boundary. A source that waits for its answer
/// stops when `cancel` does, so a run never hangs on it. `mode` is the run's
/// mode right now, which a kept interrupt may have switched since it began.
pub trait InterruptSource: Send + Sync {
    fn poll<'a>(
        &'a self,
        cancel: &'a CancelToken,
        mode: &'a AgentMode,
    ) -> BoxFuture<'a, Option<ExtractedCommand>>;
}

#[cfg(test)]
pub(crate) fn test_block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(future)
}

//! What a tool takes and returns, as a session stores it and a display
//! path draws it.

use std::fmt::Write;
use std::sync::Arc;
use std::time::Duration;

use jiff::fmt::friendly::{Designator, Spacing, SpanPrinter};
use serde::de::Deserializer;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use strum::Display;

use crate::diff::unified_text;
use crate::message::ImageSource;
use crate::paths::relative_path;

pub const MAIN_TASK_ID: &str = "main";
pub const BASH_TOOL_NAME: &str = "bash";
pub const CODE_EXECUTION_TOOL_NAME: &str = "code_execution";
pub const EDIT_TOOL_NAME: &str = "edit";
pub const GLOB_TOOL_NAME: &str = "glob";
pub const GREP_TOOL_NAME: &str = "grep";
pub const MULTIEDIT_TOOL_NAME: &str = "multiedit";
pub const QUESTION_TOOL_NAME: &str = "question";
pub const READ_TOOL_NAME: &str = "read";
pub const TASK_TOOL_NAME: &str = "task";
pub const TODOWRITE_TOOL_NAME: &str = "todo_write";
pub const VIEW_IMAGE_TOOL_NAME: &str = "view_image";
pub const WRITE_TOOL_NAME: &str = "write";

pub const NO_FILES_FOUND: &str = "No files found";
pub const DEFAULT_DATA_VERSION: u32 = 1;
const COMPACT_DURATION_UNITS: [char; 3] = ['h', 'm', 's'];
const TIMEOUT_SUFFIX: &str = " timeout";
const COMPACT_DURATION: SpanPrinter = SpanPrinter::new()
    .designator(Designator::Compact)
    .spacing(Spacing::None);

pub use maki_storage::search::{GrepFileEntry, GrepLine, GrepMatchGroup};

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MoveOutcome {
    Moved,
    NotMovable(String),
    NoHook,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
    #[serde(default)]
    pub priority: TodoPriority,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl TodoStatus {
    pub fn marker(self) -> &'static str {
        match self {
            Self::Completed => "[✓]",
            Self::InProgress => "[•]",
            Self::Pending => "[ ]",
            Self::Cancelled => "[x]",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TodoPriority {
    High,
    #[default]
    Medium,
    Low,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ToolInput {
    Code {
        language: String,
        code: String,
    },
    /// Nothing produces this anymore (script rendering moved to Lua), but
    /// old persisted sessions still contain it and must keep loading.
    Script {
        language: String,
        code: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstructionBlock {
    pub path: String,
    pub content: String,
}

fn append_instructions(out: &mut String, blocks: &[InstructionBlock]) {
    for block in blocks {
        out.push_str("\n\n---\nInstructions from: ");
        out.push_str(&block.path);
        out.push('\n');
        out.push_str(&block.content);
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TextOutput {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<Vec<InstructionBlock>>,
    /// Structured plugin state saved with the session, so `restore` never
    /// has to re-parse its own llm output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<Value>,
    /// Deferred MCP tools this call loaded, by wire name; copied into the
    /// [`ContentBlock::ToolResult`] for providers to expand.
    ///
    /// [`ContentBlock::ToolResult`]: crate::message::ContentBlock::ToolResult
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loaded_tools: Vec<String>,
}

impl From<String> for TextOutput {
    fn from(text: String) -> Self {
        Self {
            text,
            instructions: None,
            state: None,
            loaded_tools: Vec::new(),
        }
    }
}

impl From<&str> for TextOutput {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}

impl<'de> Deserialize<'de> for TextOutput {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Legacy(String),
            Full {
                text: String,
                #[serde(default)]
                instructions: Option<Vec<InstructionBlock>>,
                #[serde(default)]
                state: Option<Value>,
                #[serde(default)]
                loaded_tools: Vec<String>,
            },
        }
        match Raw::deserialize(deserializer)? {
            Raw::Legacy(text) => Ok(text.into()),
            Raw::Full {
                text,
                instructions,
                state,
                loaded_tools,
            } => Ok(Self {
                text,
                instructions,
                state,
                loaded_tools,
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolOutput {
    Plain(TextOutput),
    Markdown(TextOutput),
    ReadCode {
        path: String,
        start_line: usize,
        lines: Vec<String>,
        #[serde(default)]
        total_lines: usize,
        #[serde(default)]
        instructions: Option<Vec<InstructionBlock>>,
    },
    ReadDir(TextOutput),
    Diff {
        path: String,
        before: String,
        after: String,
        summary: String,
    },
    TodoList(Vec<TodoItem>),
    WriteCode {
        path: String,
        byte_count: usize,
        lines: Vec<String>,
    },

    GrepResult {
        entries: Vec<GrepFileEntry>,
    },
    /// Only here so legacy sessions still deserialize. Batch is a Lua
    /// plugin now and stores plain text plus a `state` payload, so the old
    /// per-child `entries` are dropped on load: nothing can render them.
    Batch {
        text: String,
    },
    Instructions {
        blocks: Vec<InstructionBlock>,
    },
    Image {
        source: ImageSource,
        /// Caption for the tool_result block, e.g. "[image: slack.jpeg 222KB]";
        /// the pixels ride separately as a `ContentBlock::Image`.
        text: String,
    },
}

/// What a call did, for a renderer to draw. It sits beside the
/// [`ToolOutput`] the model reads and never replaces it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolData {
    pub tool: Arc<str>,
    /// The tool's own, bumped when the shape of its `state` changes.
    pub version: u32,
    /// The input the call ran with, after every input hook.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub input: Value,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub annotations: Vec<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub state: Value,
    /// The calls the handler ran through `maki.agent.call_tool`, in the
    /// order it started them. A grandchild sits in its own parent's data.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ToolChild>,
    /// An output hook changed the call's output, so its `state` was cleared
    /// and no renderer draws it: the default draws the changed output.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rewritten: bool,
}

impl ToolData {
    pub fn new(tool: Arc<str>, version: u32, input: Value, summary: String) -> Self {
        Self {
            tool,
            version,
            input,
            summary,
            annotations: Vec::new(),
            state: Value::Null,
            children: Vec::new(),
            rewritten: false,
        }
    }

    /// What an output hook that changed the call's output leaves: no state
    /// a renderer could draw over text it no longer matches.
    pub fn mark_rewritten(&mut self) {
        self.state = Value::Null;
        self.rewritten = true;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChildStatus {
    Running,
    Done,
    Error,
    /// Still running when its parent ended.
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolChild {
    /// The child's invocation index under its parent, the last part of its
    /// path, which stays when an earlier sibling has no record.
    #[serde(default)]
    pub index: u32,
    pub status: ChildStatus,
    /// The start facts while it runs, then the child's own data once it ends.
    pub data: ToolData,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Arc<ToolOutput>>,
    /// The latest usage line a child that runs a subagent reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<String>,
}

fn counted(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn lines_remaining_after(total: usize, start_line: usize, shown: usize) -> usize {
    let end = start_line.saturating_add(shown).saturating_sub(1);
    total.saturating_sub(end)
}

impl ToolOutput {
    /// Short header suffix summarizing the output, e.g. `12 lines`.
    /// The UI uses it on tool completion, and `maki.agent.call_tool` falls
    /// back to it when the tool's reply has no annotation of its own.
    pub fn annotation(&self) -> Option<String> {
        match self {
            Self::ReadCode {
                lines, total_lines, ..
            } => {
                let shown = lines.len();
                if *total_lines > shown {
                    Some(format!("{shown} of {total_lines} lines"))
                } else {
                    Some(counted(shown, "line", "lines"))
                }
            }
            Self::WriteCode { byte_count, .. } => Some(counted(*byte_count, "byte", "bytes")),
            Self::GrepResult { entries } => {
                let matches: usize = entries.iter().map(|e| e.match_count()).sum();
                Some(format!(
                    "{} in {}",
                    counted(matches, "match", "matches"),
                    counted(entries.len(), "file", "files")
                ))
            }
            Self::ReadDir(t) => Some(counted(t.text.lines().count(), "entry", "entries")),
            Self::Plain(text) | Self::Markdown(text) if !text.text.is_empty() => {
                Some(counted(text.text.lines().count(), "line", "lines"))
            }
            Self::Image { text, .. } => Some(
                text.strip_prefix("[image: ")
                    .and_then(|t| t.strip_suffix(']'))
                    .unwrap_or(text)
                    .to_string(),
            ),
            _ => None,
        }
    }

    /// Only here for old persisted sessions that still have `WriteCode`/`Diff` variants.
    /// New code should use `ToolDoneEvent::written_path` instead.
    pub fn written_path(&self) -> Option<&str> {
        match self {
            Self::WriteCode { path, .. } | Self::Diff { path, .. } => Some(path),
            _ => None,
        }
    }

    pub fn instructions(&self) -> Option<&[InstructionBlock]> {
        match self {
            Self::Plain(t) | Self::Markdown(t) | Self::ReadDir(t) => t.instructions.as_deref(),
            Self::ReadCode { instructions, .. } => instructions.as_deref(),
            _ => None,
        }
    }

    pub fn loaded_tools(&self) -> &[String] {
        match self {
            Self::Plain(t) | Self::Markdown(t) => &t.loaded_tools,
            _ => &[],
        }
    }

    pub fn owned_instructions(&self) -> Option<Vec<InstructionBlock>> {
        self.instructions()
            .filter(|b| !b.is_empty())
            .map(|b| b.to_vec())
    }

    pub fn is_markdown(&self) -> bool {
        matches!(self, Self::Markdown(_))
    }

    pub fn state(&self) -> Option<&Value> {
        match self {
            Self::Plain(t) | Self::Markdown(t) | Self::ReadDir(t) => t.state.as_ref(),
            _ => None,
        }
    }

    pub fn structured_display_text(&self) -> Option<String> {
        match self {
            Self::Diff { .. }
            | Self::ReadCode { .. }
            | Self::ReadDir(_)
            | Self::WriteCode { .. }
            | Self::GrepResult { .. }
            | Self::TodoList(_) => Some(self.as_display_text()),
            _ => None,
        }
    }

    /// The text a `HookStage::Output` hook may read and rewrite, or `None`
    /// when the output has none to lend. Only a plain-text shape with no
    /// `state` qualifies: the other variants render from their own fields, and
    /// a `state` sidecar is saved with the session and re-rendered on restore,
    /// so text a hook redacted would come back verbatim after a restart.
    ///
    /// One accessor for both directions, so a getter and a setter can never
    /// drift into letting a hook read text it cannot write back.
    pub fn filterable_text_mut(&mut self) -> Option<&mut String> {
        match self {
            Self::Plain(t) | Self::Markdown(t) | Self::ReadDir(t) if t.state.is_none() => {
                Some(&mut t.text)
            }
            _ => None,
        }
    }

    pub fn as_text(&self) -> String {
        let mut out = match self {
            Self::Diff { summary, .. } => summary.clone(),
            Self::TodoList(_) => "ok".into(),
            _ => self.as_display_text(),
        };
        if let Some(blocks) = self.instructions() {
            append_instructions(&mut out, blocks);
        }
        out
    }

    /// `None` for images and diffs, which have no slot for blocks.
    pub fn instructions_slot(&mut self) -> Option<&mut Option<Vec<InstructionBlock>>> {
        match self {
            Self::Plain(t) | Self::Markdown(t) | Self::ReadDir(t) => Some(&mut t.instructions),
            Self::ReadCode { instructions, .. } => Some(instructions),
            _ => None,
        }
    }

    pub fn as_display_text(&self) -> String {
        match self {
            Self::Plain(t) | Self::Markdown(t) | Self::ReadDir(t) => t.text.clone(),
            Self::ReadCode {
                start_line,
                lines,
                total_lines,
                ..
            } => {
                let mut out: String = lines
                    .iter()
                    .enumerate()
                    .map(|(i, line)| format!("{}: {line}", start_line + i))
                    .collect::<Vec<_>>()
                    .join("\n");
                let remaining = lines_remaining_after(*total_lines, *start_line, lines.len());
                if remaining > 0 {
                    out.push_str(&format!(
                        "\n\n...\n\nTruncated lines: {}-{}. Use offset={} to read further.",
                        start_line + lines.len(),
                        total_lines,
                        start_line + lines.len(),
                    ));
                }
                out
            }
            Self::Diff {
                path,
                before,
                after,
                summary,
            } => unified_text(before, after, summary, &relative_path(path)),
            Self::TodoList(items) => {
                if items.is_empty() {
                    return "No todos.".into();
                }
                items
                    .iter()
                    .map(|t| format!("{} ({}) {}", t.status.marker(), t.priority, t.content))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Self::WriteCode {
                path, byte_count, ..
            } => {
                let display = relative_path(path);
                format!("wrote {byte_count} bytes to {display}")
            }
            Self::GrepResult { entries } => {
                let mut out = String::new();
                for (i, entry) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push('\n');
                    }
                    out.push_str(&entry.path);
                    out.push(':');
                    let has_context = entry.groups.iter().any(|g| g.lines.len() > 1);
                    for (gi, group) in entry.groups.iter().enumerate() {
                        if gi > 0 && has_context {
                            out.push_str("\n  --");
                        }
                        for line in &group.lines {
                            let sep = if line.is_match { ":" } else { " " };
                            let _ = write!(out, "\n  {}{sep} {}", line.line_nr, line.text);
                        }
                    }
                }
                out
            }
            Self::Batch { text } | Self::Image { text, .. } => text.clone(),
            Self::Instructions { blocks } => {
                let mut out = String::new();
                append_instructions(&mut out, blocks);
                out
            }
        }
    }
}

/// Whole seconds as `1h1m1s`. Hours are the largest unit, so a day reads `24h`.
pub fn compact_duration(secs: u64) -> String {
    COMPACT_DURATION.unsigned_duration_to_string(&Duration::from_secs(secs))
}

pub fn timeout_annotation(secs: u64) -> String {
    format!("{}{TIMEOUT_SUFFIX}", compact_duration(secs))
}

pub fn is_timeout_annotation(note: &str) -> bool {
    note.strip_suffix(TIMEOUT_SUFFIX).is_some_and(|span| {
        !span.is_empty()
            && span
                .chars()
                .all(|c| c.is_ascii_digit() || COMPACT_DURATION_UNITS.contains(&c))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(ToolOutput::Plain("ok".into()),                      Some("1 line")     ; "plain_short_annotates")]
    #[test_case(ToolOutput::Plain((0..20).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n").into()), Some("20 lines") ; "plain_long_annotates")]
    #[test_case(ToolOutput::Plain(String::new().into()),             None                ; "plain_empty_no_annotation")]
    #[test_case(ToolOutput::ReadCode { path: "a.rs".into(), start_line: 1, lines: vec!["x".into(); 5], total_lines: 5, instructions: None }, Some("5 lines") ; "read_code_full_file")]
    #[test_case(ToolOutput::ReadCode { path: "a.rs".into(), start_line: 10, lines: vec!["x".into(); 5], total_lines: 100, instructions: None }, Some("5 of 100 lines") ; "read_code_partial")]
    #[test_case(ToolOutput::WriteCode { path: "a.rs".into(), byte_count: 99, lines: vec![] }, Some("99 bytes") ; "write_code_bytes")]
    #[test_case(ToolOutput::GrepResult { entries: vec![GrepFileEntry { path: "a.rs".into(), groups: vec![GrepMatchGroup::single(1, "hit")] }] }, Some("1 match in 1 file") ; "grep_file_count")]
    #[test_case(ToolOutput::Diff { path: "a.rs".into(), before: String::new(), after: String::new(), summary: "ok".into() }, None ; "diff_no_annotation")]
    fn annotation_cases(output: ToolOutput, expected: Option<&str>) {
        assert_eq!(output.annotation().as_deref(), expected);
    }

    const FILTERABLE_TEXT: &str = "body";

    fn text_with_state() -> TextOutput {
        TextOutput {
            text: FILTERABLE_TEXT.into(),
            instructions: None,
            state: Some(serde_json::json!({ "text": FILTERABLE_TEXT })),
            loaded_tools: Vec::new(),
        }
    }

    #[test_case(ToolOutput::Plain(FILTERABLE_TEXT.into()),   Some(FILTERABLE_TEXT) ; "plain_without_state_lends_text")]
    #[test_case(ToolOutput::Plain(text_with_state()),        None                  ; "plain_with_state_withholds_text")]
    #[test_case(ToolOutput::Markdown(text_with_state()),     None                  ; "markdown_with_state_withholds_text")]
    #[test_case(ToolOutput::ReadDir(FILTERABLE_TEXT.into()), Some(FILTERABLE_TEXT) ; "read_dir_without_state_lends_text")]
    #[test_case(ToolOutput::Diff { path: "a.rs".into(), before: String::new(), after: String::new(), summary: FILTERABLE_TEXT.into() }, None ; "diff_withholds_text")]
    fn filterable_text_needs_text_to_be_the_sole_representation(
        mut output: ToolOutput,
        expected: Option<&str>,
    ) {
        assert_eq!(output.filterable_text_mut().map(|t| t.as_str()), expected);
    }

    #[test]
    fn legacy_batch_output_with_entries_still_deserializes() {
        let json = r#"{"Batch":{"entries":[{"tool":"read","summary":"s","status":"Success","input":null,"output":null}],"text":"stored"}}"#;
        let out: ToolOutput =
            serde_json::from_str(json).expect("old persisted session JSON must load");
        assert_eq!(out.as_text(), "stored");
    }

    #[test]
    fn as_display_text_diff_renders_unified_text() {
        let output = ToolOutput::Diff {
            path: "src/main.rs".into(),
            before: "keep\nold\n".into(),
            after: "keep\nnew\n".into(),
            summary: "Updated value".into(),
        };
        let display = output.as_display_text();
        assert!(display.starts_with("Updated value"));
        assert!(display.contains("--- src/main.rs"));
        assert!(display.contains("+++ src/main.rs"));
        assert!(display.contains("  keep"));
        assert!(display.contains("- old"));
        assert!(display.contains("+ new"));
        assert_eq!(output.as_text(), "Updated value");
    }

    #[test]
    fn as_text_grep_result_multi_file() {
        let output = ToolOutput::GrepResult {
            entries: vec![
                GrepFileEntry {
                    path: "src/a.rs".into(),
                    groups: vec![
                        GrepMatchGroup::single(3, "fn foo()"),
                        GrepMatchGroup::single(10, "fn bar()"),
                    ],
                },
                GrepFileEntry {
                    path: "src/b.rs".into(),
                    groups: vec![GrepMatchGroup::single(1, "use crate")],
                },
            ],
        };
        let text = output.as_text();
        assert!(text.contains("src/a.rs"));
        assert!(text.contains("3: fn foo()"));
        assert!(text.contains("10: fn bar()"));
        assert!(text.contains("src/b.rs"));
        assert!(text.contains("1: use crate"));
    }

    #[test]
    fn as_text_grep_result_with_context() {
        let output = ToolOutput::GrepResult {
            entries: vec![GrepFileEntry {
                path: "src/a.rs".into(),
                groups: vec![
                    GrepMatchGroup {
                        lines: vec![
                            GrepLine::context(2, "let x = 1;"),
                            GrepLine::matched(3, "fn foo()"),
                            GrepLine::context(4, "let y = 2;"),
                        ],
                    },
                    GrepMatchGroup::single(20, "fn bar()"),
                ],
            }],
        };
        let text = output.as_text();
        assert!(text.contains("2  let x = 1;"), "context before: {text}");
        assert!(text.contains("3: fn foo()"), "match line: {text}");
        assert!(text.contains("4  let y = 2;"), "context after: {text}");
        assert!(text.contains("--"), "group separator: {text}");
        assert!(text.contains("20: fn bar()"), "second group: {text}");
    }

    #[test_case(ToolOutput::WriteCode { path: "src/lib.rs".into(), byte_count: 10, lines: vec![] }, Some("src/lib.rs") ; "write_code")]
    #[test_case(ToolOutput::Diff { path: "src/lib.rs".into(), before: String::new(), after: String::new(), summary: String::new() }, Some("src/lib.rs") ; "diff")]
    #[test_case(ToolOutput::Plain("ok".into()), None ; "non_write_variant")]
    fn output_written_path(output: ToolOutput, expected: Option<&str>) {
        assert_eq!(output.written_path(), expected);
    }

    #[test_case(
        10,
        vec!["fn foo()".into(), "fn bar()".into()],
        Some(vec![InstructionBlock { path: "AGENTS.md".into(), content: "do stuff".into() }]),
        "10: fn foo()\n11: fn bar()\n\n...\n\nTruncated lines: 12-100. Use offset=12 to read further."
        ; "with_instructions"
    )]
    #[test_case(
        1,
        vec!["line1".into()],
        None,
        "1: line1\n\n...\n\nTruncated lines: 2-100. Use offset=2 to read further."
        ; "without_instructions"
    )]
    fn read_code_display_text(
        start_line: usize,
        lines: Vec<String>,
        instructions: Option<Vec<InstructionBlock>>,
        expected: &str,
    ) {
        let output = ToolOutput::ReadCode {
            path: "a.rs".into(),
            start_line,
            lines,
            total_lines: 100,
            instructions,
        };
        assert_eq!(output.as_display_text(), expected);
    }

    #[test]
    fn read_code_as_text_includes_instructions() {
        let output = ToolOutput::ReadCode {
            path: "a.rs".into(),
            start_line: 1,
            lines: vec!["fn main()".into()],
            total_lines: 1,
            instructions: Some(vec![InstructionBlock {
                path: "AGENTS.md".into(),
                content: "do stuff".into(),
            }]),
        };
        let text = output.as_text();
        assert!(text.contains("1: fn main()"));
        assert!(text.contains("Instructions from: AGENTS.md"));
        assert!(text.contains("do stuff"));
    }

    #[test]
    fn read_code_backward_compat_deserialization() {
        let json = r#"{"ReadCode":{"path":"a.rs","start_line":1,"lines":["x"]}}"#;
        let output: ToolOutput = serde_json::from_str(json).unwrap();
        match output {
            ToolOutput::ReadCode {
                total_lines,
                instructions,
                ..
            } => {
                assert_eq!(total_lines, 0);
                assert!(instructions.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test_case(100, 10, 2, 89 ; "middle_of_file")]
    #[test_case(100, 1, 1, 99  ; "first_line_only")]
    #[test_case(5, 1, 5, 0     ; "all_lines_shown")]
    #[test_case(5, 1, 2, 3     ; "partial_from_start")]
    #[test_case(5, 3, 3, 0     ; "partial_to_end")]
    #[test_case(0, 1, 1, 0     ; "backward_compat_total_zero")]
    #[test_case(0, 1, 0, 0     ; "empty_lines_total_zero")]
    #[test_case(10, 10, 1, 0   ; "last_line")]
    fn lines_remaining(total: usize, start: usize, shown: usize, expected: usize) {
        assert_eq!(lines_remaining_after(total, start, shown), expected);
    }

    #[test]
    fn text_output_serde_legacy_bare_string() {
        const MSG: &str = "old sessions store Plain as a bare string";
        let json = r#"{"Plain":"hello world"}"#;
        let output: ToolOutput = serde_json::from_str(json).unwrap();
        match &output {
            ToolOutput::Plain(t) => {
                assert_eq!(t.text, "hello world", "{MSG}");
                assert!(t.instructions.is_none(), "{MSG}");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn text_output_serde_full_roundtrip() {
        const MSG: &str = "new format with instructions must roundtrip";
        let blocks = vec![InstructionBlock {
            path: "AGENTS.md".into(),
            content: "be nice".into(),
        }];
        let output = ToolOutput::Plain(TextOutput {
            text: "file contents".into(),
            instructions: Some(blocks),
            state: None,
            loaded_tools: Vec::new(),
        });
        let json = serde_json::to_string(&output).unwrap();
        let parsed: ToolOutput = serde_json::from_str(&json).unwrap();
        match &parsed {
            ToolOutput::Plain(t) => {
                assert_eq!(t.text, "file contents", "{MSG}");
                let inst = t.instructions.as_ref().expect("instructions missing");
                assert_eq!(inst.len(), 1, "{MSG}");
                assert_eq!(inst[0].path, "AGENTS.md", "{MSG}");
                assert_eq!(inst[0].content, "be nice", "{MSG}");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn plain_with_instructions_as_text_includes_instructions() {
        const INCLUDES_MSG: &str = "as_text must include instructions";
        const EXCLUDES_MSG: &str = "as_display_text must exclude instructions";
        let output = ToolOutput::Plain(TextOutput {
            text: "fn main()".into(),
            instructions: Some(vec![InstructionBlock {
                path: "AGENTS.md".into(),
                content: "do stuff".into(),
            }]),
            state: None,
            loaded_tools: Vec::new(),
        });
        let text = output.as_text();
        assert!(text.contains("fn main()"), "{INCLUDES_MSG}");
        assert!(
            text.contains("Instructions from: AGENTS.md"),
            "{INCLUDES_MSG}"
        );
        assert!(text.contains("do stuff"), "{INCLUDES_MSG}");

        let display = output.as_display_text();
        assert!(display.contains("fn main()"), "{EXCLUDES_MSG}");
        assert!(!display.contains("Instructions from:"), "{EXCLUDES_MSG}");
    }

    #[test_case(0,     "0s"     ; "zero")]
    #[test_case(3661,  "1h1m1s" ; "all_units")]
    #[test_case(90000, "25h"    ; "days_stay_in_hours")]
    fn compact_duration_cases(secs: u64, expected: &str) {
        assert_eq!(compact_duration(secs), expected);
    }

    const CHILD_TOOL: &str = "kid";

    /// Data saved before calls had children still loads, and a call without
    /// children saves as it did.
    #[test]
    fn data_without_children_keeps_its_shape() {
        let saved = serde_json::json!({ "tool": CHILD_TOOL, "version": DEFAULT_DATA_VERSION });
        let data: ToolData = serde_json::from_value(saved.clone()).unwrap();
        assert!(data.children.is_empty());
        assert_eq!(serde_json::to_value(&data).unwrap(), saved);
    }

    #[test_case(ChildStatus::Running,   "running"   ; "running")]
    #[test_case(ChildStatus::Done,      "done"      ; "done")]
    #[test_case(ChildStatus::Error,     "error"     ; "error")]
    #[test_case(ChildStatus::Cancelled, "cancelled" ; "cancelled")]
    fn child_round_trips_with_a_lowercase_status(status: ChildStatus, expected: &str) {
        let child = ToolChild {
            index: 0,
            status,
            data: ToolData::new(
                Arc::from(CHILD_TOOL),
                DEFAULT_DATA_VERSION,
                Value::Null,
                String::new(),
            ),
            output: None,
            usage: None,
        };
        let parent = ToolData {
            children: vec![child],
            ..ToolData::new(
                Arc::from(CHILD_TOOL),
                DEFAULT_DATA_VERSION,
                Value::Null,
                String::new(),
            )
        };
        let saved = serde_json::to_value(&parent).unwrap();
        assert_eq!(saved["children"][0]["status"], expected);
        let loaded: ToolData = serde_json::from_value(saved).unwrap();
        assert_eq!(loaded.children[0].status, status);
    }

    #[test]
    fn a_rewritten_call_round_trips_without_its_state() {
        let mut data = ToolData::new(
            Arc::from(CHILD_TOOL),
            DEFAULT_DATA_VERSION,
            Value::Null,
            String::new(),
        );
        data.state = serde_json::json!({ "lines": 3 });
        data.mark_rewritten();
        let saved = serde_json::to_value(&data).unwrap();
        assert_eq!(
            saved,
            serde_json::json!({ "tool": CHILD_TOOL, "version": DEFAULT_DATA_VERSION, "rewritten": true })
        );
        let loaded: ToolData = serde_json::from_value(saved).unwrap();
        assert!(loaded.rewritten);
    }
}

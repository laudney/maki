use std::collections::VecDeque;
use std::io::BufRead;
use std::sync::{Arc, Mutex};

use maki_config::{DEFAULT_MAX_OUTPUT_BYTES, DEFAULT_MAX_OUTPUT_LINES};
use mlua::{
    FromLuaMulti, IntoLuaMulti, Lua, LuaString, ObjectLike, Result as LuaResult, Table, UserData,
    UserDataMethods, Value,
};

use crate::api::util::convert::TableExt;

pub(crate) const DEFAULT_MAX_LINE_BYTES: usize = 500;
const UTF8_MAX_BYTES: usize = 4;
const PREVIEW_TRUNCATION: &str = "…";

#[derive(Default)]
struct Limits {
    lines: Option<usize>,
    bytes: Option<usize>,
}

fn prefix(text: &str, bytes: usize) -> &str {
    &text[..text.floor_char_boundary(bytes.min(text.len()))]
}

fn truncate(text: &str, limits: &Limits, total: usize) -> String {
    let max_lines = limits.lines.unwrap_or(usize::MAX);
    let max_bytes = limits.bytes.unwrap_or(usize::MAX);
    let kept = if total <= max_bytes && text.split('\n').count() <= max_lines {
        text
    } else {
        let mut used = 0usize;
        let mut end = 0;
        for (index, line) in text.split('\n').take(max_lines).enumerate() {
            let next = used.saturating_add(line.len()).saturating_add(1);
            if next > max_bytes {
                if index == 0 {
                    end = prefix(line, max_bytes).len();
                }
                break;
            }
            end += usize::from(index > 0) + line.len();
            used = next;
        }
        &text[..end]
    };
    with_notice(kept, total)
}

fn cut_middle(text: &str, lines: Option<usize>, bytes: Option<usize>) -> String {
    let capture = OutputCapture::new_middle(Limits { lines, bytes });
    for line in text.split('\n') {
        capture.append(line, line.len());
    }
    capture.result()
}

fn with_notice(kept: &str, total: usize) -> String {
    if kept.len() < total {
        format!("{kept}\n\n[truncated {} bytes]", total - kept.len())
    } else {
        kept.to_owned()
    }
}

fn call_method<A: IntoLuaMulti, R: FromLuaMulti>(ctx: &Value, name: &str, args: A) -> LuaResult<R> {
    match ctx {
        Value::Table(ctx) => ctx.call_method(name, args),
        Value::UserData(ctx) => ctx.call_method(name, args),
        _ => Err(mlua::Error::runtime("output limits require a tool context")),
    }
}

fn resolve(opts: &Table, ctx: &Value) -> LuaResult<Limits> {
    if call_method::<_, String>(ctx, "output_mode", ())? == "full" {
        return Ok(Limits::default());
    }
    Ok(Limits {
        lines: Some(match opts.opt::<usize>("max_output_lines")? {
            Some(value) => value,
            None => call_method(
                ctx,
                "config",
                ("max_output_lines", DEFAULT_MAX_OUTPUT_LINES),
            )?,
        }),
        bytes: Some(match opts.opt::<usize>("max_output_bytes")? {
            Some(value) => value,
            None => call_method(
                ctx,
                "config",
                ("max_output_bytes", DEFAULT_MAX_OUTPUT_BYTES),
            )?,
        }),
    })
}

struct Captured {
    limits: Limits,
    middle: bool,
    lines: VecDeque<String>,
    retained: usize,
    tail_lines: VecDeque<String>,
    tail_retained: usize,
    truncated: bool,
    total: usize,
    line_count: usize,
}

impl Captured {
    fn capacity(&self) -> usize {
        self.limits
            .bytes
            .map_or(usize::MAX, |n| n.saturating_add(UTF8_MAX_BYTES))
    }

    fn append(&mut self, text: &str, bytes: usize) {
        self.total = self
            .total
            .saturating_add(bytes)
            .saturating_add(usize::from(self.line_count > 0));
        self.line_count = self.line_count.saturating_add(1);
        if self.middle {
            self.append_middle(text);
            return;
        }
        let capacity = self.capacity();
        let max_lines = self.limits.lines.unwrap_or(usize::MAX).min(
            self.limits
                .bytes
                .map_or(usize::MAX, |bytes| bytes.saturating_add(1)),
        );
        if self.lines.len() >= max_lines || self.retained >= capacity {
            return;
        }
        let available =
            capacity.saturating_sub(self.retained + usize::from(!self.lines.is_empty()));
        let text = prefix(text, available).to_owned();
        self.retained += text.len();
        self.lines.push_back(text);
    }

    fn append_middle(&mut self, text: &str) {
        let max_lines = self.limits.lines.unwrap_or(usize::MAX);
        let max_bytes = self.limits.bytes.unwrap_or(usize::MAX);
        if self.truncated {
            self.append_middle_tail(text, max_lines, max_bytes);
            return;
        }
        self.lines.push_back(text.to_owned());
        self.retained = self.retained.saturating_add(text.len());
        if self.line_count <= max_lines && self.total <= max_bytes {
            return;
        }
        self.start_truncation(max_lines, max_bytes);
    }

    fn start_truncation(&mut self, max_lines: usize, max_bytes: usize) {
        self.truncated = true;
        let head_line_limit = max_lines.div_ceil(2);
        let head_byte_limit = max_bytes.div_ceil(2);
        let tail_line_limit = max_lines.saturating_sub(head_line_limit);
        let tail_byte_limit = max_bytes.saturating_sub(head_byte_limit);

        let mut head = VecDeque::new();
        let mut head_retained = 0usize;
        let mut used = 0usize;
        let mut head_end = 0;
        while head_end < head_line_limit && head_end < self.lines.len() {
            let text = &self.lines[head_end];
            let next = used.saturating_add(text.len()).saturating_add(1);
            if next > head_byte_limit {
                break;
            }
            head_retained = head_retained.saturating_add(text.len());
            head.push_back(text.clone());
            head_end += 1;
            used = next;
        }
        if head_end == 0
            && let Some(line) = self.lines.front()
        {
            let kept = prefix(line, head_byte_limit);
            head_retained = kept.len();
            head.push_back(kept.to_owned());
            head_end = 1;
        }

        let mut tail = VecDeque::new();
        let mut tail_retained = 0usize;
        let mut tail_used = 0usize;
        let mut tail_start = self.lines.len();
        while tail_start > head_end && self.lines.len().saturating_sub(tail_start) < tail_line_limit
        {
            let text = &self.lines[tail_start - 1];
            let next = tail_used.saturating_add(text.len()).saturating_add(1);
            if next > tail_byte_limit {
                break;
            }
            tail_retained = tail_retained.saturating_add(text.len());
            tail.push_front(text.clone());
            tail_start -= 1;
            tail_used = next;
        }

        self.lines = head;
        self.retained = head_retained;
        self.tail_lines = tail;
        self.tail_retained = tail_retained;
    }

    fn append_middle_tail(&mut self, text: &str, max_lines: usize, max_bytes: usize) {
        let head_line_limit = max_lines.div_ceil(2);
        let head_byte_limit = max_bytes.div_ceil(2);
        let tail_line_limit = max_lines.saturating_sub(head_line_limit);
        let tail_byte_limit = max_bytes.saturating_sub(head_byte_limit);
        if tail_line_limit == 0 {
            return;
        }
        while self.tail_lines.len() >= tail_line_limit {
            if let Some(line) = self.tail_lines.pop_front() {
                self.tail_retained = self.tail_retained.saturating_sub(line.len());
            }
        }
        loop {
            let used = self.tail_retained.saturating_add(self.tail_lines.len());
            if self.tail_lines.is_empty()
                || used.saturating_add(text.len()).saturating_add(1) <= tail_byte_limit
            {
                break;
            }
            if let Some(line) = self.tail_lines.pop_front() {
                self.tail_retained = self.tail_retained.saturating_sub(line.len());
            }
        }
        let used = self.tail_retained.saturating_add(self.tail_lines.len());
        if used.saturating_add(text.len()).saturating_add(1) > tail_byte_limit
            && self.tail_lines.is_empty()
        {
            return;
        }
        self.tail_retained = self.tail_retained.saturating_add(text.len());
        self.tail_lines.push_back(text.to_owned());
    }

    fn result_middle(&self) -> String {
        let head = self
            .lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        if !self.truncated {
            return head;
        }
        let tail = self
            .tail_lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        let kept = self.lines.len().saturating_add(self.tail_lines.len());
        let dropped_lines = self.line_count.saturating_sub(kept);
        let dropped_bytes = self
            .total
            .saturating_sub(head.len())
            .saturating_sub(tail.len());
        let marker = format!("[truncated {dropped_lines} lines, {dropped_bytes} bytes]");
        if self.tail_lines.is_empty() {
            format!("{head}\n\n{marker}")
        } else {
            format!("{head}\n\n{marker}\n\n{tail}")
        }
    }

    fn result(&self) -> String {
        if self.middle {
            return self.result_middle();
        }
        let text = self
            .lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        truncate(&text, &self.limits, self.total)
    }
}

#[derive(Clone)]
pub(crate) struct OutputCapture(Arc<Mutex<Captured>>);

impl OutputCapture {
    fn new(limits: Limits) -> Self {
        Self::with_mode(limits, false)
    }

    fn new_middle(limits: Limits) -> Self {
        Self::with_mode(limits, true)
    }

    fn with_mode(limits: Limits, middle: bool) -> Self {
        Self(Arc::new(Mutex::new(Captured {
            limits,
            middle,
            lines: VecDeque::new(),
            retained: 0,
            tail_lines: VecDeque::new(),
            tail_retained: 0,
            truncated: false,
            total: 0,
            line_count: 0,
        })))
    }

    pub(crate) fn read_lines(
        &self,
        mut reader: impl BufRead,
        mut preview: impl FnMut(String) -> bool,
    ) {
        let mut line = Vec::new();
        let mut bytes = 0usize;
        let mut last = None;
        let limit = self.line_limit();
        while let Ok(chunk) = reader.fill_buf() {
            let newline = chunk.iter().position(|byte| *byte == b'\n');
            let end = newline.unwrap_or(chunk.len());
            let take = end.min(limit.saturating_sub(line.len()));
            line.extend_from_slice(&chunk[..take]);
            bytes = bytes.saturating_add(end);
            last = chunk[..end].last().copied().or(last);
            let eof = chunk.is_empty();
            reader.consume(end + usize::from(newline.is_some()));
            if newline.is_none() && !eof {
                continue;
            }
            if eof && bytes == 0 {
                break;
            }
            let text_bytes =
                bytes.saturating_sub(usize::from(newline.is_some() && last == Some(b'\r')));
            line.truncate(text_bytes);
            let text = String::from_utf8_lossy(&line);
            self.append(&text, text_bytes);
            let preview_end = text.floor_char_boundary(DEFAULT_MAX_LINE_BYTES.min(text.len()));
            let mut shown = text[..preview_end].to_owned();
            if text_bytes > preview_end {
                shown.push_str(PREVIEW_TRUNCATION);
            }
            if !preview(shown) {
                break;
            }
            line.clear();
            bytes = 0;
            last = None;
            if eof {
                break;
            }
        }
    }

    fn line_limit(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).capacity()
    }

    fn append(&self, text: &str, bytes: usize) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append(text, bytes);
    }

    fn result(&self) -> String {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).result()
    }
}

impl UserData for OutputCapture {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("append", |_, this, text: LuaString| {
            let text = text.to_str()?;
            this.append(&text, text.len());
            Ok(())
        });
        methods.add_method("result", |_, this, ()| Ok(this.result()));
    }
}

pub(crate) fn module(lua: &Lua, name: &str) -> Option<LuaResult<Value>> {
    match name {
        "maki.truncate" => Some(
            lua.create_function(
                |_, (text, lines, bytes): (LuaString, Option<usize>, Option<usize>)| {
                    let text = text.to_str()?;
                    Ok(truncate(&text, &Limits { lines, bytes }, text.len()))
                },
            )
            .map(Value::Function),
        ),
        "maki.output_limits" => Some(limits_module(lua).map(Value::Table)),
        _ => None,
    }
}

fn limits_module(lua: &Lua) -> LuaResult<Table> {
    let module = lua.create_table()?;
    let specs = lua.create_table()?;
    for name in ["max_output_lines", "max_output_bytes"] {
        let spec = lua.create_table()?;
        spec.set("type", "integer")?;
        spec.set("desc", format!("Override `agent.{name}` for this tool."))?;
        specs.set(name, spec)?;
    }
    module.set("DEFAULT_MAX_LINE_BYTES", DEFAULT_MAX_LINE_BYTES)?;
    module.set("specs", specs.clone())?;
    module.set(
        "extend",
        lua.create_function(move |_, spec: Table| {
            for pair in specs.pairs::<Value, Value>() {
                let (key, value) = pair?;
                spec.set(key, value)?;
            }
            Ok(spec)
        })?,
    )?;
    module.set(
        "resolve",
        lua.create_function(|_, (opts, ctx): (Table, Value)| {
            let limits = resolve(&opts, &ctx)?;
            Ok((limits.lines, limits.bytes))
        })?,
    )?;
    module.set(
        "line_bytes",
        lua.create_function(|_, (opts, ctx): (Table, Value)| {
            if call_method::<_, String>(&ctx, "output_mode", ())? == "full" {
                Ok(Some(0))
            } else {
                opts.opt::<usize>("max_line_bytes")
            }
        })?,
    )?;
    module.set(
        "cut_middle",
        lua.create_function(
            |_, (text, lines, bytes): (LuaString, Option<usize>, Option<usize>)| {
                Ok(cut_middle(&text.to_str()?, lines, bytes))
            },
        )?,
    )?;
    module.set(
        "collect_middle",
        lua.create_function(|_, (opts, ctx): (Table, Value)| {
            Ok(OutputCapture::new_middle(resolve(&opts, &ctx)?))
        })?,
    )?;
    module.set(
        "collect",
        lua.create_function(|_, (opts, ctx): (Table, Value)| {
            Ok(OutputCapture::new(resolve(&opts, &ctx)?))
        })?,
    )?;
    Ok(module)
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Cursor};
    use std::thread;

    use mlua::{Function, Lua, Table, Value};
    use test_case::test_case;

    use super::{
        DEFAULT_MAX_LINE_BYTES, Limits, OutputCapture, PREVIEW_TRUNCATION, cut_middle, module,
        truncate,
    };

    const TRUNCATED: &str = "[truncated";
    const FIRST_LINE: &str = "first";

    #[test_case("hello", 100, 1000, "hello" ; "within_limits")]
    #[test_case("aaa\nbbb\nccc\nddd", 2, 1000, "aaa\nbbb\n\n[truncated 8 bytes]" ; "line_limit")]
    #[test_case("éééééééééé", 1000, 5, "éé\n\n[truncated 16 bytes]" ; "utf8_prefix")]
    #[test_case("short\nxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\nlast", 1000, 20, "short\n\n[truncated 106 bytes]" ; "drop_later_oversized_line")]
    #[test_case("a\nb", 2, 3, "a\nb" ; "exact_bytes")]
    #[test_case("", 1, 1, "" ; "empty")]
    fn truncation(text: &str, lines: usize, bytes: usize, expected: &str) {
        let limits = Limits {
            lines: Some(lines),
            bytes: Some(bytes),
        };
        assert_eq!(truncate(text, &limits, text.len()), expected);
    }

    #[test_case("a\nb\nc", Some(3), Some(1000), "a\nb\nc" ; "within_limits")]
    #[test_case("a\nb\nc", None, None, "a\nb\nc" ; "unlimited")]
    #[test_case("", Some(1), Some(1), "" ; "empty")]
    #[test_case("a\nb\nc\nd\ne\nf", Some(4), Some(1000), "a\nb\n\n[truncated 2 lines, 5 bytes]\n\ne\nf" ; "line_limit_even")]
    #[test_case("a\nb\nc\nd\ne", Some(3), Some(1000), "a\nb\n\n[truncated 2 lines, 5 bytes]\n\ne" ; "line_limit_odd")]
    #[test_case("aaaa\nbbbb\ncccc\ndddd\neeee", Some(1000), Some(10), "aaaa\n\n[truncated 3 lines, 16 bytes]\n\neeee" ; "byte_limit")]
    #[test_case("éééééééééé", Some(1000), Some(6), "é\n\n[truncated 0 lines, 18 bytes]" ; "oversized_first_line")]
    #[test_case("xxxxxxxxxxxxxxxxxxxx\nok", Some(1000), Some(10), "xxxxx\n\n[truncated 0 lines, 16 bytes]\n\nok" ; "oversized_first_line_with_tail")]
    fn middle_truncation(text: &str, lines: Option<usize>, bytes: Option<usize>, expected: &str) {
        assert_eq!(cut_middle(text, lines, bytes), expected);
    }

    #[test_case("123456", 8, 10, "123456" ; "within_total_budget")]
    #[test_case("a\n123456789\nb", 6, 20, "a\n123456789\nb" ; "no_reordering_within_budget")]
    #[test_case("\na", 2, 100, "\na" ; "leading_empty_line")]
    #[test_case("\n", 2, 100, "\n" ; "empty_lines")]
    fn middle_collection_preserves_text_within_total_limits(
        text: &str,
        lines: usize,
        bytes: usize,
        expected: &str,
    ) {
        let capture = OutputCapture::new_middle(Limits {
            lines: Some(lines),
            bytes: Some(bytes),
        });
        for line in text.split('\n') {
            capture.append(line, line.len());
        }
        assert_eq!(capture.result(), expected);
    }

    #[test]
    fn middle_collection_populates_both_ends() {
        let capture = OutputCapture::new_middle(Limits {
            lines: Some(8),
            bytes: Some(64),
        });
        for i in 0..100 {
            let line = format!("line-{i:03}");
            capture.append(&line, line.len());
        }
        let result = capture.result();
        assert!(result.starts_with("line-000"));
        assert!(result.ends_with("line-099"));
        assert!(result.contains(TRUNCATED));
    }

    #[test]
    fn middle_storage_is_bounded() {
        const BYTES: usize = 1024;
        const LINES: usize = 8;
        let capture = OutputCapture::new_middle(Limits {
            lines: Some(LINES),
            bytes: Some(BYTES),
        });
        for _ in 0..10000 {
            capture.append(&"x".repeat(BYTES), BYTES);
        }
        let state = capture.0.lock().unwrap();
        assert!(state.retained <= state.capacity());
        assert!(state.tail_retained <= state.capacity());
        assert!(state.lines.len() <= LINES.div_ceil(2));
        assert!(state.tail_lines.len() <= LINES / 2);
        assert!(state.result().contains(TRUNCATED));
    }

    #[test]
    fn retained_storage_is_bounded() {
        const BYTES: usize = 1024;
        const LINES: usize = 8;
        let capture = OutputCapture::new(Limits {
            lines: Some(LINES),
            bytes: Some(BYTES),
        });
        for _ in 0..10000 {
            capture.append(&"x".repeat(BYTES), BYTES);
        }
        let state = capture.0.lock().unwrap();
        assert!(state.retained <= state.capacity());
        assert!(state.lines.len() <= LINES);
        assert!(state.result().contains(TRUNCATED));
    }

    #[test]
    fn empty_lines_cannot_bypass_byte_storage_limit() {
        const BYTES: usize = 16;
        let capture = OutputCapture::new(Limits {
            lines: None,
            bytes: Some(BYTES),
        });
        for _ in 0..10000 {
            capture.append("", 0);
        }
        let state = capture.0.lock().unwrap();
        assert!(state.lines.len() <= BYTES + 1);
        assert!(state.result().len() <= BYTES || state.result().contains(TRUNCATED));
    }

    #[test_case("a\nb\n", "a\nb" ; "line_feed")]
    #[test_case("a\r\nb\r\n", "a\nb" ; "carriage_return_line_feed")]
    #[test_case("a\rb\r", "a\rb\r" ; "literal_carriage_return")]
    #[test_case("a\n\n", "a\n" ; "empty_line")]
    #[test_case("éé\n世界", "éé\n世界" ; "split_utf8")]
    fn pipe_reading_preserves_line_semantics(input: &str, expected: &str) {
        let capture = OutputCapture::new(Limits::default());
        capture.read_lines(BufReader::with_capacity(1, Cursor::new(input)), |_| true);
        assert_eq!(capture.result(), expected);
    }

    #[test]
    fn pipe_previews_are_bounded_before_lua() {
        let capture = OutputCapture::new(Limits {
            lines: Some(8),
            bytes: Some(1024),
        });
        let input = "x".repeat(8192);
        let mut preview = String::new();
        capture.read_lines(Cursor::new(input), |line| {
            preview = line;
            true
        });
        assert_eq!(
            preview.len(),
            DEFAULT_MAX_LINE_BYTES + PREVIEW_TRUNCATION.len()
        );
        assert!(preview.ends_with(PREVIEW_TRUNCATION));
        assert!(capture.result().contains(TRUNCATED));
    }

    #[test]
    fn closing_a_full_preview_queue_stops_the_reader() {
        let capture = OutputCapture::new(Limits::default());
        let reader_capture = capture.clone();
        let (tx, rx) = flume::bounded(1);
        tx.send(String::new()).unwrap();
        let (entered_tx, entered_rx) = flume::bounded(1);
        let reader = thread::spawn(move || {
            reader_capture.read_lines(Cursor::new("first\nsecond"), |line| {
                entered_tx.send(()).unwrap();
                tx.send(line).is_ok()
            });
        });
        entered_rx.recv().unwrap();
        drop(rx);
        reader.join().unwrap();
        assert_eq!(capture.result(), FIRST_LINE);
    }

    #[test_case("compact", Some(3), Some(10) ; "overrides")]
    #[test_case("full", None, None ; "full_mode")]
    fn native_module_preserves_context_and_option_contract(
        mode: &str,
        lines: Option<usize>,
        bytes: Option<usize>,
    ) {
        let lua = Lua::new();
        let Value::Table(module) = module(&lua, "maki.output_limits").unwrap().unwrap() else {
            panic!()
        };
        let ctx: Table = lua
            .load("return { output_mode = function(self) return self.mode end }")
            .eval()
            .unwrap();
        ctx.set("mode", mode).unwrap();
        let opts = lua.create_table().unwrap();
        opts.set("max_output_lines", 3).unwrap();
        opts.set("max_output_bytes", 10).unwrap();
        let resolve: Function = module.get("resolve").unwrap();
        let resolved: (Option<usize>, Option<usize>) = resolve.call((opts, ctx)).unwrap();
        assert_eq!(resolved, (lines, bytes));
    }
}

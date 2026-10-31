//! The process's tracing subscriber, which writes JSON lines to the rotating
//! log file, and the panic hook that logs a panic before it unwinds.

use std::fmt;
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde_json::Value;
use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields, MakeWriter};
use tracing_subscriber::registry::LookupSpan;

use crate::log::RotatingFileWriter;

const LOG_ENV: &str = "MAKI_LOG";
const LOG_ENV_SHARED: &str = "RUST_LOG";
const DEFAULT_LOG_LEVEL: LevelFilter = LevelFilter::INFO;

pub fn install_panic_log_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_owned()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "unknown panic payload".into()
        };
        let location = info.location().map(|l| l.to_string());
        tracing::error!(
            panic.payload = %payload,
            panic.location = location.as_deref().unwrap_or("<unknown>"),
            "panic occurred"
        );
        prev(info);
    }));
}

/// The writer every event goes through.
///
/// A bare `Mutex` would do, except `tracing_subscriber`'s blanket impl unwraps
/// the poison, so a single panic raised while a line was being written turned
/// every later log call into a panic of its own. Taking the guard back out of
/// the poison keeps a wounded process writing its own post mortem.
struct SharedWriter(Mutex<RotatingFileWriter>);

struct SharedGuard<'a>(MutexGuard<'a, RotatingFileWriter>);

impl Write for SharedGuard<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<'a> MakeWriter<'a> for SharedWriter {
    type Writer = SharedGuard<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        SharedGuard(self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// `MAKI_LOG` is ours and is taken literally, `MAKI_LOG=off` included.
///
/// `RUST_LOG` belongs to every Rust program on the machine, and `EnvFilter`
/// drops any target no directive names, so a `RUST_LOG=other_crate=debug` left
/// in a shell profile for an unrelated project silenced maki entirely. We still
/// read it, and a value that names a level decides ours, but a value that only
/// lists other people's targets gets our level added underneath, so a foreign
/// variable can add to our logs and never take them away.
fn log_filter() -> EnvFilter {
    let read = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    build_log_filter(read(LOG_ENV).as_deref(), read(LOG_ENV_SHARED).as_deref())
}

/// Pure core of `log_filter`, so tests never have to set a process wide
/// variable and race every other test in the binary.
fn build_log_filter(ours: Option<&str>, shared: Option<&str>) -> EnvFilter {
    for (name, value, is_ours) in [(LOG_ENV, ours, true), (LOG_ENV_SHARED, shared, false)] {
        let Some(value) = value else {
            continue;
        };
        match EnvFilter::builder().parse(value) {
            Ok(filter) if is_ours || sets_a_level(value) => return filter,
            Ok(filter) => return filter.add_directive(DEFAULT_LOG_LEVEL.into()),
            Err(error) => eprintln!("maki: ignoring {name}={value:?}: {error}"),
        }
    }
    EnvFilter::default().add_directive(DEFAULT_LOG_LEVEL.into())
}

/// Whether a filter string carries a bare level such as `debug`, as opposed to
/// only per target directives like `foo=debug`.
///
/// Empty pieces are skipped here, as `EnvFilter` skips them too. `LevelFilter`
/// reads an empty string as `error`, so the trailing comma in
/// `RUST_LOG=other_crate=debug,` would look like a level and cost us the log.
fn sets_a_level(value: &str) -> bool {
    value
        .split(',')
        .map(str::trim)
        .any(|part| !part.is_empty() && part.parse::<LevelFilter>().is_ok())
}

struct ProjectJson<E> {
    inner: E,
    project: Value,
}

impl<S, N, E> FormatEvent<S, N> for ProjectJson<E>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
    E: FormatEvent<S, N>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut line = String::new();
        self.inner
            .format_event(ctx, Writer::new(&mut line), event)?;
        // Keep the JSON formatter's fields and spans verbatim instead of
        // parsing and serializing every event a second time.
        let fields = line.strip_prefix('{').ok_or(fmt::Error)?;
        write!(writer, "{{\"project\":{},{}", self.project, fields)
    }
}

/// Logging is the first thing to set up and the last thing allowed to stop the
/// program, but a failure here used to be swallowed whole, so an unwritable log
/// dir looked exactly like an idle one: no logs, no reason, no clue.
pub fn init_logging(max_log_bytes: u64, max_log_files: u32, project: &Path) {
    let writer = match RotatingFileWriter::new(max_log_bytes, max_log_files) {
        Ok(writer) => writer,
        Err(error) => {
            eprintln!("maki: logging disabled, cannot open the log file: {error}");
            return;
        }
    };
    tracing_subscriber::fmt()
        .json()
        .map_event_format(|inner| ProjectJson {
            inner,
            project: project.to_string_lossy().into_owned().into(),
        })
        .with_env_filter(log_filter())
        .with_writer(SharedWriter(Mutex::new(writer)))
        .init();
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use std::io::{Read, Seek};
    use std::sync::Arc;
    use std::thread;
    use test_case::test_case;

    use super::{DEFAULT_LOG_LEVEL, ProjectJson, build_log_filter};

    const FOREIGN: &str = "other_crate=debug";
    const FOREIGN_EMPTY_PIECES: &str = ",other_crate=debug,,";
    const BROKEN: &str = "not a directive=!!";

    const PROJECT: &str = "/workspace/项目 \"quoted\"\\path";
    const SPAN: &str = "turn";
    const MESSAGE: &str = "tool finished\nwith output";
    const SESSION: u64 = 7;
    const BYTES: u64 = 42;
    const WORKER_PLUGIN: &str = "background";

    #[test]
    fn project_tags_span_scoped_and_unscoped_worker_logs() {
        let output = Arc::new(tempfile::tempfile().unwrap());
        let subscriber = tracing_subscriber::fmt()
            .json()
            .map_event_format(|inner| ProjectJson {
                inner,
                project: PROJECT.into(),
            })
            .with_writer(Arc::clone(&output))
            .finish();
        let dispatch = tracing::Dispatch::new(subscriber);

        tracing::dispatcher::with_default(&dispatch, || {
            let _span = tracing::info_span!(SPAN, session = SESSION).entered();
            tracing::info!(success = true, bytes = BYTES, "{MESSAGE}");
        });
        thread::spawn(move || {
            tracing::dispatcher::with_default(&dispatch, || {
                tracing::warn!(plugin = WORKER_PLUGIN, "{MESSAGE}");
            });
        })
        .join()
        .unwrap();

        let mut reader = output.try_clone().unwrap();
        reader.rewind().unwrap();
        let mut text = String::new();
        reader.read_to_string(&mut text).unwrap();
        let records: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 2);
        for record in &records {
            assert_eq!(record["project"], PROJECT);
            assert_eq!(record["fields"]["message"], MESSAGE);
            assert!(record["timestamp"].is_string());
            assert_eq!(record["target"], module_path!());
        }
        assert_eq!(records[0]["level"], "INFO");
        assert_eq!(records[0]["fields"]["success"], true);
        assert_eq!(records[0]["fields"]["bytes"], BYTES);
        assert_eq!(records[0]["span"]["name"], SPAN);
        assert_eq!(records[0]["spans"][0]["session"], SESSION);
        assert_eq!(records[1]["level"], "WARN");
        assert_eq!(records[1]["fields"]["plugin"], WORKER_PLUGIN);
        assert!(records[1].get("span").is_none());
    }

    #[test_case(None, None, "info" ; "nothing_set_logs_at_info")]
    #[test_case(Some("warn"), None, "warn" ; "our_variable_wins")]
    #[test_case(Some("off"), None, "off" ; "our_variable_can_silence_us")]
    #[test_case(Some("warn"), Some("trace"), "warn" ; "our_variable_beats_the_shared_one")]
    #[test_case(None, Some("debug"), "debug" ; "shared_variable_with_a_level_is_taken_as_is")]
    #[test_case(None, Some(FOREIGN), "info" ; "foreign_directive_cannot_silence_us")]
    #[test_case(None, Some(FOREIGN_EMPTY_PIECES), "info" ; "empty_pieces_are_not_a_level")]
    #[test_case(None, Some(BROKEN), "info" ; "unparsable_value_falls_back")]
    fn log_filter_keeps_our_own_logs_alive(
        ours: Option<&str>,
        shared: Option<&str>,
        expected: &str,
    ) {
        let rendered = build_log_filter(ours, shared).to_string();
        assert!(
            rendered.split(',').any(|d| d == expected),
            "{ours:?} + {shared:?} rendered as {rendered:?}, wanted {expected:?}"
        );
    }

    #[test]
    fn foreign_directive_is_kept_alongside_our_level() {
        let rendered = build_log_filter(None, Some(FOREIGN)).to_string();
        assert!(rendered.contains(FOREIGN), "{FOREIGN} was dropped");
        assert!(rendered.contains(&DEFAULT_LOG_LEVEL.to_string()));
    }
}

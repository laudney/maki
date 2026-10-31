//! Bounded line reads, shared by `maki.fs` and jobs.

use std::io::{BufRead, Error as IoError, ErrorKind, Read, Result as IoResult};
use std::path::{Path, PathBuf};

use crate::runtime::LUA_MEMORY_LIMIT;

/// All plugins share one Lua heap, and callers often copy what they read. So a
/// big read fails here with a clear error, before it can run some other tool
/// out of memory.
pub const MAX_READ_BYTES: u64 = LUA_MEMORY_LIMIT as u64 / 8;
pub const NON_UTF8_CONTENT_ERR: &str = "non-utf8 content; use read_bytes";
pub const LINE_CUT_MARKER: &str = "...";

pub fn expand_tilde(path: &str) -> PathBuf {
    maki_storage::paths::expand_tilde(Path::new(path))
}

pub fn too_large() -> IoError {
    IoError::new(
        ErrorKind::FileTooLarge,
        format!("file exceeds the {MAX_READ_BYTES}-byte read limit"),
    )
}

pub fn non_utf8() -> IoError {
    IoError::new(ErrorKind::InvalidData, NON_UTF8_CONTENT_ERR)
}

/// A line as [`next_line`] hands it out.
#[derive(Clone)]
pub struct Line {
    pub text: String,
    /// Its length before any cut, so a caller can say how much was dropped.
    pub bytes: u64,
}

impl From<String> for Line {
    fn from(text: String) -> Self {
        let bytes = text.len() as u64;
        Self { text, bytes }
    }
}

#[cfg(test)]
impl From<&str> for Line {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}

/// The next line without its `\n` or `\r\n`, or `None` at the end of the file.
/// A line over `max_bytes` is cut and gets [`LINE_CUT_MARKER`]. The rest of it
/// is skipped without ever landing in memory, which is how a file that is one
/// 300 MB line still pages fine. Without `max_bytes`, a line over
/// [`MAX_READ_BYTES`] fails the read.
pub fn next_line(
    reader: &mut impl BufRead,
    max_bytes: Option<u64>,
    lossy: bool,
) -> IoResult<Option<Line>> {
    let keep = max_bytes.unwrap_or(MAX_READ_BYTES);
    // The extra 2 bytes let a line of exactly `keep` bytes see its `\r\n`, so it is not cut.
    let window = keep + 2;
    let mut line = Vec::new();
    let read = reader.by_ref().take(window).read_until(b'\n', &mut line)? as u64;
    if read == 0 {
        return Ok(None);
    }
    let bytes = if line.ends_with(b"\n") || read < window {
        line.pop_if(|b| *b == b'\n');
        line.pop_if(|b| *b == b'\r');
        line.len() as u64
    } else {
        let (rest, last) = skip_rest_of_line(reader)?;
        // Stripped like the `\r` of a line that fits: by the last byte before the newline.
        let ends_in_cr = if rest == 0 {
            line.last().copied()
        } else {
            last
        } == Some(b'\r');
        read + rest - u64::from(ends_in_cr)
    };
    if line.len() as u64 > keep {
        if max_bytes.is_none() {
            return Err(too_large());
        }
        line.truncate(keep as usize);
        // An error with no length is a char the cut split in half, so drop what is left of it.
        let split_char = line
            .utf8_chunks()
            .last()
            .map(|chunk| chunk.invalid())
            .filter(|tail| std::str::from_utf8(tail).is_err_and(|e| e.error_len().is_none()))
            .map_or(0, <[u8]>::len);
        line.truncate(line.len() - split_char);
        line.extend_from_slice(LINE_CUT_MARKER.as_bytes());
    }
    let text = match String::from_utf8(line) {
        Ok(text) => text,
        Err(e) if lossy => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        Err(_) => return Err(non_utf8()),
    };
    Ok(Some(Line { text, bytes }))
}

/// Skips to the next `\n` and returns how many bytes came before it, and the
/// last of them.
fn skip_rest_of_line(reader: &mut impl BufRead) -> IoResult<(u64, Option<u8>)> {
    let (mut skipped, mut last) = (0, None);
    loop {
        let buf = match reader.fill_buf() {
            Ok(buf) => buf,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if buf.is_empty() {
            return Ok((skipped, last));
        }
        let (rest, found) = match buf.iter().position(|b| *b == b'\n') {
            Some(newline) => (&buf[..newline], true),
            None => (buf, false),
        };
        skipped += rest.len() as u64;
        last = rest.last().copied().or(last);
        let consumed = rest.len() + usize::from(found);
        reader.consume(consumed);
        if found {
            return Ok((skipped, last));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;

    use super::*;

    #[test_case::test_case(b"a\xffb\n", true, Some("a�b") ; "lossy_job_output")]
    #[test_case::test_case(b"a\xffb\n", false, None ; "strict_file_read")]
    #[test_case::test_case("aéb\n".as_bytes(), true, Some("aéb") ; "utf8_job_output")]
    fn line_reads_choose_whether_invalid_utf8_is_lossy(
        content: &[u8],
        lossy: bool,
        expected: Option<&str>,
    ) {
        let mut reader = BufReader::new(content);
        let result = next_line(&mut reader, None, lossy);
        match expected {
            Some(expected) => assert_eq!(result.unwrap().unwrap().text, expected),
            None => assert_eq!(result.err().unwrap().kind(), ErrorKind::InvalidData),
        }
    }

    /// Fails its second read with `Interrupted`, which a reader must retry.
    struct InterruptedOnce<R> {
        inner: R,
        reads: usize,
    }

    impl<R: Read> Read for InterruptedOnce<R> {
        fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
            self.reads += 1;
            if self.reads == 2 {
                return Err(IoError::from(ErrorKind::Interrupted));
            }
            self.inner.read(buf)
        }
    }

    #[test]
    fn next_line_retries_an_interrupted_skip() {
        const CHUNK: usize = 4;
        let inner = InterruptedOnce {
            inner: &b"abcdefgh\nnext\n"[..],
            reads: 0,
        };
        let mut reader = BufReader::with_capacity(CHUNK, inner);
        let cut = next_line(&mut reader, Some(2), false).unwrap().unwrap();
        assert_eq!((cut.text.as_str(), cut.bytes), ("ab...", 8));
        let next = next_line(&mut reader, Some(2), false).unwrap().unwrap();
        assert_eq!(next.bytes, 4);
    }
}

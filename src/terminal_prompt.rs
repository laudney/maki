use std::io::{BufRead, Read, Write};

const SECRET_MASK: char = '*';

type Pair<T> = (Option<T>, Option<String>);

/// Reads one line of an answer from the terminal, off the Lua thread so a slow
/// typist does not trip the 5s watchdog.
///
/// A secret is read with the terminal in raw mode so the characters never reach
/// the scrollback, and echoed as mask characters so there is still feedback
/// that a key landed. Raw mode is given back whatever the read did.
pub fn read_answer(label: &str, secret: bool) -> Pair<String> {
    print!("{label}");
    if std::io::stdout().flush().is_err() {
        return (None, Some("cannot write to the terminal".to_owned()));
    }
    if !secret {
        let mut line = String::new();
        return match std::io::stdin().lock().read_line(&mut line) {
            Ok(_) => (Some(line.trim_end().to_owned()), None),
            Err(e) => (None, Some(e.to_string())),
        };
    }
    if let Err(e) = crossterm::terminal::enable_raw_mode() {
        return (None, Some(e.to_string()));
    }
    let answer = read_masked();
    let _ = crossterm::terminal::disable_raw_mode();
    println!();
    answer
}

/// Raw mode hands over bytes, so the answer is gathered as bytes and decoded
/// once at the end. Taking each one for a char would turn a typed accented
/// letter into two wrong ones.
fn read_masked() -> Pair<String> {
    const BACKSPACE: u8 = 0x7f;
    const CTRL_C: u8 = 0x03;
    let mut answer = Vec::new();
    for byte in std::io::stdin().lock().bytes() {
        match byte {
            Ok(b'\r' | b'\n') => break,
            Ok(CTRL_C) => return (None, Some("cancelled".to_owned())),
            Ok(BACKSPACE) => {
                if answer.pop().is_some() {
                    print!("\u{8} \u{8}");
                }
            }
            Ok(byte) => {
                answer.push(byte);
                print!("{SECRET_MASK}");
            }
            Err(e) => return (None, Some(e.to_string())),
        }
        let _ = std::io::stdout().flush();
    }
    (Some(String::from_utf8_lossy(&answer).into_owned()), None)
}

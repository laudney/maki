//! Removes terminal control sequences from text received from an agent.

use std::borrow::Cow;
use std::ops::RangeInclusive;

use serde_json::Value;

const ESC: char = '\u{1b}';
const BEL: char = '\u{7}';
const CSI: char = '[';
const ST_FINAL: char = '\\';
/// OSC, DCS, SOS, PM and APC: each opens a string that runs to its
/// terminator, all of which a terminal acts on rather than shows.
const STRING_OPENERS: [char; 5] = [']', 'P', 'X', '^', '_'];
const INTERMEDIATES: RangeInclusive<char> = '\u{20}'..='\u{2f}';
const CSI_FINALS: RangeInclusive<char> = '\u{40}'..='\u{7e}';

/// Removes what a terminal would act on rather than show: escape sequences,
/// C0 controls other than newline and tab, DEL and C1. Text from a remote
/// agent reaches the screen through this.
pub fn sanitize_display(text: &str) -> Cow<'_, str> {
    if !text.chars().any(is_unsafe) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ESC {
            match chars.next() {
                Some(CSI) => {
                    for c in chars.by_ref() {
                        if CSI_FINALS.contains(&c) {
                            break;
                        }
                    }
                }
                Some(opener) if STRING_OPENERS.contains(&opener) => {
                    while let Some(c) = chars.next() {
                        if c == BEL || (c == ESC && chars.next_if_eq(&ST_FINAL).is_some()) {
                            break;
                        }
                    }
                }
                Some(c) if INTERMEDIATES.contains(&c) => {
                    while chars.next_if(|c| INTERMEDIATES.contains(c)).is_some() {}
                    chars.next();
                }
                _ => {}
            }
        } else if !is_unsafe(c) {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

fn is_unsafe(c: char) -> bool {
    c != '\n' && c != '\t' && (c.is_ascii_control() || ('\u{80}'..='\u{9f}').contains(&c))
}

/// Sanitizes every string in a received JSON value in place, before it is
/// read into a typed update.
pub fn sanitize_value(value: &mut Value) {
    match value {
        Value::String(text) => {
            if let Cow::Owned(clean) = sanitize_display(text) {
                *text = clean;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(sanitize_value),
        Value::Object(map) => map.values_mut().for_each(sanitize_value),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    #[test_case("\u{1b}[31mred\u{1b}[0m", "red" ; "csi_colors")]
    #[test_case("\u{1b}]0;title\u{7}after", "after" ; "osc_bel")]
    #[test_case("\u{1b}]8;;https://x\u{1b}\\link", "link" ; "osc_string_terminator")]
    #[test_case("a\u{0}b\rc\u{8}d", "abcd" ; "c0_controls")]
    #[test_case("a\u{7f}b", "ab" ; "del")]
    #[test_case("a\u{85}b\u{9b}c", "abc" ; "c1_controls")]
    #[test_case("a\u{1b}(Bb", "ab" ; "charset_selector")]
    #[test_case("a\u{1b}Ppayload\u{1b}\\b", "ab" ; "dcs_string")]
    #[test_case("a\u{1b}_apc\u{1b}\\b", "ab" ; "apc_string")]
    #[test_case("a\u{1b}7b", "ab" ; "two_character_escape")]
    #[test_case("keep\n\tlayout", "keep\n\tlayout" ; "newline_and_tab_survive")]
    #[test_case("--- a.rs\n-old\n+new", "--- a.rs\n-old\n+new" ; "diff_listing_unchanged")]
    fn sanitize_display_removes_terminal_controls(input: &str, expected: &str) {
        assert_eq!(sanitize_display(input), expected);
    }

    #[test]
    fn sanitize_value_reaches_every_string() {
        let mut value = json!({"a": ["\u{1b}[1mx", {"b": "y\u{7}"}], "n": 1});
        sanitize_value(&mut value);
        assert_eq!(value, json!({"a": ["x", {"b": "y"}], "n": 1}));
    }
}

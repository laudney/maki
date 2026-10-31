//! Text helpers over what a fuzzy matcher hands back.

use nucleo_matcher::Utf32Str;
use unicode_segmentation::UnicodeSegmentation;

/// Byte offset in {text} of every unit the matcher counts in, or `None` when
/// the matcher is already counting bytes.
///
/// `maki_agent::file_haystack` byte-indexes a haystack only for a string
/// that is ASCII, so the two cases are bytes and grapheme clusters with
/// nothing in between. Which one applies still has to be read off the value
/// the matcher was actually given rather than guessed, since only that value
/// knows.
fn unit_starts(text: &str, haystack: Utf32Str<'_>) -> Option<Vec<usize>> {
    match haystack {
        Utf32Str::Ascii(_) => None,
        Utf32Str::Unicode(_) => Some(text.grapheme_indices(true).map(|(at, _)| at).collect()),
    }
}

/// The 1-based inclusive byte ranges of {text} that {indices} names, with
/// units that touch coalesced into one range, which is what a caller
/// highlighting a match needs. Ascending, non-overlapping and always on
/// character boundaries, so Lua's `sub(from, to)` hands back whole matched
/// characters however many bytes each of them took.
///
/// Ranges rather than the starts alone, because a start names nothing a
/// caller can slice: one grapheme is one to several bytes, and slicing a
/// single byte off an emoji path hands Lua a lone continuation byte.
///
/// {indices} is whatever `Pattern::indices` left behind for the same {text}
/// and {haystack}. It is sorted and deduplicated in place here so one buffer
/// can be reused for a whole list.
pub fn byte_highlights(
    text: &str,
    haystack: Utf32Str<'_>,
    indices: &mut Vec<u32>,
) -> Vec<(u32, u32)> {
    indices.sort_unstable();
    indices.dedup();
    let starts = unit_starts(text, haystack);
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    for unit in indices.iter().map(|at| *at as usize) {
        let (from, to) = match &starts {
            Some(starts) => match starts.get(unit) {
                Some(from) => (*from, starts.get(unit + 1).copied().unwrap_or(text.len())),
                None => continue,
            },
            None => (unit, unit + 1),
        };
        // Every haystack maki matches against comes from
        // `maki_agent::file_haystack`, which byte-indexes ASCII only. This is
        // public though, and a byte-indexed haystack over a string that is not
        // ASCII names bytes inside characters: Rust panics on slicing one and
        // Lua would be handed invalid UTF-8.
        if !text.is_char_boundary(from) || !text.is_char_boundary(to) {
            continue;
        }
        match ranges.last_mut() {
            // Inclusive and 1-based, so the range before this one ends on the
            // byte before {from} exactly when the two units touch.
            Some(last) if last.1 as usize == from => last.1 = to as u32,
            _ => ranges.push((from as u32 + 1, to as u32)),
        }
    }
    ranges
}

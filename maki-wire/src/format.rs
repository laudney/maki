const MIN_FENCE_LEN: usize = 3;
pub const NOTICE_ID_PREFIX: &str = "notice-";

/// Zed renders tool output as markdown, so bare text loses its newlines.
/// We wrap it in a backtick fence (longer than any run inside the text)
/// to keep the original formatting.
pub fn fenced(text: &str) -> String {
    let fence = "`".repeat(MIN_FENCE_LEN.max(longest_backtick_run(text) + 1));
    format!("{fence}\n{text}\n{fence}")
}

/// The inverse of [`fenced`]. Text that is not one fence, as another agent
/// may send, comes back unchanged.
pub fn unfenced(text: &str) -> &str {
    let fence_len = text.bytes().take_while(|&b| b == b'`').count();
    if fence_len < MIN_FENCE_LEN {
        return text;
    }
    let fence = &text[..fence_len];
    text[fence_len..]
        .strip_prefix('\n')
        .and_then(|rest| rest.strip_suffix(fence))
        .and_then(|rest| rest.strip_suffix('\n'))
        .filter(|inner| longest_backtick_run(inner) < fence_len)
        .unwrap_or(text)
}

fn longest_backtick_run(text: &str) -> usize {
    text.split(|c: char| c != '`')
        .map(str::len)
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case("1: mod render\n2: mod segment", "```\n1: mod render\n2: mod segment\n```" ; "plain_text_gets_default_fence")]
    #[test_case("has ```rust\ncode\n``` inside", "````\nhas ```rust\ncode\n``` inside\n````" ; "fence_longer_than_inner_backticks")]
    fn fenced_wraps_in_code_block(input: &str, expected: &str) {
        assert_eq!(fenced(input), expected);
    }

    #[test_case("" ; "empty")]
    #[test_case("one line" ; "plain")]
    #[test_case("``x" ; "leading_backticks")]
    #[test_case("```" ; "only_a_fence")]
    #[test_case("has ```rust\ncode\n``` inside" ; "inner_fence")]
    #[test_case("\ttab\n\nblank\n" ; "whitespace_kept")]
    fn unfenced_inverts_fenced(text: &str) {
        assert_eq!(unfenced(&fenced(text)), text);
    }

    #[test_case("no fence here" ; "plain")]
    #[test_case("```\nunclosed" ; "unclosed")]
    #[test_case("```\nmismatched\n````" ; "mismatched_close")]
    #[test_case("```\na\n```\n\n```\nb\n```" ; "two_separate_blocks")]
    fn unfenced_keeps_foreign_text(text: &str) {
        assert_eq!(unfenced(text), text);
    }
}

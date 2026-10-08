//! Char-boundary-safe truncation helpers (dedup cluster C7).
//!
//! One shared implementation of the "clip a string for display without ever
//! splitting a multi-byte UTF-8 character" idiom that previously existed as
//! copy-paste twins in `orchestrator::workers` and as a byte-slicing (panicking)
//! variant in `ui::helpers::truncate_reason`.
//!
//! Counting unit is **characters** (Unicode scalar values), not bytes. All
//! functions here are infallible: they never panic on a non-char boundary and
//! never split a multi-byte char (Swedish `å`/`ä`/`ö`, emoji, CJK, …).
//!
//! The ellipsis wording (`"..."`, three ASCII dots) matches the user-visible
//! suffix produced by the call sites this module replaced.

/// Suffix appended when [`truncate_with_ellipsis`] clips the input.
const ELLIPSIS: &str = "...";

/// Returns the prefix of `s` containing at most `max_chars` characters.
///
/// The returned slice always ends on a UTF-8 char boundary, so slicing the
/// original string is always safe. If `s` has `max_chars` characters or fewer
/// (including the empty string), `s` itself is returned unchanged. `max_chars`
/// of `0` yields `""`.
pub fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((end_byte, _)) => &s[..end_byte],
        None => s,
    }
}

/// Returns `s` unchanged when it fits in `max_chars` characters, otherwise the
/// first `max_chars - 3` characters followed by `"..."` so the *total* result
/// never exceeds `max_chars` characters (the ellipsis is part of the budget,
/// matching the 300-byte-cap + 297-char prefix + `"..."` shape of the call
/// sites this replaced).
///
/// Never splits a multi-byte character and never panics. If `max_chars` is
/// smaller than the 3-character ellipsis, the result is just `"..."` for a
/// non-empty input that does not fit.
pub fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    if truncate_chars(s, max_chars).len() == s.len() {
        return s.to_string();
    }
    let head = truncate_chars(s, max_chars.saturating_sub(ELLIPSIS.chars().count()));
    format!("{head}{ELLIPSIS}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- ASCII: shorter / equal / longer than the limit ------------------

    #[test]
    fn truncate_chars_ascii_shorter_equal_longer() {
        assert_eq!(truncate_chars("abc", 10), "abc");
        assert_eq!(truncate_chars("abc", 3), "abc"); // equal to the limit
        assert_eq!(truncate_chars("abcdef", 3), "abc");
    }

    #[test]
    fn truncate_with_ellipsis_ascii_shorter_equal_longer() {
        assert_eq!(truncate_with_ellipsis("abc", 10), "abc");
        assert_eq!(truncate_with_ellipsis("abcdefghi", 9), "abcdefghi"); // equal
        assert_eq!(truncate_with_ellipsis("abcdefghij", 9), "abcdef..."); // longer: 6 + "..." = 9 chars
        // Regression for the workers.rs twin shape (300 cap -> 297 chars + "...").
        let long = "x".repeat(301);
        let cut = truncate_with_ellipsis(&long, 300);
        assert_eq!(cut.chars().count(), 300);
        assert!(cut.ends_with("..."));
        assert_eq!(&cut[..297], "x".repeat(297));
        assert_eq!(
            truncate_with_ellipsis(&"x".repeat(300), 300),
            "x".repeat(300)
        );
    }

    // ---- Multi-byte content at a boundary-crossing limit ------------------

    #[test]
    fn truncate_chars_sweedish_at_boundary_crossing_limit() {
        // "å" is 2 bytes: byte-wise `&s[..3]` would panic, char-wise is safe.
        let s = "åäö";
        assert_eq!(truncate_chars(s, 2), "åä");
        assert_eq!(truncate_chars(s, 3), "åäö");
        assert_eq!(truncate_with_ellipsis("åäöabcdef", 6), "åäö...");
    }

    #[test]
    fn truncate_chars_emoji_at_boundary_crossing_limit() {
        // Each 🙂 is 4 bytes: a byte slice at index 6 would panic mid-char.
        let s = "🙂🙂🙂";
        assert_eq!(truncate_chars(s, 2), "🙂🙂");
        assert_eq!(truncate_chars(s, 1), "🙂");
        assert_eq!(truncate_with_ellipsis("🙂🙂🙂🙂🙂", 4), "🙂...");
    }

    // ---- Degenerate limits and inputs -------------------------------------

    #[test]
    fn zero_char_limit() {
        assert_eq!(truncate_chars("abc", 0), "");
        assert_eq!(truncate_chars("åäö", 0), "");
        // Non-empty input that does not fit below the ellipsis budget: just "...".
        assert_eq!(truncate_with_ellipsis("abcdef", 0), "...");
        // A fitting input is still returned untouched at limit 0.
        assert_eq!(truncate_with_ellipsis("", 0), "");
    }

    #[test]
    fn empty_string() {
        assert_eq!(truncate_chars("", 5), "");
        assert_eq!(truncate_with_ellipsis("", 5), "");
        assert_eq!(truncate_with_ellipsis("", 0), "");
    }

    #[test]
    fn never_splits_multibyte_char() {
        // Every possible char limit must yield valid UTF-8 prefixes.
        for n in 0..=10 {
            let cut = truncate_chars("a🙂äö漢字", n);
            assert!(cut.is_char_boundary(cut.len()));
            assert!(s_validate(cut));
        }
    }

    fn s_validate(s: &str) -> bool {
        std::str::from_utf8(s.as_bytes()).is_ok()
    }
}

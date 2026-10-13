//! One-line display text: collapse whitespace runs and cut to fit a width.
//!
//! Summaries, titles, and issue lines all share this so a width means the
//! same thing everywhere: the result never exceeds `max` characters.
pub fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Collapse to one line, cutting to `max` characters (ellipsis included).
pub fn shorten(s: &str, max: usize) -> String {
    let one_line = collapse(s);
    if one_line.chars().count() <= max {
        return one_line;
    }
    let kept: String = one_line.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", kept.trim_end())
}

/// Collapse to one line, cutting to `max` characters with no ellipsis.
/// For titles and ids where the cut must be invisible.
pub fn take(s: &str, max: usize) -> String {
    collapse(s).chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_passes_through() {
        assert_eq!(shorten("exit status: 1", 80), "exit status: 1");
        assert_eq!(shorten("plain", 0), "…");
    }

    #[test]
    fn long_text_collapses_and_fits_the_width() {
        assert_eq!(collapse("a\n b\t\tc "), "a b c");
        let sql = "exit status: 1 Error: Unexpected error | | Failed query: insert into \"message\" (\"id\", \"session_id\", \"time_created\", \"time_updated\", \"data\") values (?, ?, ?, ?, ?) on conflict do nothing | params: msg_000000-abc,ses_111";
        let line = shorten(&collapse(sql), 80);
        assert!(line.chars().count() <= 80, "{line}");
        assert!(line.ends_with('…'), "{line}");
    }
}

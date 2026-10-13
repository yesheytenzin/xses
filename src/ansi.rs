//! Strip terminal control sequences from text that came from another process.
//!
//! Agent CLIs colour their errors (`\x1b[91m`…), and `xses` both journals and
//! prints that text. The journal keeps it verbatim as evidence; anything that
//! reaches a terminal goes through here first, so a report line stays one line.
pub fn strip(s: &str) -> String {
    if !s.contains('\u{1b}') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameters then a final byte in @..~
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: runs to BEL or ST (ESC \)
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.next() == Some('\\') {
                        break;
                    }
                }
            }
            // other two-character escapes
            Some(_) => {}
            None => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colour_codes_go_away_and_text_stays() {
        let cli = "exit status: 1 \u{1b}[91m\u{1b}[1mError: \u{1b}[0mUnexpected error";
        assert_eq!(
            strip(cli),
            "exit status: 1 Error: Unexpected error",
            "the CLI's own words must survive"
        );
        assert_eq!(strip("plain text"), "plain text");
        assert_eq!(strip("a\u{1b}]0;title\u{7}b"), "ab");
        assert_eq!(strip("cut\u{1b}"), "cut");
    }
}

//! Showing text that an agent or a database controls to a person, in a
//! terminal or on a page, so it can't hide, reorder or smuggle anything.

/// Whether `c` is invisible or changes how text around it is shown: control
/// characters (Unicode `Cc`), format characters (`Cf`: bidi controls,
/// zero-width characters, the soft hyphen, tag characters), line and
/// paragraph separators (`Zl`, `Zp`) and private-use characters (`Co`).
/// Tag characters (U+E0000 to U+E007F) can carry text a person can't see
/// but another AI reading the data can ("ASCII smuggling").
pub fn is_hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            // Cf
            '\u{ad}'
                | '\u{600}'..='\u{605}'
                | '\u{61c}'
                | '\u{6dd}'
                | '\u{70f}'
                | '\u{890}'..='\u{891}'
                | '\u{8e2}'
                | '\u{180e}'
                | '\u{200b}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206f}'
                | '\u{feff}'
                | '\u{fff9}'..='\u{fffb}'
                | '\u{110bd}'
                | '\u{110cd}'
                | '\u{13430}'..='\u{1343f}'
                | '\u{1bca0}'..='\u{1bca3}'
                | '\u{1d173}'..='\u{1d17a}'
                | '\u{e0000}'..='\u{e007f}'
                // Zl, Zp
                | '\u{2028}'
                | '\u{2029}'
                // Co
                | '\u{e000}'..='\u{f8ff}'
                | '\u{f0000}'..='\u{ffffd}'
                | '\u{100000}'..='\u{10fffd}'
        )
}

/// `text` with every hidden character written out as `\u{..}`.
pub fn reveal(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if is_hidden(c) {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Found in review 4: these passed through unescaped.
    #[test]
    fn reveals_what_a_person_cannot_see() {
        assert_eq!(
            reveal("ok\u{61c}X\u{2028}Y\u{ad}Z\u{e0041}tag\u{202e}\u{1b}[8m\n"),
            "ok\\u{61c}X\\u{2028}Y\\u{ad}Z\\u{e0041}tag\\u{202e}\\u{1b}[8m\\u{a}"
        );
        assert_eq!(
            reveal("plain 'text', 12.50 é 中"),
            "plain 'text', 12.50 é 中"
        );
    }
}

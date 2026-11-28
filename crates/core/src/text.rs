// SPDX-License-Identifier: EUPL-1.2
//! Text that arrives from a server, cleaned for display.

/// `text` as a name to show: every control character and every character that
/// reorders what follows it becomes a space. Scripts that read right to left
/// keep the characters of their own.
pub fn clean_name(text: &str) -> String {
    text.chars()
        .map(|character| match shows(character) {
            true => character,
            false => ' ',
        })
        .collect()
}

/// Whether `character` shows as a glyph in a name: no control character, and
/// no explicit bidirectional embedding, override or isolate.
fn shows(character: char) -> bool {
    !character.is_control()
        && !matches!(character, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

#[cfg(test)]
mod tests {
    use super::clean_name;

    #[test]
    fn a_name_keeps_the_glyphs_it_shows_and_loses_the_rest() {
        assert_eq!(clean_name("Ada"), "Ada");
        assert_eq!(clean_name("אֲדָה"), "אֲדָה");
        assert_eq!(clean_name("∂a"), "∂a");
        // C0 and C1 control characters, and the escapes that reorder a line.
        assert_eq!(clean_name("Ada\u{7}Lovelace"), "Ada Lovelace");
        assert_eq!(clean_name("Ada\nLovelace\0"), "Ada Lovelace ");
        assert_eq!(clean_name("Ada\u{202E}ecalp"), "Ada ecalp");
        assert_eq!(clean_name("\u{2067}Ada\u{2069}"), " Ada ");
    }
}

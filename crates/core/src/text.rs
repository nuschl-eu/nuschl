// SPDX-License-Identifier: EUPL-1.2
//! Text that arrives from a server, cleaned for display.

/// `text` as a name to show: every character that shows no glyph of its own
/// becomes a space, and the spaces around the name go. A name that shows
/// nothing at all becomes an empty name, so the callers that name an unnamed
/// user keep working.
pub fn clean_name(text: &str) -> String {
    let mut clean = String::with_capacity(text.len());
    for character in text.chars() {
        let shown = match shows(character) {
            true => character,
            false => ' ',
        };
        // What shows nothing becomes one space, however much of it there is.
        if shown != ' ' || !clean.ends_with(' ') {
            clean.push(shown);
        }
    }
    clean.trim().to_string()
}

/// Whether `character` shows a glyph in a name: no control character, no line
/// or paragraph separator, and nothing invisible of its own.
fn shows(character: char) -> bool {
    !character.is_control()
        && !matches!(character, '\u{2028}' | '\u{2029}')
        && !invisible(character)
}

/// Whether `character` takes no room of its own: the zero-width spaces and
/// joiners, the marks that steer a bidirectional run, and the byte-order mark.
pub fn invisible(character: char) -> bool {
    matches!(
        character,
        '\u{061C}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}'
    )
}

#[cfg(test)]
mod tests {
    use super::clean_name;

    #[test]
    fn a_name_shows_its_own_glyphs_and_a_space_where_a_character_has_none() {
        assert_eq!(clean_name("Ada"), "Ada");
        assert_eq!(clean_name("אֲדָה"), "אֲדָה");
        assert_eq!(clean_name("∂a"), "∂a");
        assert_eq!(clean_name("Ada\u{7}Lovelace"), "Ada Lovelace");
        assert_eq!(clean_name("Ada\nLovelace\0"), "Ada Lovelace");
        assert_eq!(
            clean_name("  Countess\n of Lovelace "),
            "Countess of Lovelace"
        );
        assert_eq!(clean_name("Ada\u{202E}ecalp"), "Ada ecalp");
        assert_eq!(clean_name("Ada\u{200B}"), "Ada");
        assert_eq!(clean_name("\u{2067}Ada\u{2069}"), "Ada");
        // A name of nothing but those characters shows nothing.
        assert_eq!(clean_name("\u{202E}\u{7}"), "");
        assert_eq!(clean_name("   "), "");
        // Cleaning twice changes nothing the first pass left.
        assert_eq!(clean_name(&clean_name("Ada\u{202E}ecalp")), "Ada ecalp");
    }
}

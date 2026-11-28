// SPDX-License-Identifier: EUPL-1.2
//! Message search as people type it: words, "exact phrases" and `from:name`.
//! Storage turns a query into its full-text syntax; the desktop highlights
//! the same tokens in the results.

use crate::HistoryEntry;
use serde::{Deserialize, Serialize};
use std::ops::Range;

/// The longest query storage accepts, in bytes.
pub const MAX_QUERY_BYTES: usize = 512;

/// A parsed search query. Every phrase must match.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    /// Tokens that must appear in this order; a word is the run of its tokens.
    pub phrases: Vec<Vec<String>>,
    /// The last phrase's last token also matches longer words: it is still
    /// being typed.
    pub prefix: bool,
    /// The lowercased start of the author's name (`from:ada`, `from:"ada l"`).
    pub from: Option<String>,
}

impl Query {
    pub fn parse(text: &str) -> Self {
        let mut query = Self::default();
        let mut rest = text.trim_start();
        while !rest.is_empty() {
            if rest
                .get(..5)
                .is_some_and(|start| start.eq_ignore_ascii_case("from:"))
            {
                let (name, after) = match &rest[5..] {
                    name if name.starts_with(char::is_whitespace) => ("", name),
                    name => {
                        let (name, _, _, after) = next_word(name);
                        (name, after)
                    }
                };
                rest = after.trim_start();
                let name = fold(name.trim());
                if !name.is_empty() {
                    query.from = Some(name);
                }
                query.prefix = false;
                continue;
            }
            let (word, _, open, after) = next_word(rest);
            rest = after.trim_start();
            // Still being typed: nothing (not even a space) follows.
            let open = open && !word.ends_with(char::is_whitespace);
            let phrase: Vec<String> = tokens(word).map(|(_, token)| token).collect();
            if !phrase.is_empty() {
                query.phrases.push(phrase);
                query.prefix = open;
            }
        }
        query
    }

    pub fn is_empty(&self) -> bool {
        self.phrases.is_empty() && self.from.is_none()
    }

    /// Whether a lowercased token of a message matches one of the query's.
    pub fn matches(&self, token: &str) -> bool {
        let last = self.phrases.len().wrapping_sub(1);
        self.phrases.iter().enumerate().any(|(index, phrase)| {
            phrase.iter().enumerate().any(|(position, word)| {
                word == token
                    || (self.prefix
                        && index == last
                        && position + 1 == phrase.len()
                        && token.starts_with(word.as_str()))
            })
        })
    }

    /// Byte ranges of the words in `text` that match the query.
    pub fn highlights(&self, text: &str) -> Vec<Range<usize>> {
        tokens(text)
            .filter(|(_, token)| self.matches(token))
            .map(|(range, _)| range)
            .collect()
    }
}

/// The next word or "quoted phrase" of `text`, whether it was quoted,
/// whether it runs to the end unfinished (an unclosed quote does), and what
/// follows.
fn next_word(text: &str) -> (&str, bool, bool, &str) {
    if let Some(quoted) = text.strip_prefix('"') {
        match quoted.find('"') {
            Some(end) => (&quoted[..end], true, false, &quoted[end + 1..]),
            None => (quoted, true, true, ""),
        }
    } else {
        let end = text.find(char::is_whitespace).unwrap_or(text.len());
        (&text[..end], false, end == text.len(), &text[end..])
    }
}

/// Words as full-text search sees them: runs of letters and digits, lowercased
/// with accents folded, and the characters of a script written without spaces
/// each on their own, with their byte ranges in `text`.
pub fn tokens(text: &str) -> impl Iterator<Item = (Range<usize>, String)> + '_ {
    let mut chars = text.char_indices().peekable();
    std::iter::from_fn(move || {
        let (start, first) = chars.find(|(_, c)| c.is_alphanumeric())?;
        if unspaced(first) {
            let end = start + first.len_utf8();
            return Some((start..end, folded(text, start..end)));
        }
        let mut end = start + first.len_utf8();
        while let Some(&(index, c)) = chars.peek() {
            // A separator, or a character that is a word of its own, ends this
            // word here and starts the next one there.
            if !c.is_alphanumeric() || unspaced(c) {
                end = index;
                break;
            }
            chars.next();
            end = index + c.len_utf8();
        }
        Some((start..end, folded(text, start..end)))
    })
}

/// `text` as full-text storage keeps it: its words, separated by spaces (see
/// [`tokens`]). Searching folds a query the same way, so the two always agree.
pub fn fold(text: &str) -> String {
    tokens(text)
        .map(|(_, token)| token)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The folded letters of `text[start..end]`, which holds one word.
fn folded(text: &str, range: Range<usize>) -> String {
    let mut folded = String::with_capacity(range.len());
    for c in text[range].chars() {
        fold_char(&mut folded, c);
    }
    folded
}

/// A script that runs words together, so search counts each character as a
/// word: Han (with the iteration and numbering marks, the compatibility block
/// and the supplementary planes), kana (also halfwidth and the extensions) and
/// Hangul (syllables and jamo).
fn unspaced(c: char) -> bool {
    matches!(
        c,
        '\u{1100}'..='\u{11ff}'
            | '\u{3005}'..='\u{3007}'
            | '\u{303b}'
            | '\u{3040}'..='\u{30ff}'
            | '\u{3130}'..='\u{318f}'
            | '\u{31f0}'..='\u{31ff}'
            | '\u{3400}'..='\u{4dbf}'
            | '\u{4e00}'..='\u{9fff}'
            | '\u{a960}'..='\u{a97f}'
            | '\u{ac00}'..='\u{d7af}'
            | '\u{d7b0}'..='\u{d7ff}'
            | '\u{f900}'..='\u{faff}'
            | '\u{ff66}'..='\u{ff9f}'
            | '\u{1b000}'..='\u{1b16f}'
            | '\u{20000}'..='\u{2fa1f}'
    )
}

/// Append the lowercase base letters of `c`: a message with "Café" is found by
/// typing "cafe". The table covers the Latin letters with diacritics (Latin-1
/// and Latin Extended-A, the set `unaccent` folds); other letters are appended
/// lowercased.
fn fold_char(out: &mut String, c: char) {
    out.push_str(match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' | 'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å'
        | 'Ā' | 'Ă' | 'Ą' => "a",
        'æ' | 'Æ' => "ae",
        'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' | 'Ç' | 'Ć' | 'Ĉ' | 'Ċ' | 'Č' => "c",
        'ď' | 'đ' | 'Ď' | 'Đ' | 'Ð' | 'ð' => "d",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' | 'È' | 'É' | 'Ê' | 'Ë' | 'Ē' | 'Ĕ'
        | 'Ė' | 'Ę' | 'Ě' => "e",
        'ĝ' | 'ğ' | 'ġ' | 'ģ' | 'Ĝ' | 'Ğ' | 'Ġ' | 'Ģ' => "g",
        'ĥ' | 'ħ' | 'Ĥ' | 'Ħ' => "h",
        'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' | 'Ì' | 'Í' | 'Î' | 'Ï' | 'Ĩ' | 'Ī'
        | 'Ĭ' | 'Į' | 'İ' => "i",
        'ĳ' | 'Ĳ' => "ij",
        'ĵ' | 'Ĵ' => "j",
        'ķ' | 'Ķ' => "k",
        'ĺ' | 'ļ' | 'ľ' | 'ł' | 'Ĺ' | 'Ļ' | 'Ľ' | 'Ł' => "l",
        'ñ' | 'ń' | 'ņ' | 'ň' | 'Ñ' | 'Ń' | 'Ņ' | 'Ň' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' | 'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø'
        | 'Ō' | 'Ŏ' | 'Ő' => "o",
        'œ' | 'Œ' => "oe",
        'ŕ' | 'ŗ' | 'ř' | 'Ŕ' | 'Ŗ' | 'Ř' => "r",
        'ś' | 'ŝ' | 'ş' | 'š' | 'Ś' | 'Ŝ' | 'Ş' | 'Š' => "s",
        'ß' | 'ẞ' => "ss",
        'ţ' | 'ť' | 'ŧ' | 'Ţ' | 'Ť' | 'Ŧ' => "t",
        'þ' | 'Þ' => "th",
        'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' | 'Ù' | 'Ú' | 'Û' | 'Ü' | 'Ũ'
        | 'Ū' | 'Ŭ' | 'Ů' | 'Ű' | 'Ų' => "u",
        'ŵ' | 'Ŵ' => "w",
        'ý' | 'ÿ' | 'ŷ' | 'Ý' | 'Ÿ' | 'Ŷ' => "y",
        'ź' | 'ż' | 'ž' | 'Ź' | 'Ż' | 'Ž' => "z",
        other => {
            out.extend(other.to_lowercase());
            return;
        }
    });
}

/// Where the next page of results starts: after the last result shown,
/// scored as of the first page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchCursor {
    pub now_ms: i64,
    pub score: i64,
    pub id: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchPage {
    /// Best match first; each body is the message's visible text, without
    /// inline images.
    pub hits: Vec<HistoryEntry>,
    /// More results follow from here; None means these were the last.
    pub next: Option<SearchCursor>,
    /// Older messages are still being added to the search index.
    pub indexing: bool,
}

#[cfg(test)]
mod tests {
    use super::{Query, fold, tokens};

    fn phrases(query: &Query) -> Vec<Vec<&str>> {
        query
            .phrases
            .iter()
            .map(|phrase| phrase.iter().map(String::as_str).collect())
            .collect()
    }

    #[test]
    fn words_phrases_and_authors() {
        let query = Query::parse(r#"Deploy "the RUST server" from:Ada rel"#);
        assert_eq!(
            phrases(&query),
            [vec!["deploy"], vec!["the", "rust", "server"], vec!["rel"]]
        );
        assert!(query.prefix, "the last word is still being typed");
        assert_eq!(query.from.as_deref(), Some("ada"));
        assert!(!Query::parse("rel ").prefix, "a space ends the word");
        assert!(!Query::parse(r#""exact phrase""#).prefix);
        assert!(
            Query::parse(r#""exact phr"#).prefix,
            "an open quote still types"
        );
        let author = Query::parse(r#"from:"Ada Love" hello"#);
        assert_eq!(author.from.as_deref(), Some("ada love"));
        assert_eq!(phrases(&author), [vec!["hello"]]);
        // Punctuation separates tokens and alone is no word.
        assert_eq!(phrases(&Query::parse("foo-bar !!! ")), [vec!["foo", "bar"]]);
        assert!(Query::parse(" \"\" from: ").is_empty());
    }

    #[test]
    fn accents_fold_both_ways() {
        assert_eq!(
            tokens("Café crème, Straße, Ægir, Œuvre")
                .map(|(_, token)| token)
                .collect::<Vec<_>>(),
            ["cafe", "creme", "strasse", "aegir", "oeuvre"]
        );
        // Either spelling matches, and the word is highlighted as written.
        for typed in ["café", "cafe"] {
            let query = Query::parse(typed);
            assert_eq!(phrases(&query), [vec!["cafe"]]);
            let text = "Le Café du coin";
            let found: Vec<&str> = query
                .highlights(text)
                .into_iter()
                .map(|range| &text[range])
                .collect();
            assert_eq!(found, ["Café"]);
        }
        assert_eq!(fold("Crème Brûlée"), "creme brulee");
    }

    #[test]
    fn cjk_text_matches_inside_a_run() {
        // A word before the run keeps its own range.
        let mixed: Vec<_> = tokens("abc你好").collect();
        assert_eq!(
            mixed
                .iter()
                .map(|(range, token)| (&"abc你好"[range.clone()], token.as_str()))
                .collect::<Vec<_>>(),
            [("abc", "abc"), ("你", "你"), ("好", "好")]
        );
        let ranges: Vec<_> = tokens("你好世界").collect();
        assert_eq!(
            ranges
                .iter()
                .map(|(_, token)| token.as_str())
                .collect::<Vec<_>>(),
            ["你", "好", "世", "界"]
        );
        assert_eq!(ranges[1].0, 3..6, "each character keeps its own range");
        // A word from inside the run matches and highlights what was typed.
        let query = Query::parse("好世");
        assert_eq!(phrases(&query), [vec!["好", "世"]]);
        let text = "你好世界";
        let found: Vec<&str> = query
            .highlights(text)
            .into_iter()
            .map(|range| &text[range])
            .collect();
        assert_eq!(found, ["好", "世"]);
        assert_eq!(fold("你好世界 ok"), "你 好 世 界 ok");
        // Kana and Hangul are counted the same way.
        assert_eq!(
            tokens("ひらがな 한글")
                .map(|(_, token)| token)
                .collect::<Vec<_>>(),
            ["ひ", "ら", "が", "な", "한", "글"]
        );
        // Kana's halfwidth block and extensions, Hangul's jamo and the Han
        // planes above the basic one split the same way.
        assert_eq!(fold("ﾊﾛｰ"), "ﾊ ﾛ ｰ");
        assert_eq!(fold("ㇱㇲ"), "ㇱ ㇲ");
        assert_eq!(fold("한"), "ᄒ ᅡ ᆫ");
        assert_eq!(fold("𠀁𠀂"), "𠀁 𠀂");
    }

    #[test]
    fn highlights_follow_tokens() {
        let text = "Ärger mit dem Server, servers and SERVE";
        assert_eq!(
            tokens(text).map(|(_, token)| token).collect::<Vec<_>>(),
            ["arger", "mit", "dem", "server", "servers", "and", "serve"]
        );
        let query = Query::parse("ärger serv");
        let found: Vec<&str> = query
            .highlights(text)
            .into_iter()
            .map(|range| &text[range])
            .collect();
        assert_eq!(found, ["Ärger", "Server", "servers", "SERVE"]);
        assert_eq!(Query::parse("serv ").highlights(text), []);
    }
}

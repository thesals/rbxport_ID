//! Text normalisation and similarity for comparing artists and titles.
//!
//! Everything here is pure and deterministic. Comparisons run on
//! [`normalize`]d text, so case, accents, punctuation and the many ways of
//! writing "and" (`&`, `+`, `x`, `vs.`, `/`) never count as a difference.

use std::collections::BTreeSet;

use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

/// The token every joining word or symbol becomes.
const JOIN: &str = "and";

/// Featuring markers, longest first so `featuring` is not read as `feat`
/// and `ft.` keeps its dot.
const FEAT_MARKERS: [&str; 5] = ["featuring", "feat.", "feat", "ft.", "ft"];

/// Two names at or above this [`similarity`] count as the same artist.
pub const NAME_MATCH: f64 = 0.85;

/// Lowercases, folds accents, unifies joining words, drops punctuation and
/// collapses whitespace.
///
/// `&`, `+`, `/`, `and`, `x` and `vs`/`vs.` all become `and`. A bare `x` or
/// `vs` only counts as a join between two words, so `Malcolm X` keeps its
/// `x`. Apostrophes vanish (`don't` → `dont`); every other non-alphanumeric
/// character separates words.
pub fn normalize(text: &str) -> String {
    let mut cleaned = String::with_capacity(text.len());
    for c in text.nfkd() {
        if is_combining_mark(c) {
            continue;
        }
        for lc in c.to_lowercase() {
            match lc {
                '\'' | '\u{2019}' | '\u{2018}' | '`' | '\u{02bc}' => {}
                '&' | '+' | '/' => {
                    cleaned.push(' ');
                    cleaned.push_str(JOIN);
                    cleaned.push(' ');
                }
                // Letters NFKD leaves whole.
                'ø' => cleaned.push('o'),
                'æ' => cleaned.push_str("ae"),
                'œ' => cleaned.push_str("oe"),
                'ß' => cleaned.push_str("ss"),
                'ł' => cleaned.push('l'),
                'đ' | 'ð' => cleaned.push('d'),
                'þ' => cleaned.push_str("th"),
                c if c.is_alphanumeric() => cleaned.push(c),
                _ => cleaned.push(' '),
            }
        }
    }
    let words: Vec<&str> = cleaned.split_whitespace().collect();
    let last = words.len().saturating_sub(1);
    let mut out = String::with_capacity(cleaned.len());
    for (i, word) in words.iter().enumerate() {
        let word = if i > 0 && i < last && matches!(*word, "x" | "vs") {
            JOIN
        } else {
            word
        };
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// Removes featuring clauses (`feat.`, `feat`, `ft.`, `ft`, `featuring`,
/// any case, bare or inside `(...)`/`[...]`) and returns the remaining text
/// with the featured names in their original casing.
///
/// A bare clause runs to the next `(`, `[` or ` - `, so the mix survives:
/// `Tempted feat. Sarah Howells (Extended Mix)` → `Tempted (Extended Mix)`.
/// A marker must be a whole word (`Feather` is not one) and a bare marker
/// must follow some text, so a title starting `Ft. Lauderdale` is left alone.
pub fn strip_featuring(text: &str) -> (String, Vec<String>) {
    let mut rest = text.trim().to_owned();
    let mut featured: Vec<String> = Vec::new();
    while let Some(clause) = find_featuring(&rest) {
        let names = rest.get(clause.names.0..clause.names.1).unwrap_or("");
        for name in split_raw(names) {
            let key = normalize(&name);
            if !featured.iter().any(|f| normalize(f) == key) {
                featured.push(name);
            }
        }
        let before = rest.get(..clause.remove.0).unwrap_or("");
        let after = rest.get(clause.remove.1..).unwrap_or("");
        rest = collapse_whitespace(&format!("{before} {after}"));
    }
    (rest, featured)
}

/// Splits an artist credit into normalised individual names: on `,` `;`
/// `&` `+` `/`, brackets, the words `and`, `x`, `vs`, and featuring
/// markers. Duplicates are dropped; order is kept.
pub fn split_artists(credit: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in split_raw(credit) {
        let name = normalize(&name);
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// Similarity of two strings in `0..=1`, on normalised text.
///
/// The larger of a token-set Dice coefficient (word order and extra words
/// matter little) and a normalised Levenshtein ratio (typos and spacing such
/// as `JackRock` vs `Jack Rock`). Equal text, or both empty, is 1.0; one
/// empty is 0.0. Symmetric.
pub fn similarity(a: &str, b: &str) -> f64 {
    similarity_normalized(&normalize(a), &normalize(b))
}

/// How well two artist credits agree, in `0..=1`.
///
/// Both credits are split with [`split_artists`]. A name on one side is
/// matched when some name on the other side has [`similarity`] ≥ 0.85.
/// The score is a Dice coefficient over matched names:
///
/// `(matched_in_a + matched_in_b) / (len_a + len_b)`
///
/// so every name counts once from each side. `Mastiksoul, The Groove, Sing`
/// vs `Mastiksoul feat. The Groove` is (2 + 2) / (3 + 2) = 0.8: high, but a
/// credited artist is missing so not 1.0. Disjoint credits score 0.0. Both
/// empty is 1.0; one empty is 0.0.
pub fn artist_similarity(a: &str, b: &str) -> f64 {
    let names_a = split_artists(a);
    let names_b = split_artists(b);
    if names_a.is_empty() && names_b.is_empty() {
        return 1.0;
    }
    if names_a.is_empty() || names_b.is_empty() {
        return 0.0;
    }
    let matched = |from: &[String], to: &[String]| {
        from.iter()
            .filter(|x| to.iter().any(|y| similarity_normalized(x, y) >= NAME_MATCH))
            .count()
    };
    let hits = matched(&names_a, &names_b) + matched(&names_b, &names_a);
    as_f64(hits) / as_f64(names_a.len() + names_b.len())
}

/// [`similarity`] for text that is already normalised.
fn similarity_normalized(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    token_dice(a, b).max(levenshtein_ratio(a, b))
}

fn token_dice(a: &str, b: &str) -> f64 {
    let ta: BTreeSet<&str> = a.split_whitespace().collect();
    let tb: BTreeSet<&str> = b.split_whitespace().collect();
    let total = ta.len() + tb.len();
    if total == 0 {
        return 0.0;
    }
    2.0 * as_f64(ta.intersection(&tb).count()) / as_f64(total)
}

fn levenshtein_ratio(a: &str, b: &str) -> f64 {
    let ca: Vec<char> = a.chars().collect();
    let cb: Vec<char> = b.chars().collect();
    let longest = ca.len().max(cb.len());
    if longest == 0 {
        return 1.0;
    }
    let mut prev: Vec<usize> = (0..=cb.len()).collect();
    let mut cur = vec![0; cb.len() + 1];
    for (i, x) in ca.iter().enumerate() {
        cur[0] = i + 1;
        for (j, y) in cb.iter().enumerate() {
            let substitute = prev[j] + usize::from(x != y);
            cur[j + 1] = substitute.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    1.0 - as_f64(prev[cb.len()]) / as_f64(longest)
}

/// A count as `f64`. Counts here are lengths of short strings, far below
/// the 2^32 that `f64` holds exactly.
fn as_f64(n: usize) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX))
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Splits a credit into names as written (trimmed, original casing).
fn split_raw(credit: &str) -> Vec<String> {
    let mut names = Vec::new();
    let separators = |c: char| matches!(c, ',' | ';' | '&' | '+' | '/' | '(' | ')' | '[' | ']');
    for segment in credit.split(separators) {
        let words: Vec<&str> = segment.split_whitespace().collect();
        let last = words.len().saturating_sub(1);
        let mut current: Vec<&str> = Vec::new();
        for (i, word) in words.iter().enumerate() {
            let lower = word.to_lowercase();
            let is_feat = FEAT_MARKERS.contains(&lower.as_str());
            // A joining word between two names; `Malcolm X` keeps its X.
            let is_join = i > 0 && i < last && matches!(lower.as_str(), "and" | "x" | "vs" | "vs.");
            if is_feat || is_join {
                push_name(&mut names, &current);
                current.clear();
            } else {
                current.push(word);
            }
        }
        push_name(&mut names, &current);
    }
    names
}

fn push_name(names: &mut Vec<String>, words: &[&str]) {
    if !words.is_empty() {
        names.push(words.join(" "));
    }
}

/// Byte ranges of one featuring clause: what to remove from the text and
/// where the featured names are.
struct Clause {
    remove: (usize, usize),
    names: (usize, usize),
}

fn find_featuring(text: &str) -> Option<Clause> {
    let mut prev: Option<char> = None;
    for (i, c) in text.char_indices() {
        let at_word_start = prev.is_none_or(|p| p.is_whitespace() || p == '(' || p == '[');
        prev = Some(c);
        if !at_word_start {
            continue;
        }
        let Some(end) = marker_end(text, i) else {
            continue;
        };
        let before = text.get(..i).unwrap_or("").trim_end();
        // Inside a bracket group: remove the whole group.
        let opener = before.chars().next_back();
        if let Some(open) = opener.filter(|o| *o == '(' || *o == '[') {
            let close = if open == '(' { ')' } else { ']' };
            let open_at = before.len() - open.len_utf8();
            if let Some(rel) = text.get(end..).and_then(|t| t.find(close)) {
                return Some(Clause {
                    remove: (open_at, end + rel + close.len_utf8()),
                    names: (end, end + rel),
                });
            }
        }
        // A bare marker must follow some text.
        if before.is_empty() {
            continue;
        }
        let tail = text.get(end..).unwrap_or("");
        let stop = [tail.find('('), tail.find('['), tail.find(" - ")]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(tail.len());
        return Some(Clause {
            remove: (before.len(), end + stop),
            names: (end, end + stop),
        });
    }
    None
}

/// The byte index just past a featuring marker starting at `i`, if one does.
fn marker_end(text: &str, i: usize) -> Option<usize> {
    FEAT_MARKERS.iter().find_map(|marker| {
        let end = i + marker.len();
        let candidate = text.get(i..end)?;
        if !candidate.eq_ignore_ascii_case(marker) {
            return None;
        }
        // `feat.` ends at its dot; a dotless marker needs whitespace after
        // it, so `Feather` and `ftw` do not match.
        let next = text.get(end..).and_then(|t| t.chars().next());
        (marker.ends_with('.') || next.is_some_and(char::is_whitespace)).then_some(end)
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn normalize_folds_case_accents_and_punctuation() {
        assert_eq!(normalize("  Beyoncé  Knowles "), "beyonce knowles");
        assert_eq!(normalize("Røyksopp"), "royksopp");
        assert_eq!(normalize("Ñandú: Über-Café!"), "nandu uber cafe");
        assert_eq!(normalize("Don't Stop"), "dont stop");
        assert_eq!(normalize("Don’t Stop"), "dont stop");
        assert_eq!(normalize("Dub Stab!"), "dub stab");
        assert_eq!(normalize("Hello,   World..."), "hello world");
        assert_eq!(normalize(""), "");
        assert_eq!(normalize("?!"), "");
    }

    #[test]
    fn normalize_unifies_joining_words() {
        let expected = "above and beyond";
        for text in [
            "Above & Beyond",
            "Above and Beyond",
            "Above + Beyond",
            "Above x Beyond",
            "Above X Beyond",
            "Above vs Beyond",
            "Above vs. Beyond",
            "Above / Beyond",
            "Above&Beyond",
        ] {
            assert_eq!(normalize(text), expected, "{text}");
        }
        // A trailing or leading x is a name, not a join.
        assert_eq!(normalize("Malcolm X"), "malcolm x");
        assert_eq!(normalize("X Ambassadors"), "x ambassadors");
    }

    #[test]
    fn strip_featuring_bare_and_bracketed() {
        assert_eq!(
            strip_featuring("Tempted feat. Sarah Howells"),
            ("Tempted".to_owned(), vec!["Sarah Howells".to_owned()])
        );
        assert_eq!(
            strip_featuring("Festa Feat The Groove"),
            ("Festa".to_owned(), vec!["The Groove".to_owned()])
        );
        assert_eq!(
            strip_featuring("Song ft. A & B"),
            ("Song".to_owned(), vec!["A".to_owned(), "B".to_owned()])
        );
        assert_eq!(
            strip_featuring("Song FT X"),
            ("Song".to_owned(), vec!["X".to_owned()])
        );
        assert_eq!(
            strip_featuring("Song featuring Someone Else"),
            ("Song".to_owned(), vec!["Someone Else".to_owned()])
        );
        assert_eq!(
            strip_featuring("Song (feat. Someone)"),
            ("Song".to_owned(), vec!["Someone".to_owned()])
        );
        assert_eq!(
            strip_featuring("Song [Ft. Someone] (Extended Mix)"),
            ("Song (Extended Mix)".to_owned(), vec!["Someone".to_owned()])
        );
        assert_eq!(
            strip_featuring("Tempted feat. Sarah Howells (Extended Mix)"),
            ("Tempted (Extended Mix)".to_owned(), vec!["Sarah Howells".to_owned()])
        );
        assert_eq!(
            strip_featuring("Song feat. A - B Remix"),
            ("Song - B Remix".to_owned(), vec!["A".to_owned()])
        );
    }

    #[test]
    fn strip_featuring_leaves_lookalikes() {
        for text in ["Feather", "Feathers (Original Mix)", "Lift Off", "Ft. Lauderdale", "Aftermath", "Craft Beer"] {
            assert_eq!(strip_featuring(text), (text.to_owned(), Vec::new()), "{text}");
        }
    }

    #[test]
    fn split_artists_on_every_separator() {
        assert_eq!(
            split_artists("Balthazar & JackRock, Tekhoover"),
            ["balthazar", "jackrock", "tekhoover"]
        );
        assert_eq!(split_artists("Mastiksoul feat. The Groove"), ["mastiksoul", "the groove"]);
        assert_eq!(split_artists("A; B / C x D vs. E and F"), ["a", "b", "c", "d", "e", "f"]);
        assert_eq!(split_artists("A ft B (feat. C)"), ["a", "b", "c"]);
        assert_eq!(split_artists("Café, Cafe, CAFÉ"), ["cafe"]);
        assert_eq!(split_artists("Malcolm X"), ["malcolm x"]);
        assert_eq!(split_artists(""), Vec::<String>::new());
        assert_eq!(split_artists(" , & "), Vec::<String>::new());
    }

    #[test]
    fn similarity_edges_and_symmetry() {
        assert!((similarity("", "") - 1.0).abs() < f64::EPSILON);
        assert!((similarity("abc", "") - 0.0).abs() < f64::EPSILON);
        assert!((similarity("", "abc") - 0.0).abs() < f64::EPSILON);
        assert!((similarity("The Music", "the music!") - 1.0).abs() < f64::EPSILON);
        assert!((similarity("Above & Beyond", "Above and Beyond") - 1.0).abs() < f64::EPSILON);
        assert!(similarity("JackRock", "Jack Rock") >= NAME_MATCH);
        assert!(similarity("Aurora", "Infinity") < 0.5);
        let pairs = [
            ("The Music", "Music The"),
            ("End of Life", "End of Live"),
            ("Spirit", "Spirits"),
            ("abc", "xyz"),
            ("Last Ones Standing", "Last One Standing"),
        ];
        for (a, b) in pairs {
            let ab = similarity(a, b);
            assert!((ab - similarity(b, a)).abs() < 1e-12, "{a} / {b}");
            assert!((0.0..=1.0).contains(&ab), "{a} / {b}: {ab}");
        }
        assert!((similarity("The Music", "Music The") - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn artist_similarity_partial_and_disjoint() {
        let partial = artist_similarity("Mastiksoul, The Groove, Sing", "Mastiksoul feat. The Groove");
        assert!((0.75..1.0).contains(&partial), "{partial}");
        assert!((artist_similarity("Balthazar & JackRock", "JackRock, Balthazar") - 1.0).abs() < f64::EPSILON);
        assert!(artist_similarity("Armin van Buuren", "Deadmau5") < 0.1);
        assert!((artist_similarity("", "") - 1.0).abs() < f64::EPSILON);
        assert!(artist_similarity("A", "").abs() < f64::EPSILON);
        let ab = artist_similarity("A, B, C", "A");
        assert!((ab - artist_similarity("A", "A, B, C")).abs() < 1e-12);
    }
}

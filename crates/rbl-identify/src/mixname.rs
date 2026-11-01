//! Mix names: parsing "(Extended Mix)", "- X Remix" and friends out of a
//! title, and deciding whether two mixes are the same recording.
//!
//! The spec is explicit that a remix and the original are never the same
//! track, so [`agree`] treats any named version (remix, edit, dub, VIP,
//! instrumental) against anything else as a conflict.

use serde::Serialize;

use crate::normalize::{NAME_MATCH, similarity};

/// What kind of version a title names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum MixKind {
    /// No mix descriptor in the title.
    None,
    Original,
    /// `Extended Mix`, `Extended Version` and `Extended Edit`: the label's
    /// full-length DJ version.
    Extended,
    /// `Radio Edit`, `Radio Mix`, `Radio Version`, `Single Edit`, and a
    /// named radio edit such as `Tribal Radio Edit`.
    Radio,
    Club,
    Dub,
    Instrumental,
    /// `VIP`, `VIP Mix`, `<Name> VIP`.
    Vip,
    /// A remixer's version, holding the remixer as written: `TC`. Also
    /// `<Name> Dub`, `<Name> Rework` and `<Name> Bootleg`.
    Remix(String),
    /// An editor's version, holding the editor as written.
    Edit(String),
    /// Mix text this module does not recognise, as written: `Acoustic Mix`.
    Other(String),
}

/// A title split into its base and mix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParsedTitle {
    /// The title without the mix descriptor. A featuring group stays.
    pub base: String,
    pub mix: MixKind,
    /// The descriptor as written (`Extended Mix`, `TC Remix`); empty for
    /// [`MixKind::None`].
    pub mix_text: String,
}

/// How two mixes relate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum MixAgreement {
    /// The same version.
    Same,
    /// Different names for what is normally the same recording
    /// (`Original Mix` and `Extended Mix` on Beatport).
    Compatible,
    /// Not enough information to tell.
    Unknown,
    /// Different recordings.
    Conflict,
}

/// Splits a title into base and mix.
///
/// Only the end of the title is looked at: the last `(...)` or `[...]`
/// group, or else a trailing ` - X` part, and only when it is a mix
/// descriptor. Mix-like words elsewhere stay in the base (`Choctaw Dub
/// (Original Mix)`, `Dub Stab!`). A trailing featuring group is skipped
/// over, so `Song (Extended Mix) (feat. X)` gives base `Song (feat. X)`
/// and `Extended`.
pub fn parse_title(title: &str) -> ParsedTitle {
    let title = collapse(title);
    if let Some((before, inner)) = trailing_group(&title) {
        if let Some(mix) = classify(inner) {
            return parsed(before, mix, inner);
        }
        if is_featuring(inner) {
            let feat_group = title.get(before.len()..).unwrap_or("").trim();
            let earlier = trailing_group(before)
                .and_then(|(base, inner2)| classify(inner2).map(|mix| (base, inner2, mix)));
            if let Some((base, inner2, mix)) = earlier {
                return parsed(&format!("{base} {feat_group}"), mix, inner2);
            }
        }
        return unparsed(&title);
    }
    let dashed = trailing_dash(&title).and_then(|(before, tail)| classify(tail).map(|mix| (before, tail, mix)));
    if let Some((before, tail, mix)) = dashed {
        return parsed(before, mix, tail);
    }
    unparsed(&title)
}

/// [`parse_title`] for a MusicBrainz recording: when the title carries no
/// mix, a disambiguation that is a mix descriptor (`extended mix`, `radio
/// edit`, `X remix`) supplies it.
pub fn parse_candidate(title: &str, disambiguation: &str) -> ParsedTitle {
    let mut parsed = parse_title(title);
    if parsed.mix == MixKind::None {
        if let Some(mix) = classify(disambiguation) {
            parsed.mix = mix;
            parsed.mix_text = collapse(disambiguation);
        }
    }
    parsed
}

/// Whether two mixes are the same recording. Symmetric.
///
/// Remix and edit names (and `Other` text) are compared with
/// [`similarity`] ≥ 0.85.
// One arm per rule reads as the rule table; merging equal bodies would not.
#[allow(clippy::match_same_arms)]
pub fn agree(a: &MixKind, b: &MixKind) -> MixAgreement {
    use MixAgreement::{Compatible, Conflict, Same, Unknown};
    use MixKind::{Club, Dub, Edit, Extended, Instrumental, None, Original, Other, Radio, Remix, Vip};
    match (a, b) {
        (Remix(x), Remix(y)) | (Edit(x), Edit(y)) => {
            if similarity(x, y) >= NAME_MATCH {
                Same
            } else {
                Conflict
            }
        }
        // Never treat a remix and the original as the same track.
        (Remix(_) | Edit(_), _) | (_, Remix(_) | Edit(_)) => Conflict,
        (Dub, Dub) | (Instrumental, Instrumental) | (Vip, Vip) => Same,
        (Dub | Instrumental | Vip, _) | (_, Dub | Instrumental | Vip) => Conflict,
        (Other(x), Other(y)) => {
            if similarity(x, y) >= NAME_MATCH {
                Same
            } else {
                Unknown
            }
        }
        (Other(_), _) | (_, Other(_)) => Unknown,
        (None, None) | (Original, Original) | (Extended, Extended) | (Radio, Radio) | (Club, Club) => Same,
        (Original | None, Extended) | (Extended | None, Original) | (Original | Extended, None) => Compatible,
        (Radio, Original | Extended | Club) | (Original | Extended | Club, Radio) => Conflict,
        (Radio, None) | (None, Radio) => Unknown,
        (Club, Original | Extended | None) | (Original | Extended | None, Club) => Unknown,
    }
}

/// `The Music` + `Extended Mix` → `The Music (Extended Mix)`, the store
/// convention the spec asks for. An empty mix gives the base alone.
pub fn format_title(base: &str, mix_text: &str) -> String {
    let base = base.trim();
    let mix_text = mix_text.trim();
    if mix_text.is_empty() {
        base.to_owned()
    } else {
        format!("{base} ({mix_text})")
    }
}

/// Reads a mix descriptor (the text inside the brackets or after the dash).
/// `None` when the text is not one.
// One arm per descriptor family, as for `agree`.
#[allow(clippy::match_same_arms)]
fn classify(text: &str) -> Option<MixKind> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() || is_featuring(text) {
        return Option::None;
    }
    let lower: Vec<String> = words.iter().map(|w| w.to_lowercase()).collect();
    let lower: Vec<&str> = lower.iter().map(String::as_str).collect();
    // Everything before the last `n` words, as written: the remixer's name.
    let name = |n: usize| remixer(&words[..words.len() - n]);

    let kind = match lower.as_slice() {
        ["original"] | ["original", "mix" | "version"] => MixKind::Original,
        // "Extended Edit" is the label's lengthened DJ version, not an
        // editor's re-edit, so it sits with Extended (and is therefore
        // compatible with an Original Mix).
        ["extended"] | ["extended", "mix" | "version" | "edit"] => MixKind::Extended,
        ["radio"] | [.., "radio", "edit" | "mix" | "version"] => MixKind::Radio,
        // Single/short edits are radio-length cuts.
        ["single" | "short", "edit" | "version" | "mix"] => MixKind::Radio,
        ["club"] | ["club", "mix" | "version" | "edit"] => MixKind::Club,
        ["dub"] | ["dub", "mix" | "version"] => MixKind::Dub,
        ["instrumental"] | ["instrumental", "mix" | "version"] => MixKind::Instrumental,
        ["vip"] | ["vip", "mix" | "edit"] | [.., "vip"] | [.., "vip", "mix"] => MixKind::Vip,
        // A named dub is that producer's version of the track: a remix.
        [_, .., "dub"] => MixKind::Remix(name(1)),
        [_, .., "dub", "mix"] => MixKind::Remix(name(2)),
        [.., "remix" | "rmx" | "rework" | "bootleg"] => MixKind::Remix(name(1)),
        [.., "edit" | "re-edit" | "reedit"] => MixKind::Edit(name(1)),
        [.., "mix" | "version"] => MixKind::Other(collapse(text)),
        _ => return Option::None,
    };
    Some(kind)
}

/// The remixer from the words before `Remix`: drops a possessive (`TC's`)
/// and a trailing `Extended` (`Yan Oxygen Extended Remix` is the long cut of
/// Yan Oxygen's remix, the same version for matching).
fn remixer(words: &[&str]) -> String {
    let mut words = words.to_vec();
    if words.last().is_some_and(|w| w.eq_ignore_ascii_case("extended")) {
        words.pop();
    }
    let mut name = words.join(" ");
    for suffix in ["'s", "\u{2019}s", "'S", "\u{2019}S"] {
        if let Some(stripped) = name.strip_suffix(suffix) {
            name = stripped.to_owned();
            break;
        }
    }
    name
}

/// A group whose text starts with a featuring marker: not a mix.
fn is_featuring(text: &str) -> bool {
    let first = text.split_whitespace().next().unwrap_or("").to_lowercase();
    matches!(first.as_str(), "feat" | "feat." | "ft" | "ft." | "featuring" | "with")
}

/// The last bracket group: (text before it, trimmed; text inside it).
/// `None` when the title does not end with a balanced group or the group is
/// all there is.
fn trailing_group(title: &str) -> Option<(&str, &str)> {
    let close = title.chars().next_back()?;
    let open = match close {
        ')' => '(',
        ']' => '[',
        _ => return Option::None,
    };
    let mut depth = 0usize;
    for (i, c) in title.char_indices().rev() {
        if c == close {
            depth += 1;
        } else if c == open {
            depth -= 1;
            if depth == 0 {
                let before = title.get(..i)?.trim_end();
                let inner = title.get(i + 1..title.len() - 1)?.trim();
                return (!before.is_empty()).then_some((before, inner));
            }
        }
    }
    Option::None
}

/// A trailing ` - X` part: (text before, X).
fn trailing_dash(title: &str) -> Option<(&str, &str)> {
    let (at, dash) = [" - ", " \u{2013} ", " \u{2014} "]
        .into_iter()
        .filter_map(|d| title.rfind(d).map(|at| (at, d)))
        .max_by_key(|(at, _)| *at)?;
    let before = title.get(..at)?.trim_end();
    let tail = title.get(at + dash.len()..)?.trim();
    (!before.is_empty() && !tail.is_empty()).then_some((before, tail))
}

fn parsed(base: &str, mix: MixKind, mix_text: &str) -> ParsedTitle {
    let base = collapse(base);
    let base = base.trim_end_matches([' ', '-']).to_owned();
    ParsedTitle {
        base,
        mix,
        mix_text: collapse(mix_text),
    }
}

fn unparsed(title: &str) -> ParsedTitle {
    ParsedTitle {
        base: title.to_owned(),
        mix: MixKind::None,
        mix_text: String::new(),
    }
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn check(title: &str, base: &str, mix: MixKind, mix_text: &str) {
        let got = parse_title(title);
        assert_eq!(
            got,
            ParsedTitle {
                base: base.to_owned(),
                mix,
                mix_text: mix_text.to_owned(),
            },
            "{title}"
        );
    }

    fn remix(name: &str) -> MixKind {
        MixKind::Remix(name.to_owned())
    }

    #[test]
    fn parse_real_titles() {
        check("The Music (Extended Mix)", "The Music", MixKind::Extended, "Extended Mix");
        check("The Music (Radio Version)", "The Music", MixKind::Radio, "Radio Version");
        check("End of Life (Original Mix)", "End of Life", MixKind::Original, "Original Mix");
        check("Last Ones Standing - TC Remix", "Last Ones Standing", remix("TC"), "TC Remix");
        check(
            "Tempted feat. Sarah Howells (Extended Mix)",
            "Tempted feat. Sarah Howells",
            MixKind::Extended,
            "Extended Mix",
        );
        check("Choctaw Dub (Original Mix)", "Choctaw Dub", MixKind::Original, "Original Mix");
        check("Dub Stab!", "Dub Stab!", MixKind::None, "");
        check("Infinity 2008", "Infinity 2008", MixKind::None, "");
        check("Voodoo (Tribal Radio Edit)", "Voodoo", MixKind::Radio, "Tribal Radio Edit");
        check("Spirit (Aerofoil Remix)", "Spirit", remix("Aerofoil"), "Aerofoil Remix");
        check("Aurora (Yan Oxygen Remix)", "Aurora", remix("Yan Oxygen"), "Yan Oxygen Remix");
    }

    #[test]
    fn parse_every_descriptor_form() {
        check("T (Original)", "T", MixKind::Original, "Original");
        check("T (original mix)", "T", MixKind::Original, "original mix");
        check("T [Extended]", "T", MixKind::Extended, "Extended");
        check("T (Extended Version)", "T", MixKind::Extended, "Extended Version");
        check("T (Extended Edit)", "T", MixKind::Extended, "Extended Edit");
        check("T (Radio Edit)", "T", MixKind::Radio, "Radio Edit");
        check("T (Radio Mix)", "T", MixKind::Radio, "Radio Mix");
        check("T (Radio)", "T", MixKind::Radio, "Radio");
        check("T (Club Mix)", "T", MixKind::Club, "Club Mix");
        check("T (Dub)", "T", MixKind::Dub, "Dub");
        check("T (Dub Mix)", "T", MixKind::Dub, "Dub Mix");
        check("T (Chris Deepak Dub)", "T", remix("Chris Deepak"), "Chris Deepak Dub");
        check("T (Instrumental)", "T", MixKind::Instrumental, "Instrumental");
        check("T (VIP)", "T", MixKind::Vip, "VIP");
        check("T (VIP Mix)", "T", MixKind::Vip, "VIP Mix");
        check("T (TC Rmx)", "T", remix("TC"), "TC Rmx");
        check("T (TC's Remix)", "T", remix("TC"), "TC's Remix");
        check("T (Yan Oxygen Extended Remix)", "T", remix("Yan Oxygen"), "Yan Oxygen Extended Remix");
        check("T (Dimitri Edit)", "T", MixKind::Edit("Dimitri".to_owned()), "Dimitri Edit");
        check("T (Acoustic Mix)", "T", MixKind::Other("Acoustic Mix".to_owned()), "Acoustic Mix");
        check("T - Extended Mix", "T", MixKind::Extended, "Extended Mix");
        check("T \u{2013} Radio Edit", "T", MixKind::Radio, "Radio Edit");
    }

    #[test]
    fn parse_leaves_non_mixes_in_the_base() {
        check("Song (feat. X)", "Song (feat. X)", MixKind::None, "");
        check("Song [ft. X]", "Song [ft. X]", MixKind::None, "");
        check(
            "Song (Extended Mix) (feat. X)",
            "Song (feat. X)",
            MixKind::Extended,
            "Extended Mix",
        );
        check("Song (2008 Remaster)", "Song (2008 Remaster)", MixKind::None, "");
        check("Song - Part 2", "Song - Part 2", MixKind::None, "");
        check("(Extended Mix)", "(Extended Mix)", MixKind::None, "");
        check("Unbalanced (Extended Mix", "Unbalanced (Extended Mix", MixKind::None, "");
        check("Radio", "Radio", MixKind::None, "");
        check("", "", MixKind::None, "");
    }

    #[test]
    fn parse_candidate_uses_disambiguation() {
        let p = parse_candidate("The Music", "extended mix");
        assert_eq!((p.base.as_str(), &p.mix, p.mix_text.as_str()), ("The Music", &MixKind::Extended, "extended mix"));
        assert_eq!(parse_candidate("The Music", "radio edit").mix, MixKind::Radio);
        assert_eq!(parse_candidate("Spirit", "Aerofoil remix").mix, remix("Aerofoil"));
        // The title's own mix wins.
        assert_eq!(parse_candidate("The Music (Radio Edit)", "extended mix").mix, MixKind::Radio);
        // A disambiguation that is not a mix is ignored.
        let p = parse_candidate("The Music", "live, 2008-05-01: Berlin");
        assert_eq!((p.mix, p.mix_text.as_str()), (MixKind::None, ""));
    }

    fn all_kinds() -> Vec<MixKind> {
        vec![
            MixKind::None,
            MixKind::Original,
            MixKind::Extended,
            MixKind::Radio,
            MixKind::Club,
            MixKind::Dub,
            MixKind::Instrumental,
            MixKind::Vip,
            remix("TC"),
            remix("Aerofoil"),
            MixKind::Edit("TC".to_owned()),
            MixKind::Other("Acoustic Mix".to_owned()),
            MixKind::Other("Piano Version".to_owned()),
        ]
    }

    #[test]
    fn agree_is_symmetric_and_reflexive() {
        let kinds = all_kinds();
        for a in &kinds {
            assert_eq!(agree(a, a), MixAgreement::Same, "{a:?}");
            for b in &kinds {
                assert_eq!(agree(a, b), agree(b, a), "{a:?} / {b:?}");
            }
        }
    }

    #[test]
    fn agree_matrix() {
        use MixAgreement::{Compatible, Conflict, Same, Unknown};
        use MixKind::{Club, Dub, Extended, Instrumental, None, Original, Radio, Vip};
        let other = |s: &str| MixKind::Other(s.to_owned());
        let edit = |s: &str| MixKind::Edit(s.to_owned());
        let cases = [
            (Original, Extended, Compatible),
            (None, Original, Compatible),
            (None, Extended, Compatible),
            (Radio, Original, Conflict),
            (Radio, Extended, Conflict),
            (Radio, Club, Conflict),
            (Radio, None, Unknown),
            (Club, Original, Unknown),
            (Club, Extended, Unknown),
            (Club, None, Unknown),
            // The spec: a remix is never the original.
            (remix("TC"), Original, Conflict),
            (remix("TC"), Extended, Conflict),
            (remix("TC"), None, Conflict),
            (remix("TC"), Radio, Conflict),
            (remix("TC"), remix("tc"), Same),
            (remix("Yan Oxygen"), remix("Yan Oxygen"), Same),
            (remix("TC"), remix("Aerofoil"), Conflict),
            (remix("TC"), edit("TC"), Conflict),
            (edit("TC"), None, Conflict),
            (edit("TC"), edit("Dimitri"), Conflict),
            (Dub, None, Conflict),
            (Dub, Original, Conflict),
            (Instrumental, None, Conflict),
            (Instrumental, Extended, Conflict),
            (Vip, None, Conflict),
            (Vip, Dub, Conflict),
            (Dub, other("Acoustic Mix"), Conflict),
            (other("Acoustic Mix"), other("acoustic mix"), Same),
            (other("Acoustic Mix"), other("Piano Version"), Unknown),
            (other("Acoustic Mix"), None, Unknown),
            (other("Acoustic Mix"), Original, Unknown),
            (other("Acoustic Mix"), Radio, Unknown),
            (remix("TC"), other("TC Mix"), Conflict),
            (None, None, Same),
        ];
        for (a, b, want) in cases {
            assert_eq!(agree(&a, &b), want, "{a:?} / {b:?}");
            assert_eq!(agree(&b, &a), want, "{b:?} / {a:?}");
        }
    }

    #[test]
    fn format_title_appends_the_mix() {
        assert_eq!(format_title("The Music", "Extended Mix"), "The Music (Extended Mix)");
        assert_eq!(format_title("The Music", ""), "The Music");
        assert_eq!(format_title(" The Music ", "  "), "The Music");
        let p = parse_title("Last Ones Standing - TC Remix");
        assert_eq!(format_title(&p.base, &p.mix_text), "Last Ones Standing (TC Remix)");
    }
}

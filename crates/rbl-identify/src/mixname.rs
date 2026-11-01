//! Mix names: parsing "(Extended Mix)", "- X Remix" and friends out of a
//! title, and deciding whether two mixes are the same recording.
//!
//! The spec is explicit that a remix and the original are never the same
//! track, so [`agree`] treats any named version (remix, edit, named dub,
//! dub, VIP, instrumental, or unrecognised mix text) against anything else
//! as a conflict. When in doubt a descriptor is read as a remix: wrongly
//! rejecting a right candidate costs a review, wrongly accepting a remix
//! mis-tags a file.

use serde::Serialize;

use crate::normalize::{normalize, similarity, NAME_MATCH};

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
    /// `Album Mix`/`Album Version`: often the original, sometimes an
    /// album-only edit, so it behaves like `Club`: unknown against the
    /// full-length kinds, a conflict with a radio edit.
    Album,
    Dub,
    Instrumental,
    /// `VIP`, `VIP Mix`, `<Name> VIP`.
    Vip,
    /// A remixer's version, holding the remixer as written: `TC`. Every
    /// remix word counts (`Rework`, `Bootleg`, `Refix`, `Flip`, ...), as does
    /// `<Name> Mix`/`<Name> Version` when the name is not a generic word,
    /// and a remix's radio edit (`TC Remix Radio Edit`). Empty when the
    /// descriptor names no one.
    Remix(String),
    /// `<Name> Dub`: a remixer's dub, a different recording from the same
    /// remixer's `<Name> Remix`.
    NamedDub(String),
    /// An editor's version, holding the editor as written.
    Edit(String),
    /// A generic mix this module has no kind for, as written: `Acoustic
    /// Mix`, `Vocal Mix`, `Clean Edit`.
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
/// Only trailing parts are looked at: `(...)`/`[...]` groups and ` - X`
/// parts, from the end backwards. The first that is a mix descriptor wins;
/// parts that are not (featuring, `(2014 Remaster)`, `[Free Download]`,
/// `(Live)`) are stepped over and stay in the base, so `Song (Extended Mix)
/// (feat. X)` gives base `Song (feat. X)` and `Extended`. Mix-like words
/// outside those parts stay in the base (`Choctaw Dub (Original Mix)`,
/// `Dub Stab!`).
///
/// Backstop: when no part is a descriptor but one mentions a remix, edit,
/// dub or VIP (`(Remixed for Radio)`), the nearest such part becomes
/// `Remix(<its text>)`, which conflicts with everything but itself.
///
/// A length cut stacked on a named version (`Song - TC Remix - Radio Edit`,
/// `Song (TC Remix) (Radio Edit)`) is that version: the remix wins, and the
/// mix text joins both (`TC Remix Radio Edit`).
pub fn parse_title(title: &str) -> ParsedTitle {
    let outer = parse_once(title);
    if !matches!(
        outer.mix,
        MixKind::Original | MixKind::Extended | MixKind::Radio | MixKind::Club | MixKind::Album
    ) {
        return outer;
    }
    let inner = parse_once(&outer.base);
    if matches!(
        inner.mix,
        MixKind::Remix(_) | MixKind::NamedDub(_) | MixKind::Edit(_) | MixKind::Vip | MixKind::Dub | MixKind::Instrumental
    ) {
        return ParsedTitle {
            mix_text: format!("{} {}", inner.mix_text, outer.mix_text),
            ..inner
        };
    }
    outer
}

/// One pass of [`parse_title`]: the nearest trailing part that classifies.
fn parse_once(title: &str) -> ParsedTitle {
    let title = collapse(title);
    let mut rest: &str = &title;
    // Parts stepped over, as written, in title order: they stay in the base.
    let mut kept: Vec<&str> = Vec::new();
    // The nearest part that mentions a version: (base without it, its text).
    let mut fallback: Option<(String, &str)> = Option::None;
    // Each step shortens `rest`, so this ends.
    while let Some((before, part)) = trailing_group(rest).or_else(|| trailing_dash(rest)) {
        let raw = rest.get(before.len()..).unwrap_or("").trim();
        let base = if kept.is_empty() {
            before.to_owned()
        } else {
            format!("{before} {}", kept.join(" "))
        };
        if let Some(mix) = classify(part) {
            return parsed(&base, mix, part);
        }
        if fallback.is_none() && !is_featuring(part) && mentions_version(part) {
            fallback = Some((base, part));
        }
        kept.insert(0, raw);
        rest = before;
    }
    match fallback {
        Some((base, part)) => parsed(&base, MixKind::Remix(collapse(part)), part),
        Option::None => unparsed(&title),
    }
}

/// [`parse_title`] for a MusicBrainz recording: when the title carries no
/// mix, a disambiguation that is a mix descriptor (`extended mix`, `radio
/// edit`, `X remix`, `remix by X`) supplies it.
///
/// A disambiguation that is not a descriptor but mentions a remix, edit,
/// dub or VIP anywhere (`2008 edit for the reissue`) is taken as a remix
/// named by the whole text, so it conflicts with everything but itself.
pub fn parse_candidate(title: &str, disambiguation: &str) -> ParsedTitle {
    let mut parsed = parse_title(title);
    if parsed.mix != MixKind::None {
        return parsed;
    }
    let text = collapse(disambiguation);
    let mix = classify(&text).or_else(|| mentions_version(&text).then(|| MixKind::Remix(text.clone())));
    if let Some(mix) = mix {
        parsed.mix = mix;
        parsed.mix_text = text;
    }
    parsed
}

/// Whether two mixes are the same recording. Symmetric.
///
/// Remixer and editor names are equal when they match ignoring spaces and
/// dots (`TC` and `T.C.`) or have [`similarity`] ≥ 0.85; `Other` text is
/// compared by similarity. A generic `Other` mix (`Vocal Mix`) conflicts
/// with every other kind: it is a different recording from the original.
// One arm per rule reads as the rule table; merging equal bodies would not.
#[allow(clippy::match_same_arms)]
pub fn agree(a: &MixKind, b: &MixKind) -> MixAgreement {
    use MixAgreement::{Compatible, Conflict, Same, Unknown};
    use MixKind::{Album, Club, Dub, Edit, Extended, Instrumental, NamedDub, None, Original, Other, Radio, Remix, Vip};
    match (a, b) {
        (Remix(x), Remix(y)) | (Edit(x), Edit(y)) | (NamedDub(x), NamedDub(y)) => {
            if same_name(x, y) {
                Same
            } else {
                Conflict
            }
        }
        // Never treat a remix and the original as the same track.
        (Remix(_) | Edit(_) | NamedDub(_), _) | (_, Remix(_) | Edit(_) | NamedDub(_)) => Conflict,
        (Dub, Dub) | (Instrumental, Instrumental) | (Vip, Vip) => Same,
        (Dub | Instrumental | Vip, _) | (_, Dub | Instrumental | Vip) => Conflict,
        (Other(x), Other(y)) => {
            if similarity(x, y) >= NAME_MATCH {
                Same
            } else {
                Conflict
            }
        }
        (Other(_), _) | (_, Other(_)) => Conflict,
        (None, None) | (Original, Original) | (Extended, Extended) | (Radio, Radio) | (Club, Club) | (Album, Album) => {
            Same
        }
        (Original | None, Extended) | (Extended | None, Original) | (Original | Extended, None) => Compatible,
        (Radio, Original | Extended | Club | Album) | (Original | Extended | Club | Album, Radio) => Conflict,
        (Radio, None) | (None, Radio) => Unknown,
        (Club | Album, Original | Extended | None) | (Original | Extended | None, Club | Album) => Unknown,
        (Club, Album) | (Album, Club) => Unknown,
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

/// Words that make a descriptor a remixer's version.
const REMIX_WORDS: [&str; 14] = [
    "remix",
    "rmx",
    "re-mix",
    "rework",
    "re-work",
    "bootleg",
    "refix",
    "remode",
    "flip",
    "reconstruction",
    "interpretation",
    "remake",
    "rerub",
    "redux",
];

/// Words that describe a mix without naming anyone: `<these> Mix` is not a
/// remixer's mix.
const GENERIC_WORDS: [&str; 12] = [
    "vocal",
    "club",
    "dub",
    "radio",
    "extended",
    "original",
    "instrumental",
    "acoustic",
    "short",
    "single",
    "main",
    "album",
];

/// Reads a mix descriptor (the text inside the brackets or after the dash).
/// `None` when the text is not one.
// One arm per descriptor family, as for `agree`.
#[allow(clippy::match_same_arms)]
fn classify(text: &str) -> Option<MixKind> {
    if is_featuring(text) {
        return Option::None;
    }
    // Each word's byte offset in `text`, and the word lowercased without
    // edge punctuation (`remix,` is `remix`). Words that are only
    // punctuation (`&`, `-`) are left out of the matching but not out of
    // names, which are sliced from `text` itself: `A & B Remix` is by `A & B`.
    let mut starts: Vec<usize> = Vec::new();
    let mut bare_words: Vec<String> = Vec::new();
    let mut cursor = 0;
    for w in text.split_whitespace() {
        let start = cursor + text.get(cursor..).and_then(|t| t.find(w)).unwrap_or(0);
        cursor = start + w.len();
        let bare = w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
        if !bare.is_empty() {
            starts.push(start);
            bare_words.push(bare);
        }
    }
    if bare_words.is_empty() {
        return Option::None;
    }
    let lower: Vec<&str> = bare_words.iter().map(String::as_str).collect();
    // The text before word `i`, and from word `i` on.
    let up_to = |i: usize| text.get(..starts[i]).unwrap_or("");
    let from = |i: usize| text.get(starts[i]..).unwrap_or("");

    let generic = match lower.as_slice() {
        ["original"] | ["original", "mix" | "version"] => Some(MixKind::Original),
        // The main mix of a track is its original.
        ["main", "mix" | "version"] => Some(MixKind::Original),
        // An album version may be the original or an album-only edit.
        ["album", "mix" | "version"] => Some(MixKind::Album),
        // "Extended Edit" is the label's lengthened DJ version, not an
        // editor's re-edit, so it sits with Extended (and is therefore
        // compatible with an Original Mix).
        ["extended"] | ["extended", "mix" | "version" | "edit"] => Some(MixKind::Extended),
        ["original", "extended", "mix" | "version"] => Some(MixKind::Extended),
        ["radio"] | ["radio", "edit" | "mix" | "version"] => Some(MixKind::Radio),
        // Single/short edits are radio-length cuts.
        ["single" | "short", "edit" | "version" | "mix"] => Some(MixKind::Radio),
        ["club"] | ["club", "mix" | "version" | "edit"] => Some(MixKind::Club),
        ["dub"] | ["dub", "mix" | "version"] => Some(MixKind::Dub),
        ["instrumental"] | ["instrumental", "mix" | "version"] => Some(MixKind::Instrumental),
        ["clean" | "explicit" | "dirty", "edit" | "version" | "mix"] => Some(MixKind::Other(collapse(text))),
        _ => Option::None,
    };
    if generic.is_some() {
        return generic;
    }

    // A remix word anywhere makes it a remix, whatever follows: `TC Remix
    // Radio Edit` is TC's remix cut for radio, not the radio edit.
    if let Some(r) = lower.iter().rposition(|w| *w == "dub" || REMIX_WORDS.contains(w)) {
        let after = &lower[r + 1..];
        let by_name = (after.first() == Some(&"by") && after.len() > 1).then(|| remixer(from(r + 2)));
        let before = &lower[..r];
        if lower[r] == "dub" {
            if by_name.is_none() && before.iter().all(|w| GENERIC_WORDS.contains(w)) {
                return Some(MixKind::Dub);
            }
            return Some(MixKind::NamedDub(by_name.unwrap_or_else(|| remixer(up_to(r)))));
        }
        return Some(MixKind::Remix(by_name.unwrap_or_else(|| remixer(up_to(r)))));
    }

    // Everything before the last word, as written: the editor's or
    // remixer's name.
    let name = || remixer(up_to(lower.len() - 1));
    let kind = match lower.as_slice() {
        // A named radio edit with no remix word: `Tribal Radio Edit`.
        [.., "radio", "edit" | "mix" | "version"] => MixKind::Radio,
        [.., "vip"] | [.., "vip", "mix" | "edit"] => MixKind::Vip,
        [.., "edit" | "re-edit" | "reedit"] => MixKind::Edit(name()),
        [named @ .., "mix" | "version"] => {
            if named.iter().all(|w| GENERIC_WORDS.contains(w)) {
                MixKind::Other(collapse(text))
            } else {
                MixKind::Remix(name())
            }
        }
        _ => return Option::None,
    };
    Some(kind)
}

/// Whether text mentions a version of any kind: a remix word, `dub`,
/// `edit` or `vip`.
fn mentions_version(text: &str) -> bool {
    text.split_whitespace().any(|w| {
        let bare = w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
        REMIX_WORDS.contains(&bare.as_str())
            || bare.starts_with("remix")
            || matches!(bare.as_str(), "dub" | "edit" | "re-edit" | "reedit" | "vip")
    })
}

/// Remixer or editor names agree: equal ignoring case, accents, spaces and
/// dots (`TC`, `T.C.`, `T C`), or similar enough.
fn same_name(a: &str, b: &str) -> bool {
    let compact = |s: &str| normalize(s).replace(' ', "");
    compact(a) == compact(b) || similarity(a, b) >= NAME_MATCH
}

/// The remixer from the text around `Remix`: drops trailing punctuation
/// (`A & B -`), a possessive (`TC's`) and a trailing `Extended` (`Yan
/// Oxygen Extended Remix` is the long cut of Yan Oxygen's remix, the same
/// version for matching).
fn remixer(text: &str) -> String {
    let mut words: Vec<&str> = text.split_whitespace().collect();
    while words
        .last()
        .is_some_and(|w| w.eq_ignore_ascii_case("extended") || !w.chars().any(char::is_alphanumeric))
    {
        words.pop();
    }
    let joined = words.join(" ");
    let mut name = joined.trim_end_matches(',').to_owned();
    for suffix in ["'s", "\u{2019}s", "'S", "\u{2019}S"] {
        if let Some(stripped) = name.strip_suffix(suffix) {
            name = stripped.to_owned();
            break;
        }
    }
    name
}

/// A group whose text starts with a featuring marker: not a mix. `with` is
/// not a marker here: `(with X)` is stepped over anyway because it does not
/// classify, and `(With Love Remix)` must stay a remix.
fn is_featuring(text: &str) -> bool {
    let first = text.split_whitespace().next().unwrap_or("").to_lowercase();
    matches!(first.as_str(), "feat" | "feat." | "ft" | "ft." | "featuring")
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
        check("T (Chris Deepak Dub)", "T", named_dub("Chris Deepak"), "Chris Deepak Dub");
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
        check("Song (Live)", "Song (Live)", MixKind::None, "");
        check("Song (Live) [Bonus Track]", "Song (Live) [Bonus Track]", MixKind::None, "");
        check("", "", MixKind::None, "");
    }

    const BURIED_REMIXES: [(&str, &str); 5] = [
        ("Spirit (Aerofoil Remix) (2014 Remaster)", "Spirit (2014 Remaster)"),
        ("Spirit (Aerofoil Remix) [Free Download]", "Spirit [Free Download]"),
        ("Spirit (Aerofoil Remix) (Live)", "Spirit (Live)"),
        ("Spirit (Aerofoil Remix) [Bonus Track]", "Spirit [Bonus Track]"),
        ("Spirit - Aerofoil Remix - 2014 Remaster", "Spirit - 2014 Remaster"),
    ];

    #[test]
    fn a_mix_behind_non_mix_parts_is_found() {
        for (title, base) in BURIED_REMIXES {
            check(title, base, remix("Aerofoil"), "Aerofoil Remix");
            let p = parse_candidate(title, "");
            assert_eq!(p.mix, remix("Aerofoil"), "{title}");
        }
        check(
            "Song (Extended Mix) (feat. X) (2014 Remaster)",
            "Song (feat. X) (2014 Remaster)",
            MixKind::Extended,
            "Extended Mix",
        );
    }

    #[test]
    fn a_part_mentioning_a_version_is_a_remix() {
        check("Song (Remixed for Radio)", "Song", remix("Remixed for Radio"), "Remixed for Radio");
        check(
            "Song (2009 Edit for the reissue) (Live)",
            "Song (Live)",
            remix("2009 Edit for the reissue"),
            "2009 Edit for the reissue",
        );
        // A featuring group never is.
        check("Song (feat. DJ Edit)", "Song (feat. DJ Edit)", MixKind::None, "");
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
            MixKind::Album,
            MixKind::Dub,
            MixKind::Instrumental,
            MixKind::Vip,
            remix("TC"),
            remix("Aerofoil"),
            MixKind::Edit("TC".to_owned()),
            MixKind::Other("Acoustic Mix".to_owned()),
            MixKind::Other("Piano Version".to_owned()),
            named_dub("TC"),
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
        use MixKind::{Album, Club, Dub, Extended, Instrumental, None, Original, Radio, Vip};
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
            // An album version behaves like a club mix.
            (Album, Original, Unknown),
            (Album, Extended, Unknown),
            (Album, None, Unknown),
            (Album, Club, Unknown),
            (Album, Radio, Conflict),
            (Album, Album, Same),
            (Album, remix("TC"), Conflict),
            (Album, Dub, Conflict),
            (Album, other("Vocal Mix"), Conflict),
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
            // A generic other mix is its own recording.
            (other("Acoustic Mix"), other("Piano Version"), Conflict),
            (other("Acoustic Mix"), None, Conflict),
            (other("Acoustic Mix"), Original, Conflict),
            (other("Acoustic Mix"), Extended, Conflict),
            (other("Acoustic Mix"), Radio, Conflict),
            // A remixer's dub is not their remix.
            (named_dub("TC"), remix("TC"), Conflict),
            (named_dub("TC"), named_dub("T.C."), Same),
            (named_dub("TC"), Dub, Conflict),
            (named_dub("TC"), None, Conflict),
            // Names ignore dots and spaces.
            (remix("TC"), remix("T.C."), Same),
            (remix("T C"), remix("TC"), Same),
            (edit("T.C."), edit("TC"), Same),
            (remix("TC"), other("TC Mix"), Conflict),
            (None, None, Same),
        ];
        for (a, b, want) in cases {
            assert_eq!(agree(&a, &b), want, "{a:?} / {b:?}");
            assert_eq!(agree(&b, &a), want, "{b:?} / {a:?}");
        }
    }

    fn named_dub(name: &str) -> MixKind {
        MixKind::NamedDub(name.to_owned())
    }

    #[test]
    fn every_remix_word_is_a_remix() {
        for word in [
            "Remix",
            "Rmx",
            "Re-Mix",
            "Rework",
            "Re-Work",
            "Bootleg",
            "Refix",
            "Remode",
            "Flip",
            "Reconstruction",
            "Interpretation",
            "Remake",
            "Rerub",
            "Redux",
        ] {
            let p = parse_title(&format!("Song (TC {word})"));
            assert_eq!(p.mix, remix("TC"), "{word}");
            assert_eq!(p.base, "Song", "{word}");
        }
    }

    #[test]
    fn named_mix_or_version_is_a_remix_unless_generic() {
        check("T (Yan Oxygen Mix)", "T", remix("Yan Oxygen"), "Yan Oxygen Mix");
        check("T (Armin Version)", "T", remix("Armin"), "Armin Version");
        check("T (Vocal Mix)", "T", MixKind::Other("Vocal Mix".to_owned()), "Vocal Mix");
        check("T (Main Vocal Mix)", "T", MixKind::Other("Main Vocal Mix".to_owned()), "Main Vocal Mix");
        check("T (Acoustic Version)", "T", MixKind::Other("Acoustic Version".to_owned()), "Acoustic Version");
        check("T (Album Version)", "T", MixKind::Album, "Album Version");
        check("T (Album Mix)", "T", MixKind::Album, "Album Mix");
        check("T (A & B Remix)", "T", remix("A & B"), "A & B Remix");
        check("T (A & B Mix)", "T", remix("A & B"), "A & B Mix");
        check("T (Main Mix)", "T", MixKind::Original, "Main Mix");
        check("T (Original Extended Mix)", "T", MixKind::Extended, "Original Extended Mix");
        check("T (Vocal Dub)", "T", MixKind::Dub, "Vocal Dub");
    }

    #[test]
    fn a_remix_radio_edit_is_the_remix() {
        check("T (TC Remix Radio Edit)", "T", remix("TC"), "TC Remix Radio Edit");
        check("T - TC Remix - Radio Edit", "T", remix("TC"), "TC Remix Radio Edit");
        check("T (TC Remix) (Radio Edit)", "T", remix("TC"), "TC Remix Radio Edit");
        check("T (Dub) [Extended Mix]", "T", MixKind::Dub, "Dub Extended Mix");
        // A remix-like word in the base is not a stacked mix.
        check("Remix Me (Radio Edit)", "Remix Me", MixKind::Radio, "Radio Edit");
        check("T (TC Rework Radio Version)", "T", remix("TC"), "TC Rework Radio Version");
        check("T (Chris Deepak Dub Radio Edit)", "T", named_dub("Chris Deepak"), "Chris Deepak Dub Radio Edit");
        // A plain named radio edit stays Radio.
        check("Voodoo (Tribal Radio Edit)", "Voodoo", MixKind::Radio, "Tribal Radio Edit");
    }

    #[test]
    fn with_is_only_featuring_when_not_a_mix() {
        check("T (With Love Remix)", "T", remix("With Love"), "With Love Remix");
        check("T (with Someone)", "T (with Someone)", MixKind::None, "");
        check("T (Extended Mix) (with Someone)", "T (with Someone)", MixKind::Extended, "Extended Mix");
    }

    #[test]
    fn disambiguation_mentioning_a_version_conflicts() {
        assert_eq!(parse_candidate("T", "remix by Yan Oxygen").mix, remix("Yan Oxygen"));
        assert_eq!(parse_candidate("T", "extended remix by X").mix, remix("X"));
        assert_eq!(parse_candidate("T", "TC remix, radio edit").mix, remix("TC"));
        let odd = parse_candidate("T", "2008 edit for the reissue");
        assert_eq!(odd.mix, remix("2008 edit for the reissue"));
        assert_eq!(agree(&odd.mix, &MixKind::None), MixAgreement::Conflict);
        assert_eq!(agree(&odd.mix, &MixKind::Original), MixAgreement::Conflict);
        let vip = parse_candidate("T", "the VIP from the 2019 tour");
        assert_eq!(agree(&vip.mix, &MixKind::Extended), MixAgreement::Conflict);
        // Nothing version-like: no mix.
        assert_eq!(parse_candidate("T", "live, Berlin").mix, MixKind::None);
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

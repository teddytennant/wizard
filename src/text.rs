//! Text predicates shared by everything that puts somebody else's words on a
//! screen.
//!
//! There is one question here — *is a conforming renderer entitled to draw this
//! character as nothing at all* — and three callers that have to agree about
//! the answer: the mesh plugin sanitising a peer-chosen name at the wire
//! boundary, [`crate::memory`] storing a note, and [`crate::tools::http`]
//! defanging a fetched page. They agreed by accident before, because the table
//! lived in `mesh` and the other two reached across for it.
//!
//! That accident does not survive the plugin boundary. `mesh` is a subsystem on
//! its way out of core and the web tools are already out; an edge from one to
//! the other is a plugin-to-plugin dependency, which `docs/plugins.md` rules
//! out for the reason it always gives — deleting one plugin would break the
//! other. So the predicate moves down instead of sideways. Core holds the one
//! audited answer and everybody asks core.
//!
//! Nothing about the table changed in the move. What is invisible is a property
//! of Unicode, not of the mesh.

/// Whether a conforming renderer is entitled to draw `ch` as nothing at all.
///
/// The predicate a sanitiser actually needs, and not one `char` exposes.
/// [`char::is_control`] is `General_Category=Cc` and nothing else, and
/// [`char::is_whitespace`] is the `White_Space` property; between them they
/// miss every zero-width character there is. `U+200B`, `U+FEFF`, `U+2060`,
/// `U+00AD` and the whole `U+E0000` Tag block are `Cf`, are not `White_Space`,
/// and used to walk straight through the mesh's boundary.
///
/// The tables below are by Unicode *category and property*, not by a list of
/// characters somebody remembered:
///
/// - `Cf` (Format), the whole category.
/// - `Co` (Private Use), all three ranges. A private-use code point renders as
///   whatever a font decides, up to and including nothing, and two peers can
///   pick two different ones that both draw blank.
/// - Noncharacters (`U+FDD0`..`U+FDEF` and the last two code points of every
///   plane).
/// - `Default_Ignorable_Code_Point`, which is the property that actually
///   *means* "draw this as nothing" and which reaches past `Cf` into `Mn` (the
///   variation selectors, the combining grapheme joiner), `Lo` (the Hangul
///   fillers, which are letters that draw blank) and reserved `Cn`.
///
/// Two categories are deliberately absent. `Cs` (Surrogate) cannot exist in a
/// Rust `char` at all. `Cn` (Unassigned) is *not* included wholesale, on
/// purpose: an unassigned code point renders as a visible `.notdef` box rather
/// than as nothing, so it is not a confusable, and a hardcoded unassigned
/// table would go stale in the dangerous direction, silently deleting
/// characters from names as later Unicode versions assign them. The parts of
/// `Cn` that *are* invisible (noncharacters, and the reserved code points
/// inside the default-ignorable ranges) are covered above and will never be
/// assigned to anything else.
pub(crate) fn is_invisible(ch: char) -> bool {
    // Checked first although the format table below also covers every one of
    // them: the trojan-source set is the one an implementer verifies by hand,
    // and a table is not a checklist.
    is_bidi_control(ch)
        || is_format(ch)
        || is_default_ignorable(ch)
        || is_private_use(ch)
        || is_noncharacter(ch)
}
/// Bidirectional formatting characters: text that renders in an order other
/// than the one it is stored in.
///
/// The full trojan-source set: LRM/RLM/ALM, the LRE/RLE/PDF/LRO/RLO
/// embeddings and overrides, and the LRI/RLI/FSI/PDI isolates.
fn is_bidi_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

/// `General_Category=Cf` (Format), in full.
fn is_format(ch: char) -> bool {
    matches!(
        ch,
        '\u{00ad}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061c}'
            | '\u{06dd}'
            | '\u{070f}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08e2}'
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
            | '\u{e0001}'
            | '\u{e0020}'..='\u{e007f}'
    )
}

/// The `Default_Ignorable_Code_Point` members that are not `Cf`.
///
/// The interesting half: these are letters (`Lo`) and marks (`Mn`) by
/// category, so any filter that reasons about "the C categories" misses them
/// even though the standard says to render them as nothing. `U+3164` HANGUL
/// FILLER is the one to remember, because it is a `Lo` that draws blank and it
/// is what a bypass looks like after the obvious Cf holes are closed.
fn is_default_ignorable(ch: char) -> bool {
    matches!(
        ch,
        '\u{034f}'
            | '\u{115f}'..='\u{1160}'
            | '\u{17b4}'..='\u{17b5}'
            | '\u{180b}'..='\u{180f}'
            | '\u{2065}'
            | '\u{3164}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{ffa0}'
            | '\u{fff0}'..='\u{fff8}'
            // The Tag block, the variation selector supplement, and the
            // reserved code points between them. Tags are the standard carrier
            // for invisible ASCII: `U+E0061` is a whole letter `a` that draws
            // nothing.
            | '\u{e0000}'..='\u{e0fff}'
    )
}

/// `General_Category=Co` (Private Use), all three ranges.
fn is_private_use(ch: char) -> bool {
    matches!(
        ch,
        '\u{e000}'..='\u{f8ff}' | '\u{f0000}'..='\u{ffffd}' | '\u{100000}'..='\u{10fffd}'
    )
}

/// Noncharacters: permanently reserved, never assigned, and rendered by
/// nobody.
fn is_noncharacter(ch: char) -> bool {
    matches!(ch, '\u{fdd0}'..='\u{fdef}') || matches!(u32::from(ch) & 0xffff, 0xfffe | 0xffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The trojan-source set, verified against the hand-written table rather
    /// than against the `Cf` block that also happens to cover it. Lives here
    /// rather than beside the mesh's sanitiser because this is where the table
    /// is: a test of a table belongs next to the table.
    #[test]
    fn every_bidi_control_is_invisible() {
        for bidi in [
            '\u{061c}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202b}', '\u{202c}', '\u{202d}',
            '\u{202e}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
        ] {
            assert!(is_bidi_control(bidi), "U+{:04X}", bidi as u32);
            assert!(is_invisible(bidi), "U+{:04X}", bidi as u32);
        }
    }

    /// One representative per class the doc comment claims, so a table edited
    /// down to "the ones somebody remembered" fails here rather than in the
    /// next sanitiser that trusts it.
    #[test]
    fn each_documented_class_is_covered() {
        for (class, ch) in [
            ("soft hyphen (Cf)", '\u{00ad}'),
            ("zero-width space (Cf)", '\u{200b}'),
            ("word joiner (Cf)", '\u{2060}'),
            ("byte-order mark (Cf)", '\u{feff}'),
            ("tag block (Cf, plane 14)", '\u{e0041}'),
            ("variation selector (Mn, default-ignorable)", '\u{fe00}'),
            ("Hangul filler (Lo, default-ignorable)", '\u{115f}'),
            ("private use (Co)", '\u{e000}'),
            ("private use, plane 15 (Co)", '\u{f0000}'),
            ("noncharacter (Cn)", '\u{fdd0}'),
            ("plane-end noncharacter (Cn)", '\u{10fffe}'),
        ] {
            assert!(is_invisible(ch), "{class}: U+{:04X}", ch as u32);
        }
    }

    /// The negative half. An unassigned code point draws a visible `.notdef`
    /// box, so treating it as invisible would delete real characters from
    /// somebody's name as Unicode assigns them.
    #[test]
    fn ordinary_and_unassigned_characters_are_visible() {
        for ch in [
            'a',
            'e',
            '\u{4e2d}',
            '\u{1f642}',
            ' ',
            '\n',
            '\t',
            '\u{0}',
            '\u{0870}',
        ] {
            assert!(!is_invisible(ch), "U+{:04X}", ch as u32);
        }
    }
}

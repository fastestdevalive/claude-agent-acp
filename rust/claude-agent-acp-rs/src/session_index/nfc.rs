//! Unicode NFC (`String.prototype.normalize("NFC")`), which upstream applies to
//! paths on darwin (`normalizePath`) and unconditionally to the projects root.
//!
//! The standard library has no Unicode normalization and the crate takes no new
//! dependencies, so the canonical decomposition, composition and
//! combining-class data lives in [`super::nfc_data`] (generated, see its
//! header). The algorithm is UAX #15: full canonical decomposition, canonical
//! ordering, then canonical composition; Hangul is algorithmic.

use super::nfc_data::{COMBINING_CLASSES, COMPOSITIONS, DECOMPOSITIONS};

const S_BASE: u32 = 0xAC00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11A7;
const L_COUNT: u32 = 19;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const N_COUNT: u32 = V_COUNT * T_COUNT;
const S_COUNT: u32 = L_COUNT * N_COUNT;

/// The canonical combining class of `ch`, 0 for a starter.
fn combining_class(ch: char) -> u8 {
    let c = ch as u32;
    match COMBINING_CLASSES.binary_search_by(|&(start, end, _)| {
        if end < c {
            std::cmp::Ordering::Less
        } else if start > c {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    }) {
        Ok(i) => COMBINING_CLASSES[i].2,
        Err(_) => 0,
    }
}

fn push_code_point(code: u32, out: &mut Vec<(char, u8)>) {
    if let Some(ch) = char::from_u32(code) {
        out.push((ch, combining_class(ch)));
    }
}

/// Append the full canonical decomposition of `ch` (Hangul algorithmically).
fn decompose(ch: char, out: &mut Vec<(char, u8)>) {
    let c = ch as u32;
    if (S_BASE..S_BASE + S_COUNT).contains(&c) {
        let index = c - S_BASE;
        push_code_point(L_BASE + index / N_COUNT, out);
        push_code_point(V_BASE + (index % N_COUNT) / T_COUNT, out);
        let trailing = index % T_COUNT;
        if trailing != 0 {
            push_code_point(T_BASE + trailing, out);
        }
        return;
    }
    match DECOMPOSITIONS.binary_search_by_key(&c, |&(code, _)| code) {
        Ok(i) => {
            for &part in DECOMPOSITIONS[i].1 {
                push_code_point(part, out);
            }
        }
        Err(_) => push_code_point(c, out),
    }
}

/// The canonical composition of `first` and `second`, if there is one.
fn compose_pair(first: char, second: char) -> Option<char> {
    let (a, b) = (first as u32, second as u32);
    if (L_BASE..L_BASE + L_COUNT).contains(&a) && (V_BASE..V_BASE + V_COUNT).contains(&b) {
        return char::from_u32(S_BASE + ((a - L_BASE) * V_COUNT + (b - V_BASE)) * T_COUNT);
    }
    if (S_BASE..S_BASE + S_COUNT).contains(&a)
        && (a - S_BASE).is_multiple_of(T_COUNT)
        && (T_BASE + 1..T_BASE + T_COUNT).contains(&b)
    {
        return char::from_u32(a + (b - T_BASE));
    }
    COMPOSITIONS
        .binary_search_by(|&(x, y, _)| (x, y).cmp(&(a, b)))
        .ok()
        .and_then(|i| char::from_u32(COMPOSITIONS[i].2))
}

/// `value` in Unicode Normalization Form C.
pub fn nfc(value: &str) -> String {
    if value.is_ascii() {
        return value.to_string();
    }
    let mut decomposed: Vec<(char, u8)> = Vec::with_capacity(value.len());
    for ch in value.chars() {
        decompose(ch, &mut decomposed);
    }
    // Canonical ordering: each run of non-starters is sorted stably by class.
    let mut i = 0;
    while i < decomposed.len() {
        if decomposed[i].1 == 0 {
            i += 1;
            continue;
        }
        let start = i;
        while i < decomposed.len() && decomposed[i].1 != 0 {
            i += 1;
        }
        decomposed[start..i].sort_by_key(|&(_, class)| class);
    }
    let mut out: Vec<char> = Vec::with_capacity(decomposed.len());
    let mut starter: Option<usize> = None;
    let mut last_class = 0u8;
    for (ch, class) in decomposed {
        if let Some(at) = starter {
            if last_class == 0 || last_class < class {
                if let Some(composed) = compose_pair(out[at], ch) {
                    out[at] = composed;
                    continue;
                }
            }
        }
        if class == 0 {
            starter = Some(out.len());
        }
        last_class = class;
        out.push(ch);
    }
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NFC > composes a base letter and its combining mark
    #[test]
    fn composes_canonical_pairs() {
        assert_eq!(nfc("e\u{301}"), "\u{e9}");
        assert_eq!(nfc("A\u{30a}"), "\u{c5}");
        assert_eq!(nfc("A\u{30a}\u{301}"), "\u{1fa}");
        // A singleton decomposition is replaced by its canonical character.
        assert_eq!(nfc("\u{2126}"), "\u{3a9}");
        assert_eq!(nfc("\u{212b}"), "\u{c5}");
    }

    /// NFC > orders combining marks by their class before composing
    #[test]
    fn orders_combining_marks() {
        // U+0315 has class 232, U+0300 has class 230.
        assert_eq!(nfc("a\u{315}\u{300}"), "\u{e0}\u{315}");
        assert_eq!(nfc("a\u{315}\u{300}"), nfc("a\u{300}\u{315}"));
        assert_eq!(nfc("a\u{300}\u{315}"), "\u{e0}\u{315}");
    }

    /// NFC > leaves ASCII and already-normalised text alone
    #[test]
    fn is_idempotent() {
        assert_eq!(nfc("plain/ascii"), "plain/ascii");
        assert_eq!(nfc("\u{e9}"), "\u{e9}");
        assert_eq!(nfc(&nfc("A\u{30a}\u{301}")), nfc("A\u{30a}\u{301}"));
        assert_eq!(nfc(""), "");
    }

    /// H3 (`review-T7`): the combining classes come from the same Unicode
    /// release as the decompositions (17.0.0, the release Node 26's
    /// `String.prototype.normalize` reports), so the marks Unicode added after
    /// 13.0 order and block composition exactly as Node does. The expected
    /// strings are Node's `normalize("NFC")` results; the tables are generated
    /// and checked against it by `tools/gen_nfc_data.mjs` (see the
    /// `nfc_data.rs` header). Before the fix these marks had no class (treated
    /// as starters), so `a`+U+1AC3+U+0301 stayed uncomposed.
    #[test]
    fn post_unicode_13_marks_order_and_compose_like_node() {
        // `a` + mark + U+0301 (class 230): a mark below 230 does not block the
        // composition, so `a`+U+0301 composes to `á` and the mark follows. A
        // mark above 230 (U+1AEB, class 234) sorts behind U+0301 first.
        // `a` + U+0315 (class 232) + mark: a mark below 232 sorts before 0315.
        let marks: &[(char, &str, &str)] = &[
            ('\u{0899}', "\u{e1}\u{0899}", "\u{61}\u{0899}\u{315}"),
            ('\u{08cf}', "\u{e1}\u{08cf}", "\u{61}\u{08cf}\u{315}"),
            ('\u{08d0}', "\u{e1}\u{08d0}", "\u{61}\u{08d0}\u{315}"),
            ('\u{08d1}', "\u{e1}\u{08d1}", "\u{61}\u{08d1}\u{315}"),
            ('\u{08d2}', "\u{e1}\u{08d2}", "\u{61}\u{08d2}\u{315}"),
            ('\u{1ac3}', "\u{e1}\u{1ac3}", "\u{61}\u{1ac3}\u{315}"),
            ('\u{1ac4}', "\u{e1}\u{1ac4}", "\u{61}\u{1ac4}\u{315}"),
            ('\u{1aca}', "\u{e1}\u{1aca}", "\u{61}\u{1aca}\u{315}"),
            ('\u{1add}', "\u{e1}\u{1add}", "\u{61}\u{1add}\u{315}"),
            ('\u{1ae6}', "\u{e1}\u{1ae6}", "\u{61}\u{1ae6}\u{315}"),
            ('\u{1aeb}', "\u{e1}\u{1aeb}", "\u{61}\u{315}\u{1aeb}"),
            ('\u{1dfa}', "\u{e1}\u{1dfa}", "\u{61}\u{1dfa}\u{315}"),
            ('\u{10efa}', "\u{e1}\u{10efa}", "\u{61}\u{10efa}\u{315}"),
            ('\u{10efb}', "\u{e1}\u{10efb}", "\u{61}\u{10efb}\u{315}"),
            ('\u{113ce}', "\u{e1}\u{113ce}", "\u{61}\u{113ce}\u{315}"),
            ('\u{113cf}', "\u{e1}\u{113cf}", "\u{61}\u{113cf}\u{315}"),
            ('\u{11f41}', "\u{e1}\u{11f41}", "\u{61}\u{11f41}\u{315}"),
            ('\u{11f42}', "\u{e1}\u{11f42}", "\u{61}\u{11f42}\u{315}"),
            ('\u{1612f}', "\u{e1}\u{1612f}", "\u{61}\u{1612f}\u{315}"),
        ];
        for (mark, composed, ordered) in marks {
            assert_eq!(
                nfc(&format!("a{mark}\u{301}")),
                *composed,
                "U+{:04X} must not block the a+U+0301 composition",
                *mark as u32
            );
            assert_eq!(
                nfc(&format!("a\u{315}{mark}")),
                *ordered,
                "U+{:04X} must order against U+0315 by its class",
                *mark as u32
            );
        }
        // The audit's headline case.
        assert_eq!(nfc("a\u{1ac3}\u{301}"), "\u{e1}\u{1ac3}");
    }

    /// NFC > composes and decomposes Hangul algorithmically
    #[test]
    fn handles_hangul() {
        assert_eq!(nfc("\u{1100}\u{1161}"), "\u{ac00}");
        assert_eq!(nfc("\u{1100}\u{1161}\u{11a8}"), "\u{ac01}");
        assert_eq!(nfc("\u{ac00}\u{11a8}"), "\u{ac01}");
        assert_eq!(nfc("\u{ac01}"), "\u{ac01}");
    }
}

//! JavaScript string semantics the upstream session index relies on: `trim`,
//! `\s`, UTF-16 lengths and cuts, `Date.parse` of ISO timestamps and
//! `Date.prototype.toISOString`. Private helpers shared by the submodules.

/// A character JavaScript's `String.prototype.trim` and the regex class `\s`
/// treat as white space (WhiteSpace and LineTerminator of ECMAScript).
pub fn is_js_ws(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// ASCII white space as Java's `\s` and the upstream archive pattern see it.
pub fn is_ascii_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\u{b}' | '\u{c}' | '\r')
}

/// `String.prototype.trim`.
pub fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_ws)
}

/// `text.length`: the number of UTF-16 code units.
pub fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// At most `length` UTF-16 units of `text`, never splitting a surrogate pair.
pub fn cut_utf16(text: &str, length: usize) -> &str {
    let mut units = 0;
    for (index, c) in text.char_indices() {
        let next = units + c.len_utf16();
        if next > length {
            return &text[..index];
        }
        units = next;
    }
    text
}

/// `Date.parse` of the ISO 8601 forms the CLI writes (`2026-01-02T03:04:05.678Z`
/// and the same with a numeric offset, a date alone, or no zone, which is UTC
/// for a date-only form and local time otherwise; local is read as UTC here).
/// The result is epoch milliseconds.
pub fn parse_iso_ms(text: &str) -> Option<i64> {
    let text = text.trim();
    let bytes = text.as_bytes();
    let digits = |from: usize, count: usize| -> Option<i64> {
        let slice = bytes.get(from..from + count)?;
        if slice.iter().all(u8::is_ascii_digit) {
            std::str::from_utf8(slice).ok()?.parse().ok()
        } else {
            None
        }
    };
    let year = digits(0, 4)?;
    let mut at = 4;
    let mut month = 1;
    let mut day = 1;
    if bytes.get(at) == Some(&b'-') {
        month = digits(at + 1, 2)?;
        at += 3;
        if bytes.get(at) == Some(&b'-') {
            day = digits(at + 1, 2)?;
            at += 3;
        }
    }
    let (mut hour, mut minute, mut second, mut millis) = (0, 0, 0, 0);
    let mut offset_minutes = 0;
    if matches!(bytes.get(at), Some(b'T' | b't' | b' ')) {
        hour = digits(at + 1, 2)?;
        if bytes.get(at + 3) != Some(&b':') {
            return None;
        }
        minute = digits(at + 4, 2)?;
        at += 6;
        if bytes.get(at) == Some(&b':') {
            second = digits(at + 1, 2)?;
            at += 3;
            if bytes.get(at) == Some(&b'.') {
                let mut end = at + 1;
                while bytes.get(end).is_some_and(u8::is_ascii_digit) {
                    end += 1;
                }
                if end == at + 1 {
                    return None;
                }
                let fraction = &text[at + 1..end];
                let first3: String = fraction.chars().take(3).collect();
                millis = format!("{first3:0<3}").parse().ok()?;
                at = end;
            }
        }
        match bytes.get(at) {
            Some(b'Z' | b'z') => at += 1,
            Some(sign @ (b'+' | b'-')) => {
                let h = digits(at + 1, 2)?;
                let (m, used) = if bytes.get(at + 3) == Some(&b':') {
                    (digits(at + 4, 2)?, 6)
                } else {
                    (digits(at + 3, 2)?, 5)
                };
                offset_minutes = (h * 60 + m) * if *sign == b'-' { -1 } else { 1 };
                at += used;
            }
            _ => {}
        }
    }
    if at != bytes.len() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 24 || minute > 59 || second > 59 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset_minutes * 60;
    Some(secs * 1000 + millis)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// A JSON number the way JavaScript serializes `value`: an integral value is an
/// integer (`2`, not `2.0`).
pub fn js_number(value: f64) -> serde_json::Value {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9.007_199_254_740_992e15 {
        serde_json::Value::from(value as i64)
    } else {
        serde_json::Number::from_f64(value)
            .map_or(serde_json::Value::Null, serde_json::Value::Number)
    }
}

/// `new Date(ms).toISOString()`.
pub fn to_iso_string(ms: i64) -> String {
    crate::session_store::iso8601(u64::try_from(ms).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_matches_the_javascript_white_space_set() {
        assert_eq!(js_trim("\u{feff} a \u{a0}"), "a");
        // U+0085 is white space in Rust, not in JavaScript.
        assert_eq!(js_trim("\u{85}a"), "\u{85}a");
    }

    #[test]
    fn cut_never_splits_a_surrogate_pair() {
        assert_eq!(cut_utf16("ab\u{1f600}", 3), "ab");
        assert_eq!(cut_utf16("ab\u{1f600}", 4), "ab\u{1f600}");
        assert_eq!(utf16_len("a\u{1f600}"), 3);
    }

    #[test]
    fn parses_iso_timestamps_like_date_parse() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(
            parse_iso_ms("2026-01-01T00:00:00.000Z"),
            Some(1_767_225_600_000)
        );
        assert_eq!(
            parse_iso_ms("2026-01-01T01:00:00+01:00"),
            Some(1_767_225_600_000)
        );
        assert_eq!(parse_iso_ms("not a date"), None);
    }
}

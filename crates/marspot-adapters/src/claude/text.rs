//! Small readings of one Claude record line: a top-level field without
//! a full parse, a timestamp, and whether a model field names a model.

/// The value of a top-level string field of one record.
///
/// Depth-aware on purpose: an assistant record puts `message`, which
/// has a `"type":"message"` of its own, before the record's `type`.
/// Escapes are stepped over, not decoded.
pub fn top_level_str<'a>(line: &'a str, field: &str) -> Option<&'a str> {
    let b = line.as_bytes();
    let (mut i, mut depth) = (0usize, 0i32);
    while i < b.len() {
        match b[i] {
            b'{' | b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b']' => {
                depth -= 1;
                i += 1;
            }
            b'"' => {
                let (s, after) = scan_string(b, i)?;
                let j = skip_space(b, after);
                if depth == 1 && j < b.len() && b[j] == b':' {
                    let k = skip_space(b, j + 1);
                    if s == field {
                        return if k < b.len() && b[k] == b'"' { scan_string(b, k).map(|(v, _)| v) } else { None };
                    }
                    i = k;
                } else {
                    i = after;
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// The content of the string whose opening quote is at `at`, and the
/// index past its closing quote.
fn scan_string(b: &[u8], at: usize) -> Option<(&str, usize)> {
    let (start, mut i) = (at + 1, at + 1);
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return std::str::from_utf8(&b[start..i]).ok().map(|s| (s, i + 1)),
            _ => i += 1,
        }
    }
    None
}

fn skip_space(b: &[u8], mut j: usize) -> usize {
    while j < b.len() && b[j].is_ascii_whitespace() {
        j += 1;
    }
    j
}

/// `2026-10-01T11:51:52.440Z` (or `+00:00`) to unix seconds; what
/// follows the seconds is ignored.
pub fn unix_of_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { std::str::from_utf8(&b[r]).ok()?.parse().ok() };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let y_adj = if mo <= 2 { y - 1 } else { y };
    let era = y_adj.div_euclid(400);
    let yoe = y_adj - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3_600 + mi * 60 + sec)
}

/// The record's timestamp, in unix seconds; None before 1970.
pub fn record_time(v: &marspot_json::Value) -> Option<u64> {
    u64::try_from(unix_of_iso(v.str_at("timestamp")?)?).ok()
}

/// Whether a reply's `model` names a model the way a badge would show
/// it: what is left after colour codes, a parenthesised remark, the
/// `claude-` prefix and a snapshot date are dropped is a non-empty run
/// of letters, digits, spaces, dots and dashes (only the first sixteen
/// characters are looked at).  An error record's `<synthetic>` is not.
pub fn names_model(raw: &str) -> bool {
    let raw = raw.replace("\\u001b", "\u{1b}");
    let mut cleaned = String::with_capacity(raw.len());
    let mut it = raw.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' {
            if it.peek() == Some(&'[') {
                it.next();
                for e in it.by_ref() {
                    if e.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        cleaned.push(c);
    }
    let cleaned = cleaned.trim();
    let cleaned = cleaned.find('(').map_or(cleaned, |i| cleaned[..i].trim_end());
    let cleaned = cleaned.strip_prefix("claude-").unwrap_or(cleaned);
    let cleaned = match cleaned.rfind('-') {
        Some(i) if cleaned.len() - i == 9 && cleaned[i + 1..].bytes().all(|b| b.is_ascii_digit()) => &cleaned[..i],
        _ => cleaned,
    };
    let mut out = String::with_capacity(cleaned.len());
    for c in cleaned.chars() {
        let mapped = match c {
            ' ' | '.' => '-',
            c if c.is_ascii_alphanumeric() || c == '-' => c.to_ascii_lowercase(),
            _ => return false,
        };
        out.push(mapped);
        if out.len() >= 16 {
            break;
        }
    }
    !out.trim_matches('-').is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_text_reads_the_records_own_type_and_names_models() {
        let a = r#"{"message":{"type":"message"},"type":"assistant"}"#;
        assert_eq!(top_level_str(a, "type"), Some("assistant"));
        assert_eq!(unix_of_iso("2026-11-24T14:30:44.123Z"), Some(1_795_530_644));
        assert!(names_model("claude-opus-4-8"));
        assert!(names_model("claude-haiku-4-5-20251001"));
        assert!(!names_model("<synthetic>"));
        assert!(!names_model("claude-fable-5-2[1m]"), "a bracket is no part of a badge's name");
        assert!(!names_model("---"));
    }
}

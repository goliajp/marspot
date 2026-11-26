//! The top-level `model` member of a settings file, changed in its
//! text.  People edit this file by hand and Claude Code writes it in
//! its own layout, so putting a model back changes that one member and
//! leaves every other byte where it was, rather than writing the file
//! out again from a parse.

use marspot_peer::json::{self, Value};
use std::ops::Range;

/// The text with its top-level `model` set to `want`, or taken out for
/// `None`.  None when the edit cannot be made, or would change anything
/// but that member: the caller then leaves the file alone.
pub(super) fn with_model(text: &str, want: Option<&Value>) -> Option<String> {
    let members = members(text)?;
    let found = members.iter().find(|m| m.key == "model");
    let edited = match (found, want) {
        (Some(m), Some(v)) => format!("{}{}{}", &text[..m.value.start], v.to_json(), &text[m.value.end..]),
        (Some(m), None) => cut(text, m, &members),
        (None, Some(v)) => insert(text, &members, &v.to_json()),
        (None, None) => text.to_string(),
    };
    same_but_model(text, &edited, want).then_some(edited)
}

/// Whether `edited` parses to `text`'s object with only `model` changed
/// to `want`.
fn same_but_model(text: &str, edited: &str, want: Option<&Value>) -> bool {
    let rest = |t: &str| match json::parse(t) {
        Ok(Value::Obj(kv)) => Some(kv.into_iter().filter(|(k, _)| k != "model").collect::<Vec<_>>()),
        _ => None,
    };
    let model = json::parse(edited).ok().and_then(|v| v.get("model").cloned());
    rest(text).is_some() && rest(text) == rest(edited) && model.as_ref() == want
}

struct Member<'a> {
    key: &'a str,
    /// From the key's opening quote to the end of the value.
    span: Range<usize>,
    value: Range<usize>,
}

/// The members of the object `text` holds, in order.  Keys are taken
/// as written: an escaped key is not `model` here, and the check after
/// the edit refuses what that misses.
fn members(text: &str) -> Option<Vec<Member<'_>>> {
    let b = text.as_bytes();
    let mut i = skip_ws(b, 0);
    if b.get(i) != Some(&b'{') {
        return None;
    }
    i = skip_ws(b, i + 1);
    let mut out = Vec::new();
    if b.get(i) == Some(&b'}') {
        return Some(out);
    }
    loop {
        let key_start = i;
        let key_end = end_of_string(b, i)?;
        i = skip_ws(b, key_end);
        if b.get(i) != Some(&b':') {
            return None;
        }
        let value_start = skip_ws(b, i + 1);
        let value_end = end_of_value(b, value_start)?;
        out.push(Member { key: &text[key_start + 1..key_end - 1], span: key_start..value_end, value: value_start..value_end });
        i = skip_ws(b, value_end);
        match b.get(i) {
            Some(b',') => i = skip_ws(b, i + 1),
            Some(b'}') => return Some(out),
            _ => return None,
        }
    }
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while b.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}

/// Past the closing quote of the string opening at `i`.
fn end_of_string(b: &[u8], i: usize) -> Option<usize> {
    if b.get(i) != Some(&b'"') {
        return None;
    }
    let mut j = i + 1;
    loop {
        match b.get(j)? {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
}

/// Past the value starting at `i`.
fn end_of_value(b: &[u8], i: usize) -> Option<usize> {
    match b.get(i)? {
        b'"' => end_of_string(b, i),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut j = i;
            loop {
                match b.get(j)? {
                    b'"' => j = end_of_string(b, j)?,
                    b'{' | b'[' => (depth, j) = (depth + 1, j + 1),
                    b'}' | b']' if depth == 1 => return Some(j + 1),
                    b'}' | b']' => (depth, j) = (depth - 1, j + 1),
                    _ => j += 1,
                }
            }
        }
        _ => {
            let mut j = i;
            while b.get(j).is_some_and(|c| !matches!(c, b',' | b'}' | b']') && !c.is_ascii_whitespace()) {
                j += 1;
            }
            (j > i).then_some(j)
        }
    }
}

/// `text` without member `m`, with one of the commas joining it, and
/// its line when nothing else is on it.
fn cut(text: &str, m: &Member, members: &[Member]) -> String {
    let b = text.as_bytes();
    let last = members.last().is_some_and(|l| l.span == m.span);
    let (mut start, mut end) = (m.span.start, m.span.end);
    if !last {
        end = skip_ws(b, end) + 1;
    } else if members.len() > 1 {
        let mut k = start;
        while k > 0 && b[k - 1].is_ascii_whitespace() {
            k -= 1;
        }
        start = k - 1;
    }
    let bol = text[..start].rfind('\n').map_or(0, |i| i + 1);
    let eol = text[end..].find('\n').map_or(text.len(), |i| end + i);
    if text[bol..start].trim().is_empty() && text[end..eol].trim().is_empty() && eol < text.len() {
        (start, end) = (bol, eol + 1);
    } else if !last {
        end = skip_ws(b, end).min(eol);
    }
    format!("{}{}", &text[..start], &text[end..])
}

/// `text` with a `model` member first in its object, laid out as the
/// member after it is.
fn insert(text: &str, members: &[Member], value: &str) -> String {
    let open = text.find('{').expect("members found an object");
    let Some(first) = members.first() else {
        let close = text[open..].find('}').map_or(text.len(), |i| open + i);
        return format!("{}{{\n  \"model\": {value}\n}}{}", &text[..open], &text[close + 1..]);
    };
    let gap = &text[open + 1..first.span.start];
    format!("{}{gap}\"model\": {value},{}", &text[..=open], &text[open + 1..])
}

#[cfg(test)]
#[path = "model_key_tests.rs"]
mod tests;

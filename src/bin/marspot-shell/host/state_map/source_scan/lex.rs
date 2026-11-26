//! Just enough Rust lexing for the scan: literals, comments, brackets,
//! and the functions and closures of a source.

/// The end of the string, raw string or char literal starting at `i`, or
/// None when none starts there (a lifetime is no literal).
pub(super) fn literal_end(b: &[u8], i: usize) -> Option<usize> {
    match b[i] {
        b'"' => Some(quoted_end(b, i + 1)),
        b'r' if b.get(i + 1).is_some_and(|&c| c == b'"' || c == b'#') && (i == 0 || !is_ident(b[i - 1])) => {
            let hashes = b[i + 1..].iter().take_while(|&&c| c == b'#').count();
            let open = i + 1 + hashes;
            if b.get(open) != Some(&b'"') {
                return None;
            }
            let close: Vec<u8> = std::iter::once(b'"').chain(std::iter::repeat_n(b'#', hashes)).collect();
            Some(b[open + 1..].windows(close.len()).position(|w| w == close).map_or(b.len(), |p| open + 1 + p + close.len()))
        }
        b'b' if b.get(i + 1) == Some(&b'"') && (i == 0 || !is_ident(b[i - 1])) => Some(quoted_end(b, i + 2)),
        b'\'' => {
            let len = if b.get(i + 1) == Some(&b'\\') { b[i + 2..].iter().position(|&c| c == b'\'')? + 2 } else { utf8_len(*b.get(i + 1)?) };
            (b.get(i + 1 + len) == Some(&b'\'')).then_some(i + 2 + len)
        }
        _ => None,
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0xf0.. => 4,
        0xe0.. => 3,
        0xc0.. => 2,
        _ => 1,
    }
}

fn quoted_end(b: &[u8], mut i: usize) -> usize {
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    b.len()
}

pub(super) fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// `text` with every comment replaced by a space.
pub(super) fn strip_comments(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let (mut i, mut from) = (0, 0);
    while i < b.len() {
        if let Some(end) = literal_end(b, i) {
            i = end;
        } else if b[i..].starts_with(b"//") {
            out.push_str(&text[from..i]);
            i += b[i..].iter().position(|&c| c == b'\n').unwrap_or(b.len() - i);
            from = i;
        } else if b[i..].starts_with(b"/*") {
            out.push_str(&text[from..i]);
            out.push(' ');
            let mut depth = 0;
            while i < b.len() {
                if b[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if b[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            from = i;
        } else {
            i += 1;
        }
    }
    out.push_str(&text[from.min(text.len())..]);
    out
}

/// The index just past the bracket that closes the one at `open`.
pub(super) fn close_of(b: &[u8], open: usize) -> usize {
    let mut depth = 0usize;
    let mut i = open;
    while i < b.len() {
        if let Some(end) = literal_end(b, i) {
            i = end;
            continue;
        }
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    b.len()
}

/// The first of `stops` at bracket depth 0 from `at`, or the end.
pub(super) fn top_level(b: &[u8], at: usize, stops: &[u8]) -> usize {
    let mut i = at;
    while i < b.len() {
        if let Some(end) = literal_end(b, i) {
            i = end;
            continue;
        }
        match b[i] {
            c if stops.contains(&c) => return i,
            b'(' | b'[' | b'{' => {
                i = close_of(b, i);
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    b.len()
}

/// `text` without the items under `#[cfg(test)]`: a module, function
/// or impl with its whole body, a `mod name;` or `use` up to its `;`, a
/// field, variant or match arm up to its `,` or the bracket that closes
/// its list.
pub(super) fn without_tests(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let (mut i, mut from) = (0, 0);
    while i < b.len() {
        if let Some(end) = literal_end(b, i) {
            i = end;
        } else if b[i..].starts_with(b"#[cfg(test)]") {
            out.push_str(&text[from..i]);
            i = cfg_item_end(b, i + "#[cfg(test)]".len());
            from = i;
        } else {
            i += 1;
        }
    }
    out.push_str(&text[from.min(text.len())..]);
    out
}

/// Where the item after a `#[cfg(test)]` ends.  A `,` inside the angle
/// brackets of a type (`HashMap<K, V>`) ends nothing; `->` and `=>` open
/// none.
fn cfg_item_end(b: &[u8], at: usize) -> usize {
    let mut angle = 0usize;
    let mut i = at;
    while i < b.len() {
        if let Some(end) = literal_end(b, i) {
            i = end;
            continue;
        }
        match b[i] {
            b'{' => return close_of(b, i),
            b'(' | b'[' => {
                i = close_of(b, i);
                continue;
            }
            b';' => return i + 1,
            b',' if angle == 0 => return i + 1,
            // the end of the list the field or arm is in stays
            b'}' | b')' | b']' => return i,
            b'<' => angle += 1,
            b'>' if !matches!(b.get(i.wrapping_sub(1)), Some(b'-' | b'=')) => angle = angle.saturating_sub(1),
            _ => {}
        }
        i += 1;
    }
    b.len()
}

/// A function or a closure bound by `let`: where its body is, the
/// names of its parameters, and the type of the `impl` it is in.
#[derive(Debug)]
pub(super) struct FnDef {
    pub name: String,
    pub owner: Option<String>,
    pub params: Vec<String>,
    pub body: std::ops::Range<usize>,
    pub braced: bool,
}

/// The parameter names of a parameter list (without its parentheses).
fn param_names(list: &str) -> Vec<String> {
    let b = list.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < b.len() {
        let end = top_level(b, at, b",");
        let p = list[at..end].trim();
        let p = p.strip_prefix("mut ").unwrap_or(p);
        let name = p.split(':').next().unwrap_or("").trim();
        if !name.is_empty() && !name.ends_with("self") && name.bytes().all(is_ident) {
            out.push(name.to_string());
        }
        at = end + 1;
    }
    out
}

/// Every function with a body and every `let name = |..|` closure.
pub(super) fn functions(text: &str) -> Vec<FnDef> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut impls: Vec<(usize, String)> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if let Some(end) = literal_end(b, i) {
            i = end;
            continue;
        }
        impls.retain(|(close, _)| *close > i);
        let word_start = i == 0 || !is_ident(b[i - 1]);
        // `-> impl Trait` and `x: impl Trait` are types, not blocks
        let in_type = text[..i].trim_end().ends_with(['>', ':', '(', ',', '&', '<']);
        if word_start && !in_type && b[i..].starts_with(b"impl") && b.get(i + 4).is_some_and(|c| !is_ident(*c)) {
            let open = top_level(b, i, b"{;");
            if b.get(open) == Some(&b'{') {
                let head = &text[i + 4..open];
                let ty = head.rsplit(" for ").next().unwrap_or(head).trim();
                let ty = ty.split('<').next().unwrap_or(ty).rsplit("::").next().unwrap_or(ty).trim();
                impls.push((close_of(b, open), ty.to_string()));
            }
        } else if word_start && b[i..].starts_with(b"fn ") {
            if let Some(f) = function_at(text, i + 3, impls.last().map(|(_, t)| t.clone())) {
                out.push(f);
            }
        } else if word_start && b[i..].starts_with(b"let ") {
            if let Some(f) = closure_at(text, i + 4) {
                out.push(f);
            }
        }
        i += 1;
    }
    out
}

fn function_at(text: &str, at: usize, owner: Option<String>) -> Option<FnDef> {
    let b = text.as_bytes();
    let name_len = b[at..].iter().take_while(|&&c| is_ident(c)).count();
    let name = &text[at..at + name_len];
    let open_paren = at + text[at..].find('(')?;
    let close_paren = close_of(b, open_paren);
    let open = top_level(b, close_paren, b"{;");
    (b.get(open) == Some(&b'{')).then(|| FnDef {
        name: name.to_string(),
        owner,
        params: param_names(&text[open_paren + 1..close_paren - 1]),
        body: open + 1..close_of(b, open) - 1,
        braced: true,
    })
}

fn closure_at(text: &str, at: usize) -> Option<FnDef> {
    let b = text.as_bytes();
    let rest = &text[at..];
    let name_len = rest.bytes().take_while(|&c| is_ident(c)).count();
    let after = rest[name_len..].trim_start().strip_prefix('=')?.trim_start();
    let after = after.strip_prefix("move ").unwrap_or(after);
    let list = after.strip_prefix('|')?;
    let bar = list.find('|')?;
    let start = text.len() - list.len() + bar + 1;
    let body_start = start + (text[start..].len() - text[start..].trim_start().len());
    let braced = b.get(body_start) == Some(&b'{');
    let body = if braced { body_start + 1..close_of(b, body_start) - 1 } else { body_start..top_level(b, body_start, b";") };
    Some(FnDef { name: rest[..name_len].to_string(), owner: None, params: param_names(&list[..bar]), body, braced })
}

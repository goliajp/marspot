//! The path patterns of the state map, matched one segment at a time.
//!
//! In a segment: `<id>` and `<n>` are one or more ASCII digits, `<date>`
//! is eight digits, `<uuid>` is 36 hex digits and dashes, `<sid>` is
//! one or more ASCII letters, digits and dashes, `*` is any run of
//! characters (none included).  A segment `**` is any number of
//! segments, none included.
//!
//! A path from the source scan may hold holes (U+0001), each standing
//! for any text within its segment; such a path matches when some
//! filling of its holes would.

/// Whether `path` (relative, `/`-separated) matches `pattern`.
pub(crate) fn matches(pattern: &str, path: &str) -> bool {
    let pat: Vec<&str> = pattern.split('/').collect();
    let segs: Vec<&str> = path.split('/').collect();
    segments(&pat, &segs)
}

/// Whether the directory `dir` is one `pattern` can put files under:
/// `pattern` has more segments and its first ones match `dir`'s.
pub(crate) fn is_dir_of(pattern: &str, dir: &str) -> bool {
    let pat: Vec<&str> = pattern.split('/').collect();
    let segs: Vec<&str> = dir.split('/').collect();
    under(&pat, &segs)
}

/// `segs` are used up by the head of `pat` with a segment still to go.
fn under(pat: &[&str], segs: &[&str]) -> bool {
    match (pat.split_first(), segs.split_first()) {
        (None, _) => false,
        // it takes the rest of the directory and a file below it
        (Some((&"**", _)), _) => true,
        (Some(_), None) => true,
        (Some((p, rest)), Some((s, more))) => segment(p.as_bytes(), s.as_bytes()) && under(rest, more),
    }
}

fn segments(pat: &[&str], segs: &[&str]) -> bool {
    match pat.split_first() {
        None => segs.is_empty(),
        Some((&"**", rest)) => (0..=segs.len()).any(|skip| segments(rest, &segs[skip..])),
        Some((p, rest)) => segs.split_first().is_some_and(|(s, more)| segment(p.as_bytes(), s.as_bytes()) && segments(rest, more)),
    }
}

/// A part of a path the source scan could not read: it may stand for
/// any text in its segment, so it eats any run of the pattern's tokens.
const HOLE: u8 = 1;

fn segment(p: &[u8], s: &[u8]) -> bool {
    if s.first() == Some(&HOLE) {
        return segment(p, &s[1..]) || (!p.is_empty() && segment(after_token(p), s));
    }
    if p.is_empty() {
        return s.is_empty();
    }
    if p[0] == b'*' {
        return (0..=s.len()).any(|skip| segment(&p[1..], &s[skip..]));
    }
    if p[0] == b'<' {
        if let Some(end) = p.iter().position(|&b| b == b'>') {
            let (name, rest) = (&p[1..end], &p[end + 1..]);
            return (1..=s.len()).any(|take| placeholder(name, &s[..take]) && segment(rest, &s[take..]));
        }
    }
    s.first() == Some(&p[0]) && segment(&p[1..], &s[1..])
}

fn after_token(p: &[u8]) -> &[u8] {
    match p.iter().position(|&b| b == b'>') {
        Some(end) if p[0] == b'<' => &p[end + 1..],
        _ => &p[1..],
    }
}

fn placeholder(name: &[u8], s: &[u8]) -> bool {
    if s.contains(&HOLE) {
        return true;
    }
    match name {
        b"id" | b"n" => s.iter().all(u8::is_ascii_digit),
        b"date" => s.len() == 8 && s.iter().all(u8::is_ascii_digit),
        b"uuid" => s.len() == 36 && s.iter().all(|b| b.is_ascii_hexdigit() || *b == b'-'),
        b"sid" => s.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'-'),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_take_only_their_shape() {
        assert!(matches("sessions/<id>/bytelog.seg.<n>", "sessions/12/bytelog.seg.3"));
        assert!(!matches("sessions/<id>/bytelog.seg.<n>", "sessions/x/bytelog.seg.3"));
        assert!(matches("a/<date>.tsv", "a/20261126.tsv"));
        assert!(!matches("a/<date>.tsv", "a/2026112.tsv"));
        assert!(matches("m/<uuid>", "m/2fec8734-bf76-478b-8044-20261126b7e4"));
        assert!(!matches("m/<uuid>", "m/not-a-uuid"));
        assert!(matches("m/<sid>", "m/not-a-uuid-7"));
        assert!(!matches("m/<sid>", "m/.not-a-uuid-7.tmp"));
        assert!(matches("retired/<id>-<n>/**", "retired/7-1764000000/sock"));
        assert!(matches("retired/<id>.<n>-<n>/**", "retired/7.99-1764000000/x/y"));
    }

    #[test]
    fn stars_stay_in_their_segment_and_double_stars_span() {
        assert!(matches("*.run.log", "test-x.run.log"));
        assert!(!matches("*.run.log", "logs/test-x.run.log"));
        assert!(matches("**/*.tmp", "settings.toml.tmp"));
        assert!(matches("**/*.tmp", "plugins/claudecode/model/.u.tmp"));
        assert!(!matches("zombie", "zombie/told.tsv"));
    }

    #[test]
    fn a_directory_is_one_a_pattern_puts_files_under() {
        assert!(is_dir_of("plugins/claudecode/model/<uuid>", "plugins/claudecode"));
        assert!(is_dir_of("plugins/claudecode/model/<uuid>", "plugins"));
        assert!(!is_dir_of("plugins/claudecode/model/<uuid>", "plugins/claudecode/model/x/y"));
        assert!(!is_dir_of("zombie/told.tsv", "zombie/told.tsv"));
        assert!(is_dir_of("helpers/<id>/**", "helpers/3/home"));
    }
}

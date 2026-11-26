//! One line of a fact log: `<unix>\t<kind>\t<key>=<value>\t...\n`, kept
//! in one file per UTC day named `YYYYMMDD.tsv`.
//!
//! Readers skip a kind they do not know and a key they do not know, so
//! a newer build may add both without an older one misreading the file.
//! Kinds and keys are `[a-z0-9_.-]+`; a value has `\`, tab, newline and
//! carriage return escaped as `\\`, `\t`, `\n`, `\r`, and may hold `=`.

use std::borrow::Cow;

/// The kind of the line a writer adds once a day's byte cap made it
/// drop lines: `<unix>\tdropped\tdropped=<count>`.  The count has
/// [`DROPPED_WIDTH`] digits so the writer can raise it in place, the one
/// change a fact file allows; a reader adds up every dropped line.
pub const DROPPED: &str = "dropped";

/// Digits of the count in a dropped line, zero-padded.
pub const DROPPED_WIDTH: usize = 20;

const SECS_PER_DAY: u64 = 86_400;

fn is_word(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b))
}

fn push_escaped(out: &mut String, v: &str) {
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
}

/// Append one whole line, newline included, to `out`.
pub fn push(out: &mut String, unix: u64, kind: &str, fields: &[(&str, &str)]) {
    debug_assert!(is_word(kind), "fact kind {kind:?}");
    out.push_str(&unix.to_string());
    out.push('\t');
    out.push_str(kind);
    for (k, v) in fields {
        debug_assert!(is_word(k), "fact key {k:?}");
        out.push('\t');
        out.push_str(k);
        out.push('=');
        push_escaped(out, v);
    }
    out.push('\n');
}

/// Append the line that says `n` lines were dropped; the count is its
/// last [`DROPPED_WIDTH`] bytes before the newline.
pub fn push_dropped(out: &mut String, unix: u64, n: u64) {
    push(out, unix, DROPPED, &[(DROPPED, &format!("{n:0DROPPED_WIDTH$}"))]);
}

/// The count of a dropped line written by [`push_dropped`]; None for
/// any other line.
pub fn dropped_count(line: &str) -> Option<u64> {
    let l = Line::parse(line).filter(|l| l.kind == DROPPED)?;
    let v = l.get(DROPPED)?;
    (v.len() == DROPPED_WIDTH).then(|| v.parse().ok()).flatten()
}

fn unescape(v: &str) -> Cow<'_, str> {
    if !v.contains('\\') {
        return Cow::Borrowed(v);
    }
    let mut out = String::with_capacity(v.len());
    let mut it = v.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    Cow::Owned(out)
}

/// A parsed line, borrowing the text it was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Line<'a> {
    pub unix: u64,
    pub kind: &'a str,
    rest: &'a str,
}

impl<'a> Line<'a> {
    /// None for a line without a time and a kind: a torn last line, or
    /// text that is not a fact.
    pub fn parse(line: &'a str) -> Option<Line<'a>> {
        let line = line.strip_suffix('\n').unwrap_or(line);
        let mut parts = line.splitn(3, '\t');
        let unix = parts.next()?.parse().ok()?;
        let kind = parts.next().filter(|k| is_word(k))?;
        Some(Line { unix, kind, rest: parts.next().unwrap_or("") })
    }

    /// Every `key=value` field in order; a field without `=` or with a
    /// key that is not a word is skipped.
    pub fn fields(&self) -> impl Iterator<Item = (&'a str, Cow<'a, str>)> + 'a {
        self.rest
            .split('\t')
            .filter_map(|f| f.split_once('='))
            .filter(|(k, _)| is_word(k))
            .map(|(k, v)| (k, unescape(v)))
    }

    /// The first value of `key`.
    pub fn get(&self, key: &str) -> Option<Cow<'a, str>> {
        self.fields().find(|(k, _)| *k == key).map(|(_, v)| v)
    }
}

/// The UTC day of `unix` as `YYYYMMDD`.
pub fn day(unix: u64) -> String {
    let z = (unix / SECS_PER_DAY) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}")
}

/// The file name of `unix`'s day: `YYYYMMDD.tsv`.
pub fn day_file(unix: u64) -> String {
    format!("{}.tsv", day(unix))
}

/// The `YYYYMMDD` a file name of [`day_file`] names.
pub fn day_of_file(name: &str) -> Option<&str> {
    let d = name.strip_suffix(".tsv")?;
    (d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit())).then_some(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_reads_back_with_its_values_unescaped() {
        let mut s = String::new();
        push(&mut s, 1_764_000_000, "refused", &[("agent", "claude"), ("why", "a\tb\nc\\d=e")]);
        assert_eq!(s, "1764000000\trefused\tagent=claude\twhy=a\\tb\\nc\\\\d=e\n");
        let l = Line::parse(&s).unwrap();
        assert_eq!((l.unix, l.kind), (1_764_000_000, "refused"));
        assert_eq!(l.get("why").as_deref(), Some("a\tb\nc\\d=e"));
        assert_eq!(l.fields().count(), 2);
    }

    #[test]
    fn readers_skip_what_they_do_not_know() {
        let l = Line::parse("5\tlater_kind\tnew_key=1\tnot a field\tBad=2\tk=v").unwrap();
        assert_eq!(l.kind, "later_kind");
        let keys: Vec<_> = l.fields().map(|(k, _)| k).collect();
        assert_eq!(keys, ["new_key", "k"]);
        assert_eq!(Line::parse("x\tk"), None);
        assert_eq!(Line::parse("5"), None);
        assert_eq!(Line::parse("5\tNot-A-Kind"), None);
        assert_eq!(Line::parse("5\tbare").unwrap().fields().count(), 0);
    }

    #[test]
    fn the_dropped_line_names_its_count() {
        let mut s = String::new();
        push_dropped(&mut s, 9, 41);
        assert_eq!(s, "9\tdropped\tdropped=00000000000000000041\n");
        assert_eq!(dropped_count(&s), Some(41));
        assert_eq!(dropped_count("9\tdropped\tdropped=41"), None, "a count that cannot be raised in place");
        assert_eq!(dropped_count("9\tmoved\tdropped=00000000000000000041"), None);
    }

    #[test]
    fn days_are_utc_dates() {
        assert_eq!(day(0), "19700101");
        assert_eq!(day(1_764_115_200), "20251126");
        assert_eq!(day_file(1_764_115_199), "20251125.tsv");
        assert_eq!(day_of_file("20251126.tsv"), Some("20251126"));
        assert_eq!(day_of_file("2025112.tsv"), None);
        assert_eq!(day_of_file("20251126.log"), None);
    }
}

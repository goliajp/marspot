//! Path candidates: how far one may extend, what it looks like, and where the disk says it ends.

use crate::line::LineSegment;
use crate::oracle::{PathOracle, PathVerdict};
use crate::seam::seam_cuts_a_name;
use crate::sink::{TextBufs, unquote_span};

/// Does this span look like it could name something relative to a
/// working directory?
///
/// The filesystem is the arbiter — a candidate only becomes a link if
/// it resolves — but the filesystem must not be ASKED about every word
/// on screen, and a word that happens to match a file in `~` (`Music`,
/// `Public`) should not light up in prose.  So a candidate has to
/// carry some evidence of being a path before it is worth resolving:
///
///   * an explicit relative marker — `./x`, `../x`
///   * a separator — `src/main.rs`, `bin/run.sh`
///   * a file extension — `Cargo.toml`, `README.md`
///
/// The extension rule is what keeps numbers out: it must be 1..=8
/// characters, alphanumeric, and START with a letter, so `1.5`,
/// `2026.09` and `v1.2.3` are not candidates, while `a.c` is.  The
/// stem must contain a letter for the same reason.
pub(crate) fn looks_like_relative_path(span: &[char]) -> bool {
    if span.is_empty() || span[0] == '/' || span[0] == '~' {
        return false;
    }
    if span.starts_with(&['.', '/']) || span.starts_with(&['.', '.', '/']) {
        return true;
    }
    // A separator, but not a leading or trailing one.
    if let Some(p) = span.iter().position(|&c| c == '/') {
        return p > 0 && p + 1 < span.len();
    }
    let Some(dot) = span.iter().rposition(|&c| c == '.') else { return false };
    if dot == 0 || dot + 1 >= span.len() {
        return false;
    }
    let (stem, ext) = (&span[..dot], &span[dot + 1..]);
    if !stem.iter().any(|c| c.is_alphabetic()) {
        return false;
    }
    ext.len() <= 8
        && ext[0].is_ascii_alphabetic()
        && ext.iter().all(|c| c.is_ascii_alphanumeric())
}

/// `<cwd>/<rel>` into `out`, with a leading `./` folded away.  `..` is
/// left for the filesystem to resolve — it is the one that knows about
/// symlinks.
pub(crate) fn join_cwd_into(out: &mut String, cwd: &str, rel: &str) {
    let rel = rel.strip_prefix("./").unwrap_or(rel);
    out.clear();
    out.push_str(cwd.trim_end_matches('/'));
    out.push('/');
    out.push_str(rel);
}

/// Is the character before `i` one that would make this the middle of
/// a larger token rather than the start of one?
pub(crate) fn is_left_boundary_pathish(chars: &[char], i: usize) -> bool {
    i > 0
        && (chars[i - 1].is_alphanumeric()
            // A colon is NOT in this set.  It reads as "inside a
            // token" almost nowhere and as a separator almost
            // everywhere a program writes one: `wrote:path`,
            // `Note:path`, `写好了:.notes/x.md`.  Counting it
            // as inside cost the whole rest of the line, because a
            // candidate that starts too early and fails to resolve
            // also blocks every start position it covered — the run
            // from `写` swallowed the colon, the path and the `(442`
            // after it, resolved to nothing, and the path could never
            // be tried on its own (2026-09-11 report).
            //
            // Nothing that needs one loses it: `src/main.rs:42` starts
            // before the colon, and a URL is matched whole before any
            // path candidate is considered.
            || matches!(chars[i - 1], '/' | '.' | '-' | '_' | '~' | '@'))
}

/// Strip a trailing `:line(:col)?` suffix (compiler / panic output
/// like `/…/file.rs:120:5`) from a path span so the stat check sees
/// the bare path.  Returns the new end; unchanged when no such
/// suffix exists.  At most two `:digits` groups are stripped.
fn strip_line_col_suffix(chars: &[char], start: usize, end: usize) -> usize {
    let mut e = end;
    for _ in 0..2 {
        let mut j = e;
        while j > start && chars[j - 1].is_ascii_digit() {
            j -= 1;
        }
        if j < e && j > start && chars[j - 1] == ':' {
            e = j - 1;
        } else {
            break;
        }
    }
    e
}

/// Widest run that could still be one filesystem path.
///
/// Stops only where a path genuinely **cannot** continue: whitespace,
/// control characters, and the quoting / redirection metacharacters a
/// shell would need escaped anyway.
///
/// Punctuation is deliberately NOT a stop — not ASCII `(`, not the
/// fullwidth CJK family.  It used to be, on the reasoning that CJK
/// prose glues those onto paths far more often than filenames contain
/// them (宁可漏不可错).  That reasoning is sound only for a scanner
/// with no way to check its answer, and this one has the best check
/// there is: **a path link is only ever emitted for a path that
/// exists on disk.**  Guessing narrow could only ever lose true
/// positives — which it did: a Japanese document named
/// `株主総会議事録（役員報酬改定・20260805）.pdf` was cut at `（`,
/// and the line lost its link entirely (2026-08-05 report).
///
/// So: scan greedily, and let [`resolve_path_end`] ask the filesystem
/// where the name really ended.
pub(crate) fn scan_path_candidate(chars: &[char], start: usize) -> usize {
    let mut i = start;
    while i < chars.len() {
        let c = chars[i];
        // A quoted run, or an escaped space.  A path with a space in
        // it is not rare — it is what `~/Library/Safari/"Favicon
        // Cache"` looks like the moment anyone pastes a command that
        // touches one — and stopping at the quote leaves the link
        // ending mid-name (2026-08-09 report).  Both shells' spellings
        // are accepted; [`unquote_path`] takes them back off before
        // anything is asked of the filesystem.
        if (c == '"' || c == '\'') && i > start {
            match closing_quote(chars, i) {
                Some(close) => {
                    i = close + 1;
                    continue;
                }
                // An unmatched quote is prose, not a name.
                None => break,
            }
        }
        if c == '\\' && i + 1 < chars.len() && chars[i + 1] == ' ' {
            i += 2;
            continue;
        }
        if c.is_whitespace() || c == '\0' || (c.is_control() && c != '\t') {
            break;
        }
        if matches!(c, '<' | '>' | '"' | '\'' | '`' | '|') {
            break;
        }
        i += 1;
    }
    i
}

/// The partner of the quote at `open`, if it is on this line and
/// close enough to be one.
///
/// Bounded because an unpaired quote is common in prose (`don't`,
/// `"as we said"`) and an unbounded search would happily pair one
/// with another sentence's, swallowing the line between them.  The
/// bound is generous — names with spaces are still names, not
/// paragraphs — and anything it lets through still has to survive
/// `stat`.
fn closing_quote(chars: &[char], open: usize) -> Option<usize> {
    const MAX_QUOTED_LEN: usize = 96;
    let q = chars[open];
    let hi = (open + 1 + MAX_QUOTED_LEN).min(chars.len());
    (open + 1..hi).find(|&j| chars[j] == q)
}

/// Take shell quoting back off a candidate before the filesystem is
/// asked about it.
///
/// The screen shows `~/Library/Safari/"Favicon Cache"`; the thing on
/// disk is `~/Library/Safari/Favicon Cache`.  The link's *span* stays
/// on what is drawn — that is what the user points at — while its
/// target is what this writes into `out`.
pub(crate) fn unquote_path_into(out: &mut String, s: &str) {
    out.clear();
    let mut it = s.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = it.next() {
        match c {
            // Inside single quotes a backslash is literal, as in sh.
            '\\' if quote != Some('\'') => {
                if let Some(n) = it.next() {
                    out.push(n);
                }
            }
            '"' | '\'' if quote.is_none() => quote = Some(c),
            c if Some(c) == quote => quote = None,
            other => out.push(other),
        }
    }
}

/// Could prose have started here?
///
/// Every such position is a candidate end for the path — nothing
/// more.  Being generous costs one `stat` that the cache mostly
/// absorbs; being stingy costs a link the user can see is missing.
///
/// `:` is the one mark deliberately left out.  `path:120:5` is
/// already arbitrated by [`strip_line_col_suffix`], and the rest of
/// that family — `path:note`, `host:path` — was settled as 宁可漏 by
/// the 2026-07-13 report.  Cutting there would reach past what this
/// change is about: the greedy scan exists so that punctuation stops
/// **terminating** names, not so that every mark becomes a place to
/// chop one.
///
/// ASCII `.` is out for a sharper reason: inside a token it is not
/// prose, it is the extension separator.  Cutting there turns a file
/// that does NOT exist into its prefix that does — and the prefix is
/// typically the directory the file is about to be written into.
/// Reported 2026-09-11: `.notes/exprtool.html` had not been
/// generated yet, `.notes/exprtool/` beside it had, and the line
/// drew a link to the directory with `.html` left as plain text.
/// Nothing is lost by leaving it out — a `.` that really does end a
/// sentence sits at the end of the token, where
/// [`trim_sentence_tail`] already takes it off, and the CJK stops
/// (`。`, `，`, `——`) that end a Chinese clause are still here.
fn is_prose_cut_point(c: char) -> bool {
    matches!(
        c,
        ',' | ';' | '!' | '?' | '(' | ')' | '[' | ']' | '{' | '}'
            // A quote both opens a name-with-spaces and closes one, so
            // it is also where a name can end and prose resume.  The
            // greedy scan pairs them; this is what lets the arbitration
            // back out when the pairing was wrong.
            | '"' | '\''
    ) || matches!(
        c,
        '\u{3001}'..='\u{3003}'   // 、。〃
            | '\u{3008}'..='\u{301F}' // 〈〉《》「」『』【】〔〕〖〗〘〙〚〛〜〝〞
            | '\u{FF01}'            // ！
            | '\u{FF08}' | '\u{FF09}' // （）
            | '\u{FF0C}'            // ，
            | '\u{FF1A}' | '\u{FF1B}' // ：；
            | '\u{FF1F}'            // ？
            | '\u{2018}' | '\u{2019}' | '\u{201C}' | '\u{201D}' // 弯引号
            | '\u{2026}'            // …
            | '\u{30FB}'            // ・
            // The dash family.  `——` is how Chinese prose attaches a
            // gloss to the thing it just named, so it lands glued to
            // the end of a path with no space to stop the greedy scan
            // (`…/paper1-discovers.html——页首是三条路与定论`, 2026-09-04
            // field report).  ASCII `-` stays out — filenames are full
            // of it — but these have no business inside one, and even
            // if a name did carry one the longer candidate is offered
            // to the filesystem first and wins.
            | '\u{2013}' | '\u{2014}' | '\u{2015}' // – — ―
            | '\u{2212}' | '\u{FF0D}' // − －
            | '\u{00B7}' | '\u{2022}' // · •
            | '\u{FF0E}' | '\u{FF5E}' // ． ～
    )
}

/// Most candidate ends we will `stat` for one token.
///
/// This runs on the render path — `build_instances` scans every
/// visible pane every frame — so the greedy scan has to come with a
/// ceiling.  A real path glued to prose resolves within the first two
/// or three tries; a line of pure punctuation is what the cap is for.
const MAX_PATH_CANDIDATES: usize = 24;

/// Trim the trailing marks that end a sentence rather than a name.
///
/// Tried as a *variant* of each candidate, never instead of it: a
/// filename really ending in one of these is legal, and the untrimmed
/// form is offered to the filesystem first.
fn trim_sentence_tail(chars: &[char], lo: usize, mut end: usize) -> usize {
    while end > lo
        && matches!(chars[end - 1], ',' | '.' | ';' | ':' | ')' | ']' | '}' | '!' | '?')
    {
        end -= 1;
    }
    end
}

/// Where did the path actually end?  Ask the disk, longest first.
///
/// The scan is deliberately greedy, so this is the whole arbitration:
/// each candidate end is offered to `is_real_path`, longest first, and
/// the first one that exists wins.  Longest-first matters — when both
/// a file and the directory prefixing it exist, the line is pointing
/// at the file.
///
/// Cannot invent a link: prose does not name files that exist.
pub(crate) fn resolve_path_end(
    chars: &[char],
    lo: usize,
    hi: usize,
    segments: &[LineSegment],
    oracle: &dyn PathOracle,
    bufs: &mut TextBufs,
) -> Option<usize> {
    resolve_path_end_from(chars, lo, hi, segments, oracle, None, bufs)
}

/// [`resolve_path_end`] with an optional base directory.  When `base`
/// is set the candidate is RELATIVE: every trimming step is the same,
/// but what gets asked of the filesystem is `<base>/<candidate>`, so
/// the returned end still indexes the text as drawn.
pub(crate) fn resolve_path_end_from(
    chars: &[char],
    lo: usize,
    hi: usize,
    segments: &[LineSegment],
    oracle: &dyn PathOracle,
    base: Option<&str>,
    bufs: &mut TextBufs,
) -> Option<usize> {
    let mut tried = 0usize;
    let mut last: Option<usize> = None;
    let mut consider = |end: usize, tried: &mut usize, last: &mut Option<usize>| -> bool {
        if end <= lo || *last == Some(end) || *tried >= MAX_PATH_CANDIDATES {
            return false;
        }
        *last = Some(end);
        *tried += 1;
        let span = &chars[lo..end];
        match base {
            None => {
                if !looks_like_path(span) {
                    return false;
                }
                unquote_span(bufs, span);
                is_real_path(oracle, &bufs.path)
            }
            Some(cwd) => {
                if !looks_like_relative_path(span) {
                    return false;
                }
                unquote_span(bufs, span);
                join_cwd_into(&mut bufs.joined, cwd, &bufs.path);
                is_real_path(oracle, &bufs.joined)
            }
        }
    };
    // The whole token, then the whole token minus its sentence tail.
    if consider(hi, &mut tried, &mut last) {
        return Some(hi);
    }
    let trimmed = trim_sentence_tail(chars, lo, hi);
    if trimmed < hi && consider(trimmed, &mut tried, &mut last) {
        return Some(trimmed);
    }
    // rustc / panic output habitually appends `:line(:col)`.
    let stripped = strip_line_col_suffix(chars, lo, hi);
    if stripped < hi && consider(stripped, &mut tried, &mut last) {
        return Some(stripped);
    }
    // Then the seams claudecode broke the line at, and every point
    // prose could have started — one pass, longest first.
    //
    // A seam matters when the line broke at a space: the space
    // survives nowhere, so the merged text has two tokens run
    // together and the seam is exactly where they part.  But it is
    // one candidate end among the others, not a rule that outranks
    // them.  It used to be tried first, and a path broken right
    // before a `/` then stopped at the directory on the upper row
    // even when the full file name, ended by the `。` glued to it,
    // existed too (2026-09-16:
    // `…/buwanren/notes/rfcs` ⏎ `/20260916-handoff.md。按项目…`).
    // Longest-first is the rule everywhere else in this function —
    // when a file and the directory above it both exist, the line is
    // pointing at the file.
    for cut in (lo + 1..hi).rev() {
        if tried >= MAX_PATH_CANDIDATES {
            break;
        }
        let is_seam = segments.iter().any(|seg| seg.cc_seam && seg.char_offset == cut);
        if is_seam && !seam_cuts_a_name(chars, cut) && consider(cut, &mut tried, &mut last) {
            return Some(cut);
        }
        if !is_prose_cut_point(chars[cut]) {
            continue;
        }
        let end = trim_sentence_tail(chars, lo, cut);
        if consider(end, &mut tried, &mut last) {
            return Some(end);
        }
    }
    None
}

/// Does this slice look enough like a path that it's WORTH the
/// follow-up `stat()` check?  Cheap structural filter that throws
/// out obvious garbage so we don't spend syscalls on it:
///
///   - too short to be plausible (`/`, `~/`)
///   - contains consecutive slashes (`//`, `//./`, `////`)
///   - root + dot-only components (`/.`, `/..`, `/./`)
///   - no actual letter or digit anywhere (only slashes, dots, dashes)
///
/// Real existence is verified by `is_real_path`; this just avoids
/// asking the filesystem about strings that couldn't possibly be a
/// filename a human typed or a tool printed.
pub(crate) fn looks_like_path(chars: &[char]) -> bool {
    if chars.len() < 2 {
        return false;
    }
    // No consecutive slashes anywhere.
    for w in chars.windows(2) {
        if w[0] == '/' && w[1] == '/' {
            return false;
        }
    }
    // Skip the leading `/` or `~/` prefix.
    let body_start = if chars[0] == '~' { 2 } else { 1 };
    if body_start >= chars.len() {
        return false;
    }
    let body = &chars[body_start..];
    // Must contain at least one alphanumeric char somewhere in the body
    // (rules out `/./`, `/..`, `/-/-/`, etc).
    if !body.iter().any(|c| c.is_alphanumeric()) {
        return false;
    }
    // Reject paths whose every component is only dots (`/././.`).
    let mut all_dots = true;
    for seg in body.split(|c| *c == '/') {
        if !seg.is_empty() && !seg.iter().all(|c| *c == '.') {
            all_dots = false;
            break;
        }
    }
    if all_dots {
        return false;
    }
    true
}

/// Ask the oracle whether the candidate path is real.  Only
/// `Exists` makes a link: `Unknown` — the async oracle has not
/// resolved this path yet — reads exactly like `Missing` here, so a
/// path the oracle has not seen simply is not underlined on this
/// frame.  It becomes a link on a later frame, once the answer
/// lands.  That collapse is the whole point of the three-state
/// verdict: the scanner needs no notion of "pending".
pub(crate) fn is_real_path(oracle: &dyn PathOracle, path: &str) -> bool {
    matches!(oracle.probe(path), PathVerdict::Exists)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{chars, scan};
    use crate::{LinkKind, LinkRange};

    /// 2026-09-06 report: `file:///Users/…/paper1-discovers.html`
    /// underlined from the `/Users` on, leaving `file:` beside the
    /// link — the path matched on its own, so the click worked while
    /// the selection started four characters late.
    #[test]
    fn a_file_url_is_one_link_including_its_scheme() {
        let root = std::env::temp_dir().join(format!("marspot-fileurl-{}", std::process::id()));
        let deep = root.join(".notes");
        std::fs::create_dir_all(&deep).unwrap();
        let f = deep.join("paper1-discovers.html");
        std::fs::write(&f, b"x").unwrap();
        let p = f.display().to_string();
        let url = format!("file://{p}");

        let bare = scan(&url);
        let in_prose = scan(&format!("open {url} now"));
        std::fs::remove_dir_all(&root).ok();

        assert_eq!(bare.len(), 1, "{bare:?}");
        assert_eq!(bare[0].text, url, "the scheme belongs to the link");
        assert_eq!(bare[0].kind, LinkKind::File, "it names a file, not a web page");
        assert!(
            in_prose.iter().any(|l| l.text == url),
            "surrounded by prose it is still one link: {in_prose:?}",
        );
    }

    /// A `file://` URL naming something that is NOT there must not be
    /// underlined.  This is the whole reason it is a `File` and not a
    /// `Url`: a URL has no arbiter, and would happily link a file that
    /// does not exist.
    #[test]
    fn a_file_url_for_a_missing_path_is_not_a_link() {
        let v = scan("file:///nope/definitely/not/here-9c3f1e.html");
        assert!(
            !v.iter().any(|l| l.text.starts_with("file://")),
            "missing file must not link: {v:?}",
        );
    }

    #[test]
    fn absolute_path_stats_real_file() {
        // The test binary itself is guaranteed to exist on disk.
        let bin = std::env::current_exe().unwrap();
        let line = format!("see {} for details", bin.display());
        let v = scan(&line);
        assert_eq!(v.iter().filter(|r| r.kind == LinkKind::File).count(), 1);
        assert_eq!(v[0].kind, LinkKind::File);
        assert_eq!(v[0].text, bin.display().to_string());
    }

    /// Pinned while chasing the 2026-09-04 truncation, which stopped
    /// at `…/spg/` — the segment right before a dotted directory.
    /// The scanner turned out to be innocent (the cause was upstream,
    /// see `a_caret_resting_on_a_wrapped_tail_is_not_a_composer`), but
    /// a dotted component IS the point a wrong answer backs off to, so
    /// the boundary is worth holding: `.config`, `.git`, `.local` are
    /// where a developer's own files live.
    #[test]
    fn a_dotted_directory_component_does_not_truncate_the_path() {
        let root = std::env::temp_dir()
            .join(format!("marspot-linkify-dotdir-{}", std::process::id()));
        let deep = root.join(".notes").join("docs");
        std::fs::create_dir_all(&deep).unwrap();
        let file = deep.join("provenance-exec-tax-2026-09-04.md");
        std::fs::write(&file, b"x").unwrap();
        let p = file.display().to_string();

        let bare = scan(&p);
        // A trailing argument is how the line actually appeared.
        let with_arg = scan(&format!("{p} -n 5"));
        std::fs::remove_dir_all(&root).ok();

        assert_eq!(bare.len(), 1, "bare path: {bare:?}");
        assert_eq!(bare[0].text, p, "bare path truncated");
        assert_eq!(with_arg.len(), 1, "path + arg: {with_arg:?}");
        assert_eq!(with_arg[0].text, p, "path with trailing arg truncated");
    }

    /// 2026-08-02 field report: a `⏺` summary ending
    /// `…/sentori-feedback-reply-b-section-2.md;memory 已记(…)` showed
    /// no link at all.  The path was real, the wrap merge was right —
    /// the sentence simply resumed straight after the `;`, so the
    /// scanner stat'ed `…-2.md;memory` and got nothing.  Chinese
    /// written with ASCII punctuation puts no space after the mark, so
    /// this is not an edge case in this codebase; it is most lines.
    #[test]
    fn prose_resuming_after_a_mark_does_not_swallow_the_path() {
        let bin = std::env::current_exe().unwrap();
        let p = bin.display().to_string();
        for line in [
            format!("- 回执:{p};memory 已记"),
            format!("- 回执:{p},另见下条"),
            format!("见 {p}!下一条"),
        ] {
            let v = scan(&line);
            let files: Vec<&LinkRange> = v.iter().filter(|r| r.kind == LinkKind::File).collect();
            assert_eq!(files.len(), 1, "no link in {line:?}");
            assert_eq!(files[0].text, p, "wrong span in {line:?}");
        }
    }

    /// …and the mark is only a *candidate* end.  A filename that
    /// really contains one is legal, and the full span is stat'ed
    /// before any cut is tried, so it still wins outright.
    #[test]
    fn a_filename_that_really_contains_a_mark_still_wins() {
        let p = std::env::temp_dir().join(format!(
            "marspot-linkify-a;b,c-{}.txt",
            std::process::id()
        ));
        std::fs::write(&p, b"x").unwrap();
        let text = p.display().to_string();
        let v = scan(&format!("see {text} today"));
        let files: Vec<&LinkRange> = v.iter().filter(|r| r.kind == LinkKind::File).collect();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].text, text, "the whole name, marks and all");
        let _ = std::fs::remove_file(&p);
    }

    /// 2026-08-05 report: a Japanese document path went unlinked —
    /// and the design lesson behind the fix.
    ///
    /// `株主総会議事録（役員報酬改定・20260805）.pdf`.  The scan used to
    /// stop at `（`, because CJK prose glues brackets onto paths
    /// constantly and a filename containing them was judged rarer
    /// (宁可漏不可错).  But that trade only makes sense for a scanner
    /// that cannot check its answer, and this one can: **a path link
    /// is only emitted for a path that exists.**  Guessing narrow
    /// could therefore only ever lose true positives.
    ///
    /// So the scan is greedy and the filesystem decides.  This test
    /// pins the reported case; `punctuation_inside_a_real_name_is_not
    /// _a_terminator` pins the principle it is an instance of.
    #[test]
    fn a_filename_with_fullwidth_brackets_is_one_link() {
        let dir = std::env::temp_dir().join(format!(
            "marspot-linkify-brackets-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("株主総会議事録（役員報酬改定・20260805）.pdf");
        std::fs::write(&p, b"x").unwrap();
        let text = p.display().to_string();

        // Exactly the reported line: prose in ASCII parens glued to
        // the end, a fullwidth stop after that.
        let v = scan(&format!("一页收好了。{text}(docx 同目录同名)。"));
        let files: Vec<&LinkRange> = v.iter().filter(|r| r.kind == LinkKind::File).collect();
        assert_eq!(files.len(), 1, "no link in the reported line");
        assert_eq!(files[0].text, text, "the whole name, brackets and all");

        // The same shape naming a file that does NOT exist stays
        // unlinked — the extension is not a licence to guess.
        let ghost = dir.join("株主総会議事録（不存在）.pdf");
        let v = scan(&format!("见 {}", ghost.display()));
        assert!(
            v.iter().all(|r| r.kind != LinkKind::File),
            "a bracket group that names nothing must not become a link"
        );

        // And a genuine prose parenthetical after a real directory
        // still links only the directory.
        let v = scan(&format!("见 {}（说明）的东西", dir.display()));
        let files: Vec<&LinkRange> = v.iter().filter(|r| r.kind == LinkKind::File).collect();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].text, dir.display().to_string(), "prose stays prose");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The principle, not the instance: **no punctuation terminates a
    /// path**, because the filesystem is a better judge than any table
    /// of characters.
    ///
    /// Each of these names contains a character the old scan treated
    /// as a hard stop, so none of them could be linked at all — the
    /// span was cut inside the name, the prefix did not exist, and the
    /// line lost its link.  Each is also followed by prose glued on
    /// with the *same* character, which is the case that table was
    /// protecting against; the greedy scan gets both right by asking.
    #[test]
    fn punctuation_inside_a_real_name_is_not_a_terminator() {
        let dir = std::env::temp_dir().join(format!(
            "marspot-linkify-punct-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // (name, the prose glued straight onto it)
        let cases = [
            ("plan(v2).md", "、然后再说"),
            ("a,b.txt", ",这句话继续"),
            ("sec；1.md", ";memory 已记"),
            ("note【草稿】.md", "。下一步"),
        ];
        for (name, tail) in cases {
            let p = dir.join(name);
            std::fs::write(&p, b"x").unwrap();
            let text = p.display().to_string();
            let v = scan(&format!("见 {text}{tail}"));
            let files: Vec<&LinkRange> =
                v.iter().filter(|r| r.kind == LinkKind::File).collect();
            assert_eq!(files.len(), 1, "no link for {name:?}");
            assert_eq!(
                files[0].text, text,
                "{name:?} lost part of its name to the prose after it"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A name with a space in it, spelled the way a shell spells it.
    ///
    /// 2026-08-09 report: `rm -rf ~/Library/Safari/"Favicon Cache"`
    /// linked nothing.  The scan stopped at the quote, so the
    /// candidate was `~/Library/Safari/` — a real directory, but not
    /// the thing the line points at, and not what the user was
    /// pointing their cursor at either.
    ///
    /// Both spellings are exercised, and both halves of the contract:
    /// the **span** covers what is drawn (quotes and all — that is
    /// what the user clicks), the **text** is what opens.
    #[test]
    fn a_quoted_or_escaped_space_is_part_of_the_name() {
        let dir = std::env::temp_dir().join(format!(
            "marspot-linkify-space-{}",
            std::process::id()
        ));
        let inner = dir.join("Favicon Cache");
        std::fs::create_dir_all(&inner).unwrap();
        let real = inner.display().to_string();
        let parent = dir.display().to_string();

        // Each spelling, and what the drawn text is.
        let drawn = [
            format!("{parent}/\"Favicon Cache\""),
            format!("{parent}/'Favicon Cache'"),
            format!("{parent}/Favicon\\ Cache"),
        ];
        for d in drawn {
            let line = format!("rm -rf {d}");
            let v = scan(&line);
            let files: Vec<&LinkRange> =
                v.iter().filter(|r| r.kind == LinkKind::File).collect();
            assert_eq!(files.len(), 1, "no link for {d:?}");
            assert_eq!(
                files[0].text, real,
                "{d:?} must open the real path, not the quoted spelling"
            );
            // The span ends where the drawn text ends — including the
            // closing quote, so the whole thing underlines rather than
            // stopping mid-name.
            let last = line.chars().count() as u16 - 1;
            assert!(
                files[0].col_end >= last,
                "{d:?}: underline stops at col {} but the name runs to {last}",
                files[0].col_end
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unpaired quote is prose, not the start of a name — and the
    /// arbitration has to be able to back out of a pairing that
    /// swallowed too much.
    #[test]
    fn an_unpaired_quote_does_not_swallow_the_line() {
        let dir = std::env::temp_dir().join(format!(
            "marspot-linkify-quote-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.display().to_string();
        // The directory is real; the quote after it opens nothing.
        let v = scan(&format!("cd {d}/\" then he said \"hello\" and left"));
        let files: Vec<&LinkRange> =
            v.iter().filter(|r| r.kind == LinkKind::File).collect();
        for f in &files {
            assert!(
                !f.text.contains("hello"),
                "a quote pairing swallowed the sentence: {:?}",
                f.text
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The greedy scan runs on the render path, so it comes with a
    /// ceiling: a token that is nothing but cut points must not turn
    /// into a `stat` storm, and must still link nothing.
    #[test]
    fn a_token_of_pure_punctuation_is_bounded_and_links_nothing() {
        let junk: String = std::iter::repeat_n("/a、", 200).collect();
        let v = scan(&format!("见 {junk} 完"));
        assert!(
            v.iter().all(|r| r.kind != LinkKind::File),
            "nothing here exists, so nothing here is a link"
        );
    }

    /// 2026-09-04 field report: `——` glues a Chinese gloss straight
    /// onto the name it just introduced, with no space for the greedy
    /// scan to stop on.  The dash was not a place the arbitration
    /// would cut, so the walk back went from
    /// `…/paper1-discovers.html——页首是…` past the extension to
    /// `…/paper1-discovers`, and the link that finally existed was the
    /// directory two levels up.
    #[test]
    fn an_em_dash_gloss_does_not_eat_the_extension() {
        let p = std::env::temp_dir().join(format!(
            "marspot-linkify-emdash-{}.html",
            std::process::id()
        ));
        std::fs::write(&p, b"x").unwrap();
        let text = p.display().to_string();
        let v = scan(&format!("open {text}——页首是三条路与定论"));
        let files: Vec<&LinkRange> = v.iter().filter(|r| r.kind == LinkKind::File).collect();
        assert_eq!(
            files.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            vec![text.as_str()],
            "the path ends where the gloss begins"
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn absolute_path_nonexistent_is_rejected() {
        // Pattern matches but stat fails → no link.
        let v = scan("see /Users/x/foo_definitely_not_here.txt for details");
        assert_eq!(v.iter().filter(|r| r.kind == LinkKind::File).count(), 0);
    }

    #[test]
    fn double_slash_rejected_by_pattern() {
        assert!(!looks_like_path(&chars("//")));
        assert!(!looks_like_path(&chars("//./")));
        assert!(!looks_like_path(&chars("////")));
        assert!(!looks_like_path(&chars("/x//y")));
    }

    #[test]
    fn dot_only_components_rejected_by_pattern() {
        assert!(!looks_like_path(&chars("/.")));
        assert!(!looks_like_path(&chars("/./")));
        assert!(!looks_like_path(&chars("/../..")));
    }

    #[test]
    fn mid_word_slash_is_not_a_path() {
        // `cargo/Cargo.toml` shouldn't trip the path detector.
        let v = scan("see cargo/Cargo.toml today");
        assert_eq!(v.iter().filter(|r| r.kind == LinkKind::File).count(), 0);
    }

    #[test]
    fn bare_slash_is_not_a_path() {
        let v = scan("only a /");
        assert_eq!(v.iter().filter(|r| r.kind == LinkKind::File).count(), 0);
    }
}

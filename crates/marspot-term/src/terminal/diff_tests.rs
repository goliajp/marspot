//! The bulk feed against the reference feed: same bytes, same chunk
//! boundaries, and nothing that can be observed may differ.

use super::Terminal;
use crate::grid::Grid;
use crate::scrollback::Scrollback;
use crate::test_env::TempDir;

/// The four bench corpora and the cluster one, a few kilobytes of
/// each, built from the same lines `bin/gen-scenarios.sh` repeats.
fn corpora() -> Vec<(&'static str, Vec<u8>)> {
    let ascii = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. \
        Sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.\n\
        The quick brown fox jumps over the lazy dog. 0123456789!@#$%^&*()\n"
        .repeat(12);
    let colours = [31, 32, 33, 34, 35, 36, 37, 91, 92, 93, 94, 95, 96];
    let mut mixed = String::new();
    for i in 0..60usize {
        let c = colours[i % colours.len()];
        mixed.push_str(&format!(
            "\x1b[{c}m[INFO]\x1b[0m request {i:08} processed in {:4}ms\n",
            (i * 7) % 9999
        ));
    }
    let cjk = "你好世界 こんにちは世界 안녕하세요 세계\n\
        床前明月光 疑是地上霜 举头望明月 低头思故乡\n\
        東京の夜空に星が輝いている 心の中の温かい記憶\n"
        .repeat(16);
    let emoji = "🚀 ✨ 🎉 🌏 🌙 ⭐ 🔥 💫 🌈 🦄 🐉 🎨 🎭 🎪 🎨 🎯\n\
        deploy 🚢 done ✅ tests 🧪 passed all 100% 💯 ship 🚀 it\n"
        .repeat(16);
    let clusters = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466} \
        \u{1F469}\u{1F3FD}\u{200D}\u{1F4BB} \u{1F44D}\u{1F3FF} \
        \u{1F1EF}\u{1F1F5} \u{1F1E9}\u{1F1EA} \u{1F1FA}\u{1F1F8}\n\
        café naïve Angèle ⚠️ ❤️ क्क 각\n"
        .repeat(12);
    vec![
        ("ascii", ascii.into_bytes()),
        ("mixed", mixed.into_bytes()),
        ("cjk", cjk.into_bytes()),
        ("emoji", emoji.into_bytes()),
        ("clusters", clusters.into_bytes()),
    ]
}

/// Short inputs aimed at the places the lanes hand over to the
/// per-character path, or to each other.
fn adversarial() -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = Vec::new();
    let mut s = |x: &str| v.push(x.as_bytes().to_vec());
    // combining marks at column 0, mid-line, and after a wide glyph
    s("\u{0301}abc");
    s("a\u{0301}\u{0302}b");
    s("\r\n\u{0301}x");
    s("中\u{0301}文");
    s("e\u{0301}中文abc");
    // marks and selectors meeting the right margin (widths 2..=8 are
    // exercised by the grid sizes below)
    s("abcdefg\u{0301}h");
    s("abcdef\u{26A0}\u{FE0F}g");
    s("abcdefg\u{26A0}\u{FE0F}g");
    s("\u{26A0}\u{FE0F}\u{26A0}\u{FE0F}\u{26A0}\u{FE0F}\u{26A0}\u{FE0F}\u{26A0}");
    // wide glyphs against the margin
    s("a中文字漢字かなカナ");
    s("中文字漢字かなカナ한글");
    s("ab🚀cd🎉ef✨gh⭐");
    s("🚀🚀🚀🚀🚀🚀🚀🚀");
    s("x🚀 ✨ 🎉 🌏 🌙 ⭐");
    // ZWJ sequences, skin tones, flags, with fast-class neighbours
    s("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}!");
    s("\u{1F468}\u{200D}\u{2705}\u{2705}");
    s("\u{1F468}\u{200D}中文");
    s("\u{1F468}\u{200D}ab");
    s("\u{200D}\u{1F469}\u{1F469}");
    s("\u{1F44D}\u{1F3FF}\u{1F44D}ok");
    s("\u{1F1EF}\u{1F1F5}\u{1F1E9}\u{1F1EA}\u{1F1FA}x");
    s("a\u{1F1EF}中\u{1F1F5}");
    // Hangul: syllables, conjoining jamo, a syllable after jamo
    s("한글\u{1100}\u{1161}\u{11A8}각");
    s("\u{1100}가각\u{11A8}나");
    // a prepend character ahead of text
    s("\u{0600}abc\u{0600}中文\u{0600}🚀");
    // Indic conjunct
    s("क्क क्\u{200D}क ab");
    // Latin-1 and other two-byte scalars between runs
    s("café naïve Ünïcödé — “quoted” ‘x’ …");
    // box drawing, ambiguous width
    s("╭──╮│ab│╰──╯①②★▲");
    // controls between text
    s("ab\tcd\x08\x08ef\rgh\nij\x0b\x0ckl\x07mn");
    s("abc\x1b[31mdef\x1b[0mghi\x1b[1;4;38;5;196;48;2;1;2;3mjkl\x1b[m");
    s("ab\x1b[4:3mcd\x1b[38:2::255:0:0mef\x1b[58:5:9mgh\x1b[4:0mij");
    s("\x1b[2;3Hxy\x1b[5Gz\x1b[Kab\x1b[1Pcd\x1b[2@ef\x1b[3Xgh\x1b[2Jij");
    s("\x1b[?7lno wrap past the edge of the row\x1b[?7hwrap again past it");
    s("\x1b[4hinserted\rmode\x1b[4lreplace");
    s("\x1b(0lqqk\x1b(Blqqk");
    s("\x1b[2;4rone\ntwo\nthree\nfour\nfive\x1b[rsix\nseven");
    s("\x1b[?1049halt 中文 🚀\x1b[?1049lmain");
    s("\x1b]0;title\x07after\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\");
    s("a\x1b7b\x1b8c\x1b[sd\x1b[ue\x1bMf\x1bDg\x1bEh");
    s("\x1b[6n\x1b[c\x1b[>c\x1b[5n");
    s("one<u>two</u>three<u>four<five</u><<u>x");
    // SGR with 0..=32 parameters
    let mut sgr = String::new();
    for n in 0..=32 {
        sgr.push_str("\x1b[");
        for k in 0..n {
            if k > 0 {
                sgr.push(';');
            }
            sgr.push_str(&(30 + k * 3 % 78).to_string());
        }
        sgr.push_str("mx");
    }
    v.push(sgr.into_bytes());
    // not UTF-8: stray continuations, truncated sequences, overlong
    // forms, surrogates, out of range, bad leads
    for raw in [
        &b"ab\x80cd\xbfef"[..],
        b"\xe4\xb8",
        b"\xe4\xb8x\xe4yz",
        b"\xf0\x9f\x9a",
        b"\xf0\x9f\x9ax\xf0\x9fy\xf0z",
        b"\xe0\x80\xa0\xe0\x81\x81ab",
        b"\xf0\x80\x80\x41\xf0\x84\xb8\x80ab",
        b"\xc0\x80\xc1\xbfab",
        b"\xed\xa0\x80\xed\xbf\xbfab",
        b"\xf4\x90\x80\x80\xf5\x80\x80\x80\xff\xfeab",
        b"\xe4\xb8\xad\xe4\xe4\xb8\xad\xf0\xe4\xb8\xad",
        b"\xf0\x9f\x9a\x80\xf0\xf0\x9f\x9a\x80\xe4\xf0\x9f\x9a\x80",
        b"\xe4\xb8\x1b[31m\xad",
        b"ab\x18cd\x1aef\x7fgh\x00ij",
        b"\x1b[31\x1b[32mab\x1b[3\x18cd\x1b[\ne",
        b"\x1b\x1b[1m\x1b[;;;m\x1b[99999;1m\x1b[1;",
    ] {
        v.push(raw.to_vec());
    }
    v
}

const SIZES: [(u16, u16); 6] = [(122, 39), (40, 6), (8, 4), (7, 3), (3, 3), (2, 2)];

fn feed_both(cols: u16, rows: u16, chunks: &[&[u8]], what: &str) {
    let mut fast = Terminal::new(cols, rows);
    let mut slow = Terminal::new(cols, rows);
    for (n, chunk) in chunks.iter().enumerate() {
        fast.feed(chunk);
        slow.feed_reference(chunk);
        if let Some(d) = fast.first_difference(&slow) {
            panic!("{what}, {cols}x{rows}, after chunk {n} of {}: {d}", chunks.len());
        }
    }
}

#[test]
fn the_corpora_land_the_same_in_chunks_of_any_size() {
    for (name, bytes) in corpora() {
        for (cols, rows) in SIZES {
            for size in [1usize, 2, 3, 4, 5, 7, 13, 64, 1024, usize::MAX] {
                let chunks: Vec<&[u8]> = bytes.chunks(size.min(bytes.len())).collect();
                // checking after every chunk of a few bytes is
                // quadratic; one check at the end covers small chunks
                if size < 64 {
                    let mut fast = Terminal::new(cols, rows);
                    let mut slow = Terminal::new(cols, rows);
                    for c in &chunks {
                        fast.feed(c);
                        slow.feed_reference(c);
                    }
                    if let Some(d) = fast.first_difference(&slow) {
                        panic!("{name}, {cols}x{rows}, chunks of {size}: {d}");
                    }
                } else {
                    feed_both(cols, rows, &chunks, name);
                }
            }
        }
    }
}

#[test]
fn adversarial_input_lands_the_same_split_at_every_byte() {
    for bytes in adversarial() {
        let what = String::from_utf8_lossy(&bytes).into_owned();
        for (cols, rows) in SIZES {
            feed_both(cols, rows, &[&bytes], &what);
            // the full-size grid costs the most to compare and wraps
            // the least
            if cols < 100 {
                for cut in 1..bytes.len() {
                    feed_both(cols, rows, &[&bytes[..cut], &bytes[cut..]], &what);
                }
            }
            let singles: Vec<&[u8]> = bytes.chunks(1).collect();
            feed_both(cols, rows, &singles, &what);
        }
    }
}

#[test]
fn adversarial_inputs_run_together_land_the_same() {
    // each input starts from whatever state the one before it left
    let all: Vec<u8> = adversarial().concat();
    for (cols, rows) in SIZES {
        for size in [1usize, 3, 5, 16, 61, usize::MAX] {
            let chunks: Vec<&[u8]> = all.chunks(size.min(all.len())).collect();
            let mut fast = Terminal::new(cols, rows);
            let mut slow = Terminal::new(cols, rows);
            for c in &chunks {
                fast.feed(c);
                slow.feed_reference(c);
            }
            if let Some(d) = fast.first_difference(&slow) {
                panic!("{cols}x{rows}, chunks of {size}: {d}");
            }
        }
    }
}

#[test]
fn a_pane_that_renders_markup_lands_the_same() {
    for bytes in adversarial().into_iter().chain(corpora().into_iter().map(|c| c.1)) {
        for (cols, rows) in [(40u16, 6u16), (7, 3)] {
            for size in [1usize, 5, usize::MAX] {
                let mut fast = Terminal::new(cols, rows);
                let mut slow = Terminal::new(cols, rows);
                fast.set_render_markup(true);
                slow.set_render_markup(true);
                for c in bytes.chunks(size.min(bytes.len())) {
                    fast.feed(c);
                    slow.feed_reference(c);
                }
                if let Some(d) = fast.first_difference(&slow) {
                    panic!("{:?}, {cols}x{rows}, chunks of {size}: {d}", String::from_utf8_lossy(&bytes));
                }
            }
        }
    }
}

/// A terminal whose history goes to files under `dir`.
fn file_backed(dir: &TempDir, tag: &str, cols: u16, rows: u16) -> Terminal {
    let sb = Scrollback::file(
        dir.join(format!("{tag}.bin")),
        dir.join(format!("{tag}.idx")),
        cols as usize,
        8,
    )
    .expect("open scrollback files");
    let mut t = Terminal::new(cols, rows);
    t.grid = Grid::with_scrollback_kind(cols, rows, sb);
    t
}

#[test]
fn the_history_files_come_out_byte_for_byte_the_same() {
    let dir = TempDir::new("feed-diff");
    let mut inputs = corpora();
    inputs.push(("adversarial", adversarial().concat()));
    for (n, (name, bytes)) in inputs.iter().enumerate() {
        for (cols, rows) in [(122u16, 39u16), (40, 6), (7, 3)] {
            let tag = format!("{n}-{cols}");
            let mut fast = file_backed(&dir, &format!("fast-{tag}"), cols, rows);
            let mut slow = file_backed(&dir, &format!("slow-{tag}"), cols, rows);
            for c in bytes.chunks(4096) {
                fast.feed(c);
                slow.feed_reference(c);
            }
            if let Some(d) = fast.first_difference(&slow) {
                panic!("{name}, {cols}x{rows}: {d}");
            }
            fast.grid().scrollback_flush_for_handoff();
            slow.grid().scrollback_flush_for_handoff();
            let lines = fast.grid().scrollback_len();
            drop((fast, slow));
            // the index is offsets into this file and is not flushed
            // by a handoff, so the records are what is compared
            let mut a = std::fs::read(dir.join(format!("fast-{tag}.bin"))).expect("read");
            let mut b = std::fs::read(dir.join(format!("slow-{tag}.bin"))).expect("read");
            assert!(lines == 0 || a.len() > 32, "{name}: {lines} lines filed, none in the file");
            // each file is stamped with when it was made and an epoch
            // of its own
            a[16..32].fill(0);
            b[16..32].fill(0);
            let at = a.iter().zip(&b).position(|(x, y)| x != y);
            assert!(
                a == b,
                "{name}, {cols}x{rows}: the files differ, {} and {} bytes, first at {at:?}",
                a.len(),
                b.len()
            );
        }
    }
}

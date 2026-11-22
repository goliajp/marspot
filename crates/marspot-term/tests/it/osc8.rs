//! OSC 8 hyperlinks, bytes in: what the screen shows, what a click
//! reaches, what the clipboard gets, and what history keeps.

use marspot_term::grid::{Cell, Grid, LINK_POOL_MAX};
use marspot_term::grid_links::LinkKind;
use marspot_term::osc8::{URI_MAX, link_at};
use marspot_term::render::grid_selection_text;
use marspot_term::scrollback::Scrollback;
use marspot_term::terminal::Terminal;

const LINE: &[u8] = b"\x1b]8;;https://example.com\x1b\\Click me\x1b]8;;\x1b\\\n";
const URI: &str = "https://example.com";

fn row_text(g: &Grid, view_offset: u16, row: u16) -> String {
    (0..g.cols()).map(|c| g.cell_at_view(view_offset, c, row).ch).collect::<String>().trim_end().to_string()
}

/// Every URI any cell of the viewport at `view_offset` resolves to,
/// with the row it is on.
fn links_in_view(g: &Grid, view_offset: u16) -> Vec<(u16, String)> {
    let mut out = Vec::new();
    for row in 0..g.rows() {
        for col in 0..g.cols() {
            if let Some(l) = link_at(g, view_offset, col, row) {
                out.push((row, l.text));
            }
        }
    }
    out.dedup();
    out
}

#[test]
fn osc8_the_line_shows_its_text_and_no_escape_byte() {
    let mut t = Terminal::new(40, 4);
    t.feed(LINE);
    let g = t.grid();
    assert_eq!(row_text(g, 0, 0), "Click me");
    for row in 0..g.rows() {
        for col in 0..g.cols() {
            let ch = g.cell(col, row).ch;
            assert!(ch != '\x1b' && ch != '\x07', "an escape byte reached ({col},{row})");
        }
    }
    let l = link_at(g, 0, 3, 0).expect("the text is a link");
    assert_eq!((l.col_start, l.col_end, l.kind, l.text.as_str()), (0, 7, LinkKind::Url, URI));
}

#[test]
fn osc8_copying_the_line_gives_the_text_not_the_uri() {
    let mut t = Terminal::new(40, 4);
    t.feed(LINE);
    let abs = (t.grid().rows() - 1) as u32;
    let text = grid_selection_text(t.grid(), (0, abs), (39, abs), false).expect("a selection");
    assert_eq!(text.trim_end(), "Click me");
}

#[test]
fn osc8_cells_written_after_the_close_are_not_part_of_the_link() {
    let mut t = Terminal::new(40, 4);
    t.feed(b"\x1b]8;;https://example.com\x1b\\ab\x1b]8;;\x1b\\cd\x1b]8;;https://example.com\x07ef\x1b]8;;\x07gh");
    let g = t.grid();
    let linked: Vec<bool> = (0..8).map(|c| link_at(g, 0, c, 0).is_some()).collect();
    assert_eq!(linked, [true, true, false, false, true, true, false, false]);
    // the two pieces of one target share the pool entry
    assert_eq!(g.link_pool_len(), 1);
}

/// `CSI 0 m` is about style, not about links: the link goes on until it
/// is closed.  And no underline is added: the cells carry what the
/// program asked for and nothing else.
#[test]
fn osc8_a_link_outlives_an_sgr_reset_and_adds_no_style() {
    let mut t = Terminal::new(40, 4);
    t.feed(b"\x1b]8;;https://example.com\x1b\\a\x1b[0mb\x1b]8;;\x1b\\");
    let g = t.grid();
    assert!(link_at(g, 0, 1, 0).is_some());
    for col in 0..2 {
        let a = g.cell(col, 0).attrs;
        assert!(!a.underline, "an underline was added at {col}");
        assert_eq!(a.with_link_id(0), Cell::default().attrs);
    }
}

#[test]
fn osc8_a_scheme_off_the_list_shows_its_text_and_is_no_link() {
    for uri in ["javascript:alert(1)", "ssh://host", "x-apple.systempreferences:x", "vscode://file/a"] {
        let mut t = Terminal::new(40, 4);
        t.feed(format!("\x1b]8;;{uri}\x1b\\Click me\x1b]8;;\x1b\\").as_bytes());
        assert_eq!(row_text(t.grid(), 0, 0), "Click me", "{uri}");
        assert!(links_in_view(t.grid(), 0).is_empty(), "{uri} became a link");
        assert_eq!(t.grid().link_pool_len(), 0, "{uri} went into the pool");
    }
}

/// Over the bound the open is dropped whole, and also when the parser's
/// own bound cut it short -- what is left of a cut URI is not the URI.
#[test]
fn osc8_a_uri_over_2_kib_is_ignored_whole_and_the_text_still_shows() {
    for len in [URI_MAX + 1, 3000, 4090, 10_000] {
        let uri = format!("https://e.com/{}", "a".repeat(len - 14));
        let mut t = Terminal::new(40, 4);
        // a link already open is closed by it, not continued
        t.feed(b"\x1b]8;;https://example.com\x1b\\x");
        t.feed(format!("\x1b]8;;{uri}\x1b\\Click me\x1b]8;;\x1b\\").as_bytes());
        assert_eq!(row_text(t.grid(), 0, 0), "xClick me", "len {len}");
        assert_eq!(links_in_view(t.grid(), 0), vec![(0, URI.to_string())], "len {len}");
    }
    let mut t = Terminal::new(40, 4);
    let at_bound = format!("https://e.com/{}", "a".repeat(URI_MAX - 14));
    t.feed(format!("\x1b]8;;{at_bound}\x1b\\Click me\x1b]8;;\x1b\\").as_bytes());
    assert_eq!(links_in_view(t.grid(), 0), vec![(0, at_bound)], "exactly the bound is a link");
}

struct Dir(std::path::PathBuf);
impl Dir {
    fn new(label: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "marspot-osc8-{}-{}-{}",
            label,
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn open(&self, cols: usize) -> Scrollback {
        Scrollback::file(self.0.join("scrollback.bin"), self.0.join("scrollback.idx"), cols, 4).unwrap()
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Put a link on row 0 of `g` the way the terminal does -- cells
/// carrying the pool's id -- then scroll it `lines` into history.
fn link_then_scroll(g: &mut Grid, uri: &str, text: &str, lines: usize) {
    let id = g.link_id_for(uri).unwrap();
    for (col, ch) in text.chars().enumerate() {
        let mut cell = Cell::from(ch);
        cell.attrs.set_link_id(id);
        g.set_cell(col as u16, 0, cell);
    }
    for _ in 0..lines {
        g.scroll_up(1, Cell::default());
    }
}

/// Where in `g`'s history a link to `uri` is, by line, and the columns
/// it covers.
fn history_links(g: &Grid) -> Vec<(usize, u16, u16, String)> {
    let mut out = Vec::new();
    let mut row = Vec::new();
    for line in 0..g.scrollback_len() {
        g.scrollback_links(line, &mut row);
        out.extend(row.drain(..).map(|(a, b, u)| (line, a, b, u)));
    }
    out
}

#[test]
fn osc8_a_link_in_history_resolves_after_the_file_is_reopened() {
    let d = Dir::new("reopen");
    let before;
    {
        let mut g = Grid::with_scrollback_kind(20, 3, d.open(20));
        g.set_cell(0, 1, Cell::from('n'));
        link_then_scroll(&mut g, URI, "Click me", 6);
        before = history_links(&g);
        assert_eq!(before.len(), 1, "{before:?}");
        // the history cells hold no id
        let line = before[0].0;
        assert!(g.scrollback_line(line).unwrap().iter().all(|c| c.attrs.link_id() == 0));
    }
    let g = Grid::with_scrollback_kind(20, 3, d.open(20));
    assert_eq!(history_links(&g), before, "a new process over the same files reads the same link");
    // and through the click path, scrolled back to it
    let off = (g.scrollback_len() - before[0].0) as u16;
    let l = link_at(&g, off, 4, 0).expect("the history row is still a link");
    assert_eq!((l.text.as_str(), l.col_start, l.col_end), (URI, 0, 7));
    assert_eq!(row_text(&g, off, 0), "Click me");
}

/// Bytes in, a session that chose its file-backed history the way an
/// L3 does, then a new `Terminal` over the same files.
#[test]
fn osc8_a_link_fed_as_bytes_comes_back_after_the_session_restarts() {
    let mut sandbox = marspot_term::test_env::sandbox_state_dir();
    std::fs::create_dir_all(sandbox.dir.join("sessions").join("90801")).unwrap();
    sandbox.env.set("MARSPOT_SESSION_ID", "90801");
    let before;
    {
        let mut t = Terminal::new(40, 4);
        t.feed(b"\r\n");
        t.feed(LINE);
        t.feed(b"\r");
        for i in 0..20 {
            t.feed(format!("filler {i}\r\n").as_bytes());
        }
        before = history_links(t.grid());
        assert_eq!(before.iter().map(|l| l.3.as_str()).collect::<Vec<_>>(), [URI]);
    }
    let t = Terminal::new(40, 4);
    assert_eq!(history_links(t.grid()), before);
}

/// The reverse case.  A cell whose id is gone while the pool -- or the
/// history beside a line -- still holds the URI must read as no link,
/// and never as the link of another row.
#[test]
fn osc8_a_cell_without_an_id_never_reads_another_rows_link() {
    // on screen: the pool keeps the URI, the cells no longer point at it
    let mut t = Terminal::new(20, 4);
    t.feed(b"\x1b]8;;https://example.com\x1b\\Click me\x1b]8;;\x1b\\\r\nplain row");
    t.feed(b"\x1b[1;1HClick me");
    assert_eq!(t.grid().link_pool_len(), 1, "the pool still holds the URI");
    assert!(links_in_view(t.grid(), 0).is_empty(), "{:?}", links_in_view(t.grid(), 0));

    // in history: only the line the link was on has it, before and
    // after the files are reopened, and a reflow that renumbers every
    // line leaves none at all rather than one on the wrong line
    let d = Dir::new("reverse");
    {
        let mut g = Grid::with_scrollback_kind(20, 3, d.open(20));
        for i in 0..3 {
            g.set_cell(0, 0, Cell::from(char::from(b'a' + i)));
            g.scroll_up(1, Cell::default());
        }
        link_then_scroll(&mut g, URI, "Click me", 1);
        for _ in 0..3 {
            g.set_cell(0, 0, Cell::from('z'));
            g.scroll_up(1, Cell::default());
        }
        assert_eq!(history_links(&g).iter().map(|l| l.0).collect::<Vec<_>>(), [3]);
    }
    let mut g = Grid::with_scrollback_kind(20, 3, d.open(20));
    assert_eq!(history_links(&g).iter().map(|l| l.0).collect::<Vec<_>>(), [3]);
    g.resize(12, 3);
    assert!(history_links(&g).is_empty(), "{:?}", history_links(&g));
    for off in 0..=g.scrollback_len() as u16 {
        assert!(links_in_view(&g, off).is_empty(), "offset {off}");
    }
}

/// Past 65535 links at once no new one is given an id, and the ones
/// there keep theirs: an id is never reused while a cell may hold it.
#[test]
fn osc8_a_full_pool_makes_new_links_plain_and_leaves_the_old_ones_alone() {
    let mut g = Grid::new(4, 2);
    for i in 0..LINK_POOL_MAX {
        assert!(g.link_id_for(&format!("https://e.com/{i}")).is_some(), "link {i}");
    }
    assert_eq!(g.link_id_for("https://e.com/new"), None);
    assert_eq!(g.link_uri(1), Some("https://e.com/0"));
    assert_eq!(g.link_uri(LINK_POOL_MAX as u16), Some(format!("https://e.com/{}", LINK_POOL_MAX - 1).as_str()));
    // one already there is still found
    assert_eq!(g.link_id_for("https://e.com/7"), Some(8));

    // and through the parser, with every id on a cell of the screen
    let (cols, rows) = (256u16, 256u16);
    let mut t = Terminal::new(cols, rows);
    let mut bytes = Vec::new();
    for i in 0..LINK_POOL_MAX {
        bytes.extend_from_slice(format!("\x1b]8;;https://e.com/{i}\x1b\\x").as_bytes());
    }
    bytes.extend_from_slice(b"\x1b]8;;https://e.com/new\x1b\\y\x1b]8;;\x1b\\");
    t.feed(&bytes);
    let g = t.grid();
    assert_eq!(g.cell(cols - 1, rows - 1).ch, 'y');
    assert!(link_at(g, 0, cols - 1, rows - 1).is_none(), "the link past the bound got an id");
    for i in [0usize, 1, 300, 40_000, LINK_POOL_MAX - 1] {
        let (col, row) = ((i % cols as usize) as u16, (i / cols as usize) as u16);
        let l = link_at(g, 0, col, row).unwrap_or_else(|| panic!("link {i} lost"));
        assert_eq!(l.text, format!("https://e.com/{i}"), "link {i}");
    }
}

/// A pane whose history is kept in RAM keeps links the same way, for as
/// long as it lives.
#[test]
fn osc8_a_pane_with_history_in_ram_keeps_links_as_a_file_backed_one_does() {
    let mut t = Terminal::new(20, 3);
    t.feed(b"\r\n");
    t.feed(LINE);
    t.feed(b"\r");
    for i in 0..10 {
        t.feed(format!("filler {i}\r\n").as_bytes());
    }
    let g = t.grid();
    assert!(g.scrollback_len() > 0);
    let found = history_links(g);
    assert_eq!(found.iter().map(|l| (l.1, l.2, l.3.as_str())).collect::<Vec<_>>(), [(0, 7, URI)]);
    let off = (g.scrollback_len() - found[0].0) as u16;
    assert_eq!(link_at(g, off, 0, 0).map(|l| l.text).as_deref(), Some(URI));
    // the ring trims what it drops, links included
    let mut t = Terminal::new(20, 3);
    for i in 0..30_000 {
        t.feed(format!("\x1b]8;;https://e.com/{i}\x1b\\l\x1b]8;;\x1b\\\r\n").as_bytes());
    }
    assert!(history_links(t.grid()).len() <= t.grid().scrollback_len());
}

/// The screen changes buffer: the link the pen is writing is found in
/// the alternate screen's pool, and in the main one's on the way back.
#[test]
fn osc8_an_open_link_follows_the_pen_across_the_alternate_screen() {
    let mut t = Terminal::new(20, 3);
    t.feed(b"\x1b]8;;https://example.com\x1b\\a\x1b[?1049hb\x1b[?1049lc\x1b]8;;\x1b\\");
    let g = t.grid();
    assert_eq!(row_text(g, 0, 0), "ac");
    assert!(link_at(g, 0, 0, 0).is_some() && link_at(g, 0, 1, 0).is_some());
}

//! The OSC 8 entries: which links survive what, and which targets open.

use marspot::terminal::Terminal;

use super::{closed, says, the_page_says};

struct Dir(std::path::PathBuf);
impl Dir {
    fn new(label: &str) -> Self {
        let p =
            std::env::temp_dir().join(format!("marspot-known-gaps-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn open(&self) -> marspot::scrollback::Scrollback {
        marspot::scrollback::Scrollback::file(
            self.0.join("scrollback.bin"),
            self.0.join("scrollback.idx"),
            20,
            4,
        )
        .unwrap()
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn osc8_links_in_history_on_disk_survive_a_restart_but_not_a_width_change() {
    use marspot::grid::{Cell, Grid};
    says("OSC 8 links in history kept on disk survive scrolling back and a restart.");
    says(
        "A change of window width turns the links already in history into plain text; links on screen keep working.",
    );
    the_page_says("links in history kept on disk survive scrolling back and a restart.");
    the_page_says(
        "A change of window width turns the links already in history into plain text; links on screen keep working.",
    );

    let uri = "https://example.com/";
    let d = Dir::new("link");
    {
        let mut g = Grid::with_scrollback_kind(20, 3, d.open());
        let id = g.link_id_for(uri).unwrap();
        let mut cell = Cell::from('x');
        cell.attrs.set_link_id(id);
        g.set_cell(0, 0, cell);
        for _ in 0..8 {
            g.scroll_up(1, Cell::default());
        }
    }
    let history_link = |g: &Grid| {
        (1..=g.scrollback_len() as u16)
            .flat_map(|off| (0..g.rows()).map(move |r| (off, r)))
            .find_map(|(off, r)| marspot::osc8::link_at(g, off, 0, r))
            .map(|l| l.text)
    };
    // a new grid over the same files is what a restarted pane is
    let mut g = Grid::with_scrollback_kind(20, 3, d.open());
    assert_eq!(
        history_link(&g).as_deref(),
        Some(uri),
        "a restart lost the link"
    );
    // and one on screen, in a column the history probe does not look at
    let on_screen = "https://example.org/";
    let id = g.link_id_for(on_screen).unwrap();
    let mut cell = Cell::from('y');
    cell.attrs.set_link_id(id);
    g.set_cell(5, 0, cell);
    g.resize(16, 3);
    assert_eq!(
        history_link(&g),
        None,
        "{}",
        closed("a width change kept the link in history")
    );
    let screen_link = (0..g.rows())
        .find_map(|r| marspot::osc8::link_at(&g, 0, 5, r))
        .map(|l| l.text);
    assert_eq!(
        screen_link.as_deref(),
        Some(on_screen),
        "a width change lost the link on screen"
    );
}

#[test]
fn osc8_links_on_screen_and_in_memory_history_are_lost_on_an_update() {
    says(
        "Links still on screen are lost when the session is updated or restored from a snapshot, \
         and so are links in history kept in memory.",
    );
    the_page_says(
        "Links still on screen are lost when the session is updated or restored from a snapshot, \
         and so are links in history kept in memory.",
    );
    let link = |t: &Terminal, off: u16| {
        (0..t.grid().rows())
            .flat_map(|r| (0..t.grid().cols()).map(move |c| (c, r)))
            .find_map(|(c, r)| marspot::osc8::link_at(t.grid(), off, c, r))
    };
    let mut t = Terminal::new(40, 4);
    t.feed(b"\x1b]8;;https://example.com/\x1b\\filed\x1b]8;;\x1b\\\r\n");
    for i in 0..6 {
        t.feed(format!("filler {i}\r\n").as_bytes());
    }
    t.feed(b"\x1b]8;;https://example.org/\x1b\\shown\x1b]8;;\x1b\\");
    assert!(link(&t, 0).is_some(), "the probe finds no link on screen");
    let sb = t.grid().scrollback_len() as u16;
    assert!((1..=sb).any(|off| link(&t, off).is_some_and(|l| l.text == "https://example.com/")));

    // an update hands the screen and the history kept in memory to
    // the next image through a snapshot
    let mut next = Terminal::new(40, 4);
    next.apply_snapshot(&t.serialize_snapshot_live())
        .expect("the snapshot applies");
    let shown: String = (0..5)
        .map(|c| next.grid().cell(c, next.grid().cursor().1).ch)
        .collect();
    assert_eq!(shown, "shown", "the probe reads the wrong row");
    assert!(
        link(&next, 0).is_none(),
        "{}",
        closed("a link on screen survived an update")
    );
    let sb = next.grid().scrollback_len() as u16;
    assert!(
        (1..=sb).all(|off| link(&next, off).is_none()),
        "{}",
        closed("a link in history kept in memory survived an update")
    );
}

#[test]
fn osc8_opens_only_four_schemes() {
    says("OSC 8 links open only `http`, `https`, `file` and `mailto` targets");
    says("A link to anything else is shown as plain text, on purpose");
    the_page_says("links open only http, https, file and mailto targets");

    // the text shows either way, and is a link only in the four schemes
    let mut t = Terminal::new(40, 3);
    t.feed(b"\x1b]8;;https://example.com/\x1b\\here\x1b]8;;\x1b\\ ");
    t.feed(b"\x1b]8;;ssh://host\x1b\\there\x1b]8;;\x1b\\");
    let row: String = (0..10).map(|c| t.grid().cell(c, 0).ch).collect();
    assert_eq!(row, "here there");
    let link = marspot::osc8::link_at(t.grid(), 0, 0, 0).map(|l| l.text);
    assert_eq!(link.as_deref(), Some("https://example.com/"));
    assert!(
        marspot::osc8::link_at(t.grid(), 0, 6, 0).is_none(),
        "{}",
        closed("an ssh:// link was made")
    );

    // and the policy the parser asks is the same four
    for ok in [
        "http://a.b/",
        "https://a.b/",
        "file:///tmp/a",
        "mailto:a@b.c",
    ] {
        assert!(
            marspot::osc8::uri_allowed(ok),
            "{ok} is one of the four and was refused"
        );
    }
    for other in [
        "ssh://host",
        "ftp://a.b/",
        "sftp://a.b/",
        "smb://a.b/s",
        "afp://a.b/s",
        "vnc://a.b",
        "tel:+15551234",
        "x-man-page://ls",
        "vscode://file/tmp/a",
        "javascript:alert(1)",
        "data:text/html,x",
    ] {
        assert!(
            !marspot::osc8::uri_allowed(other),
            "{}",
            closed(&format!("{other} is allowed now"))
        );
    }
}

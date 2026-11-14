//! The grapheme clusters on a pane's screen across the session's own
//! update: the image that takes over has to publish them as clusters,
//! not as the pool indices the image before it left in the cells.

use marspot_term::grid::Cell;
use marspot_term::grid_shm::{ClusterTable, ClusterTableReader};
use marspot_term::shell_proto::{MsgType, encode_inject_input};

use crate::self_execv::{Link, Sandbox, stage, start, start_named, wait_for};
use crate::support::{self, COLS, ROWS, Session};

/// How many cluster cells the latest frame has.
fn cluster_cells(s: &Session) -> Option<usize> {
    let (mut cells, mut wrapped): (Vec<Cell>, Vec<bool>) = (Vec::new(), Vec::new());
    s.reader.read(&mut cells, &mut wrapped)?;
    Some(cells.iter().filter(|c| c.is_cluster()).count())
}

#[test]
fn clusters_on_screen_are_still_clusters_after_the_session_updates() {
    let mut sandbox = Sandbox::new();
    let mut s = start(&mut sandbox);
    let pid = s.pid();
    // two flags, printed by the shell so the terminal parses them
    s.send_frame(
        MsgType::InjectInput,
        encode_inject_input(
            0,
            b"\x15clear; printf '\\360\\237\\207\\257\\360\\237\\207\\265 \\360\\237\\207\\253\\360\\237\\207\\267\\n'\r",
        ),
    );
    wait_for(10, "two cluster cells and the prompt after them", || {
        let (_, (col, row)) = s.try_screen_and_cursor(COLS, ROWS)?;
        (cluster_cells(&s)? == 2 && row >= 1 && col > 0).then_some(())
    });

    let staged = stage(sandbox.path(), true).canonicalize().unwrap();
    s.send_frame(MsgType::SessionUpdate, Vec::new());
    wait_for(30, "the pid to run the staged image", || {
        assert_eq!(s.child.try_wait().unwrap(), None, "the L3 exited instead of handing over");
        let running =
            marspot_term::platform::pid_exe_path(pid as i32).and_then(|p| p.canonicalize().ok());
        (running.as_deref() == Some(staged.as_path())).then_some(())
    });
    // a key, so the frame read next is one the new image published
    let mut link = Link::open();
    let (_, (c0, r0)) =
        wait_for(10, "a frame after the swap", || s.try_screen_and_cursor(COLS, ROWS));
    link.send_until(
        &s,
        "the echo of a key typed after the swap",
        |mut stream| {
            let _ = support::key_frame('q').write_to(&mut stream);
        },
        || {
            let (rows, _) = s.try_screen_and_cursor(COLS, ROWS)?;
            (rows[r0 as usize].chars().nth(c0 as usize) == Some('q')).then_some(())
        },
    );
    assert_eq!(cluster_cells(&s), Some(2), "the new image published the flags as something else");
    s.kill();
}

/// The clusters of the frame published at `view_offset`, as `(col,
/// row, text)`, once that is the frame.
fn clusters_at(s: &Session, view_offset: u16) -> Option<Vec<(u16, u16, String)>> {
    let (mut cells, mut wrapped, mut table) = (Vec::new(), Vec::new(), ClusterTable::default());
    let snap = s.reader.read_with_clusters(&mut cells, &mut wrapped, &mut table)?;
    (snap.view_offset == view_offset).then(|| table.iter().map(|(c, r, t)| (c, r, t.to_string())).collect())
}

/// Scrolled back over lines that left the screen, the pane shows the
/// same cluster text after the session's update as before it.  These
/// rows are in the scrollback file.
#[test]
fn clusters_in_history_read_the_same_after_the_session_updates() {
    const BACK: u16 = 12;
    let mut sandbox = Sandbox::new();
    let (mut s, regions) = start_named(&mut sandbox);
    let pid = s.pid();
    s.send_frame(
        MsgType::InjectInput,
        encode_inject_input(
            0,
            b"\x15clear; for i in {1..40}; do printf '%s \\360\\237\\207\\257\\360\\237\\207\\265 e\\314\\201\\n' $i; done\r",
        ),
    );
    wait_for(10, "forty lines and the prompt after them", || {
        let (rows, (col, _)) = s.try_screen_and_cursor(COLS, ROWS)?;
        (col > 0 && rows.iter().any(|l| l.starts_with("40 "))).then_some(())
    });
    let table = wait_for(10, "the session's cluster region", || ClusterTableReader::open(&regions).ok());
    s.reader.attach_clusters(table);
    s.send_frame(MsgType::GridScroll, marspot_term::shell_proto::encode_grid_scroll(BACK));
    let before = wait_for(10, "the frame scrolled back", || clusters_at(&s, BACK));
    assert_eq!(before.len(), 2 * ROWS as usize, "a flag and an accented letter on every row: {before:?}");
    s.send_frame(MsgType::GridScroll, marspot_term::shell_proto::encode_grid_scroll(0));
    wait_for(10, "the live screen again", || clusters_at(&s, 0));

    let staged = stage(sandbox.path(), true).canonicalize().unwrap();
    s.send_frame(MsgType::SessionUpdate, Vec::new());
    wait_for(30, "the pid to run the staged image", || {
        assert_eq!(s.child.try_wait().unwrap(), None, "the L3 exited instead of handing over");
        let running =
            marspot_term::platform::pid_exe_path(pid as i32).and_then(|p| p.canonicalize().ok());
        (running.as_deref() == Some(staged.as_path())).then_some(())
    });
    let mut link = Link::open();
    let after = link.send_until(
        &s,
        "the frame scrolled back, from the new image",
        |mut stream| {
            let f = marspot_term::shell_proto::Frame::new(
                MsgType::GridScroll,
                marspot_term::shell_proto::encode_grid_scroll(BACK),
            );
            let _ = f.write_to(&mut stream);
        },
        || clusters_at(&s, BACK),
    );
    regions.delete();
    assert_eq!(after, before, "history reads differently after the update");
    s.kill();
}

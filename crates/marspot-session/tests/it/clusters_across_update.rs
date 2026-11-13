//! The grapheme clusters on a pane's screen across the session's own
//! update: the image that takes over has to publish them as clusters,
//! not as the pool indices the image before it left in the cells.

use marspot_term::grid::Cell;
use marspot_term::shell_proto::{MsgType, encode_inject_input};

use crate::self_execv::{Link, Sandbox, stage, start, wait_for};
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

//! Which frames a session writes: eight-byte cells only to a reader
//! that said it reads them, and twenty-byte cells again the moment
//! that reader goes, so a core from before eight-byte cells finds a
//! region it attaches to.

use std::os::unix::net::UnixStream;

use marspot_term::grid::{Cell, Color};
use marspot_term::grid_shm::cells::{CELL_SIZE_PACKED, CELL_SIZE_WIDE, VERSION_PACKED};
use marspot_term::session_registry::session_socket_path;
use marspot_term::shell_proto::{Frame, MsgType, encode_frame_cells, encode_inject_input};

use crate::move_region::{ask_to_move, await_arrival, region_to_move_to};
use crate::self_execv::{Sandbox, id, start, wait_for};
use crate::support::{self, COLS, ROWS, Session};

const WIDE: (u32, u32) = (3, CELL_SIZE_WIDE);
const PACKED: (u32, u32) = (VERSION_PACKED, CELL_SIZE_PACKED);

fn say(stream: &UnixStream, msg_type: MsgType, payload: Vec<u8>) {
    let mut w = stream;
    Frame::new(msg_type, payload).write_to(&mut w).expect("the session takes a frame");
}

/// Print `word` in red at the start of a line and wait to see it.
fn print_red(s: &Session, stream: &UnixStream, word: &str) -> (u16, u16) {
    let cmd = format!("\u{15}printf '\\033[31m{word}\\033[0m\\n'\r");
    say(stream, MsgType::InjectInput, encode_inject_input(0, cmd.as_bytes()));
    wait_for(10, &format!("{word} in red on the screen"), || {
        let (mut cells, mut wrapped) = (Vec::<Cell>::new(), Vec::new());
        s.reader.read(&mut cells, &mut wrapped)?;
        let at = (0..ROWS).find(|&r| {
            let row: String = cells[r as usize * COLS as usize..][..word.len()].iter().map(|c| c.ch).collect();
            row == word
        })?;
        let cell = cells[at as usize * COLS as usize];
        (cell.attrs.fg == Color::indexed(1)).then_some((0, at))
    })
}

#[test]
fn eight_byte_cells_go_only_to_a_reader_that_reads_them() {
    let mut sandbox = Sandbox::new();
    let mut s = start(&mut sandbox);
    // a reader that has said nothing reads twenty-byte frames
    let control = s.control.try_clone().unwrap();
    print_red(&s, &control, "before");
    assert_eq!(s.reader.layout(), WIDE);
    // once it says, the next frame is eight-byte cells, and it reads
    // the same screen
    say(&control, MsgType::SessionFrameCells, encode_frame_cells(VERSION_PACKED));
    wait_for(10, "a frame of eight-byte cells", || (s.reader.layout() == PACKED).then_some(()));
    print_red(&s, &control, "packed");
    assert_eq!(s.reader.layout(), PACKED);
    // a second reader comes while the first is still there, as a core
    // replacing another does: the region is twenty-byte cells for it at
    // once, and the first one's going changes nothing after that
    let link = support::reattach(&session_socket_path(id()), 10).expect("a reattach");
    wait_for(2, "twenty-byte cells for a reader that said nothing", || {
        (s.reader.layout() == WIDE).then_some(())
    });
    say(&link, MsgType::SessionFrameCells, encode_frame_cells(VERSION_PACKED));
    wait_for(10, "eight-byte cells for the new reader", || (s.reader.layout() == PACKED).then_some(()));
    // what the first reader says now is not about the reader there is
    say(&control, MsgType::SessionFrameCells, encode_frame_cells(3));
    // one socket under two handles: shutting it once closes both
    control.shutdown(std::net::Shutdown::Both).unwrap();
    print_red(&s, &link, "still");
    assert_eq!(s.reader.layout(), PACKED, "the first reader's going was taken for the second's");
    // the reader goes: the region is twenty-byte cells again at once,
    // with no output to carry it
    link.shutdown(std::net::Shutdown::Both).unwrap();
    wait_for(2, "the region back at twenty-byte cells", || (s.reader.layout() == WIDE).then_some(()));
    // the next reader reads twenty-byte frames until it says otherwise
    let link = support::reattach(&session_socket_path(id()), 10).expect("a reattach");
    print_red(&s, &link, "after");
    assert_eq!(s.reader.layout(), WIDE);
    say(&link, MsgType::SessionFrameCells, encode_frame_cells(VERSION_PACKED));
    wait_for(10, "eight-byte cells again", || (s.reader.layout() == PACKED).then_some(()));
    print_red(&s, &link, "again");
    // a reader that reads nothing newer than twenty-byte cells, said
    // out loud, gets those
    say(&link, MsgType::SessionFrameCells, encode_frame_cells(3));
    wait_for(10, "twenty-byte cells on request", || (s.reader.layout() == WIDE).then_some(()));
    s.kill();
}

/// A session asked to move to another region keeps writing its reader
/// the frames that reader said it reads.
#[test]
fn a_session_that_moves_writes_its_reader_the_same_frames() {
    let mut sandbox = Sandbox::new();
    let mut s = start(&mut sandbox);
    let control = s.control.try_clone().unwrap();
    say(&control, MsgType::SessionFrameCells, encode_frame_cells(VERSION_PACKED));
    wait_for(10, "a frame of eight-byte cells", || (s.reader.layout() == PACKED).then_some(()));
    let (regions, reader) = region_to_move_to();
    let serial = reader.serial().expect("the region carries a serial");
    ask_to_move(&s, &regions);
    await_arrival(&s, serial);
    s.reader = reader;
    print_red(&s, &control, "moved");
    let layout = s.reader.layout();
    regions.delete();
    assert_eq!(layout, PACKED, "the move forgot what the reader reads");
    s.kill();
}

/// A session says it reads `SessionFrameCells` after each time it says
/// where it publishes: to the core it starts with and to one that
/// connects later.  A core tells only a session that said so.
#[test]
fn a_session_says_it_reads_which_frames_its_core_reads() {
    let mut sandbox = Sandbox::new();
    let mut s = start(&mut sandbox);
    s.await_frame(MsgType::SessionPublishes, 10);
    s.await_frame(MsgType::SessionReadsFrameCells, 10);
    let link = support::reattach(&session_socket_path(id()), 10).expect("a reattach");
    link.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
    let mut r = &link;
    let mut said = Vec::new();
    while !said.contains(&MsgType::SessionReadsFrameCells) {
        let f = Frame::read_from(&mut r).expect("a frame").expect("the session still there");
        if matches!(f.msg_type, MsgType::SessionPublishes | MsgType::SessionReadsFrameCells) {
            said.push(f.msg_type);
        }
    }
    assert_eq!(said, [MsgType::SessionPublishes, MsgType::SessionReadsFrameCells]);
    s.kill();
}

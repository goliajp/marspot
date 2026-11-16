//! A grid that ran out of shared attribute numbers numbers its cells by
//! position; a row moved by a scroll then holds other numbers than the
//! ones its writes noted.

use marspot_term::terminal::Terminal;

/// What `feed_diff` found: on a 3x2 grid, colours enough to number by
/// position, then CSI T moves a row whose marks named its old numbers.
#[test]
fn a_row_moved_while_numbering_by_position_keeps_its_marks_true() {
    let text: &[u8] = b"\x5d\xc9\x8d\x0a\x40\xd9\x8d\x6f\x33\x00\x00\x6d\x6d\x6d\x6d\x78\
        \x00\x00\x00\x2c\x99\xba\xba\xba\xba\xba\xba\xba\xba\xba\x40\x61\
        \xd9\x8d\xbf\xff\x1b\x5b\x32\x00\x00\x6d\x1b\x90\x5b\x1b\x5b\x29\
        \x6d\x29\x1b\x1b\x5b\x36\x12\x02\x29\xff\x1b\x5b\x33\x00\x00\x6d\
        \x6d\x6d\x6d\x78\x00\x00\x00\x2c\x99\xba\xba\xba\xba\xba\xba\xba\
        \xba\xba\xba\xba\xba\x58\x00\x01\xc6\xd4\x01\xa1\x1b\x5b\x54\x48\
        \x48\xe5\xae\xf4\x2d";
    let mut fast = Terminal::new(3, 2);
    let mut slow = Terminal::new(3, 2);
    for part in text.chunks(46) {
        fast.feed(part);
        slow.feed_reference(part);
    }
    assert!(fast.grid().row_marks_hold());
    assert_eq!(fast.first_difference(&slow), None);
}

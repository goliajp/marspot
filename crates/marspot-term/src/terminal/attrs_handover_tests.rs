use super::Terminal;

/// A style the dead program left on must not become the new
/// shell's.  Modes are already cleared here; a colour or an
/// underline is the same kind of debt, and worse in one way — a
/// mode has a prompt that usually turns it off, while a style can
/// ride every new cell for hours.
#[test]
fn a_style_left_on_by_a_dead_program_does_not_reach_the_new_shell() {
    let mut t = Terminal::new(20, 2);
    t.feed(b"\x1b[4;31munderlined red");
    assert!(t.grid().cell(0, 0).attrs.underline);

    t.reset_process_owned_modes();
    t.feed(b"\r\n$ ");
    assert!(
        !t.grid().cell(0, 1).attrs.underline,
        "the new shell's prompt wears its own look"
    );
}

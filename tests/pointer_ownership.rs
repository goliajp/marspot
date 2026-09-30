//! Chrome that floats over the panes is asked about the pointer first.
//!
//! `mouse_down` has always done this. `mouse_moved` did not, and the
//! result was a badge menu you could click but that stopped following
//! the mouse: motion forwarding sits at the top of `mouse_moved` and
//! returns as soon as it has reported the pointer to a program that
//! asked for it, a menu floats over the panes, so moving inside one is
//! also moving inside a cell. The motion went to the program, the
//! function returned, and the menu's own hover never ran.
//!
//! There is no way to drive `mouse_moved` from a test -- it wants a
//! Metal device and a real window, and none of the sixty-odd tests in
//! that file can build one. What can be checked is the ordering that
//! broke, which is the whole of the bug.

const CORE: &str = include_str!("../src/bin/marspot-core.rs");

/// The body of a free-standing `fn` in that file, by name.
fn body_of(name: &str) -> &'static str {
    let at = CORE
        .find(&format!("fn {name}("))
        .unwrap_or_else(|| panic!("{name} is gone from marspot-core.rs"));
    let open = at + CORE[at..].find('{').expect("a body");
    let mut depth = 0usize;
    for (i, c) in CORE[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &CORE[open..open + i];
                }
            }
            _ => {}
        }
    }
    panic!("{name} never closes");
}

#[test]
fn the_extractor_finds_a_body_at_all() {
    // Every assertion below is vacuous if this stops working -- a
    // missing needle in an empty haystack looks exactly like a rule
    // being obeyed.
    let b = body_of("mouse_moved");
    assert!(b.len() > 200, "mouse_moved's body came back {} bytes", b.len());
    assert!(b.contains("hit_test_cell_pos"), "and it should mention the cell hit test");
}

#[test]
fn motion_asks_about_floating_chrome_before_the_program() {
    let b = body_of("mouse_moved");
    let chrome = b
        .find("chrome_owns_pointer")
        .expect("mouse_moved no longer asks whether chrome owns the pointer");
    let forward = b
        .find("hit_test_cell_pos")
        .expect("mouse_moved no longer forwards motion");
    assert!(
        chrome < forward,
        "motion is forwarded to the program before chrome is asked about the pointer; \
         an open menu will take clicks and stop following the mouse"
    );
}

#[test]
fn the_forwarding_is_actually_guarded_by_it() {
    let b = body_of("mouse_moved");
    let forward = b.find("hit_test_cell_pos").expect("the forward");
    // The guard has to be part of the same condition, not merely
    // earlier in the function.
    let window = &b[forward.saturating_sub(200)..forward];
    assert!(
        window.contains("!chrome_owns_pointer"),
        "the cell hit-test is not guarded by !chrome_owns_pointer"
    );
}

/// The wheel was always right, and says the rule out loud in its own
/// comment. Kept here so the three read as one rule rather than as a
/// fix and two coincidences.
#[test]
fn the_wheel_asks_the_same_question_first() {
    let b = body_of("scroll");
    let panel = b
        .find("process_panel")
        .expect("scroll no longer checks the process panel");
    let forward = b.find("focused_pane_active_session").unwrap_or(usize::MAX);
    assert!(
        panel < forward,
        "the wheel reaches the program before the modal that is sitting on top of it"
    );
}

/// A plain click in a pane starts a selection, even over a program
/// that asked for mouse reports.
///
/// Every other terminal gives that click to the program and makes you
/// hold shift to select. The panes here are agent TUIs: clicking
/// inside one does nothing, and selecting its output is what the mouse
/// is for. Following the convention meant the only way to copy a line
/// was a chord nobody had been told about, and it was reported as "I
/// can't select text in a pane any more" within a day.
#[test]
fn a_plain_click_selects_and_shift_gives_it_to_the_program() {
    let b = body_of("mouse_down");
    let at = b
        .find("l3_inject_mouse_button")
        .expect("mouse_down no longer forwards a press");
    let guard = &b[at.saturating_sub(600)..at];
    assert!(
        guard.contains("&& modifiers.shift"),
        "the press is forwarded without shift held, which takes the plain click \
         away from selection"
    );
    assert!(
        !guard.contains("&& !modifiers.shift"),
        "the shift guard is inverted: shift would select and a plain click would \
         go to the program"
    );
}

/// The half that was always right, kept so the pair reads as a rule.
#[test]
fn a_click_asks_the_same_question_first() {
    let b = body_of("mouse_down");
    let menu = b.find("context_menu").expect("mouse_down no longer checks the menu");
    let cell = b.find("hit_test_cell").unwrap_or(usize::MAX);
    assert!(menu < cell, "a click reaches a pane before it reaches an open menu");
}

//! Scripts as they parse, and the events their steps are made of.

use super::*;

const CELL: (f64, f64) = (8.0, 16.0);

fn layout() -> Layout {
    Layout::build(1600.0, 1000.0, 200.0, 56.0, 24.0, 2, 1, CELL.0, CELL.1)
        .with_chrome(2.0, 2, None)
}

fn panel() -> PanelChrome {
    PanelChrome::new(Rect { x: 400.0, y_top: 200.0, w: 800.0, h: 600.0 }, 56.0)
}

#[test]
fn a_script_parses_into_its_steps() {
    let steps = parse(
        "button:settings; esc;key:cmd-w;click:close;click:outside;click:pane-under;\
         click:pane;click:header;shift-click:pane-under;drag:header:120,-40;scroll:3;type:x;wait:500;",
    )
    .unwrap();
    assert_eq!(
        steps,
        [
            Step::Button(SurfaceKind::Settings),
            ESC,
            Step::Key(ChordKey::Char('w'), cmd()),
            Step::Click(Target::Close),
            Step::Click(Target::Outside),
            Step::Click(Target::PaneUnder),
            Step::Click(Target::Pane),
            Step::Click(Target::Header),
            Step::ShiftClick(Target::PaneUnder),
            Step::DragHeader(120.0, -40.0),
            Step::Scroll(3.0),
            Step::Type('x'),
            Step::Wait(500),
        ]
    );
    for kind in ["settings", "layout", "process", "accounts"] {
        assert!(parse(&format!("button:{kind}")).is_ok(), "{kind}");
    }
    assert_eq!(parse(""), Ok(Vec::new()));
}

fn cmd() -> Modifiers {
    Modifiers { super_: true, ..Default::default() }
}

#[test]
fn a_chord_is_its_modifiers_then_its_key() {
    let chord = |text: &str| match parse(&format!("key:{text}")).unwrap()[..] {
        [Step::Key(key, m)] => (key, (m.super_, m.shift, m.control, m.alt)),
        ref other => panic!("{text}: {other:?}"),
    };
    let c = ChordKey::Char;
    assert_eq!(chord("cmd-,"), (c(','), (true, false, false, false)));
    assert_eq!(chord("cmd-shift-l"), (c('l'), (true, true, false, false)));
    assert_eq!(chord("cmd-shift-L"), chord("cmd-shift-l"));
    assert_eq!(chord("cmd-2"), (c('2'), (true, false, false, false)));
    assert_eq!(chord("cmd-]"), (c(']'), (true, false, false, false)));
    assert_eq!(chord("cmd--"), (c('-'), (true, false, false, false)));
    assert_eq!(chord("-"), (c('-'), (false, false, false, false)));
    assert_eq!(chord("ctrl-c"), (c('c'), (false, false, true, false)));
    assert_eq!(chord("alt-b"), (c('b'), (false, false, false, true)));
    assert_eq!(chord("ctrl-cmd-f"), (c('f'), (true, false, true, false)));
    assert_eq!(chord("f"), (c('f'), (false, false, false, false)));
    assert_eq!(chord("f5").0, ChordKey::Named(NamedKey::F5));
    assert_eq!(chord("f12").0, ChordKey::Named(NamedKey::F12));
    assert_eq!(chord("up").0, ChordKey::Named(NamedKey::ArrowUp));
    assert_eq!(parse("key:esc").unwrap(), [ESC]);
}

#[test]
fn the_pointer_can_be_moved_onto_a_target_and_out_of_the_window() {
    let l = layout();
    let scene = Scene { layout: &l, chrome: Some(panel()), cell: CELL, helpers: [None; 6], strip: None };
    let steps = parse("move:close;pointer-exit").unwrap();
    assert_eq!(steps, [Step::Move(Target::Close), Step::PointerExit]);
    let at = |step| match scene.resolve(&step).unwrap()[..] {
        [Input::Move(x, y)] => (x, y),
        ref other => panic!("{other:?}"),
    };
    let (x, y) = at(steps[0].clone());
    assert!(panel().close.contains(x, y));
    let (x, y) = at(steps[1].clone());
    assert!(!(0.0..l.window_w).contains(&x) && !(0.0..l.window_h).contains(&y));
    assert!(parse("pointer-exit:now").is_err() && parse("move:nowhere").is_err());
}

#[test]
fn a_step_it_does_not_know_rejects_the_whole_script() {
    for bad in [
        "button:nope", "click", "click:middle", "shift-click:", "drag:120,40", "drag:header:1", "type:",
        "type:xy", "scroll:x", "key:", "key:cmd-", "key:hyper-x", "key:cmd-xy", "key:f13", "key:f0", "esc:1",
        "wait:soon",
    ] {
        let script = format!("esc;{bad};esc");
        assert_eq!(parse(&script), Err(format!("unknown step `{bad}`")), "{bad}");
    }
}

#[test]
fn a_button_step_presses_the_button_the_toolbar_hit_tests() {
    let l = layout();
    let scene = Scene { layout: &l, chrome: None, cell: CELL, helpers: [None; 6], strip: None };
    use crate::frame_build::chrome::{ChromeShape, ChromeSurfaces};
    use crate::ui::components::chrome_target::ChromeTarget;
    use crate::ui::components::title_strip_view::BadgesOnly;
    let mut font = crate::font_cache::FontCache::build().expect("font");
    let mut chrome = ChromeSurfaces::default();
    let strips = BadgesOnly(0, |_| "");
    let shape = ChromeShape { layout: &l, rows: 0, strips: &strips };
    let hits = [
        (SurfaceKind::Settings, 4),
        (SurfaceKind::Layout, 1),
        (SurfaceKind::Processes, 2),
        (SurfaceKind::Accounts, 3),
    ];
    for (kind, button) in hits {
        let inputs = scene.resolve(&Step::Button(kind)).unwrap();
        let [Input::Move(mx, my), Input::Down(x, y, mods), Input::Up(ux, uy)] = inputs[..] else {
            panic!("{kind:?}: {inputs:?}");
        };
        let hit = chrome.at(&mut font, &shape, (x, y));
        assert!(hit == Some(ChromeTarget::Toolbar(button)) && !mods.shift, "{kind:?}: {hit:?}");
        assert_eq!(((mx, my), (ux, uy)), ((x, y), (x, y)), "{kind:?}");
    }
}

#[test]
fn targets_land_where_their_names_say() {
    let l = layout();
    let c = panel();
    let scene = Scene { layout: &l, chrome: Some(c), cell: CELL, helpers: [None; 6], strip: None };
    let at = |target| scene.point(target).unwrap();

    let (x, y) = at(Target::Close);
    assert!(c.hit_close(x, y));
    let (x, y) = at(Target::Header);
    assert!(c.hit_header(x, y));
    let (x, y) = at(Target::Outside);
    assert!(!c.frame.contains(x, y) && scene.on_a_pane(x, y));
    let (x, y) = at(Target::PaneUnder);
    assert!(c.frame.contains(x, y) && scene.on_a_pane(x, y));
    assert!(!c.content.contains(x, y) && !c.header.contains(x, y), "on nothing of the panel's");
    let (x, y) = at(Target::Pane);
    assert_eq!(l.hit_test_cell_pos(x, y, CELL.0, CELL.1).map(|(i, _, _)| i), Some(0));
}

#[test]
fn a_panel_target_needs_a_panel() {
    let l = layout();
    let scene = Scene { layout: &l, chrome: None, cell: CELL, helpers: [None; 6], strip: None };
    for target in [Target::Close, Target::Header, Target::Outside, Target::PaneUnder] {
        assert!(scene.resolve(&Step::Click(target)).is_err(), "{target:?}");
    }
    assert!(scene.resolve(&Step::DragHeader(1.0, 1.0)).is_err());
    assert!(scene.resolve(&Step::Click(Target::Pane)).is_ok());
    let held = |step| match scene.resolve(&step).unwrap()[1] {
        Input::Down(_, _, mods) => mods.shift,
        ref other => panic!("{other:?}"),
    };
    assert!(held(Step::ShiftClick(Target::Pane)) && !held(Step::Click(Target::Pane)));
}

#[test]
fn a_drag_ends_the_asked_distance_from_where_it_began() {
    let l = layout();
    let scene = Scene { layout: &l, chrome: Some(panel()), cell: CELL, helpers: [None; 6], strip: None };
    let inputs = scene.resolve(&Step::DragHeader(120.0, 40.0)).unwrap();
    let (Some(Input::Down(x, y, _)), Some(Input::Up(ux, uy))) = (inputs.get(1), inputs.last())
    else {
        panic!("{inputs:?}");
    };
    assert_eq!((ux - x, uy - y), (120.0, 40.0));
    assert!(matches!(inputs[inputs.len() - 2], Input::Drag(x, y) if (x, y) == (*ux, *uy)));
}

#[test]
fn keys_come_pressed_then_released() {
    let l = layout();
    let scene = Scene { layout: &l, chrome: None, cell: CELL, helpers: [None; 6], strip: None };
    let keys = |step| -> Vec<(KeyState, LogicalKey, Option<String>, bool)> {
        scene
            .resolve(&step)
            .unwrap()
            .into_iter()
            .map(|i| match i {
                Input::Key(e, m) => (e.state, e.logical, e.text.as_deref().map(str::to_string), m.super_),
                other => panic!("{other:?}"),
            })
            .collect()
    };
    let pair = |logical: LogicalKey, text: Option<&str>, cmd| {
        [KeyState::Pressed, KeyState::Released]
            .map(|s| (s, logical.clone(), text.map(str::to_string), cmd))
            .to_vec()
    };
    let step = |text: &str| parse(text).unwrap().remove(0);
    assert_eq!(keys(ESC), pair(LogicalKey::Named(NamedKey::Escape), None, false));
    assert_eq!(keys(step("key:cmd-w")), pair(LogicalKey::Char('w'), None, true));
    assert_eq!(keys(step("key:ctrl-c")), pair(LogicalKey::Char('c'), None, false));
    assert_eq!(keys(step("key:alt-b")), pair(LogicalKey::Char('b'), Some("b"), false));
    assert_eq!(keys(step("key:shift-a")), pair(LogicalKey::Char('a'), Some("A"), false));
    assert_eq!(keys(step("key:f5")), pair(LogicalKey::Named(NamedKey::F5), None, false));
    assert_eq!(keys(Step::Type('x')), pair(LogicalKey::Char('x'), Some("x"), false));
    assert!(matches!(scene.resolve(&Step::Scroll(3.0)).unwrap()[..], [Input::Scroll(dy)] if dy == 48.0));
}

#[test]
fn steps_are_played_one_at_a_time_and_then_it_is_finished() {
    let l = layout();
    let scene = Scene { layout: &l, chrome: None, cell: CELL, helpers: [None; 6], strip: None };
    let t0 = Instant::now();
    let mut s = Script::new(parse("esc;wait:2000;click:close;type:x").unwrap(), t0);
    assert!(!s.due(t0), "not before the window has had time to fill");
    let t = t0 + START_DELAY;
    assert!(s.due(t));

    // No panes yet: nothing is consumed, and it looks again later.
    let mut empty = layout();
    empty.cells.clear();
    assert!(s.play(t, &Scene { layout: &empty, chrome: None, cell: CELL, helpers: [None; 6], strip: None }).is_empty());
    assert!(!s.due(t + STEP_GAP));
    let t = t + START_DELAY;

    assert_eq!(s.play(t, &scene).len(), 2);
    assert!(!s.due(t) && s.due(t + STEP_GAP));
    assert!(s.play(t, &scene).is_empty(), "the wait");
    assert!(!s.due(t + STEP_GAP) && s.due(t + Duration::from_millis(2000)));
    // A step with nowhere to go is skipped, not fatal.
    assert!(s.play(t, &scene).is_empty());
    assert_eq!(s.play(t, &scene).len(), 2);
    assert!(!s.finished());
    assert!(s.play(t, &scene).is_empty());
    assert!(s.finished() && !s.due(t + START_DELAY));
}

#[test]
fn a_line_of_the_strip_is_clicked_where_it_is() {
    assert_eq!(parse("click:strip-3").unwrap(), [Step::Click(Target::Strip(3))]);
    assert!(parse("click:strip-x").is_err());
    let l = layout();
    let mut scene = Scene { layout: &l, chrome: None, cell: CELL, helpers: [None; 6], strip: None };
    assert!(scene.resolve(&Step::Click(Target::Strip(0))).is_err(), "no strip");
    scene.strip = Some((30.0, 500.0, 16.0));
    let at = scene.resolve(&Step::Click(Target::Strip(3))).unwrap();
    assert!(matches!(at[1], Input::Down(x, y, _) if (x, y) == (30.0, 548.0)), "{at:?}");
}

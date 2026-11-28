use super::*;

fn mv(x: i32) -> Out {
    Out::Pointer(Pointer { action: Action::Move, x, y: 0, button: 0, mods: 0 })
}

fn pane() -> WebPane {
    WebPane::new(1, "file:///p.html".into(), 80, 24, Box::new(|| {}))
}

#[test]
fn web_pane_input_queue_joins_scrolls_and_moves_and_stays_bounded() {
    let t = Instant::now();
    let mut q = VecDeque::new();
    push(&mut q, Out::Scroll(100, 200, t));
    push(&mut q, Out::Scroll(-50, 300, Instant::now()));
    assert_eq!(q, [Out::Scroll(50, 500, t)], "one scroll, from the first event's time");
    push(&mut q, mv(1));
    push(&mut q, mv(2));
    assert_eq!(q.len(), 2);
    assert_eq!(q.back(), Some(&mv(2)), "the newest move only");
    push(&mut q, Out::Copy(1));
    push(&mut q, mv(3));
    assert_eq!(q.len(), 4, "a move after something else is kept");
    for n in 0..QUEUE_CAP as u64 * 2 {
        push(&mut q, Out::Copy(n));
    }
    assert_eq!(q.len(), QUEUE_CAP);
}

#[test]
fn web_pane_input_shown_and_size_are_latest_value_slots() {
    let mut p = pane();
    p.resize_px((800.0, 600.0), 200);
    assert_eq!(p.shared.lock().resize, None, "the size it opens at is not a resize");
    for _ in 0..QUEUE_CAP * 2 {
        p.copy();
    }
    for w in [700.0, 600.0, 500.0] {
        p.resize_px((w, 600.0), 200);
    }
    p.set_visible(false);
    p.set_visible(true);
    p.set_visible(false);
    let g = p.shared.lock();
    assert_eq!(g.out.len(), QUEUE_CAP);
    assert_eq!(g.resize, Some(((500.0, 600.0), 200)), "only the last size, whatever the queue holds");
    assert_eq!(g.visible, Some(false));
}

#[test]
fn web_pane_input_copied_text_must_answer_the_copy_asked_for() {
    let mut p = pane();
    assert!(!p.copy_answers(1), "nothing asked yet");
    p.copy();
    p.copy();
    assert!(!p.copy_answers(1), "an answer to an older copy");
    assert!(p.copy_answers(2));
    assert!(!p.copy_answers(2), "used up");
    assert_eq!(p.stray_copies(), 3);
}

#[test]
fn web_pane_input_a_link_opens_once_after_a_command_press() {
    let mut p = pane();
    assert!(!p.take_open(), "no press, no link");
    p.allow_open(true);
    assert!(p.take_open());
    assert!(!p.take_open(), "one link per press");
    p.allow_open(true);
    p.allow_open(false);
    assert!(!p.take_open(), "a later press without Command takes it back");
}

#[test]
fn web_pane_input_paste_is_cut_at_a_character_boundary() {
    let mut p = pane();
    let long = "é".repeat(MAX_TEXT);
    assert!(paste_cut(&long));
    assert!(!paste_cut("short"));
    p.paste(&long);
    let g = p.shared.lock();
    let Some(Out::Paste(kept)) = g.out.back() else { panic!("no paste queued") };
    assert!(kept.len() <= MAX_TEXT && kept.len() > MAX_TEXT - 2 && kept.chars().all(|c| c == 'é'));
}

#[test]
fn web_pane_input_keys_get_their_dom_names() {
    let key = |logical, text: Option<&str>| MarspotKeyEvent { logical, text: text.map(Into::into), ..Default::default() };
    let name = |e: MarspotKeyEvent| dom_key(&e).map(|k| k.as_str().to_owned());
    assert_eq!(dom_key(&key(LogicalKey::Named(NamedKey::PageDown), None)), Some(KeyName::Named("PageDown")));
    assert_eq!(name(key(LogicalKey::Char('a'), Some("A"))).as_deref(), Some("A"));
    assert_eq!(name(key(LogicalKey::Char('c'), None)).as_deref(), Some("c"), "Cmd-C types nothing");
    assert_eq!(name(key(LogicalKey::Char('é'), None)).as_deref(), Some("é"));
    assert_eq!(dom_key(&key(LogicalKey::Other, None)), None);
    assert_eq!(dom_key(&key(LogicalKey::Other, Some(&"x".repeat(MAX_KEY_NAME + 1)))), None);
    let all = Modifiers { shift: true, control: true, alt: true, super_: true };
    assert_eq!(mods_of(all), mods::SHIFT | mods::CONTROL | mods::ALT | mods::META);
    assert_eq!(mods_of(Modifiers::default()), 0);
}

#[test]
fn web_pane_input_short_strings_hold_what_fits() {
    assert_eq!(Short::<4>::new("abcd").map(|s| s.as_str().to_owned()).as_deref(), Some("abcd"));
    assert_eq!(Short::<4>::new("abcde"), None);
    assert_eq!(Short::<4>::new("").map(|s| s.as_str().len()), Some(0));
}

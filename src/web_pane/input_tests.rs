use super::*;

fn mv(x: i32) -> Out {
    Out::Pointer(Pointer { action: Action::Move, x, y: 0, button: 0, mods: 0 })
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
    push(&mut q, Out::Copy);
    push(&mut q, mv(3));
    assert_eq!(q.len(), 4, "a move after something else is kept");
    for _ in 0..QUEUE_CAP * 2 {
        push(&mut q, Out::Copy);
    }
    assert_eq!(q.len(), QUEUE_CAP);
}

#[test]
fn web_pane_input_keys_get_their_dom_names() {
    let key = |logical, text: Option<&str>| MarspotKeyEvent { logical, text: text.map(Into::into), ..Default::default() };
    assert_eq!(dom_key(&key(LogicalKey::Named(NamedKey::PageDown), None)).as_deref(), Some("PageDown"));
    assert_eq!(dom_key(&key(LogicalKey::Char('a'), Some("A"))).as_deref(), Some("A"));
    assert_eq!(dom_key(&key(LogicalKey::Char('c'), None)).as_deref(), Some("c"), "Cmd-C types nothing");
    assert_eq!(dom_key(&key(LogicalKey::Other, None)), None);
    let all = Modifiers { shift: true, control: true, alt: true, super_: true };
    assert_eq!(mods_of(all), mods::SHIFT | mods::CONTROL | mods::ALT | mods::META);
    assert_eq!(mods_of(Modifiers::default()), 0);
}

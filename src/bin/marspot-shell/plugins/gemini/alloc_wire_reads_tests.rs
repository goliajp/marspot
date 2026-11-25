//! When a gemini pane's screen is read, and read again.

use super::*;

#[test]
fn gemini_allocation_the_screen_is_read_once_per_quiet_stretch() {
    static READS: AtomicUsize = AtomicUsize::new(0);
    let mut w = World::new("gemini-wire-reads", true);
    w.plugin.alloc.screen = |_| {
        READS.fetch_add(1, Ordering::SeqCst);
        None
    };
    let written = SystemTime::now() - Duration::from_secs(120);
    let at = |w: Option<SystemTime>| AllocSeen { written: w, ..seen(None, Duration::ZERO) };
    w.pass_with(AgentAllocation::DryRun, AllocSeen { written: Some(SystemTime::now()), ..at(None) }, NOW);
    assert_eq!(READS.load(Ordering::SeqCst), 0, "not quiet yet");
    w.pass_with(AgentAllocation::DryRun, at(Some(written)), NOW);
    assert_eq!(READS.load(Ordering::SeqCst), 1);
    w.pass_with(AgentAllocation::DryRun, at(Some(written)), NOW + 2);
    w.pass_with(AgentAllocation::DryRun, at(Some(written)), NOW + 15);
    assert_eq!(READS.load(Ordering::SeqCst), 1, "no write since: nothing read");
    w.pass_with(AgentAllocation::DryRun, at(Some(written + Duration::from_secs(1))), NOW + 30);
    assert_eq!(READS.load(Ordering::SeqCst), 2, "written again, quiet again: read again");
}

static BUSY_READS: AtomicUsize = AtomicUsize::new(0);

/// gemini at a turn: its spinner and "esc to cancel" over the input box.
fn busy_then_dialog(_: u64) -> Option<String> {
    let n = BUSY_READS.fetch_add(1, Ordering::SeqCst);
    let busy = "⠏ Trying to reach gemini-3-pro-preview (esc to cancel, 25s)\n╭────╮\n│ >   Type your message or @path/to/file │\n╰────╯";
    let dialog = "│ Usage limit reached for gemini-3-pro-preview. │\n│ ● 1. Switch to gemini-3-flash-preview │\n│   2. Stop │";
    Some(if n < 2 { busy } else { dialog }.into())
}

#[test]
fn gemini_allocation_a_busy_screen_is_read_again_at_growing_gaps_then_left() {
    let mut w = World::new("gemini-wire-busy", true);
    w.plugin.alloc.screen = |_| {
        BUSY_READS.fetch_add(1, Ordering::SeqCst);
        Some("⠼ Thinking... (esc to cancel, 40s)\n╭────╮\n│ >   Type your message or @path/to/file │\n╰────╯".into())
    };
    let written = Some(SystemTime::now() - Duration::from_secs(120));
    let mut at = |now: i64| {
        w.pass_with(AgentAllocation::DryRun, AllocSeen { written, ..seen(None, Duration::ZERO) }, now);
        BUSY_READS.load(Ordering::SeqCst)
    };
    let reads: Vec<usize> = [0, 10, 20, 60, 80, 379, 380, 2000, 90_000].into_iter().map(|t| at(NOW + t)).collect();
    assert_eq!(reads, [1, 1, 2, 2, 3, 3, 4, 4, 4], "20 s, 60 s, 5 min, then no more");

    // a dialog that comes up after a quiet spell of retries is seen
    drop(w);
    BUSY_READS.store(0, Ordering::SeqCst);
    let mut w = World::new("gemini-wire-busy-dialog", false);
    w.plugin.alloc.screen = busy_then_dialog;
    for t in [0, 20, 80] {
        w.pass_with(AgentAllocation::DryRun, AllocSeen { written, ..seen(None, Duration::ZERO) }, NOW + t);
    }
    let refused = w.rows("refused");
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(refused[0].contains("how=dialog"), "{}", refused[0]);
    assert_eq!(w.rows("would_move").len(), 1);
}

#[test]
fn gemini_allocation_an_idle_or_held_screen_is_read_once() {
    static READS: AtomicUsize = AtomicUsize::new(0);
    let written = Some(SystemTime::now() - Duration::from_secs(120));
    let screens: [fn(u64) -> Option<String>; 2] = [
        |_| {
            READS.fetch_add(1, Ordering::SeqCst);
            Some("✦ done\n╭────╮\n│ >   Type your message or @path/to/file │\n╰────╯".into())
        },
        |_| {
            READS.fetch_add(1, Ordering::SeqCst);
            Some("╭────╮\n│ > half a question │\n╰────╯".into())
        },
    ];
    for screen in screens {
        READS.store(0, Ordering::SeqCst);
        let mut w = World::new("gemini-wire-idle", true);
        w.plugin.alloc.screen = screen;
        for t in [0, 20, 80, 400] {
            w.pass_with(AgentAllocation::DryRun, AllocSeen { written, ..seen(None, Duration::ZERO) }, NOW + t);
        }
        assert_eq!(READS.load(Ordering::SeqCst), 1);
    }
}

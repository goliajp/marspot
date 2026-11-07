/// What to say to a session that has just been resumed on another
/// account, by hand or by the sweep.
///
/// No reading of the transcript separates "finished" from "cut off"
/// after the fact, so it carries both branches and can be said in
/// either state: carry on if something was interrupted, stay put if
/// nothing was.  Tried by hand across every agent pane on 2026-09-30 —
/// nothing was woken that should have stayed asleep, and nothing that
/// had been cut off was missed.  Naming the idle case is what buys
/// that; without it the earlier wording still nudged an idle session
/// into finding something to continue.
pub(crate) fn resume_word() -> String {
    std::env::var("MARSPOT_CC_RESUME_WORD").unwrap_or_else(|_| {
        "额度恢复了，如果有刚被打断的工作请继续，如果是 idle 状态请保持".to_string()
    })
}

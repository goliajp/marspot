//! The values a sentence is built around, typed: the language decides
//! where each goes.

use std::fmt;

/// A month, day and time of day, local. Written `M/D HH:mm` in every
/// language: all of them put the month first, and the cards it is drawn
/// in are too narrow for a spelled-out date.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MonthDayTime {
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
}

impl fmt::Display for MonthDayTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{} {:02}:{:02}", self.month, self.day, self.hour, self.minute)
    }
}

/// A sentence built around a value.
///
/// The call site hands over the values and nothing else. Word order,
/// plurals and the words themselves are the language's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Arg<'a> {
    /// Context menu: move this pane to the window numbered so.
    MoveToWindow(usize),
    /// Layout panel footer: the grid's shape and how many panes it holds.
    PaneCount { cols: usize, rows: usize },
    /// Process monitor: which pane the right-hand table is showing.
    ProcessDetail { pane: &'a str, sid: u64, shell_pid: i32 },
    /// Account card: when its soonest window turns over.
    ResetAt(MonthDayTime),
    /// Accounts panel header: when the numbers were read.
    UpdatedAt(MonthDayTime),
    /// An empty pane whose session is on its way.
    SessionStarting(u64),
    /// An empty pane whose session did not come up.
    SessionUnavailable(u64),
    /// Badge menu: one account profile, marked when it is the pane's.
    ProfileRow { n: u8, current: bool },
    /// Badge menu: hand the conversation to another agent, named as its
    /// command and profile (`codex P2`), which no language translates.
    HandOffTo(&'a str),
    /// Badge menu: move this pane to another account profile.
    SwitchToProfile(u8),
    /// Sidebar: how many shadow panes there are.
    Helpers(u32),
    /// Badge menu: open the helper panel for a pane with so many.
    HelpersOf(u32),
    /// After a child pane's name in a menu: whose child it is.
    ChildOf(u16),
    /// After the child panes a question names: how many more there are.
    MoreChildren(usize),
    /// A window holds so many panes, the most it can, and takes no more.
    WindowFull(usize),
    /// Helper panel: how long a pane has run.
    RanFor { minutes: u64 },
    /// The shared default model changed without confirmation and was put back.
    ModelPutBack { to: &'a str, back: &'a str },
    /// The shared default model changed and was kept.
    ModelNowIs(&'a str),
    /// Over the focused pane: may this agent CLI be installed, there, taking that much.
    InstallAsk { cli: &'a str, place: &'a str, megabytes: u32 },
    /// A web pane opening its page.
    WebOpening(&'a str),
    /// A menu row: show this page in the pane's place
    WebPageRow(&'a str),
    /// A menu row: open this page in a new pane
    WebOpenRow(&'a str),
    /// A web pane whose other side speaks no version this build does:
    /// which side, its range when it said one, and ours.
    WebVersion { side: &'a str, theirs: Option<(u32, u32)>, ours: (u32, u32) },
    /// A web pane that could not show its page, and why.
    WebFailed(&'a str),
}

//! Every command marspot has, in one table.
//!
//! The key path, the menu bar and the shortcuts panel read [`TABLE`],
//! and tests hold the key tables in the README and on the site to it;
//! nothing else keeps a list of chords.  Adding a command is a row here and the
//! code that runs it; for a command the core runs that is an arm of a
//! `match` with no wildcard, so the compiler asks for it.
//!
//! Only Cmd chords are taken, and Esc while a panel is open.  Ctrl and
//! Option belong to the program in the pane, and a test below holds
//! the table to that.

use std::fmt;

use marspot_term::input_core::{KeyState, LogicalKey, MarspotKeyEvent, Modifiers, NamedKey};

use crate::ui::strings::Msg;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    About,
    Settings,
    Hide,
    HideOthers,
    ShowAll,
    Quit,
    NewWindow,
    NewPane,
    ClosePanel,
    Copy,
    Paste,
    Find,
    ToggleSidebar,
    Layout,
    Processes,
    Accounts,
    FullScreen,
    Minimize,
    PrevPane,
    NextPane,
    /// The pane at this index, counted from zero.
    FocusPane(u8),
    Shortcuts,
}

impl Command {
    /// Does it read from or type into the focused pane?  A pane whose
    /// keyboard a plugin holds keeps these for the plugin.
    pub fn acts_on_focused_pane(self) -> bool {
        matches!(self, Self::Copy | Self::Paste | Self::Find)
    }
}

/// The menu a command is listed under, in menu-bar order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Menu {
    App,
    File,
    Edit,
    View,
    Window,
    Help,
}

impl Menu {
    pub const ALL: [Menu; 6] =
        [Self::App, Self::File, Self::Edit, Self::View, Self::Window, Self::Help];

    pub fn title(self) -> Msg {
        match self {
            Self::App => Msg::MenuBarApp,
            Self::File => Msg::MenuBarFile,
            Self::Edit => Msg::MenuBarEdit,
            Self::View => Msg::MenuBarView,
            Self::Window => Msg::MenuBarWindow,
            Self::Help => Msg::MenuBarHelp,
        }
    }
}

/// When a command answers, judged by what is on top of the panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    Always,
    /// It acts on the panes, which a panel covers.
    NoPanel,
    /// It acts on the open panel.
    Panel,
}

impl When {
    pub fn holds(self, panel_open: bool) -> bool {
        match self {
            Self::Always => true,
            Self::NoPanel => !panel_open,
            Self::Panel => panel_open,
        }
    }
}

/// What a system action is sent to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Responder {
    App,
    Window,
}

/// Which layer runs a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runs {
    /// AppKit, by one of its own actions sent up the responder chain.
    /// Its chord exists only as the menu item's key equivalent.
    System { selector: &'static str, on: Responder },
    /// The process that owns the windows.
    Shell,
    /// The process that owns the panes and the panels.
    Core,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Esc,
}

pub const SHIFT: u8 = 1;
pub const CTRL: u8 = 2;
pub const OPT: u8 = 4;
pub const CMD: u8 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chord {
    pub key: Key,
    /// `SHIFT | CTRL | OPT | CMD`.
    pub mods: u8,
}

/// A row's chord: `c` with `mods` held.
const fn chord(c: char, mods: u8) -> Option<Chord> {
    Some(Chord { key: Key::Char(c), mods })
}

const fn cmd(c: char) -> Option<Chord> {
    chord(c, CMD)
}

impl Chord {
    pub fn modifiers(self) -> Modifiers {
        Modifiers {
            shift: self.mods & SHIFT != 0,
            control: self.mods & CTRL != 0,
            alt: self.mods & OPT != 0,
            super_: self.mods & CMD != 0,
        }
    }

    /// The press a keyboard would have sent for this chord.
    pub fn key_event(self) -> MarspotKeyEvent {
        let logical = match self.key {
            Key::Char(c) => LogicalKey::Char(c),
            Key::Esc => LogicalKey::Named(NamedKey::Escape),
        };
        MarspotKeyEvent { state: KeyState::Pressed, logical, ..Default::default() }
    }
}

/// As macOS writes it: modifiers in the system's order, then the key.
impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (bit, glyph) in [(CTRL, '⌃'), (OPT, '⌥'), (SHIFT, '⇧'), (CMD, '⌘')] {
            if self.mods & bit != 0 {
                write!(f, "{glyph}")?;
            }
        }
        match self.key {
            Key::Char(c) => write!(f, "{}", c.to_ascii_uppercase()),
            Key::Esc => f.write_str("Esc"),
        }
    }
}

pub struct Row {
    pub command: Command,
    pub title: Msg,
    /// The chord the menu shows.
    pub chord: Option<Chord>,
    /// A second chord for the same command, which no menu can show.
    pub also: Option<Chord>,
    pub menu: Menu,
    pub when: When,
    pub runs: Runs,
    /// Starts a new group in its menu.
    pub separated: bool,
}

const fn row(
    command: Command,
    title: Msg,
    chord: Option<Chord>,
    menu: Menu,
    when: When,
    runs: Runs,
) -> Row {
    Row { command, title, chord, also: None, menu, when, runs, separated: false }
}

const fn sep(mut r: Row) -> Row {
    r.separated = true;
    r
}

const fn system(
    command: Command,
    title: Msg,
    chord: Option<Chord>,
    menu: Menu,
    selector: &'static str,
    on: Responder,
) -> Row {
    row(command, title, chord, menu, When::Always, Runs::System { selector, on })
}

const fn pane(n: u8, title: Msg) -> Row {
    let chord = cmd((b'1' + n) as char);
    row(Command::FocusPane(n), title, chord, Menu::Window, When::NoPanel, Runs::Core)
}

use Command as C;
use Menu as M;
use Responder::{App, Window};
use Runs::{Core, Shell};
use When::{Always, NoPanel, Panel};

/// Every command, in the order the menus list them.
pub const TABLE: &[Row] = &[
    system(C::About, Msg::CmdAbout, None, M::App, "orderFrontStandardAboutPanel:", App),
    sep(row(C::Settings, Msg::CmdSettings, cmd(','), M::App, Always, Core)),
    sep(system(C::Hide, Msg::CmdHide, cmd('h'), M::App, "hide:", App)),
    system(
        C::HideOthers,
        Msg::CmdHideOthers,
        chord('h', OPT | CMD),
        M::App,
        "hideOtherApplications:",
        App,
    ),
    system(C::ShowAll, Msg::CmdShowAll, None, M::App, "unhideAllApplications:", App),
    sep(system(C::Quit, Msg::CmdQuit, cmd('q'), M::App, "terminate:", App)),
    row(C::NewWindow, Msg::CmdNewWindow, cmd('n'), M::File, Always, Shell),
    row(C::NewPane, Msg::CmdNewPane, cmd('t'), M::File, NoPanel, Core),
    sep(Row {
        also: Some(Chord { key: Key::Esc, mods: 0 }),
        ..row(C::ClosePanel, Msg::CmdClosePanel, cmd('w'), M::File, Panel, Core)
    }),
    row(C::Copy, Msg::MenuCopy, cmd('c'), M::Edit, NoPanel, Core),
    row(C::Paste, Msg::MenuPaste, cmd('v'), M::Edit, NoPanel, Core),
    sep(row(C::Find, Msg::CmdFind, cmd('f'), M::Edit, Always, Core)),
    row(C::ToggleSidebar, Msg::CmdToggleSidebar, cmd('b'), M::View, NoPanel, Core),
    sep(row(C::Layout, Msg::CmdLayout, chord('l', SHIFT | CMD), M::View, Always, Core)),
    row(C::Processes, Msg::CmdProcesses, chord('p', SHIFT | CMD), M::View, Always, Core),
    row(C::Accounts, Msg::CmdAccounts, chord('c', SHIFT | CMD), M::View, Always, Core),
    sep(system(
        C::FullScreen,
        Msg::CmdFullScreen,
        chord('f', CTRL | CMD),
        M::View,
        "toggleFullScreen:",
        Window,
    )),
    system(C::Minimize, Msg::CmdMinimize, cmd('m'), M::Window, "performMiniaturize:", Window),
    sep(row(C::PrevPane, Msg::CmdPrevPane, cmd('['), M::Window, NoPanel, Core)),
    row(C::NextPane, Msg::CmdNextPane, cmd(']'), M::Window, NoPanel, Core),
    sep(pane(0, Msg::CmdPane1)),
    pane(1, Msg::CmdPane2),
    pane(2, Msg::CmdPane3),
    pane(3, Msg::CmdPane4),
    pane(4, Msg::CmdPane5),
    pane(5, Msg::CmdPane6),
    pane(6, Msg::CmdPane7),
    pane(7, Msg::CmdPane8),
    pane(8, Msg::CmdPane9),
    row(C::Shortcuts, Msg::CmdShortcuts, cmd('/'), M::Help, Always, Core),
];

fn row_for(key: Key, mods: u8) -> Option<&'static Row> {
    let chord = Some(Chord { key, mods });
    TABLE.iter().find(|r| r.chord == chord || r.also == chord)
}

/// The command a key is the chord of.
///
/// A chord names a character, and which key types that character is
/// the keyboard layout's business, so the match is on what the press
/// typed (`text`: what the layout gives the key under Cmd, or under
/// Shift where Shift makes it another character), as a menu's key
/// equivalent is:
///
/// 1. A letter keeps every modifier held: Shift does not make it
///    another key.  Any other character has used up the Shift or
///    Option that typed it, so Cmd-Shift-7 is Cmd-/ on a German layout
///    and Cmd-Option-5 is Cmd-[.
/// 2. With Option or Control held the typed character is not the
///    key's (`˙`, a control code), and the key's own character stands
///    in with the modifiers as held.
/// 3. Cmd-1 to Cmd-9 that nothing above matched go by the physical
///    digit row, so they need no Shift where digits do (AZERTY).
///
/// Runs on every keystroke: a key without Cmd that is not Esc leaves
/// after two comparisons, and nothing here allocates.
pub fn for_key(event: &MarspotKeyEvent, m: Modifiers) -> Option<&'static Row> {
    let bare = match event.logical {
        LogicalKey::Char(c) if m.super_ => lower(c),
        LogicalKey::Named(NamedKey::Escape) => {
            let none = !(m.shift || m.control || m.alt || m.super_);
            return if none { row_for(Key::Esc, 0) } else { None };
        }
        _ => return None,
    };
    let bit = |held: bool, bit: u8| if held { bit } else { 0 };
    let mods = bit(m.shift, SHIFT) | bit(m.control, CTRL) | bit(m.alt, OPT) | bit(m.super_, CMD);
    let typed = event.text.as_deref().and_then(only_char).filter(|c| !c.is_control()).map_or(bare, lower);
    let spent = if typed.is_alphabetic() || typed == bare { 0 } else { SHIFT | OPT };
    row_for(Key::Char(typed), mods & !spent)
        .or_else(|| row_for(Key::Char(bare), mods).filter(|_| m.alt || m.control))
        .or_else(|| {
            let digit = event.base_layout.filter(|c| ('1'..='9').contains(c) && !typed.is_ascii_digit())?;
            row_for(Key::Char(digit), mods)
        })
}

/// Lowercase without allocating; a character with no single-character
/// lowercase is itself.
fn lower(c: char) -> char {
    let mut l = c.to_lowercase();
    match (l.next(), l.next()) {
        (Some(one), None) => one,
        _ => c,
    }
}

fn only_char(text: &str) -> Option<char> {
    let mut chars = text.chars();
    chars.next().filter(|_| chars.next().is_none())
}

/// The row of `command`.
pub fn row_of(command: Command) -> &'static Row {
    TABLE.iter().find(|r| r.command == command).expect("every command has a row")
}

/// `command`'s chord as a menu shows it, empty when it has none.
pub fn hint(command: Command) -> String {
    row_of(command).chord.map(|c| c.to_string()).unwrap_or_default()
}

/// The keys of one line of a keyboard reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keys {
    One(Chord),
    /// Either does it.
    Either(Chord, Chord),
    /// The first to the last of a run of numbered chords.
    Range(Chord, Chord),
}

impl fmt::Display for Keys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::One(a) => write!(f, "{a}"),
            Self::Either(a, b) => write!(f, "{a}, {b}"),
            Self::Range(a, b) => write!(f, "{a} – {b}"),
        }
    }
}

/// One line of a keyboard reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reference {
    pub menu: Menu,
    pub title: Msg,
    pub keys: Keys,
}

/// The table as a reader wants it: commands that have a chord, and the
/// nine numbered panes as one line.
pub fn reference() -> Vec<Reference> {
    let mut out = Vec::new();
    reference_into(&mut out);
    out
}

/// [`reference`] written into `out`, which is emptied first.
pub fn reference_into(out: &mut Vec<Reference>) {
    out.clear();
    for r in TABLE {
        let Some(chord) = r.chord else { continue };
        if let C::FocusPane(n) = r.command {
            // One line for the run: it ends at whichever pane came last.
            match out.last_mut() {
                Some(Reference { keys: Keys::Range(_, last), .. }) if n > 0 => *last = chord,
                _ => out.push(Reference {
                    menu: r.menu,
                    title: Msg::CmdPaneRange,
                    keys: Keys::Range(chord, chord),
                }),
            }
            continue;
        }
        let keys = r.also.map_or(Keys::One(chord), |also| Keys::Either(chord, also));
        out.push(Reference { menu: r.menu, title: r.title, keys });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::strings::t;

    fn mods(bits: u8) -> Modifiers {
        Chord { key: Key::Esc, mods: bits }.modifiers()
    }

    fn key(logical: LogicalKey) -> MarspotKeyEvent {
        MarspotKeyEvent { logical, ..Default::default() }
    }

    fn command(c: char, bits: u8) -> Option<Command> {
        for_key(&key(LogicalKey::Char(c)), mods(bits)).map(|r| r.command)
    }

    #[test]
    fn the_default_chords_are_the_ones_the_design_lists() {
        let want = [
            (',', CMD, C::Settings),
            ('h', CMD, C::Hide),
            ('h', OPT | CMD, C::HideOthers),
            ('q', CMD, C::Quit),
            ('n', CMD, C::NewWindow),
            ('t', CMD, C::NewPane),
            ('w', CMD, C::ClosePanel),
            ('c', CMD, C::Copy),
            ('v', CMD, C::Paste),
            ('f', CMD, C::Find),
            ('b', CMD, C::ToggleSidebar),
            ('l', SHIFT | CMD, C::Layout),
            ('p', SHIFT | CMD, C::Processes),
            ('c', SHIFT | CMD, C::Accounts),
            ('f', CTRL | CMD, C::FullScreen),
            ('[', CMD, C::PrevPane),
            (']', CMD, C::NextPane),
            ('1', CMD, C::FocusPane(0)),
            ('5', CMD, C::FocusPane(4)),
            ('9', CMD, C::FocusPane(8)),
            ('m', CMD, C::Minimize),
            ('/', CMD, C::Shortcuts),
        ];
        for (c, bits, cmd) in want {
            assert_eq!(command(c, bits), Some(cmd), "{c} with {bits:#06b}");
        }
        let esc = for_key(&key(LogicalKey::Named(NamedKey::Escape)), mods(0)).map(|r| r.command);
        assert_eq!(esc, Some(C::ClosePanel));
        // A capital is the same key: caps lock does not unbind a chord.
        assert_eq!(command('C', CMD), Some(C::Copy));
        let with_chord = TABLE.iter().filter(|r| r.chord.is_some()).count();
        assert_eq!(with_chord, want.len() + 6, "the six panes not spelled out above");
    }

    /// The modifiers are matched exactly, so a chord one modifier away
    /// from a command is not that command.
    #[test]
    fn a_chord_is_all_of_its_modifiers_and_no_others() {
        assert_eq!(command('c', SHIFT | CMD), Some(C::Accounts));
        assert_eq!(command('c', CTRL | CMD), None);
        assert_eq!(command('f', SHIFT | CMD), None);
        assert_eq!(command('l', CMD), None);
        assert_eq!(command('0', CMD), None);
        assert_eq!(for_key(&key(LogicalKey::Named(NamedKey::Escape)), mods(SHIFT)).map(|r| r.command), None);
    }

    /// What the program in the pane is sent has to stay the program's:
    /// nothing without Cmd is a command, Esc apart, and Esc is one
    /// only while a panel is open.
    #[test]
    fn nothing_typed_without_cmd_is_a_command() {
        for bits in [0, SHIFT, CTRL, OPT, CTRL | SHIFT, OPT | SHIFT, CTRL | OPT] {
            for c in (' '..='~').chain(['é', '∫', 'ƒ']) {
                assert_eq!(command(c, bits), None, "{c:?} with {bits:#06b}");
            }
            for named in [
                NamedKey::Enter, NamedKey::Tab, NamedKey::Backspace, NamedKey::ArrowUp,
                NamedKey::ArrowLeft, NamedKey::F5, NamedKey::Home, NamedKey::Delete,
            ] {
                assert!(for_key(&key(LogicalKey::Named(named)), mods(bits)).is_none(), "{named:?}");
            }
        }
        assert!(for_key(&key(LogicalKey::Other), mods(CMD)).is_none());
        for r in TABLE {
            for chord in [r.chord, r.also].into_iter().flatten() {
                let esc = chord == Chord { key: Key::Esc, mods: 0 };
                assert!(chord.mods & CMD != 0 || esc, "{:?} is bound without Cmd", r.command);
                assert!(!esc || r.when == Panel, "{:?} takes Esc from the panes", r.command);
            }
        }
    }

    #[test]
    fn no_two_commands_share_a_chord_and_none_is_listed_twice() {
        let mut chords: Vec<Chord> = Vec::new();
        let mut commands: Vec<Command> = Vec::new();
        for r in TABLE {
            assert!(!commands.contains(&r.command), "{:?} has two rows", r.command);
            commands.push(r.command);
            for chord in [r.chord, r.also].into_iter().flatten() {
                assert!(!chords.contains(&chord), "{chord} is bound twice");
                chords.push(chord);
            }
            assert_eq!(row_of(r.command).command, r.command);
        }
    }

    /// Closing a pane ends its session, so no key does it.
    #[test]
    fn cmd_w_closes_a_panel_and_never_a_pane() {
        let r = for_key(&key(LogicalKey::Char('w')), mods(CMD)).unwrap();
        assert_eq!((r.command, r.when), (C::ClosePanel, Panel));
        assert!(!r.when.holds(false) && r.when.holds(true));
    }

    #[test]
    fn availability_follows_what_is_on_top() {
        let when = |c: Command| row_of(c).when;
        for c in [
            C::NewWindow, C::Quit, C::Hide, C::HideOthers, C::Minimize, C::FullScreen, C::Settings,
            C::Layout, C::Processes, C::Accounts, C::Shortcuts, C::Find,
        ] {
            assert_eq!(when(c), Always, "{c:?}");
        }
        for c in [
            C::Copy, C::Paste, C::ToggleSidebar, C::NewPane, C::PrevPane, C::NextPane,
            C::FocusPane(0), C::FocusPane(8),
        ] {
            assert_eq!(when(c), NoPanel, "{c:?}");
        }
        assert_eq!(when(C::ClosePanel), Panel);
        assert!(Always.holds(true) && Always.holds(false));
        assert!(NoPanel.holds(false) && !NoPanel.holds(true));
    }

    /// The menu bar as the table gives it: which items, in what order,
    /// where the separators fall, and which are AppKit's own actions.
    #[test]
    fn the_menus_are_the_table_read_in_order() {
        let menu = |m: Menu| -> Vec<String> {
            TABLE
                .iter()
                .filter(|r| r.menu == m)
                .map(|r| {
                    let line = if r.separated { "| " } else { "" };
                    let chord = r.chord.map(|c| format!(" {c}")).unwrap_or_default();
                    let system = match r.runs {
                        Runs::System { selector, .. } => format!(" -> {selector}"),
                        Shell | Core => String::new(),
                    };
                    format!("{line}{}{chord}{system}", t(r.title))
                })
                .collect()
        };
        assert_eq!(Menu::ALL.map(|m| t(m.title())), ["Marspot", "File", "Edit", "View", "Window", "Help"]);
        assert_eq!(
            menu(M::App),
            [
                "About Marspot -> orderFrontStandardAboutPanel:",
                "| Settings… ⌘,",
                "| Hide Marspot ⌘H -> hide:",
                "Hide Others ⌥⌘H -> hideOtherApplications:",
                "Show All -> unhideAllApplications:",
                "| Quit Marspot ⌘Q -> terminate:",
            ]
        );
        assert_eq!(menu(M::File), ["New Window ⌘N", "New Pane ⌘T", "| Close Panel ⌘W"]);
        assert_eq!(menu(M::Edit), ["Copy ⌘C", "Paste ⌘V", "| Find ⌘F"]);
        assert_eq!(
            menu(M::View),
            [
                "Show or Hide Sidebar ⌘B",
                "| Layout ⇧⌘L",
                "Process Monitor ⇧⌘P",
                "Agent Accounts ⇧⌘C",
                "| Enter or Exit Full Screen ⌃⌘F -> toggleFullScreen:",
            ]
        );
        let window = menu(M::Window);
        assert_eq!(
            window[..4],
            ["Minimize ⌘M -> performMiniaturize:", "| Previous Pane ⌘[", "Next Pane ⌘]", "| Pane 1 ⌘1"]
        );
        assert_eq!((window.len(), window[11].as_str()), (12, "Pane 9 ⌘9"));
        assert_eq!(menu(M::Help), ["Keyboard Shortcuts ⌘/"]);
        // A menu never opens on a separator, and every row is in one.
        for m in Menu::ALL {
            let first = TABLE.iter().find(|r| r.menu == m).expect("no menu is empty");
            assert!(!first.separated, "{m:?}");
        }
        let listed: usize = Menu::ALL.iter().map(|m| menu(*m).len()).sum();
        assert_eq!(listed, TABLE.len());
        // Rows of one menu are together, in menu-bar order.
        let order: Vec<usize> =
            TABLE.iter().map(|r| Menu::ALL.iter().position(|m| *m == r.menu).unwrap()).collect();
        assert!(order.windows(2).all(|w| w[0] <= w[1]));
    }

    /// AppKit's actions are run by AppKit; everything else has a chord
    /// the menu can replay as a key.
    #[test]
    fn what_is_not_a_system_action_can_be_replayed_as_its_chord() {
        for r in TABLE {
            match r.runs {
                Runs::System { selector, .. } => assert!(selector.ends_with(':'), "{selector}"),
                Shell | Core => {
                    let chord = r.chord.unwrap_or_else(|| panic!("{:?} has no chord", r.command));
                    let replayed = for_key(&chord.key_event(), chord.modifiers());
                    assert_eq!(replayed.map(|r| r.command), Some(r.command));
                }
            }
        }
        assert_eq!(row_of(C::NewWindow).runs, Shell, "the shell owns the windows");
    }

    #[test]
    fn the_reference_lists_each_chord_once_and_the_panes_as_a_range() {
        let lines: Vec<String> =
            reference().iter().map(|r| format!("{} {}", r.keys, t(r.title))).collect();
        assert_eq!(lines.len(), 20);
        assert!(lines.contains(&"⌘W, Esc Close Panel".to_string()), "{lines:?}");
        assert!(lines.contains(&"⌘1 – ⌘9 Pane 1 – 9".to_string()), "{lines:?}");
        assert!(lines.contains(&"⌃⌘F Enter or Exit Full Screen".to_string()), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("About") || l.contains("Show All")));
        assert_eq!((hint(C::Copy), hint(C::Accounts), hint(C::About)), ("⌘C".into(), "⇧⌘C".into(), String::new()));
    }
}

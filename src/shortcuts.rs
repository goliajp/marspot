//! Every keyboard shortcut marspot answers, in one place.
//!
//! Not a dispatcher: the handlers stay where they are, in L1's window
//! code and L2's key path.  This is the list of what they are, so the
//! context menu, the site's keyboard reference and anything else that
//! tells the user about a chord are all saying the same thing.
//!
//! Each entry names the file its handler lives in and the literal that
//! handler matches on.  `tests/shortcuts_are_real.rs` looks for that
//! literal in that file, so deleting a handler and leaving the
//! documentation behind fails a test rather than misleading someone.

/// One shortcut: what to press, what it does, and where to find the
/// code that does it.
pub struct Shortcut {
    /// As the user reads it, with the macOS glyphs.
    pub chord: &'static str,
    pub action: &'static str,
    /// Repo-relative path of the file holding the handler.
    pub handler_file: &'static str,
    /// A literal that appears in that handler.  The test greps for it.
    pub handler_match: &'static str,
}

/// The whole list, in the order a keyboard reference should read.
pub const SHORTCUTS: &[Shortcut] = &[
    Shortcut {
        chord: "⌘N",
        action: "New window",
        handler_file: "src/bin/marspot-shell/main.rs",
        handler_match: "eq_ignore_ascii_case(&'n')",
    },
    Shortcut {
        chord: "⌘C",
        action: "Copy the selection",
        handler_file: "src/bin/marspot-core.rs",
        handler_match: "eq_ignore_ascii_case(&'c')",
    },
    Shortcut {
        chord: "⌘V",
        action: "Paste",
        handler_file: "src/bin/marspot-core.rs",
        handler_match: "eq_ignore_ascii_case(&'v')",
    },
    Shortcut {
        chord: "⌘F",
        action: "Search this pane's scrollback",
        handler_file: "src/bin/marspot-core.rs",
        handler_match: "eq_ignore_ascii_case(&'f')",
    },
    Shortcut {
        chord: "⌘B",
        action: "Show or hide the sidebar",
        handler_file: "src/bin/marspot-core.rs",
        handler_match: "eq_ignore_ascii_case(&'b')",
    },
    Shortcut {
        chord: "⇧⌘C",
        action: "Agent usage for this account",
        handler_file: "src/bin/marspot-core.rs",
        handler_match: "toggle_cc_usage_modal",
    },
    Shortcut {
        chord: "⌘W",
        action: "Close the panel that is open",
        handler_file: "src/bin/marspot-core.rs",
        handler_match: "LogicalKey::Char('w')",
    },
    Shortcut {
        chord: "Esc",
        action: "Close the panel that is open; three times in five seconds ends a stuck agent session",
        handler_file: "src/bin/marspot-core.rs",
        handler_match: "NamedKey::Escape",
    },
];

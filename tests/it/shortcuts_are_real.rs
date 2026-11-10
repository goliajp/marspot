//! Every command in the table is run by the layer the table says
//! runs it.
//!
//! The table is what the menus, the shortcuts panel and the key tables
//! on the site are made from, so a row with nothing behind it is a
//! promise in four places.  The layers match on `Command` without a
//! wildcard, which makes the compiler ask for an arm per command; what
//! it cannot see is whether the table sends a command to the layer
//! that has more than an empty arm for it.  This looks for the
//! command's name in the file where that layer does its work.
//!
//! AppKit's own actions have no code here to look for;
//! `app_menu`'s tests ask AppKit whether it answers each selector.

use marspot::commands::{Command, Runs, TABLE};

const CORE: &str = "src/bin/marspot-core/keys.rs";
const SHELL: &str = "src/bin/marspot-shell/main.rs";

/// `Command::Copy`, `Command::FocusPane` — the variant as source
/// spells it, without what it carries.
fn spelled(command: Command) -> String {
    let debug = format!("{command:?}");
    format!("Command::{}", debug.split('(').next().expect("split yields one piece at least"))
}

/// The arms of the core's `run_command` that do something: the text
/// from its `match` to the arm that lists what is not the core's.
fn core_arms(src: &str) -> &str {
    let start = src.find("fn run_command").expect("the core has no run_command");
    let body = &src[start..];
    let end = body.find("// AppKit's and the shell's").expect("the arm for what is not the core's");
    &body[..end]
}

#[test]
fn every_command_is_run_where_the_table_says() {
    let core = std::fs::read_to_string(CORE).expect("read the core's key module");
    let shell = std::fs::read_to_string(SHELL).expect("read the shell");
    let arms = core_arms(&core);

    let mut checked = 0;
    for row in TABLE {
        let name = spelled(row.command);
        match row.runs {
            Runs::Core => assert!(
                arms.contains(&format!("{name} =>")) || arms.contains(&format!("{name}(n) =>")),
                "the table sends {name} to the core, which has no arm that runs it in {CORE}"
            ),
            Runs::Shell => assert!(
                shell.contains(&name),
                "the table sends {name} to the shell, and {SHELL} never names it"
            ),
            Runs::System { .. } => assert!(
                !arms.contains(&name),
                "{name} is AppKit's by the table and the core runs it too"
            ),
        }
        checked += 1;
    }
    assert_eq!(checked, TABLE.len());
    assert!(checked > 20, "a short table would pass every check above");
}

#[test]
fn the_check_would_notice_a_command_nothing_runs() {
    // Every row passes above, which reads the same whether the search
    // finds arms or the text happens to hold every string handed to
    // it.  These are names no arm has.
    let core = std::fs::read_to_string(CORE).expect("read");
    let arms = core_arms(&core);
    assert!(!arms.contains("Command::Quit =>"), "an AppKit action has an arm in the core");
    assert!(!arms.contains("Command::CloseWindow"), "a command chosen for not existing");
    assert!(arms.contains("Command::Copy =>"));
}

/// The keys go through the table and nowhere else: a chord matched by
/// hand beside it is a shortcut the menus and the reference never
/// hear about.
#[test]
fn no_layer_matches_a_chord_by_hand() {
    for path in [CORE, SHELL, "src/bin/marspot-core.rs", "src/main.rs"] {
        let src = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"));
        let prod = src.split("#[cfg(test)]\nmod tests").next().expect("split yields one piece");
        assert!(
            !prod.contains("eq_ignore_ascii_case(&'"),
            "{path} compares a key with a letter; chords belong in the command table"
        );
    }
}

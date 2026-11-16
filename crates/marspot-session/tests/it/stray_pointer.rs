//! Pointer events the core sends on stale modes never reach the tty.
//!
//! On 2026-11-15 the core kept sending mouse reports to a pane after
//! claude had turned reporting off and exited, and zsh ran them as
//! commands.  Each test here sends what a core with a stale copy of the
//! modes would, and reads what the tty was actually given.

use crate::support;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use marspot_term::shell_proto::{
    MsgType, PointerInput, PointerKind, encode_inject_input, encode_pane_hold_grid,
    encode_pane_pointer_input,
};
use marspot_term::test_env::StateSandbox;

const COLS: u16 = 200;
const ROWS: u16 = 30;

struct Pane {
    s: support::Session,
    dir: PathBuf,
    _sandbox: StateSandbox,
}

/// A pane whose shell is zsh without rc files, as a person's would be
/// minus their configuration.
fn zsh_pane() -> Pane {
    let mut sandbox = marspot_term::test_env::sandbox_state_dir();
    let dir = std::fs::canonicalize(sandbox.path()).unwrap();
    let shell = marspot_term::test_env::shell_without_rc_files(&dir, "/bin/zsh");
    sandbox.env.set("SHELL", &shell).set("HOME", &dir);
    let bin = Path::new(env!("CARGO_BIN_EXE_marspot-session"));
    let s = support::Session::spawn(bin, COLS, ROWS);
    s.wait_ready("zsh");
    let p = Pane {
        s,
        dir,
        _sandbox: sandbox,
    };
    p.type_in("PS1='P> '; print SET''UP-DONE\r");
    p.wait_screen("SETUP-DONE");
    p
}

impl Pane {
    fn type_in(&self, text: &str) {
        self.s.send_frame(
            MsgType::InjectInput,
            encode_inject_input(0, text.as_bytes()),
        );
    }

    fn screen(&self) -> String {
        self.s
            .try_screen(COLS, ROWS)
            .map(|r| r.join("\n"))
            .unwrap_or_default()
    }

    fn wait_screen(&self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self.screen().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "never showed {needle:?}:\n{}",
                self.screen()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn pointer(&self, kind: PointerKind, button: u8, count: u16) {
        let ev = PointerInput {
            kind,
            button,
            count,
            col: 3,
            row: 1,
            keys: Vec::new(),
        };
        self.s
            .send_frame(MsgType::PanePointerInput, encode_pane_pointer_input(0, &ev));
    }

    /// What a core on stale modes sends while the pointer crosses the pane.
    fn stray_reports(&self) {
        self.pointer(PointerKind::Motion, 3, 1);
        self.pointer(PointerKind::Motion, 3, 1);
        self.pointer(PointerKind::Press, 0, 1);
        self.pointer(PointerKind::Release, 0, 1);
        self.pointer(PointerKind::Wheel, 64, 3);
    }

    /// A script in the pane's directory, run from the prompt so it gets
    /// a process group of its own, as any program would.
    fn script(&self, name: &str, body: &str) -> PathBuf {
        let p = self.dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    /// The first `n` bytes the program read, once it has read them.
    fn got(&self, n: usize) -> Vec<u8> {
        let path = self.dir.join("got");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let b = std::fs::read(&path).unwrap_or_default();
            if b.len() >= n || Instant::now() >= deadline {
                return b;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// The program reads its tty in raw mode into `got`, after `before`.
fn reader(before: &str, n: usize) -> String {
    format!("stty raw -echo\n{before}\ndd bs=1 count={n} of=got 2>/dev/null\nsleep 5")
}

const ALL_ON: &str = "\\033[?1000h\\033[?1002h\\033[?1003h\\033[?1006h";
const ALL_OFF: &str = "\\033[?1006l\\033[?1003l\\033[?1002l\\033[?1000l";

/// The control: what the program asked for, it gets, in the encoding
/// it asked for.
#[test]
fn a_report_the_program_asked_for_reaches_it() {
    let p = zsh_pane();
    p.script("prog", &reader(&format!("printf '{ALL_ON}PROG-ON'"), 10));
    p.type_in("./prog\r");
    p.wait_screen("PROG-ON");
    p.pointer(PointerKind::Motion, 3, 1);
    assert_eq!(p.got(10), b"\x1b[<35;3;1M");
}

/// A: the program turned reporting off and kept reading; the core's
/// copy still says on.
#[test]
fn a_report_after_the_program_turned_reporting_off_does_not_reach_it() {
    let p = zsh_pane();
    p.script(
        "prog",
        &reader(
            &format!("printf '{ALL_ON}'; sleep 0.3; printf '{ALL_OFF}PROG-OFF'"),
            1,
        ),
    );
    p.type_in("./prog\r");
    p.wait_screen("PROG-OFF");
    p.stray_reports();
    std::thread::sleep(Duration::from_millis(300));
    p.type_in("Z");
    assert_eq!(
        p.got(1),
        b"Z",
        "the program read a report it had turned off"
    );
}

/// B: a full-screen program that asked for no reports left the alt
/// screen; arrows from the wheel would now be keys it never wanted.
#[test]
fn wheel_arrows_after_the_alt_screen_is_left_do_not_reach_the_program() {
    let p = zsh_pane();
    p.script(
        "prog",
        &reader(
            "printf '\\033[?1049hPROG-ALT'; sleep 0.3; printf '\\033[?1049lPROG-MAIN'",
            1,
        ),
    );
    p.type_in("./prog\r");
    p.wait_screen("PROG-MAIN");
    p.pointer(PointerKind::Arrows, 64, 3);
    std::thread::sleep(Duration::from_millis(300));
    p.type_in("Z");
    assert_eq!(p.got(1), b"Z", "the program read arrows from the wheel");
}

/// B, the control: on the alt screen the arrows arrive, in the cursor
/// key mode the program set.
#[test]
fn wheel_arrows_reach_a_full_screen_program_in_its_cursor_key_mode() {
    let p = zsh_pane();
    p.script("prog", &reader("printf '\\033[?1049h\\033[?1hPROG-ALT'", 3));
    p.type_in("./prog\r");
    p.wait_screen("PROG-ALT");
    p.pointer(PointerKind::Arrows, 64, 1);
    assert_eq!(p.got(3), b"\x1bOA");
}

/// Held: the grid's modes stop at the hold, so nothing is written.
#[test]
fn a_report_to_a_held_pane_is_not_written() {
    let p = zsh_pane();
    p.script("prog", &reader(&format!("printf '{ALL_ON}PROG-ON'"), 1));
    p.type_in("./prog\r");
    p.wait_screen("PROG-ON");
    p.s.send_frame(MsgType::PaneHoldGrid, encode_pane_hold_grid(0, true));
    p.stray_reports();
    std::thread::sleep(Duration::from_millis(300));
    p.type_in("Z");
    assert_eq!(p.got(1), b"Z", "a held pane wrote a report");
}

/// A shell at its prompt with `3` aliased to `cd -3` and a stack of
/// directories, as oh-my-zsh leaves it.
fn incident_shell() -> (Pane, String) {
    let p = zsh_pane();
    let d = p.dir.display().to_string();
    for sub in ["a", "b", "c", "home"] {
        std::fs::create_dir_all(p.dir.join(sub)).unwrap();
    }
    p.type_in(&format!(
        "setopt autopushd pushdsilent; alias 1='cd -1' 2='cd -2' 3='cd -3'; \
         cd {d}/a; cd {d}/b; cd {d}/c; cd {d}/home; print MARK-''ONE\r"
    ));
    p.wait_screen("MARK-ONE");
    (p, d)
}

/// C, the incident: modes on and the shell itself at the prompt.  A
/// report that reached its line would move it when the next line ran.
#[test]
fn reports_do_not_reach_a_shell_at_its_prompt_even_with_the_modes_on() {
    let (p, d) = incident_shell();
    // the program turned everything on and the shell took the tty back
    // without anyone turning it off
    p.type_in(&format!("printf '{ALL_ON}'; print RE''ADY\r"));
    p.wait_screen("READY");
    p.stray_reports();
    std::thread::sleep(Duration::from_millis(500));
    p.type_in("\rprint AT:$PWD\r");
    p.wait_screen("AT:/");
    let screen = p.screen();
    // a long path wraps
    let unwrapped = screen.replace('\n', "");
    assert!(
        unwrapped.contains(&format!("AT:{d}/homeP>")),
        "the shell moved:\n{screen}"
    );
    assert!(
        !screen.contains("not found"),
        "residue ran as a command:\n{screen}"
    );
}

/// C for the wheel as arrows: the alt screen on and the shell at the
/// prompt.  Two arrows that reached it would recall `print MARK-ONE`,
/// and the return after them would run it again.
#[test]
fn wheel_arrows_do_not_reach_a_shell_at_its_prompt_even_on_the_alt_screen() {
    let (p, _) = incident_shell();
    p.type_in("printf '\\033[?1049h'; print RE''ADY\r");
    p.wait_screen("READY");
    p.pointer(PointerKind::Arrows, 64, 2);
    std::thread::sleep(Duration::from_millis(500));
    p.type_in("\rprint AT:$PWD\r");
    p.wait_screen("AT:/");
    let screen = p.screen();
    assert_eq!(
        screen.matches("MARK-ONE").count(),
        0,
        "an arrow recalled history:\n{screen}"
    );
}

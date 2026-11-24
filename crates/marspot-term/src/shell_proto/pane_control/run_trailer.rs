//! The line `pane.run` types after a command to learn its exit status:
//! it prints the status as an `OSC 133 ; D` command end.
//!
//! The end carries the option `marspot=run` after the status, which
//! tells it from the ends a shell's own hooks print: a command-end
//! watch whose text is that option takes another only once the prompt
//! has been quiet after it, as when Ctrl-C or a syntax error kept the
//! line from running.  A session from before such watches ignores the option and the text alike.  The
//! line's text is fixed, so a reader of a pane's screen can find the
//! echoed line and leave it out.

/// The option on the end a run's line prints, and the text of a watch
/// for it.
pub const RUN_END_MARK: &str = "marspot=run";

const SH: &str = "printf '\\033]133;D;%s;marspot=run\\007' $?";
const FISH: &str = "printf '\\033]133;D;%s;marspot=run\\007' $status";

/// The line for fish or for a POSIX-style shell.
pub fn run_trailer(fish: bool) -> &'static str {
    if fish { FISH } else { SH }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::Terminal;

    #[test]
    fn the_tagged_end_reads_as_its_status() {
        let mut t = Terminal::new(20, 4);
        t.feed(b"\x1b]133;D;3;marspot=run\x07");
        assert_eq!(t.command_end(), (1, Some(3)));
        t.feed(b"\x1b]133;D;0;marspot=run\x07");
        assert_eq!(t.command_end(), (2, Some(0)));
    }

    #[test]
    fn both_lines_print_the_mark() {
        for line in [run_trailer(false), run_trailer(true)] {
            assert!(line.contains(&format!(";{RUN_END_MARK}\\007")), "{line}");
        }
    }

    #[test]
    fn only_a_marked_end_is_a_runs() {
        let mut t = Terminal::new(20, 4);
        for end in ["133;D", "133;D;1", "133;D;1;aid=7", "133;D;1;marspot=running", "133;D;1;x;xmarspot=run", "133;A;marspot=run"] {
            t.feed(format!("\x1b]{end}\x07").as_bytes());
        }
        assert_eq!((t.command_end().0, t.run_end()), (5, (0, None)));
        t.feed(b"\x1b]133;D;7;aid=3;marspot=run\x1b\\");
        t.feed(b"\x1b]133;D;;marspot=run\x07");
        assert_eq!(t.run_end(), (2, None));
        assert_eq!(t.run_end_status(1), Some(Some(7)));
    }

    #[test]
    fn a_hooks_end_stands_until_a_command_starts() {
        let mut t = Terminal::new(20, 4);
        assert_eq!(t.hook_end(), (0, None));
        t.feed(b"\x1b]133;D;130\x07");
        assert_eq!(t.hook_end(), (1, Some(Some(130))));
        t.feed(b"\x1b]133;D;7;marspot=run\x07\x1b]133;A\x07\x1b]133;B\x07");
        assert_eq!(t.hook_end(), (1, Some(Some(130))), "a run's end and a prompt change nothing");
        t.feed(b"\x1b]133;C\x07");
        assert_eq!(t.hook_end(), (2, None));
        t.feed(b"\x1b]133;C\x07\x1b]133;D;4\x07\x1b]133;D\x07");
        assert_eq!(t.hook_end(), (5, Some(None)), "the latest, which said no status");
    }

    #[test]
    fn a_runs_end_outlasts_any_number_of_others() {
        let mut t = Terminal::new(20, 4);
        t.feed(b"\x1b]133;D;7;marspot=run\x07");
        for _ in 0..20 {
            t.feed(b"\x1b]133;D;1\x07");
        }
        assert_eq!(t.run_end(), (1, Some(7)));
        assert_eq!(t.command_end(), (21, Some(1)));
    }
}

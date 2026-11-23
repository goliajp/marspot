//! The line `pane.run` types after a command to learn its exit status:
//! it prints the status as an `OSC 133 ; D` command end.
//!
//! The end carries the option `marspot=run` after the status.  A
//! terminal reads the status from the field after `D` and ignores any
//! option after it, so every session build reads the same status.  The
//! line's text is fixed, so a reader of a pane's screen can find the
//! echoed line and leave it out.

const SH: &str = "printf '\\033]133;D;%s;marspot=run\\007' $?";
const FISH: &str = "printf '\\033]133;D;%s;marspot=run\\007' $status";

/// The line for fish or for a POSIX-style shell.
pub fn run_trailer(fish: bool) -> &'static str {
    if fish { FISH } else { SH }
}

#[cfg(test)]
mod tests {
    use crate::terminal::Terminal;

    #[test]
    fn the_tagged_end_reads_as_its_status() {
        let mut t = Terminal::new(20, 4);
        t.feed(b"\x1b]133;D;3;marspot=run\x07");
        assert_eq!(t.command_end(), (1, Some(3)));
        t.feed(b"\x1b]133;D;0;marspot=run\x07");
        assert_eq!(t.command_end(), (2, Some(0)));
    }
}

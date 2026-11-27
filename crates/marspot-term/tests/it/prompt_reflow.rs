//! A width change re-cuts every line of history; the line a prompt
//! started on still says so afterwards, in RAM and in the file, so a
//! jump back after a resize still lands on the prompt.

use marspot_term::grid::{Cell, Grid, PromptMark};
use marspot_term::scrollback::Scrollback;

struct Dir(std::path::PathBuf);
impl Dir {
    fn new(label: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "marspot-promptreflow-{}-{}-{}",
            label,
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    /// Four lines in RAM: nearly all of the history is on disk only.
    fn open(&self, cols: usize) -> Scrollback {
        Scrollback::file(self.0.join("scrollback.bin"), self.0.join("scrollback.idx"), cols, 4).unwrap()
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A new line at the bottom of `g`, its text wrapped onto as many rows
/// as it needs, the first of them carrying `mark`.
fn line(g: &mut Grid, text: &str, mark: PromptMark) {
    let cols = g.cols() as usize;
    let chars: Vec<char> = text.chars().collect();
    for (k, chunk) in chars.chunks(cols).enumerate() {
        g.scroll_up(1, Cell::default());
        let r = g.rows() - 1;
        for (c, ch) in chunk.iter().enumerate() {
            g.set_cell(c as u16, r, Cell::from(*ch));
        }
        g.set_row_wrapped(r, k > 0);
        g.set_row_prompt(r, if k == 0 { mark } else { PromptMark::None });
    }
}

/// Three commands, each a prompt longer than the narrow width and
/// twelve lines of output, then a fourth prompt on the live screen.
fn commands(g: &mut Grid) {
    for n in 1..=3 {
        line(g, &format!("$ cmd{n} --with --args"), PromptMark::PromptStart);
        for i in 0..12 {
            line(g, &format!("out{n}.{i}"), PromptMark::None);
        }
    }
    line(g, "$ cmd4", PromptMark::PromptStart);
}

fn top_line(g: &Grid, off: u16) -> String {
    (0..g.cols()).map(|c| g.cell_at_view(off, c, 0).ch).collect::<String>().trim_end().to_string()
}

/// From the live screen back through every prompt, each one's first
/// row on top; then forward to the live screen and the row its prompt
/// is on there.
fn steps_through(g: &Grid, wants: [&str; 3]) {
    let mut off = 0;
    for want in wants {
        off = g.prompt_above(off).unwrap_or_else(|| panic!("no prompt behind offset {off}, wanted {want}"));
        assert!(top_line(g, off).starts_with(want), "{:?} at {off}, wanted {want}", top_line(g, off));
    }
    assert_eq!(g.prompt_above(off), None);
    let mut last = None;
    while let Some((o, row)) = g.prompt_below(off) {
        if o == 0 {
            last = Some(row);
            break;
        }
        assert_eq!(row, 0, "a prompt in the history is put on the top row");
        off = o;
    }
    let row = last.expect("the prompt on the live screen is reached by going live");
    assert_eq!((0..g.cols()).map(|c| g.cell_at_view(0, c, row).ch).collect::<String>().trim_end(), "$ cmd4");
}

fn resizes_keep_the_marks(mut g: Grid) {
    commands(&mut g);
    steps_through(&g, ["$ cmd3", "$ cmd2", "$ cmd1"]);
    // narrower: each prompt now takes two rows, the mark on the first
    g.resize(12, 4);
    steps_through(&g, ["$ cmd3 --wit", "$ cmd2 --wit", "$ cmd1 --wit"]);
    let sb = g.scrollback_len();
    let marked = (0..sb).filter(|&i| g.scrollback_prompt(i) == PromptMark::PromptStart).count();
    assert_eq!(marked, 3, "a continuation row took a mark of its own");
    // and back wider: one row each again
    g.resize(30, 4);
    steps_through(&g, ["$ cmd3 --with --args", "$ cmd2 --with --args", "$ cmd1 --with --args"]);
}

#[test]
fn a_jump_back_still_lands_on_the_prompt_after_a_resize_in_ram() {
    resizes_keep_the_marks(Grid::with_scrollback_kind(24, 4, Scrollback::memory(1000, 24)));
}

#[test]
fn a_jump_back_still_lands_on_the_prompt_after_a_resize_on_disk() {
    let d = Dir::new("disk");
    resizes_keep_the_marks(Grid::with_scrollback_kind(24, 4, d.open(24)));
}

/// The marks the resize filed are in the file itself: a grid that
/// opens it again, as an updated session does, finds them.
#[test]
fn the_marks_a_resize_filed_outlive_the_process() {
    let d = Dir::new("reopen");
    {
        let mut g = Grid::with_scrollback_kind(24, 4, d.open(24));
        commands(&mut g);
        g.resize(12, 4);
    }
    let g = Grid::with_scrollback_kind(12, 4, d.open(12));
    let marked: Vec<String> = (0..g.scrollback_len())
        .filter(|&i| g.scrollback_prompt(i) == PromptMark::PromptStart)
        .map(|i| g.scrollback_line(i).unwrap().iter().map(|c| c.ch).collect::<String>().trim_end().to_string())
        .collect();
    assert_eq!(marked, ["$ cmd1 --wit", "$ cmd2 --wit", "$ cmd3 --wit"]);
}

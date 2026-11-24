//! What a pane's window title says about the Codex in it, and what is
//! kept of it over a restart of marspot.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use marspot::pidtree::{self, ProcId};
use marspot_term::lx_warn;

use super::bind::{self, Pane};
use super::records::private_dir;
use super::worker::Worker;

/// Each pane's title since its process was first seen.
pub(super) type Titles = HashMap<u64, TitleWatch<ProcId>>;

impl Worker {
    /// Follow the wanted panes' titles.  A process the observer has not
    /// found again yet keeps its entry for as long as it runs.
    pub(super) fn follow_titles(&mut self) {
        let mut titles = self.titles.clone();
        titles.retain(|_, w| pidtree::proc_id(w.process.pid) == Some(w.process));
        for w in &self.wants {
            titles.insert(w.sid, TitleWatch::seen(self.titles.remove(&w.sid), w.codex, &w.title_thread));
        }
        if titles != self.titles_saved {
            if let Err(e) = path().and_then(|p| save(&p, &titles)) {
                lx_warn!("peerbus.titles.save_failed", &e.to_string());
            }
            self.titles_saved = titles.clone();
        }
        self.titles = titles;
    }

    /// What the marspot before this one had seen: a TUI that showed
    /// another thread then does not write its title again for a new
    /// marspot to see it move.
    pub(super) fn load_titles(&mut self) {
        self.titles = path().map(|p| load(&p)).unwrap_or_default();
        self.titles_saved = self.titles.clone();
    }

    /// When a pane among `waiting` is next worth another look.
    pub(super) fn wait_for_titles<'a>(&mut self, waiting: impl Iterator<Item = &'a Pane>) {
        let left = waiting.filter_map(bind::title_wait_ms).min();
        self.titles_at = left.map(|ms| std::time::Instant::now() + std::time::Duration::from_millis(ms));
    }

    /// Asked on a names pass: a wait that has run out is looked at again.
    pub(super) fn titles_due(&mut self) {
        if self.titles_at.is_some_and(|t| t <= std::time::Instant::now()) {
            self.refresh();
        }
    }
}

fn path() -> std::io::Result<PathBuf> {
    private_dir().map(|d| d.join("titles.tsv"))
}

fn save(path: &Path, titles: &Titles) -> std::io::Result<()> {
    let mut out = String::new();
    for (sid, w) in titles {
        let first = w.first.as_deref().unwrap_or("-");
        out.push_str(&format!("{sid}\t{}\t{}\t{}\t{first}\n", w.process.pid, w.process.start_us, w.moved as u8));
    }
    let tmp = path.with_extension("tsv.tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, path)
}

fn load(path: &Path) -> Titles {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    text.lines().filter_map(entry).collect()
}

fn entry(line: &str) -> Option<(u64, TitleWatch<ProcId>)> {
    let f: Vec<&str> = line.split('\t').collect();
    let [sid, pid, start_us, moved, first] = f.as_slice() else { return None };
    let process = ProcId { pid: pid.parse().ok()?, start_us: start_us.parse().ok()? };
    let first = (*first != "-").then(|| first.to_string());
    Some((sid.parse().ok()?, TitleWatch { process, first, moved: *moved == "1" }))
}

/// Whether this Codex shows its thread id in the window title: what
/// its command line sets (`-c tui.terminal_title=…`), else its home's
/// `config.toml`.
pub(super) fn title_expected(home: &Path, argv: &[String]) -> bool {
    let set = argv.iter().find_map(|a| a.strip_prefix("--config=").unwrap_or(a.as_str()).strip_prefix("tui.terminal_title="));
    match set {
        Some(list) => title_shows_thread(&format!("tui.terminal_title={list}")),
        None => std::fs::read_to_string(home.join("config.toml")).is_ok_and(|t| title_shows_thread(&t)),
    }
}

/// Whether a `config.toml` has the TUI show the thread id in its
/// window title (`[tui] terminal_title` listing `thread-id`).
pub(super) fn title_shows_thread(config: &str) -> bool {
    let mut table = String::new();
    let mut value: Option<String> = None;
    for line in config.lines() {
        let body = line.trim();
        if let Some(v) = &mut value {
            v.push_str(body);
        } else if body.starts_with('[') {
            table = body.trim_matches(|c| c == '[' || c == ']' || c == ' ').to_string();
        } else if let Some((key, rest)) = body.split_once('=') {
            let key = key.trim();
            if (table == "tui" && key == "terminal_title") || (table.is_empty() && key == "tui.terminal_title") {
                value = Some(rest.to_string());
            }
        }
        if let Some(v) = &value
            && let Some(end) = v.find(']')
        {
            return v[..end].split(',').any(|item| item.trim_matches(|c| " [\"'".contains(c)) == "thread-id");
        }
    }
    false
}

/// One pane's title, followed across the processes that run in it:
/// whether it has named anything else since the present process was
/// first seen.  What was there at first sight may be an earlier
/// process's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TitleWatch<P> {
    process: P,
    first: Option<String>,
    pub moved: bool,
}

impl<P: PartialEq> TitleWatch<P> {
    pub(super) fn seen(before: Option<Self>, process: P, title: &Option<String>) -> Self {
        match before {
            Some(w) if w.process == process => Self { moved: w.moved || w.first != *title, ..w },
            _ => Self { process, first: title.clone(), moved: false },
        }
    }
}

/// The thread id a Codex window title shows, as much of it as the
/// title holds: Codex cuts each title item to 32 characters, which
/// leaves the first 29 of the 36 (seen with 0.163.0:
/// `probe | 01a161ee-50e9-7b50-8ad6-4bcae...`).  That still carries
/// tens of random bits, far more than the threads one daemon holds.
/// Two different ids in one title say nothing certain.
pub(super) fn thread_in_title(title: &str) -> Option<String> {
    const GROUPS: [usize; 4] = [8, 4, 4, 4];
    let b = title.as_bytes();
    let mut found: Option<String> = None;
    let mut i = 0;
    while i < b.len() {
        let starts_word = i == 0 || !b[i - 1].is_ascii_alphanumeric();
        let mut at = i;
        let mut ok = starts_word;
        for g in GROUPS {
            if !ok {
                break;
            }
            ok = b.len() > at + g
                && b[at..at + g].iter().all(u8::is_ascii_hexdigit)
                && b[at + g] == b'-';
            at += g + 1;
        }
        if !ok {
            i += 1;
            continue;
        }
        let tail = b[at..].iter().take(12).take_while(|c| c.is_ascii_hexdigit()).count();
        let id = title[i..at + tail].to_ascii_lowercase();
        match &found {
            Some(f) if *f != id => return None,
            _ => found = Some(id),
        }
        i = at + tail;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const T1: &str = "01a161ee-50e9-7b50-8ad6-4bcae0a1b2c3";
    const T2: &str = "01a161ee-50e9-7b50-8ad6-9f00d0a1b2c3";

    /// The title names the thread a `/resume` went to before marspot
    /// was replaced, and names the same one after.
    #[test]
    fn a_title_seen_to_move_is_still_so_after_a_restart() {
        let me = ProcId { pid: 41_685, start_us: 1_795_008_854_123_456 };
        let other = ProcId { pid: 7, start_us: 9 };
        let t = |s: &str| Some(s.to_string());
        let before = TitleWatch::seen(Some(TitleWatch::seen(None, me, &t(T1))), me, &t(T2));
        assert!(before.moved);
        let titles = HashMap::from([(437, before), (5, TitleWatch::seen(None, other, &None))]);
        let dir = std::env::temp_dir().join(format!("marspot-titles-{}-{}", std::process::id(), marspot_peer::rand::hex(6)));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("titles.tsv");
        assert!(load(&file).is_empty(), "none yet");
        save(&file, &titles).unwrap();
        let mut back = load(&file);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(back, titles);
        let w = TitleWatch::seen(back.remove(&437), me, &t(T2));
        assert!(w.moved, "the process that wrote it is the same one");
        assert!(!TitleWatch::seen(Some(w), other, &t(T2)).moved, "the next process in the pane wrote nothing");
        // moved while there was no marspot
        assert!(TitleWatch::seen(back.remove(&5), other, &t(T1)).moved);
    }

    #[test]
    fn a_line_that_is_not_an_entry_is_none() {
        assert!(entry("").is_none());
        assert!(entry("1\t2\t3\t1").is_none());
        assert!(entry("1\tx\t3\t1\t-").is_none());
    }

    #[test]
    fn a_title_has_moved_once_it_differs_from_what_the_process_was_first_seen_under() {
        let t = |s: &str| Some(s.to_string());
        let w = TitleWatch::seen(None, 1, &t("a"));
        assert!(!w.moved);
        let w = TitleWatch::seen(Some(w), 1, &t("a"));
        assert!(!w.moved, "the same title again");
        let w = TitleWatch::seen(Some(w), 1, &None);
        assert!(w.moved);
        let w = TitleWatch::seen(Some(w), 1, &t("a"));
        assert!(w.moved, "back to the first one is still this process's doing");
        let w = TitleWatch::seen(Some(w), 2, &t("a"));
        assert!(!w.moved, "another process starts under the title it found");
    }

    #[test]
    fn the_title_setting_is_read_from_a_config() {
        assert!(title_shows_thread("[tui]\nterminal_title = [\"activity\", \"thread-name\", \"project-name\", \"thread-id\"]\n"));
        assert!(title_shows_thread("model = \"x\"\ntui.terminal_title = ['thread-id']\n[mcp_servers.marspot]\ncommand = \"p\"\n"));
        assert!(title_shows_thread("[tui]\nterminal_title = [\n  \"project-name\",\n  \"thread-id\",\n]\n"));
        assert!(!title_shows_thread("[tui]\nterminal_title = [\"project-name\"]\n"));
        assert!(!title_shows_thread("[other]\nterminal_title = [\"thread-id\"]\n"));
        assert!(!title_shows_thread("[tui]\nnotifications = [\"thread-id\"]\n"));
        assert!(!title_shows_thread(""));
    }

    #[test]
    fn the_thread_is_read_from_a_codex_title() {
        assert_eq!(thread_in_title("⠋ probe | 01a161ee-50e9-7b50-8ad6-4bcae..."), Some("01a161ee-50e9-7b50-8ad6-4bcae".into()));
        assert_eq!(thread_in_title(&format!("spg | {}", T1.to_uppercase())), Some(T1.into()));
        assert_eq!(thread_in_title("probe"), None);
        assert_eq!(thread_in_title("x01a161ee-50e9-7b50-8ad6-4bca"), None, "only at the start of a word");
        assert_eq!(thread_in_title(&format!("{T1} {T1}")), Some(T1.into()));
        assert_eq!(thread_in_title(&format!("{T1} {T2}")), None, "two ids say nothing certain");
    }
}

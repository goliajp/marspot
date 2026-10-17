//! Fixtures shared by the scan-into tests: a column-accurate cell
//! surface, an oracle that answers from a fixed list, and a corpus of
//! screens covering every kind the scanner emits.

#![allow(dead_code)]

use marspot_linkify::{CellSource, LinkRange, PathOracle, PathVerdict, ScanOpts};

pub fn is_wide(ch: char) -> bool {
    matches!(ch as u32, 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 | 0xFF00..=0xFF60 | 0x1F300..=0x1FAFF)
}

/// Rows laid out by column, as a terminal grid holds them: a wide char
/// takes two cells and the second one is `'\0'`.
pub struct Screen {
    cells: Vec<Vec<char>>,
    cols: u16,
    soft: Vec<bool>,
    cursor: (u16, u16),
}

impl Screen {
    pub fn new(rows: &[&str], cols: u16) -> Self {
        let cells = rows
            .iter()
            .map(|r| {
                let mut v = Vec::with_capacity(cols as usize + 1);
                for ch in r.chars() {
                    if v.len() >= cols as usize {
                        break;
                    }
                    v.push(ch);
                    if is_wide(ch) {
                        v.push('\0');
                    }
                }
                v.truncate(cols as usize);
                v.resize(cols as usize, ' ');
                v
            })
            .collect::<Vec<_>>();
        let n = cells.len();
        Self { cells, cols, soft: vec![false; n], cursor: (0, 0) }
    }

    pub fn soft(mut self, rows: &[usize]) -> Self {
        for &r in rows {
            self.soft[r] = true;
        }
        self
    }

    pub fn cursor(mut self, col: u16, row: u16) -> Self {
        self.cursor = (col, row);
        self
    }
}

impl CellSource for Screen {
    fn cols(&self) -> u16 {
        self.cols
    }
    fn rows(&self) -> u16 {
        self.cells.len() as u16
    }
    fn char_at(&self, col: u16, row: u16) -> char {
        self.cells[row as usize][col as usize]
    }
    fn is_soft_wrap_continuation(&self, row: u16) -> bool {
        self.soft[row as usize]
    }
    fn cursor(&self) -> (u16, u16) {
        self.cursor
    }
    fn is_wide(&self, ch: char) -> bool {
        is_wide(ch)
    }
}

/// Exists exactly for the listed paths.  Looks up without allocating.
pub struct Listed(pub &'static [&'static str]);

impl PathOracle for Listed {
    fn probe(&self, path: &str) -> PathVerdict {
        if self.0.contains(&path) {
            PathVerdict::Exists
        } else {
            PathVerdict::Missing
        }
    }
}

pub const EXISTS: &[&str] = &[
    "/etc/hosts",
    "/usr/bin",
    "/usr/local/bin/marspot",
    "~/Downloads/決算明細_2025-2026.xlsx",
    "~/Library/Safari/Favicon Cache",
    "/Users/me/workspace/goliajp/marspot/src/render_metal.rs",
    "/Users/me/workspace/goliajp/marspot/crates/marspot-linkify/src/lib.rs",
    "/Users/me/docs/株主総会議事録（役員報酬改定・20260805）.pdf",
    "/w/p/src/main.rs",
    "/w/p/Cargo.toml",
    "/w/p/README.md",
    "/w/p/notes/20260907-maintenance-experiment-priorities.md",
    "/w/p/文档/说明.md",
    "/w/p/../shared/config.toml",
    "/tmp/site/index-ja.html",
];

pub struct Case {
    pub name: &'static str,
    pub screen: Screen,
    pub opts: ScanOpts<'static>,
}

fn opts(cwd: Option<&'static str>, tui_mode: bool) -> ScanOpts<'static> {
    ScanOpts { cwd, tui_mode }
}

pub fn corpus() -> Vec<Case> {
    let rule = "─".repeat(40);
    vec![
        Case { name: "empty", screen: Screen::new(&[], 40), opts: opts(None, false) },
        Case {
            name: "blank rows",
            screen: Screen::new(&["", "", ""], 40),
            opts: opts(Some("/w/p"), true),
        },
        Case {
            name: "mixed prose",
            screen: Screen::new(
                &[
                    "see https://example.com/a/b?q=1#frag, then mail ops@golia.jp.",
                    "server at 192.168.1.10:8080/status and [::1]:3000/x or 2001:db8::1",
                    "id 3acb2d64-504d-49d5-9030-a179087f4bce wrote /etc/hosts (and /usr/bin).",
                    "version pkg@1.0.23 and v1.2.3.4 and 12:34:56 are not links",
                    "wiki https://en.wikipedia.org/wiki/Rust_(programming_language) ok",
                    "cjk https://example.com/calendar(刷新一下)。 done",
                    "dev http://localhost:6014/tools/documents but not https://localhost",
                ],
                90,
            ),
            opts: opts(None, false),
        },
        Case {
            name: "paths with cwd",
            screen: Screen::new(
                &[
                    "error at src/main.rs:120:5 here, see Cargo.toml and ./README.md",
                    "open ../shared/config.toml or 文档/说明.md, not Music or 1.5",
                    "abs /usr/local/bin/marspot and ~/Downloads/決算明細_2025-2026.xlsx。",
                    "quoted ~/Library/Safari/\"Favicon Cache\" and escaped ~/Library/Safari/Favicon\\ Cache",
                    "doc /Users/me/docs/株主総会議事録（役員報酬改定・20260805）.pdf，已发",
                    "file:///tmp/site/index-ja.html#principles and file:///nope/x",
                    "wrote:src/main.rs and 写好了:README.md",
                ],
                100,
            ),
            opts: opts(Some("/w/p"), false),
        },
        Case {
            name: "soft-wrapped url",
            screen: Screen::new(
                &[
                    "see https://example.c",
                    "om/long/path now ok  ",
                    "and https://golia.jp/",
                    "a/b/c/d/e/f/g/h/i/j/k",
                    "/end then ops@golia.j",
                    "p                    ",
                ],
                21,
            )
            .soft(&[1, 3, 4, 5]),
            opts: opts(None, false),
        },
        Case {
            name: "tui hard wrap",
            screen: Screen::new(
                &[
                    "  - one item with attachments (notes/20260907-maintenance-experiment-",
                    "    priorities.md)",
                    "**************fvMA. You can find your API key at https://",
                    "platform.openai.com/account/api-keys., url: https://chatgpt.com/backend-",
                    "api/codex/responses, cf-ray: a40db52ee9100dce-NRT, request id: 3acb2d64-",
                    "504d-49d5-9030-a179087f4bce",
                    "● Write(/Users/me/workspace/goliajp/marspot/crates/marspot-linkify/s",
                    "  rc/lib.rs)",
                    "",
                ],
                73,
            )
            .cursor(0, 8),
            opts: opts(Some("/w/p"), true),
        },
        Case {
            name: "table cells",
            screen: Screen::new(
                &[
                    "│ file      │ Source link                     │",
                    "│ detect.   │ https://raw.githubusercontent.c │",
                    "│ caffemo   │ om/WeChatCV/opencv_3rdparty/a8b │",
                    "│ del       │ 69ccc/detect.caffemodel         │",
                    "├───────────┼─────────────────────────────────┤",
                    "│ detect.p  │ same     /etc/hosts             │",
                    "│ https://a.example/one │ ops@golia.jp      │",
                    "│ https://b.example/two │ more words        │",
                ],
                47,
            ),
            opts: opts(None, true),
        },
        Case {
            name: "composer box",
            screen: Screen::new(
                &[
                    "see https://example.c",
                    "om/long/path now ok  ",
                    "                     ",
                    "╭───────────────────╮",
                    "│ > https://foo.com │",
                    "╰───────────────────╯",
                ],
                21,
            )
            .soft(&[1])
            .cursor(4, 4),
            opts: opts(None, true),
        },
        Case {
            name: "chromeless composer",
            screen: Screen::new(
                &[
                    "wrote /etc/hosts just now",
                    "",
                    "  Ran 1 shell command",
                    "",
                    "some more output https://golia.jp/x",
                    rule.as_str(),
                    "❯ /usr/bin https://typing.example/now",
                    rule.as_str(),
                    "⏵⏵ bypass permissions on (shift+tab to cycle)",
                ],
                46,
            )
            .cursor(10, 6),
            opts: opts(Some("/w/p"), true),
        },
        Case {
            name: "wide chars in paths",
            screen: Screen::new(
                &[
                    "看 ~/Downloads/決算明細_2025-2026.xlsx 和 文档/说明.md 吧",
                    "链接 https://example.com/路径 和 /etc/hosts：好",
                ],
                70,
            ),
            opts: opts(Some("/w/p"), true),
        },
    ]
}

/// One line per range, every field.
pub fn render(links: &[LinkRange]) -> String {
    let mut s = String::new();
    for l in links {
        s.push_str(&format!(
            "{} {} {} {:?} {:?} {:?}\n",
            l.row, l.col_start, l.col_end, l.kind, l.text, l.target
        ));
    }
    s
}

/// The whole corpus through `scan`, one block per case.
pub fn render_corpus(mut scan: impl FnMut(&Case) -> Vec<LinkRange>) -> String {
    let mut s = String::new();
    for case in corpus() {
        s.push_str(&format!("== {}\n", case.name));
        s.push_str(&render(&scan(&case)));
    }
    s
}

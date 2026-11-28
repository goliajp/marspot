//! The menu a click on a link opens, and what its Open and Reveal
//! rows hand to `open(1)`.

use marspot::grid_links::LinkKind;
use marspot::link_open::{file_open_for, open_arg_for, FileOpen};
use marspot::lx_warn;
use marspot::ui::strings::{t, Msg};

use super::ContextMenuAction;

/// Cells of target text one menu row holds: the menu is at most
/// `MENU_MAX_W` points wide, about 24 cells of its label type.
const TARGET_ROW_CELLS: usize = 22;
/// Rows the target may take; a longer one keeps its start and its end.
const TARGET_ROWS_MAX: usize = 6;

/// The rows a declared link's menu starts with: its whole target, cut
/// to the menu's width. Characters that draw as nothing or reorder the
/// text around them (format and control characters) are shown as
/// `%XX` escapes, so the rows cannot be made to read as another target.
fn target_rows(uri: &str) -> Vec<String> {
    use marspot::ui::core::text_fit::cells;
    let mut shown = String::with_capacity(uri.len());
    for c in uri.chars() {
        if invisible(c) {
            let mut b = [0u8; 4];
            for byte in c.encode_utf8(&mut b).bytes() {
                shown.push_str(&format!("%{byte:02X}"));
            }
        } else {
            shown.push(c);
        }
    }
    let max = TARGET_ROW_CELLS * TARGET_ROWS_MAX;
    if cells(&shown) > max {
        let chars: Vec<char> = shown.chars().collect();
        let keep = (max - 1) / 2;
        let head: String = chars[..keep].iter().collect();
        let tail: String = chars[chars.len() - keep..].iter().collect();
        shown = format!("{head}…{tail}");
    }
    let mut rows = vec![String::new()];
    for c in shown.chars() {
        let row = rows.last_mut().expect("one row at least");
        if !row.is_empty() && cells(row) + cells(c.encode_utf8(&mut [0u8; 4])) > TARGET_ROW_CELLS {
            rows.push(String::new());
        }
        rows.last_mut().expect("just pushed").push(c);
    }
    rows
}

/// Unicode's format characters (category Cf) and the C0 / C1 controls.
fn invisible(c: char) -> bool {
    matches!(c as u32,
        0x00..=0x1F | 0x7F..=0x9F | 0xAD | 0x600..=0x605 | 0x61C | 0x6DD | 0x70F | 0x890..=0x891 | 0x8E2
        | 0x180E | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F | 0xFEFF
        | 0xFFF9..=0xFFFB | 0x110BD | 0x110CD | 0x13430..=0x1343F | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A
        | 0xE0001 | 0xE0020..=0xE007F)
}

/// Open (or, with `reveal`, show in Finder) what `link` names.  A file
/// link to something `open(1)` would launch is only shown in Finder.
/// Runs off the main thread: deciding that reads the filesystem, and
/// one stat on a network mount can take seconds.
pub(super) fn open(link: &LinkContext, reveal: bool) {
    let Some(arg) = open_arg_for(link.kind, link.actionable()) else { return };
    let (file, declared) = (link.kind == LinkKind::File, link.declared);
    std::thread::spawn(move || {
        let reveal = reveal || (file && file_open_for(&arg, declared) == FileOpen::Reveal);
        spawn_open_with(&arg, reveal);
    });
}

/// Owned snapshot of the URL/path the menu was opened on.  Lives on
/// `ContextMenuState` so the dispatcher can pull it after the menu
/// has been cleared.  Kept tiny — the kind drives label wording, the
/// text drives Open + Copy actions.
#[derive(Debug, Clone)]
pub(super) struct LinkContext {
    pub(super) text: String,
    /// What the span NAMES, when that is not what it shows — a
    /// relative path is drawn `src/main.rs` and means
    /// `<cwd>/src/main.rs`.  Open and Copy act on this; the label and
    /// the underline stay with the text.
    pub(super) target: Option<String>,
    pub(super) kind: marspot::grid_links::LinkKind,
    /// The program named the target itself (OSC 8), so the text on
    /// screen says nothing about where the link goes.
    pub(super) declared: bool,
}

impl LinkContext {
    /// The string Open and Copy operate on.
    pub(super) fn actionable(&self) -> &str {
        self.target.as_deref().unwrap_or(&self.text)
    }
}

/// Build the right-click menu items shown when the user clicks (left
/// or right) on an underlined URL / file-path / email span.  Returns
/// Copy + Open entries (Copy on top — primary intent in a terminal
/// context is "grab this URL/path/address", not "launch the app").
/// Email routes through `/usr/bin/open mailto:…` so the system's
/// default mail client picks it up.
///
/// **Regression-gate**: this is the contract behind the "click a URL,
/// get a panel" feature.  The tail of the click chain is:
///
///   AppKit `(rightM|m)ouseDown:` → L1 shell → MsgType wire → L2
///   `CoreApp::mouse_(right_)down` → `hit_test_link_at_xy` (`grid_links::scan_visible_links`)
///   → `link_menu_items_for` → `ContextMenuState` set → renderer
///   `set_context_menu` → ContextMenu paint over grid.
///
/// User noted a transient regression of this feature on 2026-06-25
/// without a root cause; the unit tests below pin the contract on
/// each LinkKind so a future drift surfaces at `cargo nextest`.
pub(super) fn link_menu_items_for(
    link: &LinkContext,
) -> Vec<marspot::ui::components::MenuItem> {
    use marspot::ui::components::MenuItem;
    // UUID is copy-only (nothing sensible to `open(1)`); every other
    // kind surfaces both Copy and Open.
    if link.kind == LinkKind::Uuid {
        return vec![MenuItem::entry(
            t(Msg::LinkCopyUuid),
            ContextMenuAction::CopyLink.tag(),
        )];
    }
    let (open_label, copy_label) = match link.kind {
        LinkKind::Url => (Msg::LinkOpenUrl, Msg::LinkCopyUrl),
        LinkKind::File => (Msg::LinkOpenFile, Msg::LinkCopyPath),
        LinkKind::Email => (Msg::LinkSendEmail, Msg::LinkCopyEmail),
        LinkKind::Ip => (Msg::LinkOpenInBrowser, Msg::LinkCopyIp),
        LinkKind::Uuid => unreachable!("handled above"),
    };
    let mut items = Vec::with_capacity(4 + TARGET_ROWS_MAX);
    if link.declared {
        // the whole target first: what is on screen may be anything
        for row in target_rows(&link.text) {
            items.push(MenuItem::entry(&row, 0).disabled());
        }
    }
    items.push(MenuItem::entry(t(copy_label), ContextMenuAction::CopyLink.tag()));
    items.push(MenuItem::entry(t(open_label), ContextMenuAction::OpenLink.tag()));
    if link.kind == LinkKind::File {
        items.push(MenuItem::entry(
            t(Msg::LinkRevealFile),
            ContextMenuAction::RevealLink.tag(),
        ));
    }
    marspot::ui::components::grouped([items])
}

/// Hand `arg` off to macOS `open(1)` so the system routes it to the
/// right helper: URL → default browser, file → default app for the
/// file's UTI.  Fire and forget; we don't wait on the child.  Stderr
/// inherited so a malformed arg surfaces in marspot.log via the usual
/// stderr pipe instead of vanishing.  `reveal` shows the file selected
/// in its folder instead of opening it.
fn spawn_open_with(arg: &str, reveal: bool) {
    // `spawn` only reports that the process could not START.  When
    // `open(1)` itself fails — the usual reason being a path that no
    // longer exists — it exits non-zero and prints to a stderr nobody
    // reads, so a click that does nothing leaves no trace at all.  A
    // link is only drawn after linkify stats the path, so a miss here
    // means the file moved between the frame and the click; say so.
    if !arg.starts_with("http://")
        && !arg.starts_with("https://")
        && !arg.starts_with("mailto:")
        && std::fs::symlink_metadata(arg).is_err()
    {
        lx_warn!(
            "core.link_open_missing",
            "link target is gone — the pane still shows it because \
             nothing has redrawn that row since",
            arg = arg
        );
    }
    let mut cmd = std::process::Command::new("/usr/bin/open");
    if reveal {
        cmd.arg("-R");
    }
    if let Err(e) = cmd
        .arg(arg)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .spawn()
    {
        lx_warn!(
            "core.link_open_failed",
            &format!("{e}"),
            arg = arg
        );
    }
}

#[cfg(test)]
mod link_menu_tests {
    use super::*;
    use marspot::grid_links::LinkKind;

    fn ctx(kind: LinkKind) -> LinkContext {
        LinkContext { text: "https://example.com".into(), target: None, kind, declared: false }
    }

    #[test]
    fn url_yields_copy_then_open() {
        let items = link_menu_items_for(&ctx(LinkKind::Url));
        assert_eq!(items.len(), 2, "URL must surface Copy + Open");
        assert_eq!(items[0].label, "Copy URL");
        assert_eq!(items[0].action_tag, ContextMenuAction::CopyLink.tag());
        assert_eq!(items[1].label, "Open URL");
        assert_eq!(items[1].action_tag, ContextMenuAction::OpenLink.tag());
    }

    /// A `~/…` file link must reach `open(1)` already expanded.
    ///
    /// `open(1)` does no shell expansion — handed `~/x`, it looks for a
    /// directory literally named `~` under its cwd and reports the file
    /// missing.  linkify DOES expand `~` for its existence check, so
    /// such links underlined normally and then did nothing when
    /// clicked (2026-08-21 report).  Nothing surfaced the failure
    /// either: `spawn` succeeds as long as the process starts.
    #[test]
    fn a_tilde_file_link_is_expanded_before_open() {
        let home = std::env::var("HOME").expect("HOME");
        let arg = open_arg_for(marspot::grid_links::LinkKind::File, "~/Downloads/x.pdf")
            .expect("file links have an open action");
        assert!(
            !arg.contains('~'),
            "open(1) gets no shell: `~` must already be gone, got {arg:?}"
        );
        assert_eq!(arg, format!("{home}/Downloads/x.pdf"));

        // An absolute path is passed through untouched.
        let abs = open_arg_for(marspot::grid_links::LinkKind::File, "/tmp/y.log").unwrap();
        assert_eq!(abs, "/tmp/y.log");
    }

    #[test]
    fn file_yields_copy_path_then_open_file() {
        let items = link_menu_items_for(&ctx(LinkKind::File));
        assert_eq!(items.len(), 3, "File must surface Copy + Open + Reveal");
        assert_eq!(items[0].label, "Copy path");
        assert_eq!(items[1].label, "Open file");
        assert_eq!(items[2].label, "Open containing folder");
        assert_eq!(items[2].action_tag, ContextMenuAction::RevealLink.tag());
    }

    #[test]
    fn email_yields_copy_email_then_send_email() {
        let items = link_menu_items_for(&ctx(LinkKind::Email));
        assert_eq!(items.len(), 2, "Email must surface Copy + Send");
        assert_eq!(items[0].label, "Copy email");
        assert_eq!(items[0].action_tag, ContextMenuAction::CopyLink.tag());
        assert_eq!(items[1].label, "Send email");
        assert_eq!(items[1].action_tag, ContextMenuAction::OpenLink.tag());
    }

    /// Belt-and-braces: a NEW LinkKind variant (e.g. a future
    /// `Anchor` for HTTP fragment links) must not silently produce
    /// an empty menu without an explicit `return Vec::new()` — the
    /// `match` above is non-exhaustive-by-design and a new variant
    /// would force a compile-time decision.  This test fails to
    /// compile (not just fails to pass) if the variant is missed —
    /// listing each known variant pins the surface.
    #[test]
    fn every_known_linkkind_is_handled() {
        for kind in [
            LinkKind::Url,
            LinkKind::File,
            LinkKind::Email,
            LinkKind::Ip,
            LinkKind::Uuid,
        ] {
            let items = link_menu_items_for(&ctx(kind));
            let expected = match kind {
                LinkKind::Uuid => 1, // copy-only
                LinkKind::File => 3,
                _ => 2,
            };
            assert_eq!(items.len(), expected, "{kind:?}");
        }
    }

    #[test]
    fn ip_yields_copy_ip_then_open_in_browser() {
        let items = link_menu_items_for(&ctx(LinkKind::Ip));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "Copy IP");
        assert_eq!(items[0].action_tag, ContextMenuAction::CopyLink.tag());
        assert_eq!(items[1].label, "Open in browser");
        assert_eq!(items[1].action_tag, ContextMenuAction::OpenLink.tag());
    }

    #[test]
    fn uuid_yields_copy_only() {
        let items = link_menu_items_for(&ctx(LinkKind::Uuid));
        assert_eq!(items.len(), 1, "UUID must be copy-only");
        assert_eq!(items[0].label, "Copy UUID");
        assert_eq!(items[0].action_tag, ContextMenuAction::CopyLink.tag());
    }

    #[test]
    fn osc8_the_menu_of_a_declared_link_starts_with_its_whole_target() {
        let target = |items: &[marspot::ui::components::MenuItem]| -> String {
            items.iter().take_while(|i| !i.enabled).map(|i| i.label.as_str()).collect()
        };
        let uri = "file:///Applications/Calculator.app";
        let link = LinkContext { text: uri.into(), target: Some(uri.into()), kind: LinkKind::File, declared: true };
        let items = link_menu_items_for(&link);
        assert_eq!(target(&items), uri, "the rows before the first action are the whole target");
        assert!(items.iter().take_while(|i| !i.enabled).all(|i| i.label.chars().count() <= TARGET_ROW_CELLS));
        let first_action = items.iter().position(|i| i.enabled).unwrap();
        assert_eq!(items[first_action].action_tag, ContextMenuAction::CopyLink.tag());

        let long = format!("https://example.com/{}", "a".repeat(400));
        let link = LinkContext { text: long, target: None, kind: LinkKind::Url, declared: true };
        let items = link_menu_items_for(&link);
        let shown = target(&items);
        assert_eq!(items.iter().filter(|i| !i.enabled).count(), TARGET_ROWS_MAX);
        assert!(shown.starts_with("https://example.com/") && shown.ends_with("aaa") && shown.contains('…'));

        let spoof = "https://good.example/\u{202E}gpj.exe";
        let link = LinkContext { text: spoof.into(), target: None, kind: LinkKind::Url, declared: true };
        assert_eq!(target(&link_menu_items_for(&link)), "https://good.example/%E2%80%AEgpj.exe");

        let shaped = LinkContext { declared: false, ..link };
        assert_eq!(link_menu_items_for(&shaped)[0].action_tag, ContextMenuAction::CopyLink.tag());
    }
}

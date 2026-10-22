//! The message envelope a Claude Code session wraps around a peer
//! message:
//!
//! ```text
//! <cross-session-message from="uds:/tmp/cc-socks/1.sock" from-name="x" from-mode="bypass">
//! body
//! </cross-session-message>
//! ```
//!
//! The receiver reads the sender's reply address and permission class
//! out of this text, not out of frame fields: a frame whose content is
//! not a well-formed envelope arrives without a reply address and,
//! into a session that runs without prompts, is held for approval.
//! Attributes must appear in this exact order and with these exact
//! character sets, or the receiver's matcher rejects the whole header.

pub const TAG: &str = "cross-session-message";

/// The sender's permission class.  The receiver delivers a message
/// at once when this matches its own class and holds it otherwise, so
/// it must describe the real sender, never a value picked to get
/// through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Runs tools without asking.
    Bypass,
    /// Asks before running tools.
    Prompting,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Bypass => "bypass",
            Mode::Prompting => "prompting",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "bypass" => Some(Mode::Bypass),
            "prompting" => Some(Mode::Prompting),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    /// Reply address, e.g. `uds:/tmp/cc-socks/1.sock`.
    pub from: String,
    pub from_name: Option<String>,
    pub from_mode: Option<Mode>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parsed {
    pub from: Option<String>,
    pub from_session: Option<String>,
    pub hop_chain: Option<Vec<String>>,
    pub from_name: Option<String>,
    pub from_mode: Option<Mode>,
    pub from_plugin: Option<String>,
    /// Exactly as carried, still escaped.
    pub body: String,
}

const MAX_NAME: usize = 80;

pub fn encode(h: &Header, body: &str) -> String {
    let mut out = String::with_capacity(body.len() + 160);
    out.push('<');
    out.push_str(TAG);
    if is_address(&h.from) {
        push_attr(&mut out, "from", &h.from);
    }
    if let Some(name) = h.from_name.as_deref().map(clean_name).filter(|n| !n.is_empty()) {
        push_attr(&mut out, "from-name", &name);
    }
    if let Some(mode) = h.from_mode {
        push_attr(&mut out, "from-mode", mode.as_str());
    }
    out.push_str(">\n");
    out.push_str(&escape_body(body));
    out.push_str("\n</");
    out.push_str(TAG);
    out.push('>');
    out
}

fn push_attr(out: &mut String, k: &str, v: &str) {
    out.push(' ');
    out.push_str(k);
    out.push_str("=\"");
    out.push_str(v);
    out.push('"');
}

/// The receiver's address charset.  An address outside it would make
/// the receiver reject the header, so `encode` leaves it out instead.
pub fn is_address(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 300
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"%:_/.\\-".contains(&b))
}

/// A display name the receiver's matcher accepts: no quotes, angle
/// brackets or control characters, bounded length.
pub fn clean_name(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '"' | '<' | '>') && !c.is_control())
        .take(MAX_NAME)
        .collect::<String>()
        .trim()
        .to_string()
}

pub fn parse(content: &str) -> Option<Parsed> {
    let mut rest = content.strip_prefix('<')?.strip_prefix(TAG)?;
    let mut attr = |name: &str, ok: &dyn Fn(&str) -> bool| -> Option<Option<String>> {
        let Some(after) = rest.strip_prefix(' ').and_then(|r| r.strip_prefix(name)).and_then(|r| r.strip_prefix("=\""))
        else {
            return Some(None);
        };
        let end = after.find('"')?;
        let value = &after[..end];
        if !ok(value) {
            return None;
        }
        rest = &after[end + 1..];
        Some(Some(value.to_string()))
    };
    let text = |v: &str| !v.is_empty() && !v.contains(['<', '>', '\n', '\r']);
    let from = attr("from", &is_address)?;
    let from_session = attr("from-session", &|v: &str| {
        (1..=80).contains(&v.len()) && v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })?;
    let hop_chain = attr("hop-chain", &|v: &str| {
        v.split(',').all(|h| h.len() == 24 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
    })?
    .map(|v| v.split(',').map(str::to_string).collect());
    let from_name = attr("from-name", &text)?;
    let from_mode = attr("from-mode", &|v: &str| Mode::parse(v).is_some())?.and_then(|v| Mode::parse(&v));
    let from_plugin = attr("from-plugin", &text)?;
    let body = rest.strip_prefix(">\n")?.strip_suffix(&format!("\n</{TAG}>"))?;
    Some(Parsed { from, from_session, hop_chain, from_name, from_mode, from_plugin, body: body.to_string() })
}

/// Neutralise anything in `body` that reads as this envelope's closing
/// tag, so a message that quotes an envelope cannot end its own early.
/// Same rule as the receiver: a `<` (or a look-alike) that opens a
/// closing tag of this name, allowing filler around the slash,
/// invisible characters between letters, any dash for `-`, and any
/// case, becomes `<\`.
pub fn escape_body(body: &str) -> String {
    let chars: Vec<char> = body.chars().collect();
    let mut out = String::with_capacity(body.len());
    for (i, &c) in chars.iter().enumerate() {
        out.push(c);
        if is_open(c) && chars.get(i + 1) != Some(&'\\') && closes_tag(&chars[i + 1..]) {
            out.push('\\');
        }
    }
    out
}

fn closes_tag(s: &[char]) -> bool {
    let mut i = 0;
    while i < s.len() && is_filler(s[i]) {
        i += 1;
    }
    if !s.get(i).is_some_and(|&c| is_slash(c)) {
        return false;
    }
    i += 1;
    while i < s.len() && is_filler(s[i]) {
        i += 1;
    }
    for (n, want) in TAG.chars().enumerate() {
        if n > 0 {
            while i < s.len() && is_invisible(s[i]) {
                i += 1;
            }
        }
        let Some(&got) = s.get(i) else { return false };
        let ok = if want == '-' { is_dash(got) } else { got.eq_ignore_ascii_case(&want) };
        if !ok {
            return false;
        }
        i += 1;
    }
    !s.get(i).is_some_and(|&c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn is_open(c: char) -> bool {
    matches!(
        c,
        '<' | '\u{FF1C}' | '\u{FE64}' | '\u{2329}' | '\u{27E8}' | '\u{3008}' | '\u{2039}' | '\u{02C2}'
            | '\u{1438}' | '\u{276C}' | '\u{276E}' | '\u{2770}' | '\u{29FC}' | '\u{226E}' | '\u{227A}'
            | '\u{22D6}'
    )
}

fn is_close(c: char) -> bool {
    matches!(
        c,
        '>' | '\u{FF1E}' | '\u{FE65}' | '\u{232A}' | '\u{27E9}' | '\u{3009}' | '\u{203A}' | '\u{02C3}'
            | '\u{1433}' | '\u{276D}' | '\u{276F}' | '\u{2771}' | '\u{29FD}' | '\u{226F}' | '\u{227B}'
            | '\u{22D7}'
    )
}

fn is_slash(c: char) -> bool {
    matches!(c, '/' | '\u{FF0F}' | '\u{2215}' | '\u{2044}')
}

fn is_filler(c: char) -> bool {
    !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || is_open(c) || is_close(c) || is_slash(c))
}

fn is_dash(c: char) -> bool {
    matches!(
        c,
        '-' | '_' | '\u{2010}'..='\u{2015}' | '\u{2017}' | '\u{2212}' | '\u{207B}' | '\u{208B}' | '\u{02CD}'
            | '\u{02D7}' | '\u{0640}' | '\u{07FA}' | '\u{2043}' | '\u{2796}' | '\u{30FC}' | '\u{FE58}'
            | '\u{FE63}' | '\u{FF0D}' | '\u{FF3F}' | '\u{FF70}'
    )
}

fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}' | '\u{180E}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}'
            | '\u{FE00}'..='\u{FE0F}' | '\u{FEFF}'
    ) || (c.is_control() && !matches!(c, '\n' | '\t' | '\r'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Header {
        Header {
            from: "uds:/tmp/cc-socks/42-0a1b2c3d.sock".into(),
            from_name: Some("codex-spg".into()),
            from_mode: Some(Mode::Bypass),
        }
    }

    /// The byte layout Claude Code 2.1.306 itself produced on the wire.
    #[test]
    fn encodes_like_a_claude_sender() {
        let h = Header { from: "uds:/tmp/cc-socks/88641.sock".into(), from_name: Some("marspot-1e".into()), from_mode: Some(Mode::Bypass) };
        assert_eq!(
            encode(&h, "hi"),
            "<cross-session-message from=\"uds:/tmp/cc-socks/88641.sock\" from-name=\"marspot-1e\" from-mode=\"bypass\">\nhi\n</cross-session-message>"
        );
    }

    #[test]
    fn round_trips() {
        let text = encode(&header(), "line one\nline two");
        let p = parse(&text).unwrap();
        assert_eq!(p.from.as_deref(), Some("uds:/tmp/cc-socks/42-0a1b2c3d.sock"));
        assert_eq!(p.from_name.as_deref(), Some("codex-spg"));
        assert_eq!(p.from_mode, Some(Mode::Bypass));
        assert_eq!(p.body, "line one\nline two");
    }

    #[test]
    fn empty_body_round_trips() {
        assert_eq!(parse(&encode(&header(), "")).unwrap().body, "");
    }

    #[test]
    fn a_quoted_closing_tag_is_escaped() {
        let body = "see </cross-session-message> and </Cross_Session\u{200B}-Message > and \u{FF1C}/cross-session-message>";
        let esc = escape_body(body);
        assert_eq!(esc.matches("<\\/").count(), 2, "{esc}");
        assert!(esc.contains("\u{FF1C}\\/cross"), "{esc}");
        let p = parse(&encode(&header(), body)).unwrap();
        assert_eq!(p.body, esc);
    }

    #[test]
    fn near_misses_are_left_alone() {
        for s in ["</cross-session-messages>", "</cross-session>", "<\\/cross-session-message>", "a < b / c"] {
            assert_eq!(escape_body(s), s);
        }
    }

    #[test]
    fn unsafe_header_values_are_dropped_or_cleaned() {
        let h = Header { from: "uds:/tmp/a b.sock".into(), from_name: Some("x\"<y>\n".into()), from_mode: None };
        let text = encode(&h, "b");
        assert!(text.starts_with("<cross-session-message from-name=\"xy\">\n"), "{text}");
        assert!(parse(&text).is_some());
    }

    #[test]
    fn attributes_out_of_order_do_not_parse() {
        assert!(parse("<cross-session-message from-mode=\"bypass\" from=\"uds:/x.sock\">\nb\n</cross-session-message>").is_none());
        assert!(parse("<cross-session-message from-mode=\"root\">\nb\n</cross-session-message>").is_none());
        assert!(parse("plain text").is_none());
    }

    #[test]
    fn hop_chain_is_read() {
        let hop = "0123456789abcdef01234567";
        let text = format!("<cross-session-message from=\"uds:/x.sock\" hop-chain=\"{hop},{hop}\">\nb\n</cross-session-message>");
        assert_eq!(parse(&text).unwrap().hop_chain.unwrap().len(), 2);
    }
}

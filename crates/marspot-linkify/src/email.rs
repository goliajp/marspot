//! Email address recognition.

/// Walk backwards from an `@` to find the start of the local part.
/// Stops as soon as we hit something that can't be in an email local
/// part (whitespace, punctuation we don't allow, etc.).
pub(crate) fn scan_back_local(chars: &[char], at_pos: usize) -> usize {
    let mut i = at_pos;
    while i > 0 {
        let c = chars[i - 1];
        if is_email_local_char(c) {
            i -= 1;
        } else {
            break;
        }
    }
    i
}

/// Walk forward from after `@` for the host part: letters, digits,
/// `-`, `.`.  Stops at the first foreign char.
pub(crate) fn scan_forward_host(chars: &[char], start: usize) -> usize {
    let mut i = start;
    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
            i += 1;
        } else {
            break;
        }
    }
    // Trim a trailing dot (common in prose: "ping foo@bar.com.").
    if i > start && chars[i - 1] == '.' {
        i -= 1;
    }
    i
}

/// Structural hostname check for the email scanner, distilled from
/// how mature linkifiers avoid the `pkg@1.0.23` false-positive class
/// (npm/cargo version strings, `tag@sha`, `image@digest`, …):
///
///   - dot-separated labels, ≥ 2 of them
///   - every label non-empty, `[a-z0-9-]`, no leading/trailing `-`
///   - the FINAL label (TLD) is purely ALPHABETIC, length ≥ 2 —
///     this is the discriminating rule; no real TLD is numeric
///
/// GitHub's linkifier and commonmark autolinks apply the same TLD
/// constraint; WezTerm's default `\w+@[\w-]+(\.[\w-]+)+` does not
/// and underlines version strings — the exact trap we hit
/// (2026-07-12: `adapter-maestro@1.0.23` underlined).  IP-literal
/// mail hosts are RFC-legal but never appear in prose worth
/// linking; deliberately left unmatched.
pub(crate) fn is_email_host(chars: &[char]) -> bool {
    if chars.is_empty() {
        return false;
    }
    let mut label_count = 0usize;
    let mut last_label_alpha = false;
    let mut last_label_len = 0usize;
    for label in chars.split(|c| *c == '.') {
        if label.is_empty() {
            return false;
        }
        if label[0] == '-' || label[label.len() - 1] == '-' {
            return false;
        }
        if !label
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == '-')
        {
            return false;
        }
        label_count += 1;
        last_label_alpha = label.iter().all(|c| c.is_ascii_alphabetic());
        last_label_len = label.len();
    }
    label_count >= 2 && last_label_alpha && last_label_len >= 2
}

fn is_email_local_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-' | '%')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LinkKind;
    use crate::tests::{chars, scan};

    #[test]
    fn email_ok() {
        let v = scan("ping ada@example.jp today");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, LinkKind::Email);
        assert_eq!(v[0].text, "ada@example.jp");
    }

    #[test]
    fn email_at_alone_is_not_an_email() {
        let v = scan("hey @everyone");
        assert_eq!(v.iter().filter(|r| r.kind == LinkKind::Email).count(), 0);
    }

    #[test]
    fn email_trims_trailing_period() {
        let v = scan("contact foo@bar.com.");
        let emails: Vec<_> = v.iter().filter(|r| r.kind == LinkKind::Email).collect();
        assert_eq!(emails.len(), 1);
        assert_eq!(emails[0].text, "foo@bar.com");
    }

    /// 2026-07-12 regression: `name@version` tokens (npm / cargo /
    /// image digests) must NOT match as email — a real mail domain's
    /// TLD is alphabetic, a version's final label is numeric.
    #[test]
    fn version_strings_are_not_emails() {
        let line = "crates.io smix-cli/adapter-maestro@1.0.23, npm @goliapkg/smix@1.0.23,";
        let v = scan(line);
        assert!(
            v.is_empty(),
            "version strings must not produce any link: {v:?}"
        );
        assert!(scan("docker pull app@sha256.0abc").is_empty());
        assert!(scan("pinned pkg@2.x today").is_empty()); // 1-char TLD
        // Real addresses keep matching.
        assert_eq!(scan("mail cara@example.com now").len(), 1);
        assert_eq!(scan("cc user.name+tag@sub-1.example.co").len(), 1);
    }

    #[test]
    fn email_host_label_rules() {
        assert!(is_email_host(&chars("golia.jp")));
        assert!(is_email_host(&chars("sub-1.example.co")));
        assert!(!is_email_host(&chars("1.0.23"))); // numeric TLD
        assert!(!is_email_host(&chars("example"))); // single label
        assert!(!is_email_host(&chars("bar-.com"))); // label ends with '-'
        assert!(!is_email_host(&chars("-bar.com"))); // label starts with '-'
        assert!(!is_email_host(&chars("bar..com"))); // empty label
        assert!(!is_email_host(&chars("bar.c"))); // 1-char TLD
    }
}

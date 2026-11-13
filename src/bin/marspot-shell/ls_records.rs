//! What LaunchServices has on record, read from `lsappinfo list`.
//!
//! No public interface says which record a coalition is listed under,
//! or that a record is one whose process is gone: `NSRunningApplication`
//! shows neither.  The system's own `lsappinfo` prints both, for
//! people; this reads its text.  It is run when a shell takes the place
//! of another and at no other time.

/// One application record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    pub pid: i32,
    /// Its process is gone; the record stays for what is still listed
    /// under it.
    pub dead: bool,
    pub bundle_id: Option<String>,
    /// The coalition listed under it, if one is.
    pub coalition: Option<u64>,
}

fn quoted<'a>(block: &'a str, key: &str) -> Option<&'a str> {
    let rest = &block[block.find(key)? + key.len()..];
    let rest = rest.strip_prefix("=\"")?;
    Some(&rest[..rest.find('"')?])
}

fn number<T: std::str::FromStr>(block: &str, key: &str) -> Option<T> {
    let rest = block[block.find(key)? + key.len()..].trim_start();
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// A record starts at a line of the form ` 12) "Name" ASN:…`.
fn starts_record(line: &str) -> bool {
    let line = line.trim_start();
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && line[digits..].starts_with(") \"")
}

pub(crate) fn parse(text: &str) -> Vec<Record> {
    let mut blocks: Vec<String> = Vec::new();
    for line in text.lines() {
        if starts_record(line) {
            blocks.push(String::new());
        }
        if let Some(block) = blocks.last_mut() {
            block.push_str(line);
            block.push('\n');
        }
    }
    blocks
        .iter()
        .filter_map(|b| {
            Some(Record {
                pid: number(b, "pid = ")?,
                dead: b.contains("(exited"),
                bundle_id: quoted(b, "bundleID").map(str::to_string),
                // on a line of its own: `relatedCoalitionIDs:` is not it
                coalition: b.lines().find_map(|l| number(l.trim_start().strip_prefix("coalition:")?, "")),
            })
        })
        .collect()
}

pub(crate) fn list() -> Vec<Record> {
    match std::process::Command::new("/usr/bin/lsappinfo").arg("list").stdin(std::process::Stdio::null()).output() {
        Ok(out) => parse(&String::from_utf8_lossy(&out.stdout)),
        Err(_) => Vec::new(),
    }
}

/// Who a coalition is listed under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Holder {
    Nobody,
    /// This process.
    Us,
    /// A record of this app whose process is gone.
    OurDead,
    /// Another program's record, of a living process or a dead one.
    Other { pid: i32, dead: bool },
}

pub(crate) fn holder_of(records: &[Record], coalition: u64, me: i32, bundle_id: &str) -> Holder {
    match records.iter().find(|r| r.coalition == Some(coalition)) {
        None => Holder::Nobody,
        Some(r) if r.pid == me && !r.dead => Holder::Us,
        Some(r) if r.dead && r.bundle_id.as_deref() == Some(bundle_id) => Holder::OurDead,
        Some(r) => Holder::Other { pid: r.pid, dead: r.dead },
    }
}

/// The records of this app whose process is gone.
pub(crate) fn our_dead(records: &[Record], bundle_id: &str) -> usize {
    records.iter().filter(|r| r.dead && r.bundle_id.as_deref() == Some(bundle_id)).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = r#" 1) "loginwindow" ASN:0x0-0x1001:
    bundleID="com.apple.loginwindow"
    bundle path="/System/Library/CoreServices/loginwindow.app"
    executable path="/System/Library/CoreServices/loginwindow.app/Contents/MacOS/loginwindow"
    pid = 668 token=[sess=100017 pid=668 uid:0,0,4294967295 g:0,0 pV:1527] type="UIElement" flavor=3 Version="3109.2.5" fileType="APPL" creator="lgnw" Arch=ARM64
    childASNs: ASN:0x0-0x27027: ASN:0x0-0x2a02a:
    coalition: 406  { 668 52210 }
    checkin time = 2026/11/10 10:05:19 ( 3 days, 2 hours, 42 minutes, 12.1778 seconds ago )
 2) "universalaccessd" ASN:0x0-0xb00b:
    bundleID=[ NULL ]
    bundle path="/usr/sbin/universalaccessd"
    executable path="/usr/sbin/universalaccessd"
    pid = 1040 token=[sess=100017 pid=1040 uid:501,501,501 g:20,20 pV:2500] !signalled type="BackgroundOnly" flavor=3 Version=[ NULL ]  fileType="????" creator="????" Arch=ARM64
    coalition: 769
    checkin time = 2026/11/10 10:05:48 ( 3 days, 2 hours, 41 minutes, 42.9552 seconds ago )
"#;

    #[test]
    fn a_listing_reads_as_its_records() {
        let records = parse(LIST);
        assert_eq!(
            records,
            vec![
                Record { pid: 668, dead: false, bundle_id: Some("com.apple.loginwindow".into()), coalition: Some(406) },
                Record { pid: 1040, dead: false, bundle_id: None, coalition: Some(769) },
            ]
        );
        assert_eq!(parse(""), vec![]);
        assert_eq!(parse("lsappinfo: no such thing\n"), vec![]);
    }

    /// As listed on macOS 27.2.1 for an instance that took a dead
    /// record over and exited, and for a program a pane started.
    const DEAD: &str = r#" 46) "RcProbe" ASN:0x0-0x191191:
    bundleID="com.goliajp.rcprobe.exp"
    bundle path="/private/tmp/rc-exp.lMTZzm/RcProbe.app"
    executable path="/private/tmp/rc-exp.lMTZzm/RcProbe.app/Contents/MacOS/rcprobe"
    pid = 76183 token=[sess=100002 pid=76183 uid:501,501,501 g:20,20 pV:469661] type="Foreground" flavor=3 Version=[ NULL ]  fileType="APPL" creator="????" Arch=ARM64 (exited-with-subordinates)
    coalition: 2922    relatedCoalitionIDs: ( 2921)  relatedCoalitionPIDs: ( 75464, 75465)
    checkin time = 8 seconds ago, 2026/11/13 14:34:22 ( 7.71099 seconds ago )
 47) "stranger" ASN:0x0-0x193193:
    bundleID=[ NULL ]
    bundle path="/private/tmp/rc-exp.lMTZzm/stranger"
    executable path="/tmp/rc-exp.lMTZzm/stranger"
    pid = 76587 token=[sess=100002 pid=76587 uid:501,501,501 g:20,20 pV:470383] type="Foreground" flavor=3 Version=[ NULL ]  fileType="????" creator="????" Arch=ARM64 (exited-with-subordinates)
    coalition: 2921  { 75464 75465 75466 77268 }
 48) "RcProbe" ASN:0x0-0x195195:
    bundleID="com.goliajp.rcprobe.exp"
    pid = 77268 token=[sess=100002 pid=77268 uid:501,501,501 g:20,20 pV:471574] type="UIElement" flavor=3 Version=[ NULL ]  fileType="APPL" creator="????" Arch=ARM64
    checkin time = 4 seconds ago, 2026/11/13 14:34:25 ( 3.76318 seconds ago )
"#;

    #[test]
    fn a_dead_record_and_a_related_coalition_are_told_apart() {
        let id = "com.goliajp.rcprobe.exp";
        let records = parse(DEAD);
        assert_eq!(
            records,
            vec![
                Record { pid: 76183, dead: true, bundle_id: Some(id.into()), coalition: Some(2922) },
                Record { pid: 76587, dead: true, bundle_id: None, coalition: Some(2921) },
                Record { pid: 77268, dead: false, bundle_id: Some(id.into()), coalition: None },
            ]
        );
        // related to the dead instance, listed under the dead program
        assert_eq!(holder_of(&records, 2921, 77268, id), Holder::Other { pid: 76587, dead: true });
        assert_eq!(our_dead(&records, id), 1);
    }

    #[test]
    fn a_coalition_is_under_one_record_or_none() {
        let rec = |pid, dead, id: Option<&str>, c| Record { pid, dead, bundle_id: id.map(str::to_string), coalition: c };
        let records = [
            rec(10, false, Some("app"), None),
            rec(11, true, Some("app"), Some(7)),
            rec(12, true, None, Some(8)),
            rec(13, false, None, Some(9)),
            rec(14, true, Some("app"), None),
        ];
        assert_eq!(holder_of(&records, 7, 10, "app"), Holder::OurDead);
        assert_eq!(holder_of(&records, 8, 10, "app"), Holder::Other { pid: 12, dead: true });
        assert_eq!(holder_of(&records, 9, 10, "app"), Holder::Other { pid: 13, dead: false });
        assert_eq!(holder_of(&records, 9, 13, "app"), Holder::Us);
        assert_eq!(holder_of(&records, 6, 10, "app"), Holder::Nobody);
        assert_eq!(our_dead(&records, "app"), 2);
    }
}

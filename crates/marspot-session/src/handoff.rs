//! The manifest one L3 image leaves for the next across its self-execv,
//! and the fingerprint test that decides whether there is a next one.

use std::os::fd::RawFd;
use std::path::Path;

use marspot_term::lx_warn;

use crate::track::{End, LatestEnds};

/// RFC-003 §6 Amendment 16 — L3 self-execv handoff (L4 shelld model).
///
/// On SIGTERM, the L3 looks at the `current/marspot-session` binary
/// next door.  If its MARSPOT_FP fingerprint differs from this
/// process's rodata fingerprint, the L3 stages an execv handoff:
///
///   1. clear CLOEXEC on PTY master fd + UDS listener fd
///   2. write Terminal snapshot to `sessions/<id>/state.bin`
///   3. write manifest TSV to /tmp keyed by pid
///   4. set env sentinel `MARSPOT_L3_HANDOFF_MANIFEST=<path>`
///   5. `execv("current/marspot-session", argv)`
///
/// PID is preserved across execv; the master fd / listener fd / shell
/// child / PTY tty session are all unchanged.  The new image detects
/// the env sentinel at boot, reads the manifest, and rebuilds
/// LocalSession + SessionListener via `from_handoff` instead of
/// forkpty + bind.  L2 sees a brief read pause on its control
/// channel while the new image rebinds its reader; keystrokes resume
/// transparently.
///
/// The same build in `current/` (or none) is not an update: see
/// `staged_is_another_build`.
pub(crate) const ENV_HANDOFF_MANIFEST: &str = "MARSPOT_L3_HANDOFF_MANIFEST";
// Manifest version 2 (2026-06-17): added optional `control_stream_fd`
// so the L3 self-execv can carry the adopted L2↔L3 control UnixStream
// across the image swap.  Without it the stream's fd kept its default
// CLOEXEC=1, closed across execv, and L2 saw EOF + had no auto-reconnect
// → user couldn't type for many minutes after every install.
// Forward-compat: a v1 manifest is still accepted (`control_stream_fd`
// defaults to -1, meaning "no inherited stream — wait for NewClient").
const HANDOFF_MANIFEST_VERSION: u32 = 2;
const HANDOFF_MANIFEST_MIN_COMPAT: u32 = 1;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct L3Handoff {
    pub(crate) master_fd: RawFd,
    pub(crate) child_pid: i32,
    pub(crate) listen_fd: RawFd,
    pub(crate) cols: u16,
    pub(crate) rows: u16,
    /// Adopted L2↔L3 control UnixStream raw fd, or `-1` if there was
    /// no live control stream when the execv handoff fired (which
    /// happens when L2 is mid-swap and the previous client EOF'd
    /// before NewClient adopted a new one).
    pub(crate) control_stream_fd: RawFd,
    /// Not in a manifest from an image before them: zero and none.
    pub(crate) marks: crate::pane_ctl::Marks,
    /// Whose shadow pane this is, and its idle timeout; not in a
    /// manifest from an image before them: no shadow.
    pub(crate) shadow: crate::shadow::ShadowCfg,
}

pub(crate) fn handoff_manifest_path() -> std::path::PathBuf {
    std::path::PathBuf::from(format!(
        "/tmp/marspot-session-handoff.{}.tsv",
        std::process::id()
    ))
}

pub(crate) fn write_handoff_manifest(h: &L3Handoff, path: &std::path::Path) -> std::io::Result<()> {
    let mut body = format!(
        "version={}\nmaster_fd={}\nchild_pid={}\nlisten_fd={}\ncols={}\nrows={}\ncontrol_stream_fd={}\npos={}\n",
        HANDOFF_MANIFEST_VERSION,
        h.master_fd,
        h.child_pid,
        h.listen_fd,
        h.cols,
        h.rows,
        h.control_stream_fd,
        h.marks.pos,
    );
    if let Some(end) = h.marks.ends.any {
        body.push_str(&format!("command_end={}\n", end_value(end)));
    }
    // written without one too: an image that knows runs' ends has seen none
    body.push_str(&format!("run_end={}\n", h.marks.ends.run.map_or("-".to_string(), end_value)));
    if let Some(pos) = h.marks.input_start {
        body.push_str(&format!("input_start={pos}\n"));
    }
    if h.shadow.parent != 0 {
        body.push_str(&format!("shadow_of={}\nshadow_idle_s={}\n", h.shadow.parent, h.shadow.idle_s));
    }
    let tmp = path.with_extension("tsv.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn end_value((pos, status): End) -> String {
    format!("{pos}:{}", status.map_or("-".to_string(), |s| s.to_string()))
}

/// An image that does not know a run's ends from others leaves no
/// `run_end`: its latest end is taken for one, as it would have done.
fn read_ends(fields: &std::collections::HashMap<String, String>) -> LatestEnds {
    let end = |s: &String| {
        let (pos, status) = s.split_once(':')?;
        Some((pos.parse().ok()?, status.parse().ok()))
    };
    let any = fields.get("command_end").and_then(end);
    LatestEnds { any, run: fields.get("run_end").map_or(any, end) }
}

pub(crate) fn read_handoff_manifest(path: &std::path::Path) -> std::io::Result<L3Handoff> {
    let body = std::fs::read_to_string(path)?;
    let mut fields = std::collections::HashMap::new();
    for (k, v) in body.lines().filter_map(|l| l.trim().split_once('=')) {
        fields.insert(k.trim().to_string(), v.trim().to_string());
    }
    let inv = |k: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("manifest bad/missing {k}"));
    let version: u32 = fields.get("version").ok_or_else(|| inv("version"))?.parse().map_err(|_| inv("version"))?;
    if !(HANDOFF_MANIFEST_MIN_COMPAT..=HANDOFF_MANIFEST_VERSION).contains(&version) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "manifest version {version} not in [{HANDOFF_MANIFEST_MIN_COMPAT}, {HANDOFF_MANIFEST_VERSION}]"
            ),
        ));
    }
    // v1 lacked control_stream_fd — default to -1 (no inherited stream).
    let control_stream_fd: RawFd = fields
        .get("control_stream_fd")
        .and_then(|s| s.parse().ok())
        .unwrap_or(-1);
    let marks = crate::pane_ctl::Marks {
        pos: fields.get("pos").and_then(|s| s.parse().ok()).unwrap_or(0),
        ends: read_ends(&fields),
        input_start: fields.get("input_start").and_then(|s| s.parse().ok()),
    };
    let shadow = crate::shadow::ShadowCfg::new(
        fields.get("shadow_of").and_then(|s| s.parse().ok()).unwrap_or(0),
        fields.get("shadow_idle_s").and_then(|s| s.parse().ok()).unwrap_or(0),
    );
    Ok(L3Handoff {
        master_fd: fields
            .get("master_fd")
            .ok_or_else(|| inv("master_fd"))?
            .parse()
            .map_err(|_| inv("master_fd"))?,
        child_pid: fields
            .get("child_pid")
            .ok_or_else(|| inv("child_pid"))?
            .parse()
            .map_err(|_| inv("child_pid"))?,
        listen_fd: fields
            .get("listen_fd")
            .ok_or_else(|| inv("listen_fd"))?
            .parse()
            .map_err(|_| inv("listen_fd"))?,
        cols: fields
            .get("cols")
            .ok_or_else(|| inv("cols"))?
            .parse()
            .map_err(|_| inv("cols"))?,
        rows: fields
            .get("rows")
            .ok_or_else(|| inv("rows"))?
            .parse()
            .map_err(|_| inv("rows"))?,
        control_stream_fd,
        marks,
        shadow,
    })
}

pub(crate) fn try_resume_handoff() -> Option<L3Handoff> {
    let path = std::env::var(ENV_HANDOFF_MANIFEST).ok()?;
    unsafe {
        std::env::remove_var(ENV_HANDOFF_MANIFEST);
    }
    let path = std::path::PathBuf::from(path);
    match read_handoff_manifest(&path) {
        Ok(h) => {
            let _ = std::fs::remove_file(&path);
            Some(h)
        }
        Err(e) => {
            lx_warn!(
                "l3.handoff.manifest_read_failed",
                &format!("{e}; cold start"),
                path = path.display()
            );
            let _ = std::fs::remove_file(&path);
            None
        }
    }
}

/// True when `current/marspot-session` is a different build from this
/// one: the only case in which this session should move.
pub(crate) fn staged_is_another_build() -> bool {
    let Ok(tree) = marspot_term::binary_tree::BinaryTree::default_for("marspot-session") else {
        return false;
    };
    execv_wanted(&tree.current(), marspot_term::build::own())
}

/// True when `current` is a build other than `own`.
///
/// Both sides are compared in one form.  The running marker used to be
/// compared with its `|END` and the file's without, so they never
/// matched and every request re-execed, a copy of the running build
/// included.
fn execv_wanted(current: &Path, own: &str) -> bool {
    if !current.exists() {
        return false;
    }
    let Some(target) = marspot_term::build::of_file(current) else {
        return false;
    };
    target != own
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot_term::test_env::{EnvGuard, TempDir};

    fn sample() -> L3Handoff {
        L3Handoff {
            master_fd: 7,
            child_pid: 4242,
            listen_fd: 9,
            cols: 132,
            rows: 43,
            control_stream_fd: 11,
            marks: crate::pane_ctl::Marks { pos: 123_456, ends: LatestEnds { any: Some((120_000, Some(1))), run: Some((110_000, Some(7))) }, input_start: Some(119_000) },
            shadow: crate::shadow::ShadowCfg::default(),
        }
    }

    fn read_body(dir: &TempDir, body: &str) -> std::io::Result<L3Handoff> {
        let path = dir.join("manifest.tsv");
        std::fs::write(&path, body).unwrap();
        read_handoff_manifest(&path)
    }

    const V2: &str = "version=2\nmaster_fd=7\nchild_pid=4242\nlisten_fd=9\ncols=132\nrows=43\ncontrol_stream_fd=11\npos=123456\ncommand_end=120000:1\nrun_end=110000:7\ninput_start=119000\n";

    /// What one image writes, the next reads back field for field, and
    /// the write leaves no temp file for a later boot to trip over.
    #[test]
    fn a_written_manifest_reads_back_whole() {
        let dir = TempDir::new("handoff");
        let path = dir.join("m.tsv");
        write_handoff_manifest(&sample(), &path).unwrap();
        assert_eq!(read_handoff_manifest(&path).unwrap(), sample());
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(
            left,
            vec![std::ffi::OsString::from("m.tsv")],
            "only the manifest is left"
        );
        let no_stream = L3Handoff {
            control_stream_fd: -1,
            marks: crate::pane_ctl::Marks { pos: 9, ends: LatestEnds { any: Some((5, None)), run: None }, input_start: None },
            ..sample()
        };
        write_handoff_manifest(&no_stream, &path).unwrap();
        assert_eq!(read_handoff_manifest(&path).unwrap(), no_stream);
    }

    /// Every version in [MIN_COMPAT, CURRENT] is taken, and only those:
    /// a manifest from a newer build may carry fields this image does
    /// not know how to adopt.
    #[test]
    fn only_versions_in_the_compatible_range_are_accepted() {
        let dir = TempDir::new("handoff");
        for v in HANDOFF_MANIFEST_MIN_COMPAT..=HANDOFF_MANIFEST_VERSION {
            let body = V2.replace("version=2", &format!("version={v}"));
            assert!(read_body(&dir, &body).is_ok(), "version {v} is in range");
        }
        for v in [0, HANDOFF_MANIFEST_VERSION + 1, 99] {
            let body = V2.replace("version=2", &format!("version={v}"));
            let e = read_body(&dir, &body).unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::InvalidData, "version {v}");
        }
    }

    /// A v1 image had no control stream to pass on; the reader must not
    /// invent one, or the new image would adopt whatever fd that number
    /// happens to name.
    #[test]
    fn a_v1_manifest_without_a_control_stream_reads_as_none() {
        let dir = TempDir::new("handoff");
        let v1 = "version=1\nmaster_fd=7\nchild_pid=4242\nlisten_fd=9\ncols=132\nrows=43\n";
        let h = read_body(&dir, v1).unwrap();
        assert_eq!(
            h,
            L3Handoff {
                control_stream_fd: -1,
                marks: Default::default(),
                ..sample()
            }
        );
    }

    /// An image from before the output position left none: the new one
    /// starts it at zero rather than refusing the handoff.
    #[test]
    fn a_manifest_without_marks_reads_as_zero_and_none() {
        let dir = TempDir::new("handoff");
        let old: String = V2.lines().filter(|l| !l.starts_with("pos=") && !l.contains("_end=") && !l.starts_with("input_start=")).map(|l| format!("{l}\n")).collect();
        assert_eq!(read_body(&dir, &old).unwrap(), L3Handoff { marks: Default::default(), ..sample() });
    }

    /// An image from before runs' ends were told apart (a first
    /// update, or an update after a rollback) hands over its latest end
    /// alone; a watch for a run's end is answered from that one.
    #[test]
    fn a_manifest_without_a_runs_end_takes_the_latest_for_it() {
        let dir = TempDir::new("handoff");
        let old = V2.replace("run_end=110000:7\n", "");
        let end = Some((120_000, Some(1)));
        assert_eq!(read_body(&dir, &old).unwrap().marks.ends, LatestEnds { any: end, run: end });
        let none = V2.replace("run_end=110000:7", "run_end=-");
        assert_eq!(read_body(&dir, &none).unwrap().marks.ends, LatestEnds { any: end, run: None });
    }

    /// A required field that is missing or does not parse is an error,
    /// not a panic and not a zero: adopting fd 0 as the PTY master would
    /// wire the pane to stdin.
    #[test]
    fn a_missing_or_garbled_field_is_rejected() {
        let dir = TempDir::new("handoff");
        for key in [
            "version",
            "master_fd",
            "child_pid",
            "listen_fd",
            "cols",
            "rows",
        ] {
            let prefix = format!("{key}=");
            let without: String = V2
                .lines()
                .filter(|l| !l.starts_with(&prefix))
                .map(|l| format!("{l}\n"))
                .collect();
            let e = read_body(&dir, &without).unwrap_err();
            assert!(e.to_string().contains(key), "missing {key}: {e}");
            for bad in ["", "x", "-", "1.5", "99999999999999999999"] {
                let garbled: String = V2
                    .lines()
                    .map(|l| {
                        if l.starts_with(&prefix) {
                            format!("{key}={bad}\n")
                        } else {
                            format!("{l}\n")
                        }
                    })
                    .collect();
                assert!(
                    read_body(&dir, &garbled).is_err(),
                    "{key}={bad:?} must be rejected"
                );
            }
        }
        let wide = V2.replace("cols=132", "cols=70000");
        assert!(read_body(&dir, &wide).is_err(), "a width that is not a u16");
        assert!(read_body(&dir, "").is_err(), "an empty file");
        assert!(read_body(&dir, "\u{0}\u{1}garbage without equals\n").is_err());
        assert!(
            read_handoff_manifest(&dir.join("never-written.tsv")).is_err(),
            "no file at all"
        );
    }

    /// A shadow pane stays one across the swap, with its timeout; a
    /// manifest without the fields, as from an image before them, is a
    /// pane that is none.
    #[test]
    fn shadow_session_parent_and_timeout_cross_the_manifest() {
        let dir = TempDir::new("handoff");
        let path = dir.join("m.tsv");
        let shadow = L3Handoff { shadow: crate::shadow::ShadowCfg::new(4_200_000_007, 2), ..sample() };
        write_handoff_manifest(&shadow, &path).unwrap();
        assert_eq!(read_handoff_manifest(&path).unwrap(), shadow);
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("shadow_of=4200000007\n") && body.contains("shadow_idle_s=2\n"), "{body}");
        write_handoff_manifest(&sample(), &path).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("shadow"), "nothing for a pane that is none");
        assert_eq!(read_body(&dir, V2).unwrap().shadow, crate::shadow::ShadowCfg::default());
    }

    /// A newer writer may add fields; a reader in range ignores them
    /// instead of refusing the whole handoff.
    #[test]
    fn unknown_fields_are_ignored() {
        let dir = TempDir::new("handoff");
        let body = format!("future_thing=on\n{V2}  \nno equals sign here\nanother=\n");
        assert_eq!(read_body(&dir, &body).unwrap(), sample());
    }

    /// The new image consumes the manifest exactly once, whether it
    /// could use it or not: the variable leaves the environment (the
    /// shell's children must not inherit it), and the file goes, or
    /// every later boot of this pid would try it again.
    #[test]
    fn resume_consumes_the_manifest_and_the_variable() {
        let dir = TempDir::new("handoff");
        let mut env = EnvGuard::new();
        env.remove(ENV_HANDOFF_MANIFEST);
        assert!(try_resume_handoff().is_none(), "no variable, cold start");

        let good = dir.join("good.tsv");
        write_handoff_manifest(&sample(), &good).unwrap();
        env.set(ENV_HANDOFF_MANIFEST, &good);
        assert_eq!(try_resume_handoff(), Some(sample()));
        assert!(std::env::var_os(ENV_HANDOFF_MANIFEST).is_none());
        assert!(!good.exists());

        let bad = dir.join("bad.tsv");
        std::fs::write(&bad, "version=2\nmaster_fd=oops\n").unwrap();
        env.set(ENV_HANDOFF_MANIFEST, &bad);
        assert_eq!(try_resume_handoff(), None, "unusable, cold start");
        assert!(std::env::var_os(ENV_HANDOFF_MANIFEST).is_none());
        assert!(!bad.exists());
    }

    /// The manifest is keyed by this pid, which execv keeps: the new
    /// image finds it at the same name without being told.
    #[test]
    fn the_manifest_path_is_this_pids() {
        let p = handoff_manifest_path();
        assert!(
            p.to_string_lossy()
                .ends_with(&format!(".{}.tsv", std::process::id()))
        );
    }

    fn binary_with(dir: &TempDir, name: &str, fp: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        let mut body = b"\x7fELF-ish header bytes\0\0".to_vec();
        body.extend_from_slice(fp.as_bytes());
        body.extend_from_slice(b"\0trailing rodata\0");
        std::fs::write(&p, body).unwrap();
        p
    }

    /// A different build next door is an update; no file, or a file
    /// without a fingerprint, is never one.
    #[test]
    fn a_different_build_next_door_is_an_update_and_nothing_else_is() {
        let dir = TempDir::new("handoff-fp");
        let me = "MARSPOT_FP=abc123|2026-10-17T00:00:00Z";
        let other = binary_with(&dir, "other", "MARSPOT_FP=def456|2026-10-17T00:00:00Z|END");
        let none = dir.join("none");
        std::fs::write(&none, b"no marker in here").unwrap();
        assert!(execv_wanted(&other, me), "a different build is an update");
        assert!(!execv_wanted(&none, me), "an unmarked file is not a build");
        assert!(!execv_wanted(&dir.join("absent"), me), "nothing staged");
    }


    /// A copy of the running build is not an update.  The two markers
    /// used to be compared in different forms and never matched, so a
    /// copy counted as one and every SIGTERM re-execed.
    #[test]
    fn a_copy_of_the_running_build_is_not_an_update() {
        let dir = TempDir::new("handoff-fp");
        let same = binary_with(&dir, "same", marspot_term::MARSPOT_FP_TERM);
        assert!(!execv_wanted(&same, marspot_term::build::own()));
    }
}

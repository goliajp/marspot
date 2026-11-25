//! The rules about processes outside every pane: a `tail` following
//! dead files (O1) and a leftover temporary server (O2).

use super::check::{Hit, Key, OutsideIn, Scene, Watch, proc_key, secs, span};
use super::exclude;
use super::probe::Probe;
use super::rules::{self, Outside, Rule};

pub(super) fn hits(w: &mut Watch, s: &Scene, o: &OutsideIn, probe: &mut dyn Probe) -> Vec<Hit> {
    let l = w.limits.clone();
    let row = o.row;
    let argv = probe.argv(row);
    if exclude::comm(&row.comm, argv.as_deref()) || argv.as_deref().is_some_and(exclude::argv) {
        return Vec::new();
    }
    let mut hits = Vec::new();
    let key = |rule| Key { sid: o.sid, rule, id: proc_key(row) };

    if row.comm == "tail"
        && let (Some(argv), Some(cwd)) = (argv.as_deref(), probe.cwd(row))
        && rules::o1(argv, &cwd, |p| probe.written(p), s.now, &l)
    {
        let files = rules::tail_follows(argv).unwrap_or_default().join(" ");
        let group: Vec<String> = o.group.iter().map(|r| format!("{} {}", r.comm, proc_key(r))).collect();
        let evidence = format!(
            "被收养的 tail（pid {}），跟着的文件（{}，在 {}）{} 以上没写过或已不存在；同一进程组：{}。{}",
            proc_key(row),
            files,
            cwd.display(),
            span(l.o1_stale),
            if group.is_empty() { "无".into() } else { group.join("、") },
            o.why
        );
        hits.push(Hit { key: key(Rule::O1), evidence });
    }

    let age = s.now.saturating_sub(row.start_unix);
    if age >= l.o2_age && o.session_gone {
        let (cwd, exe) = (probe.cwd(row), probe.exe(row));
        if exe.as_deref().is_some_and(exclude::exe) {
            return hits;
        }
        let temp = |p: &Option<std::path::PathBuf>| {
            p.as_deref().is_some_and(|p| rules::temporary(p, s.uid, s.tmpdir.as_deref()))
        };
        if temp(&cwd) || temp(&exe) {
            let cpu = w.cpu_over(s, row, l.o2_window, probe);
            let view = Outside { started: row.start_unix, cwd: cwd.as_deref(), exe: exe.as_deref(), session_gone: true, cpu };
            if rules::o2(&view, s.uid, s.tmpdir.as_deref(), s.now, &l) {
                let evidence = format!(
                    "被收养的进程 {}（pid {}），已跑 {}；工作目录 {}；启动它的会话已不在任何窗格里运行；最近 {} 子树 CPU 约 {}。{}",
                    row.comm,
                    proc_key(row),
                    span(age),
                    cwd.as_deref().map_or("（读不出）".into(), |c| c.display().to_string()),
                    span(l.o2_window),
                    secs(cpu.unwrap_or(0)),
                    o.why
                );
                hits.push(Hit { key: key(Rule::O2), evidence });
            }
        }
    }
    hits
}

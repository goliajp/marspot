//! The rules about one claude pane's own work: its tasks (B1, B3) and
//! its subagents (S1, S2).
//!
//! A task is a child of the claude process whose standard output is
//! `…/tasks/<id>.output`; claude starts every command that way, in the
//! foreground or not.  Its identity is that child's `ProcId`, not the
//! leaf's: a polling script's leaf is a new `sleep` every minute.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::pidtree::ProcRow;

use super::check::{Hit, Key, PaneIn, Scene, Watch, proc_key, secs, span};
use super::exclude;
use super::probe::Probe;
use super::rules::{self, BgTask, Rule};

struct Task<'a> {
    row: &'a ProcRow,
    id: String,
    out: PathBuf,
}

/// The task id an output path names.
pub fn task_of(out: &Path) -> Option<String> {
    let name = out.file_name()?.to_str()?.strip_suffix(".output")?;
    let in_tasks = out.parent()?.file_name()? == "tasks";
    (in_tasks && !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric())).then(|| name.to_string())
}

fn tasks<'a>(s: &Scene<'a>, p: &PaneIn<'a>, probe: &mut dyn Probe) -> Vec<Task<'a>> {
    let mut out = Vec::new();
    for c in s.table.children(p.agent.pid) {
        // the agent's own job: its MCP servers run in its group
        if c.pgid == p.agent.pgid || (s.helper)(c, s.table.children(c.pid).next().is_some()) {
            continue;
        }
        let Some(path) = probe.stdout(c) else { continue };
        if let Some(id) = task_of(&path) {
            out.push(Task { row: c, id, out: path });
        }
    }
    out
}

/// No rule names the task: its root is a system, app, marspot or codex
/// process, or something in its subtree holds or waits for the
/// measurement lock or keeps the machine awake, which leaves every
/// process above it up to the agent alone.  A marspot process further
/// down is left alone itself, not the task above it.
fn excluded(s: &Scene, root: &ProcRow, probe: &mut dyn Probe) -> bool {
    if probe.exe(root).is_some_and(|e| exclude::exe(&e)) || exclude::comm(&root.comm, probe.argv(root).as_deref()) {
        return true;
    }
    std::iter::once(root).chain(s.table.descendants(root.pid)).any(|r| probe.argv(r).is_some_and(|a| exclude::argv(&a)))
}

pub(super) fn hits(w: &mut Watch, s: &Scene, p: &PaneIn, probe: &mut dyn Probe) -> Vec<Hit> {
    let tasks = tasks(s, p, probe);
    let mut hits = b1(w, s, p, &tasks, probe);
    hits.extend(b3(w, s, p, &tasks, probe));
    hits.extend(subagents(w, s, p, &tasks));
    hits
}

/// B1, with each task's command as the agent issued it, else as its
/// root runs it.
fn b1(w: &mut Watch, s: &Scene, p: &PaneIn, tasks: &[Task], probe: &mut dyn Probe) -> Vec<Hit> {
    let l = w.limits.clone();
    let mut bg = Vec::new();
    for t in tasks {
        let rec = p.log.tasks.started.get(&t.id);
        if rec.is_some_and(|r| !r.background) {
            continue;
        }
        let command = match rec.and_then(|r| r.command.clone()) {
            Some(c) => c,
            None => probe.argv(t.row).map(|a| a.join(" ")).unwrap_or_default(),
        };
        let written = probe.written(&t.out);
        let silent = written.is_some_and(|w| s.now.saturating_sub(w) >= l.b1_silent);
        let cpu = if silent { w.cpu_over(s, t.row, l.b1_window, probe) } else { None };
        bg.push((t, command, written, cpu, rec.is_some_and(|r| r.said_background())));
    }
    let views: Vec<BgTask> = bg
        .iter()
        .filter(|(_, c, ..)| !c.is_empty())
        .map(|(t, c, written, cpu, _)| BgTask { id: &t.id, command: c, started: t.row.start_unix, written: *written, cpu: *cpu })
        .collect();
    let mut hits = Vec::new();
    for (old, newer) in rules::b1(&views, s.now, &l) {
        let (t, command, written, cpu, said_bg) = bg.iter().find(|b| b.0.id == old).expect("a hit is one of the tasks");
        if excluded(s, t.row, probe) {
            continue;
        }
        let n = bg.iter().find(|b| b.0.id == newer).expect("so is its newer copy").0;
        let kind = if *said_bg { "后台任务" } else { "任务" };
        let evidence = format!(
            "{kind} {old}（pid {}），命令：{command}\n   已跑 {}；输出文件 {} 没写过；最近 {} 子树 CPU（含已退出的子进程）约 {}；同一条命令还有更新的一份在跑：{newer}（pid {}）。这只说明它很久没有产出，轮询脚本可能还在按时检查",
            proc_key(t.row),
            span(s.now.saturating_sub(t.row.start_unix)),
            span(s.now.saturating_sub(written.unwrap_or(0))),
            span(l.b1_window),
            secs(cpu.unwrap_or(0)),
            n.row.pid,
        );
        hits.push(Hit { key: Key { sid: p.sid, rule: Rule::B1, id: proc_key(t.row) }, evidence });
    }
    hits
}

fn b3(w: &Watch, s: &Scene, p: &PaneIn, tasks: &[Task], probe: &mut dyn Probe) -> Vec<Hit> {
    let mut hits = Vec::new();
    for t in tasks {
        let told = p.log.tasks.ended.get(&t.id).copied();
        if rules::b3(told, true, s.now, &w.limits) && !excluded(s, t.row, probe) {
            let bg = p.log.tasks.started.get(&t.id).is_some_and(|r| r.said_background());
            let evidence = format!(
                "{} {}（pid {}）：{} 前已通知结束，进程还在",
                if bg { "后台任务" } else { "任务" },
                t.id,
                proc_key(t.row),
                span(s.now.saturating_sub(told.unwrap_or(0)))
            );
            hits.push(Hit { key: Key { sid: p.sid, rule: Rule::B3, id: proc_key(t.row) }, evidence });
        }
    }
    hits
}

/// S1 and S2.  A subagent's task is alive while some task root of the
/// agent writes to its output.
fn subagents(w: &Watch, s: &Scene, p: &PaneIn, tasks: &[Task]) -> Vec<Hit> {
    let l = &w.limits;
    let alive: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    let mut hits = Vec::new();
    for (id, (sub, meta)) in &p.log.subs {
        let mut v = sub.view(id, meta.finished, |t| alive.contains(t));
        // a subagent it started wakes it when done, as a task does; the
        // log holds only this process's unfinished ones
        v.tasks_alive += p.log.subs.values().filter(|(_, m)| m.parent.as_deref() == Some(id.as_str())).count();
        let who = format!("子代理 {id}「{}」（深度 {}{}）", meta.description, meta.depth, parent(meta.parent.as_deref()));
        let key = |rule| Key { sid: p.sid, rule, id: format!("agent:{id}") };
        if rules::s1(&v, p.agent.start_unix, p.log.main.turn_ended, s.now, l) {
            let evidence = format!(
                "{who}\n   最后一条记录在 {} 前：一轮已结束，没有未完成的工具调用；它启动的后台任务都已经没有进程。主会话这一轮也已结束。如果它确实停住了，用 TaskStop {id} 停它",
                span(s.now.saturating_sub(v.last_at))
            );
            hits.push(Hit { key: key(Rule::S1), evidence });
        }
        if let Some(waited) = rules::s2(&v, s.now, l) {
            let evidence = format!("{who}\n   一条前台 Bash 调用已经等了 {}，超过它自己的时限", span(waited));
            hits.push(Hit { key: key(Rule::S2), evidence });
        }
    }
    hits
}

fn parent(p: Option<&str>) -> String {
    p.map_or(String::new(), |p| format!("，上级 {p}"))
}

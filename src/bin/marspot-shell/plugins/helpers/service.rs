//! `helper.start`, `helper.list`, `helper.stop` on the control socket's
//! threads.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use marspot_peer::control::{AgentKind, ErrorCode, HelperStartArgs, HelperStop, Isolation, Reject, sha256};
use marspot_peer::json::{Value, obj};

use super::table::{Event, Helper, Started};
use super::{Shared, caller_kind, container, launch, records, settle, stop_all};
use crate::pane_control::helpers_port::{HelperService, Port};

/// How long a new pane's shell may take to show its prompt.
const PROMPT_WITHIN: Duration = Duration::from_secs(10);

pub(super) struct Service(pub Arc<Shared>);

fn internal(e: impl std::fmt::Display) -> Reject {
    Reject::new(ErrorCode::Internal, e.to_string())
}

/// Agent sessions already in `cwd`, by full name, but `skip`.
fn others_in(cwd: &str, skip: &[String]) -> Vec<Value> {
    let entries = marspot_peer::peers::live_entries();
    let panes = marspot_peer::panes::live(&entries);
    let mut out: Vec<String> = Vec::new();
    for e in entries.iter().filter(|e| e.cwd.as_deref() == Some(cwd)) {
        let name = match marspot_peer::panes::of_pid(&panes, e.pid) {
            Some(p) => p.base.as_ref().zip(p.code).map_or_else(|| p.name.clone(), |(b, c)| format!("{b}#{c}")),
            None => e.name.clone().unwrap_or_default(),
        };
        if !name.is_empty() && !skip.contains(&name) && !out.contains(&name) {
            out.push(name);
        }
    }
    out.into_iter().map(Value::from).collect()
}

/// How long pane `sid` has printed nothing, by when its byte log was
/// last written; None when it has none.
pub(super) fn quiet_ms(sid: u64, now_ms: u64) -> Option<u64> {
    let dir = std::fs::read_dir(marspot_term::session_registry::session_dir(sid)).ok()?;
    let logs = dir.flatten().filter(|e| e.file_name().to_string_lossy().starts_with("bytelog"));
    // the segments of one log: the one in use was written last
    let at = logs.filter_map(|e| e.metadata().ok()?.modified().ok()).max()?;
    Some(now_ms.saturating_sub(at.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as u64))
}

/// What `helper.list` says `h` is doing.
pub(super) fn state_of(h: &Helper) -> &'static str {
    if h.preparing {
        "preparing"
    } else if h.delivered_ms.is_none() {
        "starting"
    } else if h.noticed {
        "stopped"
    } else {
        "running"
    }
}

fn first_line(task: &str) -> String {
    task.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().chars().take(120).collect()
}

impl Service {
    /// Write the task and the script; the result file's path.
    fn write_files(&self, sid: u64, a: &HelperStartArgs, (parent, caller): (&str, &str), cwd: &str, account: &launch::Account, mode: &str, resume: bool) -> Result<(), Reject> {
        let dir = self.0.dir_of(sid);
        std::fs::create_dir_all(&dir).map_err(internal)?;
        let (task_file, result) = (dir.join("task.md"), dir.join("result.md"));
        let mut flags = launch::mode_flags(a.agent, mode, &dir)?;
        flags.extend(launch::tool_flags(a.agent, mode, &account.home));
        let script = launch::run_script(a.agent, account, a.model.as_deref(), &flags, resume, &task_file)?;
        std::fs::write(&task_file, launch::task_text(parent, cwd, a.budget_minutes, &result, &a.task, launch::by_result(caller, a.agent, mode))).map_err(internal)?;
        std::fs::write(dir.join("run.sh"), script).map_err(internal)
    }

    /// Run the script in the pane.
    fn begin(&self, port: &mut Port, sid: u64, agent: AgentKind) -> Result<(), Reject> {
        let pos = port.type_at_prompt(sid, &launch::start_line(&self.0.dir_of(sid)), PROMPT_WITHIN)?;
        if agent != AgentKind::Kimi {
            self.0.record(sid, Event::TaskDelivered);
        }
        settle::settle(Arc::clone(&self.0), sid, agent, pos);
        Ok(())
    }

    /// Undo a start that failed after its pane opened.
    fn abandon(&self, port: &mut Port, sid: u64, r: Reject) -> Reject {
        let _ = port.close(sid);
        self.0.record(sid, Event::Stopped { exited: false });
        self.0.sweep_dirs();
        r
    }
}

impl HelperService for Service {
    fn start(&self, port: &mut Port, a: &HelperStartArgs) -> Result<Value, Reject> {
        let parent = match &a.parent {
            Some(p) => port.target(p)?,
            None => port.caller().ok_or_else(|| Reject::new(ErrorCode::BadRequest, "from outside the panes, name the helper's `parent` pane"))?,
        };
        let cwd = a.cwd.clone().or_else(|| port.cwd_of(parent)).ok_or_else(|| Reject::new(ErrorCode::BadRequest, "no `cwd`, and the parent pane's is not known"))?;
        if !Path::new(&cwd).is_dir() {
            return Err(Reject::new(ErrorCode::BadRequest, format!("{cwd} is not a directory")));
        }
        let mode = launch::mode_of(a.mode.as_deref());
        let boxed = (a.isolation == Isolation::Container).then(|| container::check(a, &cwd)).transpose()?;
        let account = match &boxed {
            Some(b) => b.account.clone(),
            None => launch::pick_account(a.agent, a.profile, a.model.as_deref())?,
        };
        let mut note = None;
        let resume = a.resume_last
            && match launch::has_last(a.agent, &account.home, &cwd) {
                Some(false) => {
                    note = Some(format!("no {} conversation in {cwd} to go on with; this is a new one", a.agent.as_str()));
                    false
                }
                _ => true,
            };
        let parent_name = port.full_name(parent);
        let caller = caller_kind::of(port);
        let (sid, name) = port.open_shadow(parent, &cwd)?;
        let started = Started {
            parent,
            caller: caller.to_string(),
            agent: a.agent,
            account: account.label(),
            model: a.model.clone().unwrap_or_else(|| "default".into()),
            mode: mode.clone(),
            isolation: a.isolation,
            budget_min: a.budget_minutes,
            task_line: first_line(&a.task),
            task_sha: sha256::digest(a.task.as_bytes()).iter().map(|b| format!("{b:02x}")).collect(),
            label: a.name.clone().unwrap_or_default(),
            conv: crate::plugins::peerbus::conversation_of(parent).map(|c| c.id),
            root: Some(self.0.root_over(parent)),
        };
        // written before anything runs in the pane: a shell started over
        // between the two would otherwise find an agent nobody accounts for
        self.0.record(sid, Event::Started(started));
        let written = match &boxed {
            Some(b) => container::begin(&self.0, sid, a, &parent_name, &mode, b),
            None => self.write_files(sid, a, (&parent_name, caller), &cwd, &account, &mode, resume),
        };
        if let Err(r) = written {
            return Err(self.abandon(port, sid, r));
        }
        let preparing = boxed.as_ref().is_some_and(|b| !b.image_there);
        if preparing {
            self.0.record(sid, Event::Preparing);
            container::prepare(&self.0, sid, a.agent);
        } else if let Err(r) = self.begin(port, sid, a.agent) {
            return Err(self.abandon(port, sid, r));
        }
        self.0.watch_result(sid);
        port.audit("helper.start", sid, &format!("{} {} mode={mode} {}", a.agent.as_str(), account.label(), first_line(&a.task)), "started");
        let mut v = obj([
            ("name", name.clone().into()),
            ("id", (sid as i64).into()),
            ("state", if preparing { "preparing" } else { "running" }.into()),
            ("agent", a.agent.as_str().into()),
            ("account", account.label().into()),
            ("model", a.model.clone().unwrap_or_else(|| "default".into()).into()),
            ("mode", mode.into()),
            ("isolation", a.isolation.as_str().into()),
            ("result_path", self.0.result_path(sid).display().to_string().into()),
            ("others_in_cwd", Value::Arr(others_in(&cwd, &[parent_name, name]))),
        ]);
        if let Some(n) = note {
            v.set("note", n.into());
        }
        Ok(v)
    }

    fn list(&self, caller: Option<u64>, all: bool) -> Value {
        let t = self.0.table();
        let now = records::now_ms();
        // a pane opened a moment ago is named only once observed
        let obs = crate::observe::shared().fresh(crate::observe::INTERVAL);
        let rows = t
            .map
            .values()
            .filter(|h| all || Some(h.started.parent) == caller)
            .map(|h| {
                let secs = |ms: u64| Value::from((ms / 1000) as i64);
                let state = state_of(h);
                obj([
                    ("name", crate::pane_control::names::full_name_or_entry(h.sid, &obs).into()),
                    ("id", (h.sid as i64).into()),
                    ("parent", crate::pane_control::names::full_name_or_entry(h.started.parent, &obs).into()),
                    ("agent", h.started.agent.as_str().into()),
                    ("model", h.started.model.as_str().into()),
                    ("account", h.account.as_str().into()),
                    ("isolation", h.started.isolation.as_str().into()),
                    ("state", state.into()),
                    ("task", h.started.task_line.as_str().into()),
                    ("label", h.started.label.as_str().into()),
                    ("running_s", secs(now.saturating_sub(h.started_ms))),
                    ("budget_left_s", h.deadline_ms().map_or(Value::Null, |d| secs(d.saturating_sub(now)))),
                    ("quiet_s", quiet_ms(h.sid, now).map_or(Value::Null, secs)),
                    ("result_path", self.0.result_path(h.sid).display().to_string().into()),
                ])
            })
            .collect();
        obj([("helpers", Value::Arr(rows))])
    }

    fn stop(&self, port: &mut Port, which: &HelperStop) -> Result<Value, Reject> {
        let t = self.0.table();
        let sids: Vec<u64> = match which {
            HelperStop::All => {
                let me = port.caller().ok_or_else(|| Reject::new(ErrorCode::BadRequest, "from outside the panes, name the helper to stop"))?;
                t.under(me).map(|h| h.sid).collect()
            }
            HelperStop::Name(n) => {
                let sid = port.target(n)?;
                if !t.map.contains_key(&sid) {
                    return Err(Reject::new(ErrorCode::NoSuchPane, format!("{n} is not a helper")));
                }
                vec![sid]
            }
        };
        if let HelperStop::All = which {
            let me = port.caller().unwrap_or_default();
            let named = sids.into_iter().map(|sid| (sid, port.full_name(sid))).collect();
            return Ok(stop_all::stop_all(&self.0, me, named));
        }
        let mut stopped = Vec::new();
        for sid in sids {
            let name = port.full_name(sid);
            stop_all::stop_one(&self.0, sid, || port.close(sid))?;
            stopped.push(Value::from(name));
        }
        Ok(obj([("stopped", Value::Arr(stopped))]))
    }

    fn stop_sid(&self, port: &mut Port, sid: u64) -> Option<Result<(), Reject>> {
        let is_helper = self.0.table().map.contains_key(&sid);
        is_helper.then(|| stop_all::stop_one(&self.0, sid, || port.close(sid)))
    }

    fn input_seen(&self, sid: u64) {
        self.0.input_seen(sid);
    }
}

#[cfg(test)]
mod tests {
    use marspot::shell_proto::RetireMode;
    use marspot_peer::control::AgentKind;

    use super::super::tests::{feed, shared_at, started, temp};
    use super::*;
    use crate::pane_control::helpers_port;
    use crate::pane_control::tests::panel_act_tests::close_two;

    /// Helper 2, closed on the panel, is stopped: written down, its
    /// directory gone, its pane deleted and not recycled.
    #[test]
    fn helper_panel_close_of_a_helper_stops_it() {
        let _sandbox = marspot_term::test_env::sandbox_state_dir();
        let dir = temp("panel-stop");
        let (s, _rx) = shared_at(&dir);
        feed(&s, 2, vec![Event::Started(started(1, "claude", AgentKind::Codex)), Event::TaskDelivered]);
        std::fs::create_dir_all(s.dir_of(2)).unwrap();
        helpers_port::register(Box::new(Service(Arc::clone(&s))));
        let (closed, counts) = close_two();
        assert_eq!(closed, [(2, RetireMode::Delete)]);
        assert_eq!(counts, (1, 0));
        assert!(!s.table().map.contains_key(&2) && !s.dir_of(2).exists());
        let log = std::fs::read_to_string(dir.join("events.log")).unwrap();
        assert!(log.lines().last().is_some_and(|l| l.contains("stopped")), "{log}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

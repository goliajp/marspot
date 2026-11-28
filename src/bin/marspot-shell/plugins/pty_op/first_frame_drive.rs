//! A whole resume script run on the fake pane, for the plugins whose
//! scripts end in a first-frame wait: the shell takes the line, the
//! program comes up, and its first frame either keeps drawing, paints
//! and stops, or never reaches the floor.

use super::*;

/// What the program draws once it is up.
#[derive(Clone, Copy, PartialEq)]
pub(in crate::plugins) enum Frame {
    /// Repaints at 400 bytes a frame past every deadline.
    Late,
    /// Paints 40 KB and goes still.
    Stops,
    /// 600 bytes of startup noise, then nothing.
    Short,
}

pub(in crate::plugins) struct Ran {
    pub outcome: Option<OpOutcome>,
    pub logs: Vec<String>,
}

impl Ran {
    /// The first log line under `tag`, or an empty string.
    pub fn line(&self, tag: &str) -> String {
        self.logs.iter().find(|l| l.starts_with(&format!("{tag} "))).cloned().unwrap_or_default()
    }
}

/// Run `op` to its end: a parked run is woken, the shell's prompt comes
/// back after the interrupt, and once the line named `resume` is sent
/// `program` runs under the shell and draws `frame` during the step
/// named `first_frame`.
pub(in crate::plugins) fn run_resume(op: PtyOp, program: i32, frame: Frame) -> Ran {
    let resume = op.index_of("resume").expect("the script types a resume line");
    let (state, env, host) = setup();
    let mut r = OpRunner::new(op, env);
    let mut drawn = 0u64;
    for _ in 0..(120_000 / 16) {
        if *host.ended.lock().unwrap() {
            break;
        }
        r.wake(&host, "focus");
        if r.at > resume && !state.present.lock().unwrap().contains(&program) {
            state.present.lock().unwrap().push(program);
            state.alive.lock().unwrap().push(program);
        }
        let grow = match (r.op.steps.get(r.at).map_or("", |s| s.label), frame) {
            ("fresh_prompt", _) if drawn == 0 => 40,
            ("first_frame", Frame::Late) => 400,
            ("first_frame", Frame::Stops) if drawn < 40_000 => 400,
            ("first_frame", Frame::Short) if drawn < 600 => 600,
            _ => 0,
        };
        drawn += grow;
        *state.out_len.lock().unwrap() += grow;
        r.on_tick(&host);
        advance(&state, 16);
    }
    let logs = host.logs.lock().unwrap().clone();
    Ran { outcome: r.finished.clone(), logs }
}

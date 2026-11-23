//! The stdio loop of `marspot-peer mcp`.
//!
//! A pane wait can last as long as the client lets a tool call take, so
//! every `tools/call` runs on a thread of its own and the loop goes on
//! reading: other calls, pings and cancellations are answered meanwhile.
//! `initialize`, `tools/list` and `ping` are answered on the loop.
//! Answers go out whole, one line each, through one lock.
//!
//! A request the client cancels (`notifications/cancelled`) gets no
//! answer, and the control connection its wait holds is closed, which
//! ends that wait in the app and frees its slot there.  When stdin
//! closes, waits still in flight are cancelled the same way; every other
//! call is answered before the server returns.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::{Arc, Condvar, Mutex};

use crate::control::client::Cancel;
use crate::json::Value;
use crate::mcp::{Server, Step, error, now_ms};

/// Tool calls carried out at once; one more is refused.
pub const MAX_IN_FLIGHT: usize = 16;

struct Out(Mutex<Box<dyn Write + Send>>);

impl Out {
    fn send(&self, v: &Value) -> bool {
        let mut w = self.0.lock().unwrap();
        writeln!(w, "{}", v.to_json()).and_then(|()| w.flush()).is_ok()
    }
}

struct Call {
    cancel: Arc<Cancel>,
    /// A PaneRun, or a PaneRead with a wait: it can last minutes.
    waits: bool,
}

/// Calls in flight, by the JSON text of their request id, and how many
/// threads have yet to end.
#[derive(Default)]
struct InFlight {
    calls: Mutex<HashMap<String, Call>>,
    threads: Mutex<usize>,
    ended: Condvar,
}

impl InFlight {
    /// A new entry for `key`; Err with the reason it cannot be taken.
    fn start(&self, key: &str, waits: bool) -> Result<Arc<Cancel>, &'static str> {
        let mut m = self.calls.lock().unwrap();
        if m.contains_key(key) {
            return Err("a request with this id is still in flight");
        }
        if m.len() >= MAX_IN_FLIGHT {
            return Err("too many tool calls in flight; wait for one to finish");
        }
        let cancel = Arc::new(Cancel::default());
        m.insert(key.to_string(), Call { cancel: cancel.clone(), waits });
        *self.threads.lock().unwrap() += 1;
        Ok(cancel)
    }

    /// Done with `key`: true when its answer is still wanted.
    fn finish(&self, key: &str, mine: &Arc<Cancel>) -> bool {
        let mut m = self.calls.lock().unwrap();
        match m.get(key) {
            Some(c) if Arc::ptr_eq(&c.cancel, mine) => {
                m.remove(key);
                true
            }
            // cancelled: the entry was taken out when the cancel came
            _ => false,
        }
    }

    fn ended(&self) {
        *self.threads.lock().unwrap() -= 1;
        self.ended.notify_all();
    }

    fn cancel(&self, key: &str) {
        if let Some(c) = self.calls.lock().unwrap().remove(key) {
            c.cancel.cancel();
        }
    }

    /// Cancel the waits, then let every other call end.
    fn close(&self) {
        self.calls.lock().unwrap().retain(|_, c| {
            if c.waits {
                c.cancel.cancel();
            }
            !c.waits
        });
        let mut n = self.threads.lock().unwrap();
        while *n > 0 {
            n = self.ended.wait(n).unwrap();
        }
    }
}

fn waits(params: Option<&Value>) -> bool {
    let has_wait = params.and_then(|p| p.at(&["arguments", "wait"])).is_some_and(|w| !matches!(w, Value::Null));
    match params.and_then(|p| p.str_at("name")) {
        Some("PaneRun") => true,
        Some("PaneRead") => has_wait,
        _ => false,
    }
}

/// Serve until stdin closes.
pub fn serve(mut server: Server, input: impl BufRead, output: impl Write + Send + 'static) {
    let out = Arc::new(Out(Mutex::new(Box::new(output))));
    let calls = Arc::new(InFlight::default());
    for line in input.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        match server.step(&line) {
            Step::Answer(v) => {
                if !out.send(&v) {
                    break;
                }
            }
            Step::Call(id, params) => {
                let key = id.to_json();
                let cancel = match calls.start(&key, waits(params.as_ref())) {
                    Ok(c) => c,
                    Err(why) => {
                        out.send(&error(id, -32000, why));
                        continue;
                    }
                };
                let (server, out, calls) = (server.clone(), out.clone(), calls.clone());
                std::thread::spawn(move || {
                    let v = server.answer_call(id, params.as_ref(), now_ms(), &cancel);
                    if calls.finish(&key, &cancel) {
                        out.send(&v);
                    }
                    calls.ended();
                });
            }
            Step::Cancelled(id) => calls.cancel(&id.to_json()),
            Step::Nothing => {}
        }
    }
    calls.close();
}

#[cfg(test)]
#[path = "mcp_serve_tests.rs"]
mod mcp_tests;

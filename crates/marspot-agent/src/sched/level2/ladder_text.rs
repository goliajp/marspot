//! The ladder as text: one line for the log, and `key=value` lines to
//! survive a restart.

use super::{DRY_NOTICES, DRY_RECORDS, Ladder, ROUTE_RECORDS, Step, Why};

fn rolled_word(r: Option<(Step, Why)>) -> String {
    r.map_or("-".to_string(), |(s, w)| format!("{}:{}", s.word(), w.word()))
}

impl Ladder {
    /// A ladder kept by a build that entered steps above `max` is held
    /// at `max`; true when it was.
    pub fn cap(&mut self, max: Step) -> bool {
        let over = self.step > max;
        if over {
            self.step = max;
        }
        over
    }

    /// The step and every count, one line.
    pub fn words(&self) -> String {
        format!(
            "step={} since={} rolled={} route={}/{} bad={} placed_refused={} dry={}/{} notices={}/{} mismatch={} done={} refusals_before={} attended_wait={}",
            self.step.word(),
            self.since,
            rolled_word(self.rolled),
            self.route_run,
            ROUTE_RECORDS,
            self.route_bad,
            self.placed_refused.len(),
            self.dry,
            DRY_RECORDS,
            self.dry_notices,
            DRY_NOTICES,
            self.dry_mismatch,
            self.done,
            self.refusals_before,
            u8::from(self.attended_wait),
        )
    }

    /// What is kept across a restart, one `key=value` a line.
    pub fn encode(&self) -> String {
        let refused: Vec<String> = self.placed_refused.iter().map(i64::to_string).collect();
        format!(
            "step={}\nsince={}\na_at={}\nrolled={}\nroute_run={}\nroute_bad={}\nplaced_refused={}\ndry={}\ndry_notices={}\ndry_mismatch={}\ndone={}\nrefusals_before={}\nattended_wait={}\nreset_at={}\n",
            self.step.word(),
            self.since,
            self.a_at.map_or("-".to_string(), |a| a.to_string()),
            rolled_word(self.rolled),
            self.route_run,
            self.route_bad,
            if refused.is_empty() { "-".into() } else { refused.join(",") },
            self.dry,
            self.dry_notices,
            self.dry_mismatch,
            self.done,
            self.refusals_before,
            u8::from(self.attended_wait),
            self.reset_at,
        )
    }

    /// `encode`'s text back; `None` when any line is missing or wrong.
    /// `reset_at` came later: a file without it was never reset.
    pub fn decode(text: &str) -> Option<Ladder> {
        let get = |k: &str| text.lines().find_map(|l| l.strip_prefix(k)?.strip_prefix('='));
        let num = |k: &str| get(k)?.parse::<u32>().ok();
        let rolled = match get("rolled")? {
            "-" => None,
            w => {
                let (s, why) = w.split_once(':')?;
                Some((Step::of(s)?, Why::of(why)?))
            }
        };
        let refused = match get("placed_refused")? {
            "-" => Vec::new(),
            v => v.split(',').map(|x| x.parse().ok()).collect::<Option<Vec<i64>>>()?,
        };
        Some(Ladder {
            step: Step::of(get("step")?)?,
            since: get("since")?.parse().ok()?,
            a_at: match get("a_at")? {
                "-" => None,
                v => Some(v.parse().ok()?),
            },
            rolled,
            route_run: num("route_run")?,
            route_bad: num("route_bad")?,
            placed_refused: refused,
            dry: num("dry")?,
            dry_notices: num("dry_notices")?,
            dry_mismatch: num("dry_mismatch")?,
            done: num("done")?,
            refusals_before: num("refusals_before")?,
            attended_wait: get("attended_wait")? == "1",
            reset_at: match get("reset_at") {
                None => 0,
                Some(v) => v.parse().ok()?,
            },
        })
    }
}

//! [`StepKind::AwaitQuiet`]: when a pane has finished drawing, and
//! what is said when it never does.

use std::time::Duration;

use super::{LogLevel, OpOutcome, OpRunner, PaneSessionHost, Step, StepKind};

/// How often an `or_composer` wait samples the output counter, and at
/// most how often it looks at the pane's frame.
const SAMPLE_EVERY: Duration = Duration::from_millis(500);

/// Output an agent's spinner may produce and still count as still.
/// Measured on a working claude: 1.1-1.9 KB per 1.5 s, with one 5 KB
/// burst; a resumed conversation repainting is tens of KB at a time.
pub(super) const TRICKLE_BYTES_PER_SEC: u64 = 4 * 1024;

/// Samples an `or_composer` wait keeps: enough to span its window.
const SAMPLES_KEPT: usize = 8;

/// Is an agent's input box drawn on this screen?
///
/// Claude Code rules the box off above its prompt line, which is what
/// tells it from the messages already sent: those carry the same glyph
/// but no rule.  Kimi's prompt is a plain `>` under a box's top edge.
pub(super) fn composer_on_screen(screen: &str) -> bool {
    let bare = |l: &str| l.trim().trim_matches(['│', '┃', '|']).trim().to_string();
    let lines: Vec<String> = screen.lines().map(bare).collect();
    lines.windows(2).any(|w| {
        !w[0].is_empty()
            && w[0].chars().all(|c| matches!(c, '─' | '━' | '╭' | '╮'))
            && (w[1].starts_with(['\u{276f}', '\u{203a}']) || (w[0].starts_with('╭') && w[1].starts_with('>')))
    })
}

/// Steps that write to the pane. Once a first frame has run out on a
/// program that is still drawing, these are the ones left out: text
/// typed into a program mid-redraw can land as half a line.
fn types_into_pane(kind: &StepKind) -> bool {
    matches!(
        kind,
        StepKind::Send(_)
            | StepKind::Paste(_)
            | StepKind::PasteJob(_)
            | StepKind::RestoreComposer(_)
            | StepKind::Submit { .. }
    )
}

impl Step {
    /// A deadline passing on a program that has drawn at least the
    /// floor is not a failure: the program is up and still drawing.
    /// The run goes on without the steps that type into the pane and
    /// ends [`OpOutcome::DrawnLate`].
    pub fn or_drawn_late(mut self) -> Self {
        if let StepKind::AwaitQuiet { or_drawn_late, .. } = &mut self.kind {
            *or_drawn_late = true;
        }
        self
    }
}

/// What the two ways an `AwaitQuiet` can finish saw when it gave up.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Missed {
    pub drawn: u64,
    pub floor: u64,
    pub quiet: Duration,
    pub still: Duration,
    /// Output over the last `still`, once the samples reach that far.
    pub grew: Option<u64>,
    /// `None`: the screen could not be read.
    pub boxed: Option<bool>,
    pub or_composer: bool,
}

impl Missed {
    fn allowance(&self) -> u64 {
        TRICKLE_BYTES_PER_SEC * self.still.as_millis() as u64 / 1000
    }

    /// How far each condition got, from 0 to 1, and the nearer one.
    fn nearest(&self) -> (&'static str, String) {
        let ratio = |a: f64, b: f64| if b <= 0.0 { 1.0 } else { (a / b).min(1.0) };
        let quiet = if self.drawn < self.floor {
            0.5 * ratio(self.drawn as f64, self.floor as f64)
        } else {
            0.5 + 0.5 * ratio(self.quiet.as_millis() as f64, self.still.as_millis() as f64)
        };
        let quiet_why = if self.drawn < self.floor {
            format!("{}B short of the floor", self.floor - self.drawn)
        } else {
            format!("{}ms short of still", self.still.saturating_sub(self.quiet).as_millis())
        };
        if !self.or_composer {
            return ("quiet", quiet_why);
        }
        let allowance = self.allowance();
        let trickle = match self.grew {
            Some(g) if g <= allowance => 1.0,
            Some(g) => ratio(allowance as f64, g as f64),
            None => 0.0,
        };
        let boxed = self.boxed == Some(true);
        let composer = 0.5 * trickle + if boxed { 0.5 } else { 0.0 };
        if composer <= quiet {
            return ("quiet", quiet_why);
        }
        let mut why = Vec::new();
        match self.grew {
            Some(g) if g > allowance => why.push(format!("{}B over the trickle allowance", g - allowance)),
            None => why.push("no full window sampled".to_string()),
            _ => {}
        }
        if !boxed {
            why.push("no input box on screen".to_string());
        }
        ("composer", why.join(", "))
    }
}

impl std::fmt::Display for Missed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let grew = self.grew.map_or("?".to_string(), |g| format!("{g}B"));
        let boxed = match self.boxed {
            Some(true) => "yes",
            Some(false) => "no",
            None => "unread",
        };
        let (nearest, why) = self.nearest();
        write!(
            f,
            "drew={}B floor={}B quiet={}ms/{}ms grew={grew}/{}B over the last {}ms input_box={boxed} \
             or_composer={} closest={nearest} ({why})",
            self.drawn,
            self.floor,
            self.quiet.as_millis(),
            self.still.as_millis(),
            self.allowance(),
            self.still.as_millis(),
            self.or_composer,
        )
    }
}

impl OpRunner {
    /// `(len, drawn, painted, quiet)` for the step running now.
    fn quiet_inputs(&mut self, sid: u64) -> (u64, u64, bool, Duration) {
        let len = self.env.output_len(sid);
        if len != self.seen_len {
            self.seen_len = len;
            self.still_since = self.env.now();
            self.drew = true;
        }
        let quiet = self.env.now().duration_since(self.still_since).unwrap_or_default();
        // Counted from the line that started the program, not from
        // this step: a program quick enough to paint and go quiet
        // before the step opened has drawn nothing by this step's own
        // reckoning, and the floor is then unreachable. The quiet itself
        // is still measured from the last byte, which is the thing it is
        // about.
        let from = self.sent_len.unwrap_or(self.entered_len).min(self.entered_len);
        let drawn = self.seen_len.saturating_sub(from);
        // `drew` is this step having seen the stream move. A program
        // that was already finished moves it no more, so what counts is
        // that the line produced bytes at all -- and the floor is what
        // says how many.
        (len, drawn, self.drew || drawn > 0, quiet)
    }

    /// One tick of an `AwaitQuiet`; `true` = the pane has finished drawing.
    pub(super) fn await_quiet(
        &mut self,
        host: &dyn PaneSessionHost,
        label: &'static str,
        still: Duration,
        min_bytes: u64,
        or_composer: bool,
    ) -> bool {
        let sid = host.shelld_session_id();
        let (len, drawn, painted, quiet) = self.quiet_inputs(sid);
        let done = painted && drawn >= min_bytes && quiet >= still;
        if !done && or_composer && painted && self.sample(len) {
            let allowance = TRICKLE_BYTES_PER_SEC * still.as_millis() as u64 / 1000;
            let trickle = self.grown_over(still, len).filter(|&g| g <= allowance);
            if let Some(grew) = trickle
                && self.env.pane_screen(sid).is_some_and(|s| composer_on_screen(&s))
            {
                let msg = format!(
                    "sid={sid} step={label} drew={drawn}B floor={min_bytes}B, \
                     {grew}B in the last {}ms with the input box up",
                    still.as_millis()
                );
                host.log(LogLevel::Info, &format!("{}.composer", self.op.name), &msg);
                return true;
            }
        }
        if done {
            // How much a resuming CLI actually paints before it stops,
            // logged so the floor can be set from this machine's traces.
            let msg = format!("sid={sid} step={label} drew={drawn}B floor={min_bytes}B");
            host.log(LogLevel::Info, &format!("{}.quiet", self.op.name), &msg);
        }
        done
    }

    /// Handle an `AwaitQuiet` whose deadline has passed; `true` = handled.
    ///
    /// Either way the line that ends the step says what both of its
    /// conditions saw, so a wait that runs out names its cause.
    pub(super) fn quiet_ran_out(&mut self, host: &dyn PaneSessionHost, elapsed: Duration) -> bool {
        let Some(step) = self.op.steps.get(self.at) else { return false };
        let StepKind::AwaitQuiet { still, min_bytes, or_composer, or_drawn_late } = step.kind else {
            return false;
        };
        if step.proceed_on_timeout || step.timeout.is_none_or(|l| elapsed < l) {
            return false;
        }
        let label = step.label;
        let sid = host.shelld_session_id();
        let (len, drawn, painted, quiet) = self.quiet_inputs(sid);
        let missed = Missed {
            drawn,
            floor: min_bytes,
            quiet,
            still,
            grew: self.grown_over(still, len),
            boxed: self.env.pane_screen(sid).map(|s| composer_on_screen(&s)),
            or_composer,
        };
        if or_drawn_late && painted && drawn >= min_bytes {
            let msg = format!(
                "sid={sid} step={label} waited {}ms: {missed}; the program is up and still drawing, \
                 going on without typing into it",
                elapsed.as_millis()
            );
            host.log(LogLevel::Info, &format!("{}.drawn_late", self.op.name), &msg);
            self.late = Some((self.at, label));
            self.why = Some(missed.to_string());
            self.advance(host);
        } else {
            self.why = Some(missed.to_string());
            self.finish(host, OpOutcome::TimedOut { step: self.at, label });
        }
        true
    }

    /// Leave out a step that types, once the first frame came late;
    /// `true` = left out.
    pub(super) fn skip_when_late(&mut self, host: &dyn PaneSessionHost, step: &Step) -> bool {
        if self.late.is_none() || !types_into_pane(&step.kind) {
            return false;
        }
        let held = match &step.kind {
            StepKind::RestoreComposer(job) => job.text().map_or(0, |t| t.chars().count()),
            _ => 0,
        };
        let msg = format!(
            "pane {} step={}: the first frame came late, not typing into a program still drawing \
             (unsent text not put back: {held} chars)",
            host.shelld_session_id(),
            step.label
        );
        host.log(LogLevel::Info, &format!("{}.skipped", self.op.name), &msg);
        self.skipped.push(step.label);
        true
    }

    /// Take a sample if one is due, and say whether one was taken.
    fn sample(&mut self, len: u64) -> bool {
        let now = self.env.now();
        let due = self
            .samples
            .back()
            .is_none_or(|(t, _)| now.duration_since(*t).unwrap_or_default() >= SAMPLE_EVERY);
        if due {
            if self.samples.len() == SAMPLES_KEPT {
                self.samples.pop_front();
            }
            self.samples.push_back((now, len));
        }
        due
    }

    /// Output over the last `window`, once the samples reach that far.
    fn grown_over(&self, window: Duration, len: u64) -> Option<u64> {
        let now = self.env.now();
        self.samples
            .iter()
            .rev()
            .find(|(t, _)| now.duration_since(*t).unwrap_or_default() >= window)
            .map(|(_, l)| len.saturating_sub(*l))
    }
}

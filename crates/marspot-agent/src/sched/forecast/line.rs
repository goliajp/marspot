//! The forecast as one log line, speeds in points an hour.

use std::fmt::Write;

use super::{Band, BandFrom, Forecast, Source};

const CLASS: [&str; 2] = ["fable", "other"];

fn pts(w: f64) -> String {
    format!("{:.2}", w * 100.0)
}

fn opt(v: Option<f64>) -> String {
    v.map_or_else(|| "-".into(), pts)
}

fn band(b: &Band) -> String {
    let from = match b.from {
        BandFrom::Default => "default",
        BandFrom::Measured => "measured",
    };
    let widened = if b.widened { ",widened" } else { "" };
    format!("{:.2}-{:.2}({from}{widened},{}/{})", b.lo, b.hi, b.covered, b.pairs)
}

fn source(s: Source) -> &'static str {
    match s {
        Source::Pane => "pane",
        Source::Fleet => "fleet",
        Source::Nothing => "none",
    }
}

impl Forecast {
    /// `sched.forecast`: per class the fleet level, band, `A_hi` and
    /// time-of-day test, the pane band; the fleet's clean-rest rate and
    /// background length; helper lifetimes; each live pane's day levels.
    pub fn line(&self) -> String {
        let mut s = String::new();
        for (c, name) in CLASS.iter().enumerate() {
            let f = &self.fleet[c];
            let t = &f.time_of_day;
            let tod = if t.on { "on" } else { "off" };
            let _ = write!(
                s,
                "{name}=level:{} band:{} a_hi:{} tod:{tod}({:.2}/{:.2},n{}) pane_band:{} ",
                opt(f.level),
                band(&f.band),
                opt(f.a_hi),
                t.mae_profile * 100.0,
                t.mae_day * 100.0,
                t.origins,
                band(&self.pane_band[c]),
            );
        }
        let clean = self.clean_per_h_fleet.map_or_else(|| "-".into(), |r| format!("{r:.2}"));
        let bg = self.background_s_fleet.map_or_else(|| "-".into(), |b| b.to_string());
        let _ = write!(s, "clean_per_h={clean} background_s={bg} helpers=");
        let lives: Vec<String> = self.helpers.iter().map(|h| format!("{}:{}s/{}", h.agent, h.p75_s, h.finished)).collect();
        s.push_str(if lives.is_empty() { "-" } else { "" });
        s.push_str(&lives.join(","));
        let counted = self.panes.iter().filter(|p| p.counts).count();
        let _ = write!(s, " panes={}/{counted}", self.panes.len());
        for p in self.panes.iter().filter(|p| p.counts) {
            let _ = write!(s, " {}={}/{}:{}", p.sid, pts(p.day[0]), pts(p.day[1]), source(p.clean_from));
        }
        s
    }
}

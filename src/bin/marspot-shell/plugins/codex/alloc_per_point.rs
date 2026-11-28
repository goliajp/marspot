//! How many credits one point of a codex week is worth, measured: per
//! account and week, what the replies cost at list price per point of
//! the week before it was used up, and per credit after.
//!
//! `per_point = (credits / cost after) / (points / cost before)`.  A
//! week counts only when one model made nearly all the cost on both
//! sides (a shared week cannot be split between models running side by
//! side), at least five points were measured before and a hundred
//! credits after.  Each ended week is one line of `per_point.tsv`, kept
//! to the last `KEPT` lines; the default stands until three weeks of one
//! model measured.  The open week of each account is kept in
//! `wallet_periods.tsv`, written at most every five minutes, so a
//! restart loses at most that much of it.  Nothing decides from it yet.

use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use super::alloc_wallet::Seen;

/// Credits a point is taken to be worth until measured: one pane's
/// week before and after its allowance ran out, 2026-11-27 to 28.
pub(super) const DEFAULT_PER_POINT: f64 = 209.0;

/// Weeks of one model a measured value needs.
const WEEKS_NEEDED: usize = 3;
/// The newest weeks the median is taken over.
const MEDIAN_OF: usize = 12;
const KEPT: usize = 256;

const MIN_POINTS: f64 = 5.0;
const MIN_CREDITS: f64 = 100.0;
/// The share of the cost one model must make on each side.
const ONE_MODEL: f64 = 0.9;

const SAVE_EVERY_S: i64 = 300;
/// The records give one week's renewal a second or two apart; a claimed
/// reset or the next week moves it by hours or days.
const SAME_RENEWAL_S: i64 = 3600;
const FULL: f64 = 1.0 - 1e-9;

/// One account's week as far as it was read.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Period {
    pub renews: i64,
    /// The newest reading counted.
    pub through: i64,
    pub start_used: f64,
    pub pre_used: f64,
    pub post_credits: f64,
    /// Dollars at list price per model, before and after the week ran out.
    pub pre_cost: Vec<(String, f64)>,
    pub post_cost: Vec<(String, f64)>,
}

impl Period {
    fn points(&self) -> f64 {
        (self.pre_used - self.start_used).max(0.0) * 100.0
    }

    /// The value this week measured and the model it is of: `None` for
    /// a week that does not count, with the model word written anyway.
    pub(super) fn measure(&self) -> (String, Option<f64>) {
        let (pre_m, pre_share, pre) = top(&self.pre_cost);
        let (post_m, post_share, post) = top(&self.post_cost);
        let one = pre_m == post_m && pre_share >= ONE_MODEL && post_share >= ONE_MODEL;
        let model = if one { pre_m } else { "mixed".to_string() };
        let counts = one && self.points() >= MIN_POINTS && self.post_credits >= MIN_CREDITS && pre > 0.0 && post > 0.0;
        (model, counts.then(|| (self.post_credits / post) / (self.points() / pre)))
    }
}

/// The model with the most cost, its share, and the total.
fn top(cost: &[(String, f64)]) -> (String, f64, f64) {
    let total: f64 = cost.iter().map(|(_, c)| c).sum();
    let best = cost.iter().max_by(|a, b| a.1.total_cmp(&b.1));
    match best {
        Some((m, c)) if total > 0.0 => (m.clone(), c / total, total),
        _ => ("-".to_string(), 0.0, total),
    }
}

fn add(cost: &mut Vec<(String, f64)>, model: &str, dollars: f64) {
    match cost.iter_mut().find(|(m, _)| m == model) {
        Some((_, c)) => *c += dollars,
        None => cost.push((model.to_string(), dollars)),
    }
}

pub(super) struct PeriodStore {
    dir: PathBuf,
    open: HashMap<u8, Period>,
    dirty: bool,
    saved_at: i64,
}

impl PeriodStore {
    pub(super) fn new(dir: &Path) -> PeriodStore {
        let open = fs::read_to_string(dir.join("wallet_periods.tsv")).unwrap_or_default().lines().filter_map(parse_open).collect();
        PeriodStore { dir: dir.to_path_buf(), open, dirty: false, saved_at: 0 }
    }

    /// Count one reading into its account's week; the week it ends, if
    /// its renewal moved.  `spent` is what the reading's spend fact took.
    /// A reading no newer than the week's newest counted is none: a
    /// tail read again, or an older record still naming last week's
    /// renewal, neither counts twice nor ends the week.  A reading
    /// without the reply's usage adds no cost, and after the week ran out
    /// its credits are left out too, so the two sides stay matched.
    pub(super) fn take(&mut self, s: &Seen, model: Option<&str>, spent: f64) -> Option<Period> {
        let (used, renews) = s.week?;
        if self.open.get(&s.profile).is_some_and(|p| s.at <= p.through) {
            return None;
        }
        let done = self.open.get(&s.profile).is_some_and(|p| (p.renews - renews).abs() > SAME_RENEWAL_S).then(|| self.open.remove(&s.profile)).flatten();
        let p = self.open.entry(s.profile).or_insert_with(|| Period {
            renews,
            through: s.at - 1,
            start_used: used.min(1.0),
            pre_used: used.min(1.0),
            post_credits: 0.0,
            pre_cost: Vec::new(),
            post_cost: Vec::new(),
        });
        let model = model.map_or_else(|| "-".to_string(), |m| m.replace(['\t', ' ', ',', '=', ';'], "_"));
        let dollars = s.tokens.as_ref().map(|t| t.dollars());
        if used < FULL {
            p.pre_used = p.pre_used.max(used);
            if let Some(d) = dollars {
                add(&mut p.pre_cost, &model, d);
            }
        } else {
            p.pre_used = 1.0;
            if let Some(d) = dollars {
                p.post_credits += spent;
                add(&mut p.post_cost, &model, d);
            }
        }
        p.through = s.at;
        self.dirty = true;
        if done.is_some() {
            // an ended week is written at once with the facts it closes
            self.saved_at = 0;
        }
        done
    }

    /// Write the open weeks, if changed and not written in five minutes.
    pub(super) fn save(&mut self, now: i64) -> std::io::Result<()> {
        if !self.dirty || now - self.saved_at < SAVE_EVERY_S {
            return Ok(());
        }
        let mut rows: Vec<(&u8, &Period)> = self.open.iter().collect();
        rows.sort_by_key(|(p, _)| **p);
        let text: String = rows.iter().map(|(profile, p)| open_line(**profile, p)).collect();
        write_atomic(&self.dir.join("wallet_periods.tsv"), &text)?;
        self.dirty = false;
        self.saved_at = now;
        Ok(())
    }

    /// Write the ended week `p` of account `profile` once; the log line
    /// for a week written now.
    pub(super) fn record(&self, profile: u8, p: &Period, now: i64) -> std::io::Result<Option<String>> {
        let path = self.dir.join("per_point.tsv");
        let text = fs::read_to_string(&path).unwrap_or_default();
        let key = format!("\t{profile}\t{}\t", p.renews);
        if text.lines().any(|l| l.contains(&key)) {
            return Ok(None);
        }
        let (model, value) = p.measure();
        let shown = value.map_or_else(|| "-".to_string(), |v| format!("{v:.2}"));
        let pre: f64 = p.pre_cost.iter().map(|(_, c)| c).sum();
        let post: f64 = p.post_cost.iter().map(|(_, c)| c).sum();
        let line = format!("{now}\t{profile}\t{}\t{model}\t{:.2}\t{pre:.4}\t{:.6}\t{post:.4}\t{shown}\n", p.renews, p.points(), p.post_credits);
        let mut lines: Vec<&str> = text.lines().collect();
        lines.push(line.trim_end());
        if lines.len() > KEPT {
            let body: String = lines[lines.len() - KEPT..].iter().map(|l| format!("{l}\n")).collect();
            write_atomic(&path, &body)?;
        } else {
            fs::create_dir_all(&self.dir)?;
            fs::OpenOptions::new().create(true).append(true).open(&path)?.write_all(line.as_bytes())?;
        }
        let all = fs::read_to_string(&path).unwrap_or_default();
        let (median, weeks) = per_point(&all, &model);
        Ok(Some(format!("P{profile} renews={} model={model} per_point={shown} median={median:.2} weeks={weeks}", p.renews)))
    }
}

/// The credits per point to use for `model` from the lines of
/// `per_point.tsv`, and how many weeks it rests on: the median of the
/// newest weeks once there are enough, the default before.
pub(super) fn per_point(text: &str, model: &str) -> (f64, usize) {
    let mut v: Vec<f64> = text
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            (f.len() == 9 && f[3] == model).then(|| f[8].parse().ok()).flatten()
        })
        .collect();
    let weeks = v.len();
    if weeks < WEEKS_NEEDED {
        return (DEFAULT_PER_POINT, weeks);
    }
    let mut v = v.split_off(weeks - weeks.min(MEDIAN_OF));
    v.sort_by(f64::total_cmp);
    let n = v.len();
    let median = if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 };
    (median, weeks)
}

/// Replace `path` in one step, so a reader never sees half a file.
fn write_atomic(path: &Path, body: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, body)?;
    fs::rename(&tmp, path)
}

fn costs(c: &[(String, f64)]) -> String {
    if c.is_empty() {
        return "-".into();
    }
    c.iter().map(|(m, d)| format!("{m}={d}")).collect::<Vec<_>>().join(",")
}

fn parse_costs(s: &str) -> Option<Vec<(String, f64)>> {
    if s == "-" {
        return Some(Vec::new());
    }
    s.split(',').map(|kv| kv.split_once('=').and_then(|(m, d)| Some((m.to_string(), d.parse().ok()?)))).collect()
}

fn open_line(profile: u8, p: &Period) -> String {
    format!(
        "{profile}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
        p.renews,
        p.through,
        p.start_used,
        p.pre_used,
        p.post_credits,
        costs(&p.pre_cost),
        costs(&p.post_cost)
    )
}

fn parse_open(line: &str) -> Option<(u8, Period)> {
    let f: Vec<&str> = line.split('\t').collect();
    if f.len() != 8 {
        return None;
    }
    let p = Period {
        renews: f[1].parse().ok()?,
        through: f[2].parse().ok()?,
        start_used: f[3].parse().ok()?,
        pre_used: f[4].parse().ok()?,
        post_credits: f[5].parse().ok()?,
        pre_cost: parse_costs(f[6])?,
        post_cost: parse_costs(f[7])?,
    };
    Some((f[0].parse().ok()?, p))
}

#[cfg(test)]
mod tests {
    use super::super::alloc_rollout::CxTokens;
    use super::*;

    fn period(pre: &[(&str, f64)], post: &[(&str, f64)], points: f64, credits: f64) -> Period {
        let v = |c: &[(&str, f64)]| c.iter().map(|(m, d)| (m.to_string(), *d)).collect();
        Period { renews: 1000, through: 900, start_used: 0.0, pre_used: points / 100.0, post_credits: credits, pre_cost: v(pre), post_cost: v(post) }
    }

    #[test]
    fn a_week_of_one_model_measures_credits_per_point() {
        // 100 points for $1600, then 20900 credits for $1600: 209 a point
        let p = period(&[("m", 1600.0)], &[("m", 1600.0)], 100.0, 20_900.0);
        let (model, v) = p.measure();
        assert_eq!(model, "m");
        assert!((v.unwrap() - 209.0).abs() < 1e-9, "{v:?}");
        assert_eq!(period(&[("m", 50.0), ("n", 50.0)], &[("m", 10.0)], 100.0, 20_900.0).measure(), ("mixed".to_string(), None));
        assert_eq!(period(&[("m", 1.0)], &[("m", 1.0)], 4.0, 20_900.0).measure().1, None, "too few points");
        assert_eq!(period(&[("m", 1.0)], &[("m", 1.0)], 40.0, 99.0).measure().1, None, "too few credits");
    }

    #[test]
    fn the_default_stands_until_three_weeks_of_the_model() {
        let line = |v: &str, m: &str| format!("1\t1\t2\t{m}\t50\t1\t100\t1\t{v}\n");
        let mut text = line("300", "m") + &line("-", "m") + &line("100", "m") + &line("999", "other");
        assert_eq!(per_point(&text, "m"), (DEFAULT_PER_POINT, 2));
        text += &line("250", "m");
        assert_eq!(per_point(&text, "m"), (250.0, 3));
        text += &line("260", "m");
        assert_eq!(per_point(&text, "m"), (255.0, 4));
    }

    fn seen(at: i64, used: f64, renews: i64) -> Seen {
        let tokens = Some(CxTokens { input: 1000, cached: 0, output: 10 });
        Seen { at, sid: 1, profile: 2, class: "plain", week: Some((used, renews)), balance: None, cap: None, tokens }
    }

    #[test]
    fn a_reading_no_newer_than_the_week_counts_nowhere_and_ends_nothing() {
        let tmp = marspot_term::test_env::TempDir::new("codex-per-point-older");
        let mut store = PeriodStore::new(&tmp.join("codex"));
        let week = 2_000_000;
        assert_eq!(store.take(&seen(100, 0.2, week), Some("m"), 0.0), None);
        assert_eq!(store.take(&seen(200, 0.4, week), Some("m"), 0.0), None);
        let cost = store.open[&2].pre_cost.clone();
        assert_eq!(store.take(&seen(200, 0.4, week), Some("m"), 0.0), None, "the same reading again");
        assert_eq!(store.take(&seen(150, 0.9, week - 7 * 86_400), Some("m"), 0.0), None, "an older one naming last week's renewal");
        let p = &store.open[&2];
        assert_eq!((p.renews, p.through, p.pre_used, &p.pre_cost), (week, 200, 0.4, &cost), "the week goes on untouched");
        let mut unknown = seen(300, 1.0, week);
        unknown.tokens = None;
        assert_eq!(store.take(&unknown, Some("m"), 50.0), None);
        assert_eq!((store.open[&2].post_credits, store.open[&2].post_cost.len()), (0.0, 0), "no usage: neither credits nor cost");
    }

    #[test]
    fn an_open_week_reads_back_as_written() {
        let p = period(&[("m", 1.5), ("n", 0.25)], &[], 30.0, 0.0);
        let (profile, back) = parse_open(open_line(2, &p).trim_end()).unwrap();
        assert_eq!((profile, back), (2, p));
    }
}

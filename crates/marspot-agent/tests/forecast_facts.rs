//! The forecast on the facts kept on 2026-11-26 (the fixture holds no
//! account, model or task text): the band covers 80% of six-hour
//! actuals per class, the time-of-day profile stays off, and a store
//! built hour by hour equals one built at once.

use marspot_agent::quota::Class;
use marspot_agent::sched::forecast::{self, Boundary, Facts, HOUR, HourRow};

const FIXTURE: &str = include_str!("fixtures/forecast_facts_20261126.tsv");

enum Fact {
    Used(i64, u64, Class, f64),
    Seen(i64, u64),
    Boundary(u64, Boundary),
}

impl Fact {
    fn at(&self) -> i64 {
        match self {
            Fact::Used(at, ..) | Fact::Seen(at, _) => *at,
            Fact::Boundary(_, b) => b.at,
        }
    }

    fn feed(&self, f: &mut Facts) {
        match *self {
            Fact::Used(at, sid, c, w) => f.used(at, sid, c, w),
            Fact::Seen(at, sid) => f.seen(at, sid),
            Fact::Boundary(sid, b) => f.boundary(sid, b),
        }
    }
}

struct Kept {
    now: i64,
    facts: Vec<Fact>,
    hours: Vec<HourRow>,
    helpers: Vec<(String, u32)>,
}

fn kept() -> Kept {
    let mut k = Kept { now: 0, facts: Vec::new(), hours: Vec::new(), helpers: Vec::new() };
    for line in FIXTURE.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let n = |i: usize| f[i].parse::<i64>().unwrap();
        let w = |i: usize| f[i].parse::<f64>().ok();
        match f[0] {
            "now" => k.now = n(1),
            "h" => k.hours.push((n(1), w(2).zip(w(3)).map(|(a, b)| [a, b]))),
            "u" => k.facts.push(Fact::Used(n(1), n(2) as u64, if f[3] == "f" { Class::Restricted } else { Class::Plain }, w(4).unwrap())),
            "s" => k.facts.push(Fact::Seen(n(1), n(2) as u64)),
            "b" => k.facts.push(Fact::Boundary(
                n(2) as u64,
                Boundary { at: n(1), clean: f[3] == "1", tasks: n(4) as u32, oldest_s: f[5].parse().ok() },
            )),
            "x" => k.helpers.push((f[1].to_string(), n(2) as u32)),
            other => panic!("fixture line kind {other}"),
        }
    }
    k
}

fn all_at_once(k: &Kept) -> Facts {
    let mut f = Facts::default();
    for e in &k.facts {
        e.feed(&mut f);
    }
    f.set_hours(k.hours.iter().copied());
    f.set_helpers(k.helpers.iter().map(|(a, s)| (a.as_str(), *s)));
    f.prune(k.now);
    f
}

fn live(f: &Facts, now: i64) -> Vec<u64> {
    f.panes.iter().filter(|(_, p)| now - p.last_at < HOUR).map(|(s, _)| *s).collect()
}

#[test]
fn forecast_facts_band_covers_80_percent_per_class() {
    let k = kept();
    let f = all_at_once(&k);
    let fc = forecast::build(&f, k.now, &live(&f, k.now));
    eprintln!("sched.forecast {}", fc.line());
    for c in 0..2 {
        for (what, b) in [("fleet", fc.fleet[c].band), ("pane", fc.pane_band[c])] {
            assert!(b.pairs >= 10, "{what} class {c}: {b:?}");
            assert!(b.covered * 5 >= b.pairs * 4, "{what} class {c} covers under 80%: {b:?}");
            assert!(b.lo <= b.hi);
        }
    }
    // under seven days of facts every band starts from its default
    assert!(fc.fleet.iter().all(|x| x.band.from == forecast::BandFrom::Default));
    assert!(fc.panes.iter().any(|p| p.counts));
    assert_eq!(fc.helpers.iter().map(|h| h.finished).sum::<usize>(), k.helpers.len());
}

/// 3.3's test on the hourly roll-up kept: the profile does not beat the
/// 24-hour mean by 10%, so it stays off.
#[test]
fn forecast_facts_time_of_day_stays_off() {
    let k = kept();
    let fc = forecast::build(&all_at_once(&k), k.now, &[]);
    for (c, x) in fc.fleet.iter().enumerate() {
        let t = x.time_of_day;
        eprintln!("time of day class {c}: origins {} mae profile {:.4} day {:.4} points/h", t.origins, t.mae_profile * 100.0, t.mae_day * 100.0);
        assert!(t.origins > 0);
        assert!(!t.on, "{t:?}");
    }
}

#[test]
fn forecast_facts_rebuild_equals_hour_by_hour() {
    let k = kept();
    let mut inc = Facts::default();
    let mut order: Vec<&Fact> = k.facts.iter().collect();
    order.sort_by_key(|e| e.at());
    let mut hour = order[0].at().div_euclid(HOUR);
    for e in order {
        if e.at().div_euclid(HOUR) != hour {
            hour = e.at().div_euclid(HOUR);
            inc.prune(e.at());
        }
        e.feed(&mut inc);
    }
    inc.set_hours(k.hours.iter().copied());
    inc.set_helpers(k.helpers.iter().map(|(a, s)| (a.as_str(), *s)));
    inc.prune(k.now);
    let mut once = all_at_once(&k);
    assert_eq!(inc, once);
    let lv = live(&once, k.now);
    let a = forecast::build(&inc, k.now, &lv);
    // deleting the derived forecast and building it again gives the same
    once.prune(k.now);
    assert_eq!(a, forecast::build(&once, k.now, &lv));
    assert_eq!(a.line(), forecast::build(&once, k.now, &lv).line());
}

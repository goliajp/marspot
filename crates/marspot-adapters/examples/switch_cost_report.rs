//! What switches of account cost on this machine, from its own records.
//!
//! `switch_cost_report <state dir> [transcript.jsonl …]`
//!
//! 1. The `switch_cost` facts the shell wrote (one per measured switch)
//!    under `<state dir>/plugins/claudecode/alloc_events/`.
//! 2. For each transcript named, the switches the `bound … from P…`
//!    facts record, matched to the transcript where its first reply
//!    after the switch, within a quarter of an hour, is cold; a cold
//!    reply that close after a pane changed account is that pane's.
//!
//! Each part prints its samples split by context length: how much of
//! the context was sent uncached again, how much of it went to the
//! one-hour cache, how much output was said again over the next
//! replies, and the cache hit of the first reply against the replies
//! before the switch.  Nothing is written.

use std::fs;
use std::path::Path;

use marspot_adapters::claude::switch_turns::turns;
use marspot_agent::sched::switch_cost::{self, BUCKETS, Fit, Sample, by_length, sample};

/// A transcript's first reply after a switch is looked for this long.
const MATCH_S: i64 = 900;

fn events(state: &Path) -> Vec<(i64, String, String)> {
    let dir = state.join("plugins/claudecode/alloc_events");
    let mut names: Vec<_> = fs::read_dir(&dir).into_iter().flatten().filter_map(|e| e.ok()).map(|e| e.path()).collect();
    names.sort();
    let mut out = Vec::new();
    for p in names {
        let Ok(text) = fs::read_to_string(&p) else { continue };
        for line in text.lines() {
            let f: Vec<&str> = line.splitn(7, '\t').collect();
            if f.len() == 7
                && let Ok(at) = f[0].parse()
            {
                out.push((at, f[3].to_string(), f[6].to_string()));
            }
        }
    }
    out
}

fn print(title: &str, samples: &[Sample]) {
    println!("{title}: {} samples", samples.len());
    let mut all = Fit::default();
    samples.iter().for_each(|s| all.add(s));
    let row = |name: String, f: &Fit, hits: (u64, u64)| {
        let ctx = f.ctx.max(1) as f64;
        println!(
            "  {name:>12}  n={:<4} ctx_mean={:<8} rewrite/ctx={:.3} one_hour={:.3} regen/ctx={:.4} hit_first={:.3} hit_before={:.3}",
            f.n,
            f.ctx / u64::from(f.n.max(1)),
            f.rewrite as f64 / ctx,
            f.rewrite_1h as f64 / f.rewrite.max(1) as f64,
            f.regen as f64 / ctx,
            hits.0 as f64 / 1000.0 / f64::from(f.n.max(1)),
            hits.1 as f64 / 1000.0 / f64::from(f.n.max(1)),
        );
    };
    let hits = |lo: u64, hi: u64| samples.iter().filter(|s| s.ctx >= lo && s.ctx < hi).fold((0, 0), |a, s| (a.0 + u64::from(s.hit_after), a.1 + u64::from(s.hit_base)));
    let mut lo = 0;
    for (k, f) in by_length(samples).iter().enumerate() {
        let name = if BUCKETS[k] == u64::MAX { format!(">={}k", lo / 1000) } else { format!("<{}k", BUCKETS[k] / 1000) };
        row(name, f, hits(lo, BUCKETS[k]));
        lo = BUCKETS[k];
    }
    row("all".into(), &all, hits(0, u64::MAX));
    match all.measured() {
        Some(s) => println!("  shares the shell prices with: rewrite={:.3} one_hour={:.3} regen={:.4}", s.rewrite, s.one_hour, s.regen),
        None => println!("  under {} samples: the shell prices switches as before anything was measured", switch_cost::FIT_MIN),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(state) = args.first() else {
        eprintln!("usage: switch_cost_report <state dir> [transcript.jsonl …]");
        std::process::exit(2);
    };
    let ev = events(Path::new(state));
    let live: Vec<Sample> = ev.iter().filter(|e| e.1 == "switch_cost").filter_map(|e| Sample::parse(e.0, &e.2)).collect();
    print("measured by the shell", &live);
    let mut switches: Vec<i64> = ev.iter().filter(|e| e.1 == "bound" && e.2.starts_with("from P")).map(|e| e.0).collect();
    switches.sort_unstable();
    let mut retro = Vec::new();
    for path in &args[1..] {
        let Ok(text) = fs::read_to_string(path) else {
            eprintln!("cannot read {path}");
            continue;
        };
        let t = turns(&text);
        for (k, at) in switches.iter().enumerate() {
            let first = t.iter().find(|x| x.at > *at);
            if !first.is_some_and(|x| x.at - at <= MATCH_S && x.cold()) {
                continue;
            }
            if let Some(s) = sample(&t, *at, switches.get(k + 1).copied()) {
                retro.push(s);
            }
        }
    }
    retro.sort_by_key(|s| s.at);
    retro.dedup_by_key(|s| s.at);
    print("matched in the transcripts", &retro);
}

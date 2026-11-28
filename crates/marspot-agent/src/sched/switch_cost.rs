//! What a switch of account costs, measured from the replies around it.
//!
//! Every account here is in its own organization, so a session resumed
//! on another account finds none of its prompt cache: the first reply
//! after the switch writes the whole conversation to the cache again
//! instead of reading it, and the thinking the old organization signed
//! cannot be sent back, so the next replies think again.  The local
//! transcript is shared, so nothing else is lost.
//!
//! A sample is one switch: the first reply after it against the replies
//! just before it (the baseline).  What the first reply wrote and sent
//! uncached beyond the baseline's median is the rewrite; what the first
//! `AFTER` replies said beyond the baseline's median is the thinking
//! done again.  Both are compared with the context the first reply
//! sent, so a fit is a share of the context and the cost of a switch
//! grows with the session's length.  A switch made after the cache had
//! expired anyway (the baseline's last reply over an hour old) rewrites
//! nothing because of the switch and is not sampled.
//!
//! Sums are integers, so a fit does not depend on the order samples
//! are folded in.

/// Replies before a switch its baseline is taken from, at most.
pub const BASE: usize = 8;
/// Fewest baseline replies a sample needs.
pub const BASE_MIN: usize = 3;
/// Replies after a switch whose output is compared.
pub const AFTER: usize = 3;
/// A cache this old has expired anyway, seconds.
pub const EXPIRED_S: i64 = 3600;
/// Samples a fit needs before it replaces the default.
pub const FIT_MIN: u32 = 5;

/// One reply's counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Turn {
    pub at: i64,
    pub input: u64,
    pub write_5m: u64,
    pub write_1h: u64,
    pub read: u64,
    pub output: u64,
}

impl Turn {
    /// Everything the reply sent.
    pub fn prompt(&self) -> u64 {
        self.input + self.write_5m + self.write_1h + self.read
    }

    /// What it sent that the cache did not hold.
    pub fn uncached(&self) -> u64 {
        self.input + self.write_5m + self.write_1h
    }

    /// Less than half of what it sent came from the cache.
    pub fn cold(&self) -> bool {
        self.read * 2 < self.prompt()
    }
}

/// One measured switch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    pub at: i64,
    /// What the first reply after it sent, tokens.
    pub ctx: u64,
    /// Sent uncached beyond the baseline's median.
    pub rewrite: u64,
    /// Of that, written to the one-hour cache.
    pub rewrite_1h: u64,
    /// Said by the first `AFTER` replies beyond the baseline's median,
    /// replies counted; may be negative.
    pub regen: i64,
    /// What share of the first reply's prompt the cache held, and the
    /// baseline's median share, thousandths.
    pub hit_after: u16,
    pub hit_base: u16,
}

impl Sample {
    /// `ctx=… rw=… rw1h=… regen=… hit=…/…`.
    pub fn words(&self) -> String {
        format!("ctx={} rw={} rw1h={} regen={} hit={}/{}", self.ctx, self.rewrite, self.rewrite_1h, self.regen, self.hit_after, self.hit_base)
    }

    pub fn parse(at: i64, detail: &str) -> Option<Sample> {
        let get = |k: &str| detail.split(' ').find_map(|w| w.strip_prefix(k)?.strip_prefix('='));
        let (a, b) = get("hit")?.split_once('/')?;
        Some(Sample {
            at,
            ctx: get("ctx")?.parse().ok()?,
            rewrite: get("rw")?.parse().ok()?,
            rewrite_1h: get("rw1h")?.parse().ok()?,
            regen: get("regen")?.parse().ok()?,
            hit_after: a.parse().ok()?,
            hit_base: b.parse().ok()?,
        })
    }
}

fn median(mut v: Vec<u64>) -> u64 {
    v.sort_unstable();
    v[v.len() / 2]
}

fn hit(t: &Turn) -> u16 {
    (t.read * 1000).checked_div(t.prompt()).map_or(0, |h| h as u16)
}

/// The sample of the switch at `switch_at` in `turns` (one session's
/// own replies, oldest first), or `None` when there are not enough
/// replies on either side, or the cache had expired anyway.  The
/// replies after it stop at `next_switch`.
pub fn sample(turns: &[Turn], switch_at: i64, next_switch: Option<i64>) -> Option<Sample> {
    let split = turns.partition_point(|t| t.at <= switch_at);
    let before = &turns[split.saturating_sub(BASE)..split];
    let base: Vec<&Turn> = before.iter().filter(|t| !t.cold()).collect();
    if base.len() < BASE_MIN {
        return None;
    }
    let last = before.last()?;
    let after: Vec<&Turn> = turns[split..].iter().take_while(|t| next_switch.is_none_or(|n| t.at <= n)).take(AFTER).collect();
    let first = **after.first()?;
    if first.at - last.at >= EXPIRED_S {
        return None;
    }
    let base_unc = median(base.iter().map(|t| t.uncached()).collect());
    let base_out = median(base.iter().map(|t| t.output).collect());
    let rewrite = first.uncached().saturating_sub(base_unc);
    let said: u64 = after.iter().map(|t| t.output).sum();
    Some(Sample {
        at: first.at,
        ctx: first.prompt(),
        rewrite,
        rewrite_1h: first.write_1h.min(rewrite),
        regen: said as i64 - (base_out * after.len() as u64) as i64,
        hit_after: hit(&first),
        hit_base: median(base.iter().map(|t| u64::from(hit(t))).collect()) as u16,
    })
}

/// Samples folded together.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fit {
    pub n: u32,
    pub ctx: u64,
    pub rewrite: u64,
    pub rewrite_1h: u64,
    pub regen: i64,
}

/// What a switch costs per token of context, as shares.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shares {
    /// Sent uncached again.
    pub rewrite: f64,
    /// Of the rewrite, written to the one-hour cache.
    pub one_hour: f64,
    /// Said again, as output.
    pub regen: f64,
}

/// Before anything is measured: the whole context is written once to
/// the five-minute cache and nothing is thought again, which is what
/// the cost of a switch was before it was measured.
pub const DEFAULT: Shares = Shares { rewrite: 1.0, one_hour: 0.0, regen: 0.0 };

impl Fit {
    pub fn add(&mut self, s: &Sample) {
        self.n += 1;
        self.ctx += s.ctx;
        self.rewrite += s.rewrite;
        self.rewrite_1h += s.rewrite_1h;
        self.regen += s.regen;
    }

    /// The shares, or `DEFAULT` with fewer than `FIT_MIN` samples.  A
    /// rewrite is at most the context twice over, and thinking said
    /// again is never negative.
    pub fn shares(&self) -> Shares {
        if self.n < FIT_MIN || self.ctx == 0 {
            return DEFAULT;
        }
        let ctx = self.ctx as f64;
        Shares {
            rewrite: (self.rewrite as f64 / ctx).min(2.0),
            one_hour: if self.rewrite == 0 { 0.0 } else { self.rewrite_1h as f64 / self.rewrite as f64 },
            regen: (self.regen.max(0) as f64 / ctx).min(1.0),
        }
    }
}

/// Context buckets a report splits samples by, upper bounds in tokens.
pub const BUCKETS: [u64; 4] = [100_000, 300_000, 600_000, u64::MAX];

/// Samples split by context, one fit per bucket of `BUCKETS`.
pub fn by_length(samples: &[Sample]) -> [Fit; 4] {
    let mut out = [Fit::default(); 4];
    for s in samples {
        let k = BUCKETS.iter().position(|b| s.ctx < *b).unwrap_or(3);
        out[k].add(s);
    }
    out
}

#[cfg(test)]
#[path = "switch_cost_tests.rs"]
mod tests;

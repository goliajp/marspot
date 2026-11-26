//! What a reply's tokens cost in weeks of an account.
//!
//! The dollar prices are the devops control plane's Claude pricing
//! table (Anthropic's published prices, as the control plane had them
//! on 2026-11-24), matched the same way: the first name fragment the
//! model contains, Opus when none does.  Dollars become weeks at what
//! the accounts' own readings say a point costs: a regression of the
//! week's daily rise on the day's Fable and non-Fable dollars gives
//! 11.2 dollars a point for Fable and 30.5 for the other models.
//!
//! The event log keeps the tokens, not the weeks, so a better price or
//! rate later is applied by reading the log again.

use marspot_peer::json::Value;

use crate::plugins::allocation::Class;

const DOLLARS_PER_POINT_RESTRICTED: f64 = 11.2;
const DOLLARS_PER_POINT_PLAIN: f64 = 30.5;

/// Dollars per million tokens: input, cache write (5 minutes), cache
/// write (1 hour), cache read, output.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Price(f64, f64, f64, f64, f64);

const OPUS: Price = Price(4.0, 5.0, 8.0, 0.2, 20.0);

/// In the control plane's order; the first fragment the model contains wins.
const RULES: [(&str, Price); 26] = [
    ("mythos-5-1", Price(10.0, 12.5, 20.0, 0.25, 50.0)),
    ("mythos-5-2", Price(8.0, 10.0, 16.0, 0.2, 40.0)),
    ("sonnet-4-5", Price(3.0, 3.75, 6.0, 0.3, 15.0)),
    ("sonnet-4-6", Price(3.0, 3.75, 6.0, 0.3, 15.0)),
    ("sonnet-5-5", Price(2.0, 2.5, 4.0, 0.1, 10.0)),
    ("fable-5-1", Price(10.0, 12.5, 20.0, 0.25, 50.0)),
    ("fable-5-2", Price(8.0, 10.0, 16.0, 0.2, 40.0)),
    ("haiku-4-5", Price(1.0, 1.25, 2.0, 0.1, 5.0)),
    ("haiku-5-5", Price(0.5, 0.625, 1.0, 0.05, 2.5)),
    ("mythos-5", Price(10.0, 12.5, 20.0, 1.0, 50.0)),
    ("opus-4-1", Price(15.0, 18.75, 30.0, 1.5, 75.0)),
    ("opus-4-5", Price(5.0, 6.25, 10.0, 0.5, 25.0)),
    ("opus-4-6", Price(5.0, 6.25, 10.0, 0.5, 25.0)),
    ("opus-4-7", Price(5.0, 6.25, 10.0, 0.5, 25.0)),
    ("opus-4-8", Price(5.0, 6.25, 10.0, 0.5, 25.0)),
    ("opus-5-5", OPUS),
    ("sonnet-4", Price(3.0, 3.75, 6.0, 0.3, 15.0)),
    ("sonnet-5", Price(2.0, 2.5, 4.0, 0.2, 10.0)),
    ("fable-5", Price(10.0, 12.5, 20.0, 1.0, 50.0)),
    ("opus-4", Price(15.0, 18.75, 30.0, 1.5, 75.0)),
    ("opus-5", Price(5.0, 6.25, 10.0, 0.5, 25.0)),
    ("fable", Price(8.0, 10.0, 16.0, 0.2, 40.0)),
    ("mythos", Price(8.0, 10.0, 16.0, 0.2, 40.0)),
    ("opus", OPUS),
    ("sonnet", Price(2.0, 2.5, 4.0, 0.1, 10.0)),
    ("haiku", Price(0.1, 0.125, 0.2, 0.01, 0.5)),
];

/// The tokens of one or more replies, by kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Tokens {
    pub input: u64,
    pub cache_5m: u64,
    pub cache_1h: u64,
    pub cache_read: u64,
    pub output: u64,
}

impl Tokens {
    /// From a reply's `message.usage`.  Cache writes not split by
    /// lifetime count as five-minute ones.
    pub(super) fn of(usage: &Value) -> Tokens {
        let n = |v: &Value, k: &str| v.get(k).and_then(Value::i64).unwrap_or(0).max(0) as u64;
        let written = n(usage, "cache_creation_input_tokens");
        let (cache_5m, cache_1h) = match usage.get("cache_creation") {
            Some(c) => (n(c, "ephemeral_5m_input_tokens"), n(c, "ephemeral_1h_input_tokens")),
            None => (written, 0),
        };
        Tokens { input: n(usage, "input_tokens"), cache_5m, cache_1h, cache_read: n(usage, "cache_read_input_tokens"), output: n(usage, "output_tokens") }
    }

    fn fields(&self) -> [u64; 5] {
        [self.input, self.cache_5m, self.cache_1h, self.cache_read, self.output]
    }

    fn from_fields(f: [u64; 5]) -> Tokens {
        Tokens { input: f[0], cache_5m: f[1], cache_1h: f[2], cache_read: f[3], output: f[4] }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.fields().iter().all(|x| *x == 0)
    }

    pub(super) fn add(&mut self, o: &Tokens) {
        let (a, b) = (self.fields(), o.fields());
        *self = Tokens::from_fields(std::array::from_fn(|i| a[i] + b[i]));
    }

    /// What `self` has beyond `was`, field by field.
    pub(super) fn beyond(&self, was: &Tokens) -> Tokens {
        let (a, b) = (self.fields(), was.fields());
        Tokens::from_fields(std::array::from_fn(|i| a[i].saturating_sub(b[i])))
    }

    /// The larger of each field.
    pub(super) fn max(&self, o: &Tokens) -> Tokens {
        let (a, b) = (self.fields(), o.fields());
        Tokens::from_fields(std::array::from_fn(|i| a[i].max(b[i])))
    }

    /// `in=… cc5m=… cc1h=… cr=… out=…`.
    pub(super) fn words(&self) -> String {
        format!("in={} cc5m={} cc1h={} cr={} out={}", self.input, self.cache_5m, self.cache_1h, self.cache_read, self.output)
    }

    pub(super) fn parse(detail: &str) -> Option<Tokens> {
        let get = |k: &str| detail.split(' ').find_map(|w| w.strip_prefix(k)?.strip_prefix('='))?.parse::<u64>().ok();
        Some(Tokens { input: get("in")?, cache_5m: get("cc5m")?, cache_1h: get("cc1h")?, cache_read: get("cr")?, output: get("out")? })
    }
}

pub(super) fn class_of(model: &str) -> Class {
    let m = model.to_ascii_lowercase();
    if m.contains("fable") || m.contains("mythos") { Class::Restricted } else { Class::Plain }
}

/// Dollars `t` costs on `model`.
pub(super) fn dollars(model: &str, t: &Tokens) -> f64 {
    let m = model.to_ascii_lowercase();
    let p = RULES.iter().find(|(k, _)| m.contains(k)).map_or(OPUS, |(_, p)| *p);
    let f = t.fields().map(|x| x as f64);
    (f[0] * p.0 + f[1] * p.1 + f[2] * p.2 + f[3] * p.3 + f[4] * p.4) / 1e6
}

/// Weeks of an account `t` uses on `model`.
pub(super) fn weeks(model: &str, t: &Tokens) -> f64 {
    let per_point = if class_of(model) == Class::Restricted { DOLLARS_PER_POINT_RESTRICTED } else { DOLLARS_PER_POINT_PLAIN };
    dollars(model, t) / per_point / 100.0
}

/// A million replies' worth of tokens split as this machine's replies
/// split their cost at Opus prices: 52% cache reads, 33% cache writes,
/// 15% output.
const TYPICAL: Tokens = Tokens { input: 0, cache_5m: 66_000, cache_1h: 0, cache_read: 2_600_000, output: 7_500 };

/// How many times the week the same work costs on `model` what it
/// costs on a model with no window of its own (Opus, the price of a
/// model the table does not name).
pub(super) fn week_cost_against_plain(model: &str) -> f64 {
    weeks(model, &TYPICAL) / weeks("opus", &TYPICAL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use marspot_peer::json;

    #[test]
    fn allocation_prices_follow_the_control_plane_table() {
        let t = |i, c5, c1, cr, o| Tokens { input: i, cache_5m: c5, cache_1h: c1, cache_read: cr, output: o };
        let m = 1_000_000;
        let near = |x: f64, y: f64| assert!((x - y).abs() < 1e-9, "{x} vs {y}");
        near(dollars("claude-fable-5-2", &t(m, m, m, m, m)), 74.2);
        near(dollars("claude-opus-5-5", &t(m, m, m, m, m)), 37.2);
        near(dollars("claude-fable-5", &t(0, 0, 0, m, 0)), 1.0);
        near(dollars("claude-haiku-5-5-20261001", &t(m, 0, 0, 0, 0)), 0.5);
        near(dollars("something-new", &t(m, 0, 0, 0, 0)), 4.0);
        assert!((weeks("claude-fable-5-2", &t(0, 0, 0, 0, m)) - 40.0 / 11.2 / 100.0).abs() < 1e-15);
        assert!((weeks("claude-opus-5-5", &t(0, 0, 0, 0, m)) - 20.0 / 30.5 / 100.0).abs() < 1e-15);
        assert_eq!(class_of("claude-fable-5-2"), Class::Restricted);
        assert_eq!(class_of("claude-opus-5-5"), Class::Plain);
        // Fable is twice Opus's price but for cache reads, and a dollar of
        // it is 30.5 / 11.2 times the week
        near(week_cost_against_plain("claude-fable-5-2[1m]"), 1.48 * 30.5 / 11.2);
        near(week_cost_against_plain("claude-opus-5-5"), 1.0);
    }

    #[test]
    fn allocation_prices_read_usage_and_round_trip() {
        let split = json::parse(r#"{"input_tokens":2,"cache_creation_input_tokens":52584,"cache_read_input_tokens":25050,"output_tokens":263,"cache_creation":{"ephemeral_1h_input_tokens":52000,"ephemeral_5m_input_tokens":584}}"#).unwrap();
        let t = Tokens::of(&split);
        assert_eq!(t, Tokens { input: 2, cache_5m: 584, cache_1h: 52000, cache_read: 25050, output: 263 });
        let plain = json::parse(r#"{"input_tokens":1,"cache_creation_input_tokens":7,"output_tokens":3}"#).unwrap();
        assert_eq!(Tokens::of(&plain).cache_5m, 7);
        assert_eq!(Tokens::parse(&format!("model=x {}", t.words())), Some(t));
        let more = Tokens { output: 300, ..t };
        assert_eq!(more.beyond(&t), Tokens { output: 37, ..Tokens::default() });
        assert_eq!(t.beyond(&more), Tokens::default());
    }
}

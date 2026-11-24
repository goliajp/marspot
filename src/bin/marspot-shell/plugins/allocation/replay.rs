//! Everything one decision about one pane reads, as one line of text,
//! so a logged would-move can be fed back to the same pure functions
//! and must come out the same.
//!
//! `key=value` words separated by spaces; numbers as Rust prints an
//! `f64`, which reads back to the same bits.  Accounts are
//! `id:R:f:T:T_F:open:stale` and loads `restricted:plain`, each list
//! joined by `;`.  Words it does not know are skipped.  Reading a line
//! back is for the replay of a dry run and its tests; the shell only
//! writes them.

use super::choose::{Mover, pick};
use super::plan::Plan;
#[cfg(test)]
use super::plan::plan;
use super::rest::{RestMove, Resting, rest_move};
use super::{Class, Load, Quant};

/// Why a pane would move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Why {
    /// Its account cannot answer it: rule (a), or (c) for a helper.
    Out,
    /// Its account will not last two hours.
    LeaveEarly,
    /// Supply exceeds demand and the plan wants it elsewhere.
    Plan,
}

impl Why {
    pub(crate) fn word(self) -> &'static str {
        match self {
            Why::Out => "out",
            Why::LeaveEarly => "leave_early",
            Why::Plan => "plan",
        }
    }
}

/// What kind of decision, with what only that kind reads.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Kind {
    /// Must move; `loads` leave the pane out.
    Out { horizon_h: f64 },
    /// Resting; `loads` count the pane.
    Rest {
        keeper: bool,
        since_move_h: Option<f64>,
        left: Vec<(u8, f64)>,
        band_last_hour: usize,
        demand: f64,
        stale_feed: bool,
        speed_known: bool,
        keeps_restricted: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Inputs {
    pub accounts: Vec<Quant>,
    pub b_f: f64,
    pub b_n: f64,
    pub kappa: Option<f64>,
    pub loads: Vec<Load>,
    pub mover: Mover,
    pub here: u8,
    pub kind: Kind,
}

impl Inputs {
    #[cfg(test)]
    pub(crate) fn plan(&self) -> Plan {
        plan(&self.accounts, self.b_f, self.b_n, self.kappa)
    }

    /// The decision, given the plan these inputs give.
    pub(crate) fn decide(&self, plan: &Plan) -> Option<(u8, Why)> {
        match &self.kind {
            Kind::Out { horizon_h } => pick(&self.accounts, plan, &self.loads, self.mover, Some(self.here), *horizon_h).map(|t| (t, Why::Out)),
            Kind::Rest { keeper, since_move_h, left, band_last_hour, demand, stale_feed, speed_known, keeps_restricted } => {
                let r = Resting {
                    here: self.here,
                    mover: self.mover,
                    loads: &self.loads,
                    keeper: *keeper,
                    since_move_h: *since_move_h,
                    left,
                    band_last_hour: *band_last_hour,
                    demand: *demand,
                    stale_feed: *stale_feed,
                    speed_known: *speed_known,
                    keeps_restricted: *keeps_restricted,
                };
                match rest_move(&self.accounts, plan, &r)? {
                    RestMove::LeaveEarly(t) => Some((t, Why::LeaveEarly)),
                    RestMove::Band(t) => Some((t, Why::Plan)),
                }
            }
        }
    }

    pub(crate) fn encode(&self) -> String {
        let b = |x: bool| u8::from(x);
        let acc: Vec<String> = self.accounts.iter().map(|q| format!("{}:{}:{}:{}:{}:{}:{}", q.id, q.r, q.f, q.t, q.tf, b(q.open), b(q.stale))).collect();
        let loads: Vec<String> = self.loads.iter().map(|l| format!("{}:{}", l.restricted, l.plain)).collect();
        let class = if self.mover.class == Class::Restricted { "restricted" } else { "plain" };
        let kappa = self.kappa.map_or_else(|| "-".to_string(), |k| k.to_string());
        let mut s = format!(
            "here={} class={class} rate={} kappa={kappa} bf={} bn={} acc={} loads={}",
            self.here,
            self.mover.rate,
            self.b_f,
            self.b_n,
            acc.join(";"),
            loads.join(";")
        );
        match &self.kind {
            Kind::Out { horizon_h } => s.push_str(&format!(" kind=out horizon={horizon_h}")),
            Kind::Rest { keeper, since_move_h, left, band_last_hour, demand, stale_feed, speed_known, keeps_restricted } => {
                let since = since_move_h.map_or_else(|| "-".to_string(), |h| h.to_string());
                let left: Vec<String> = left.iter().map(|(id, h)| format!("{id}:{h}")).collect();
                s.push_str(&format!(
                    " kind=rest keeper={} since={since} left={} band={band_last_hour} demand={demand} stale_feed={} known={} keeps={}",
                    b(*keeper),
                    left.join(";"),
                    b(*stale_feed),
                    b(*speed_known),
                    b(*keeps_restricted)
                ));
            }
        }
        s
    }

    #[cfg(test)]
    pub(crate) fn decode(line: &str) -> Option<Inputs> {
        let get = |k: &str| line.split(' ').find_map(|w| w.strip_prefix(k)?.strip_prefix('='));
        let num = |k: &str| get(k)?.parse::<f64>().ok();
        let flag = |k: &str| Some(get(k)? == "1");
        let list = |k: &str| get(k).map(|v| v.split(';').filter(|x| !x.is_empty()).map(|x| x.split(':').collect::<Vec<_>>()).collect::<Vec<_>>());
        let accounts = list("acc")?.into_iter().map(|f| quant(&f)).collect::<Option<Vec<_>>>()?;
        let loads = list("loads")?
            .into_iter()
            .map(|f| Some(Load { restricted: f.first()?.parse().ok()?, plain: f.get(1)?.parse().ok()? }))
            .collect::<Option<Vec<_>>>()?;
        let class = if get("class")? == "restricted" { Class::Restricted } else { Class::Plain };
        let kappa = match get("kappa")? {
            "-" => None,
            k => Some(k.parse().ok()?),
        };
        let kind = match get("kind")? {
            "out" => Kind::Out { horizon_h: num("horizon")? },
            _ => Kind::Rest {
                keeper: flag("keeper")?,
                since_move_h: match get("since")? {
                    "-" => None,
                    h => Some(h.parse().ok()?),
                },
                left: list("left")?.into_iter().map(|f| Some((f.first()?.parse().ok()?, f.get(1)?.parse().ok()?))).collect::<Option<Vec<_>>>()?,
                band_last_hour: get("band")?.parse().ok()?,
                demand: num("demand")?,
                stale_feed: flag("stale_feed")?,
                speed_known: flag("known")?,
                keeps_restricted: flag("keeps")?,
            },
        };
        Some(Inputs { accounts, b_f: num("bf")?, b_n: num("bn")?, kappa, loads, mover: Mover { class, rate: num("rate")? }, here: get("here")?.parse().ok()?, kind })
    }
}

#[cfg(test)]
fn quant(f: &[&str]) -> Option<Quant> {
    let n = |i: usize| f.get(i)?.parse::<f64>().ok();
    let b = |i: usize| Some(*f.get(i)? == "1");
    Some(Quant { id: f.first()?.parse().ok()?, r: n(1)?, f: n(2)?, t: n(3)?, tf: n(4)?, open: b(5)?, stale: b(6)? })
}

/// What a logged line's inputs decide, read back and run again.
#[cfg(test)]
pub(crate) fn replay(line: &str) -> Option<Option<(u8, Why)>> {
    let i = Inputs::decode(line)?;
    Some(i.decide(&i.plan()))
}

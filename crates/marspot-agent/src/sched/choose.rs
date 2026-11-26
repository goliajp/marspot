//! The selector: constrain first, then optimise, tier by tier.
//!
//! Every candidate the hard constraints left, and "do nothing" when it
//! is allowed, carries three terms.  Money comes first with no
//! tolerance beyond float noise: only the cheapest stay.  A dearer
//! candidate can still win when its efficiency beats the cheapest by
//! more than `delta_eff` and the week's spending for efficiency stays
//! within `week_eff_cap`; that is the only way efficiency buys money.
//! Among the cheapest, efficiency decides, and a move must beat doing
//! nothing by `delta_eff`; then fairness, then fewer actions (doing
//! nothing first), then the order the caller listed them in.
//!
//! Units: money in points (a hundredth of an account's week), efficiency
//! in stall hours, fairness in hours.  Lower is better in each.

/// One candidate's cost in the three tiers.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Terms {
    pub money: f64,
    pub efficiency: f64,
    pub fairness: f64,
}

/// The tier that separated the chosen candidate from the runner-up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    Money,
    BoughtByEfficiency,
    Efficiency,
    Fairness,
    Fewer,
    Id,
}

impl Tier {
    pub fn word(self) -> &'static str {
        match self {
            Tier::Money => "money",
            Tier::BoughtByEfficiency => "bought",
            Tier::Efficiency => "efficiency",
            Tier::Fairness => "fairness",
            Tier::Fewer => "fewer",
            Tier::Id => "id",
        }
    }
}

/// The selector's constants.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Policy {
    /// Two money terms closer than this are equal, points.
    pub eps_money: f64,
    /// How much better in stall hours a candidate must be: to beat doing
    /// nothing, or to be bought with money.
    pub delta_eff: f64,
    /// Points all panes may spend for efficiency in a week.
    pub week_eff_cap: f64,
}

pub const POLICY: Policy = Policy { eps_money: 1e-6, delta_eff: 0.25, week_eff_cap: 1.0 };

/// Efficiency and fairness terms closer than this are equal.
const EPS_TERM: f64 = 1e-9;

/// What `choose` picked: `None` is doing nothing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Choice<A> {
    pub action: Option<A>,
    pub terms: Terms,
    pub decided_by: Tier,
    pub runner_up: Option<(Option<A>, Terms)>,
    /// The week's spending for efficiency once this choice is made.
    pub week_eff_after: f64,
}

/// One entry: candidate `i < n`, or doing nothing at `i == n`.
struct View<'a, A> {
    cands: &'a [(A, Terms)],
    none: Option<Terms>,
}

impl<A: Copy> View<'_, A> {
    fn len(&self) -> usize {
        self.cands.len() + usize::from(self.none.is_some())
    }

    fn is_none(&self, i: usize) -> bool {
        i == self.cands.len()
    }

    fn terms(&self, i: usize) -> Terms {
        if self.is_none(i) { self.none.unwrap_or_default() } else { self.cands[i].1 }
    }

    fn action(&self, i: usize) -> Option<A> {
        if self.is_none(i) { None } else { Some(self.cands[i].0) }
    }

    /// `a` comes before `b` once money is settled: efficiency, fairness,
    /// doing nothing, the caller's order.
    fn before(&self, a: usize, b: usize) -> bool {
        let (ta, tb) = (self.terms(a), self.terms(b));
        let cmp = |x: f64, y: f64| if (x - y).abs() <= EPS_TERM { std::cmp::Ordering::Equal } else { x.total_cmp(&y) };
        let fewer = |i: usize| !self.is_none(i);
        cmp(ta.efficiency, tb.efficiency).then(cmp(ta.fairness, tb.fairness)).then(fewer(a).cmp(&fewer(b))).then(a.cmp(&b)).is_lt()
    }

    fn best(&self, ok: impl Fn(usize) -> bool) -> Option<usize> {
        (0..self.len()).filter(|i| ok(*i)).fold(None, |b, i| match b {
            Some(j) if !self.before(i, j) => Some(j),
            _ => Some(i),
        })
    }
}

/// Picks among `cands` and doing nothing (`none`, when allowed), with
/// `week_eff` points already spent for efficiency this week.
pub fn choose<A: Copy>(cands: &[(A, Terms)], none: Option<Terms>, week_eff: f64, p: &Policy) -> Choice<A> {
    let v = View { cands, none };
    let n = v.len();
    if n == 0 {
        return Choice { action: None, terms: Terms::default(), decided_by: Tier::Money, runner_up: None, week_eff_after: week_eff };
    }
    let m_min = (0..n).map(|i| v.terms(i).money).fold(f64::INFINITY, f64::min);
    let cheap = |i: usize| v.terms(i).money <= m_min + p.eps_money;
    let mut win = settle(&v, &cheap, p);
    let mut bought = 0.0;
    let base = v.terms(win).efficiency;
    let buyable = |i: usize| {
        let d = v.terms(i).money - m_min;
        !cheap(i) && week_eff + d <= p.week_eff_cap + p.eps_money && v.terms(i).efficiency < base - p.delta_eff
    };
    if let Some(b) = v.best(buyable) {
        bought = v.terms(b).money - m_min;
        win = b;
    }
    let runner = (0..n).filter(|i| *i != win).fold(None, |b: Option<usize>, i| match b {
        Some(j) if !full_before(&v, i, j, p) => Some(j),
        _ => Some(i),
    });
    let decided_by = if bought > 0.0 { Tier::BoughtByEfficiency } else { runner.map_or(Tier::Money, |r| tier_between(&v, win, r, p)) };
    Choice {
        action: v.action(win),
        terms: v.terms(win),
        decided_by,
        runner_up: runner.map(|r| (v.action(r), v.terms(r))),
        week_eff_after: week_eff + bought,
    }
}

/// The winner among the cheapest: a move has to beat doing nothing by
/// `delta_eff` when doing nothing is among them.
fn settle<A: Copy>(v: &View<'_, A>, cheap: &impl Fn(usize) -> bool, p: &Policy) -> usize {
    let none_cheap = v.none.is_some() && cheap(v.cands.len());
    let mv = v.best(|i| cheap(i) && !v.is_none(i));
    match (none_cheap, mv) {
        (true, Some(m)) if v.terms(m).efficiency < v.terms(v.cands.len()).efficiency - p.delta_eff => m,
        (true, _) => v.cands.len(),
        (false, Some(m)) => m,
        (false, None) => unreachable!("the cheapest entry is a candidate or doing nothing"),
    }
}

/// The order the runner-up is found by: money, then the rest.
fn full_before<A: Copy>(v: &View<'_, A>, a: usize, b: usize, p: &Policy) -> bool {
    let (ma, mb) = (v.terms(a).money, v.terms(b).money);
    if (ma - mb).abs() > p.eps_money {
        return ma < mb;
    }
    v.before(a, b)
}

fn tier_between<A: Copy>(v: &View<'_, A>, win: usize, run: usize, p: &Policy) -> Tier {
    let (w, r) = (v.terms(win), v.terms(run));
    if (w.money - r.money).abs() > p.eps_money {
        Tier::Money
    } else if (w.efficiency - r.efficiency).abs() > EPS_TERM || v.is_none(win) && r.efficiency < w.efficiency {
        Tier::Efficiency
    } else if (w.fairness - r.fairness).abs() > EPS_TERM {
        Tier::Fairness
    } else if v.is_none(win) != v.is_none(run) {
        Tier::Fewer
    } else {
        Tier::Id
    }
}

#[cfg(test)]
#[path = "choose_tests.rs"]
mod tests;

//! The planner's limits as the settings file sets them: `sched.profile`
//! picks a set, and each `sched.<name>` number overrides one of it.
//! Read on every scan, so a change takes effect at the next one.

use marspot::settings::{SchedProfile, SchedSettings};
use marspot_agent::sched::level2::{Limits, Profile};

pub(super) fn of(s: &SchedSettings) -> Limits {
    let p = match s.profile {
        SchedProfile::Steady => Profile::Steady,
        SchedProfile::Balanced => Profile::Balanced,
        SchedProfile::Full => Profile::Full,
    };
    s.overrides.iter().fold(Limits::of(p), |l, (k, v)| l.with(k, *v))
}

pub(in crate::plugins) fn current() -> Limits {
    of(&marspot::settings::get().sched)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_limits_follow_the_profile_and_its_overrides() {
        let s = |body: &str| marspot::settings::parse(body).sched;
        assert_eq!(of(&s("")), Limits::BALANCED);
        assert_eq!(of(&s("sched.profile = full\n")), Limits::FULL);
        assert_eq!(of(&s("sched.profile = whatever\n")), Limits::STEADY);
        let o = of(&s("sched.profile = steady\nsched.min_stay_h = 2\nsched.bogus = 3\n"));
        assert_eq!(o, Limits::STEADY.with("min_stay_h", 2.0));
        assert_ne!(o.words(), Limits::STEADY.words());
    }
}

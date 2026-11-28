//! How freely the planner may switch: one set of numbers per profile,
//! each of which a single line may override.
//!
//! `steady` is the set the planner was proved with and keeps its
//! replays unchanged; `balanced` (the default) and `full` let a pane
//! move sooner and more often and protect what only Fable can use more
//! strongly; none charges a switch anything beyond its rewrite.  Every
//! plan line names the profile and a digest of the numbers, so one
//! history can be replayed under each set and compared.

/// The profile a user picks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Profile {
    Steady,
    #[default]
    Balanced,
    Full,
}

impl Profile {
    pub fn word(self) -> &'static str {
        match self {
            Profile::Steady => "steady",
            Profile::Balanced => "balanced",
            Profile::Full => "full",
        }
    }
}

/// What other models may do with what only Fable can use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FableGuard {
    /// They may take it, at `pi` a point.
    Price,
    /// While Fable will use it up before the renewal, no other-model
    /// pane is placed on or moved to an account with nothing else left.
    Protect,
    /// As `Protect`, and an unattended other-model pane waits once no
    /// account has anything else left.
    Strict,
}

/// The numbers of one profile.  Hours, counts, and weeks for money.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limits {
    pub profile: Profile,
    /// A pane stays this long after a planned switch.
    pub dwell_h: f64,
    /// A target must last a newcomer this long at the high band.
    pub lasts_h: f64,
    /// Planned switches a pane may take in a week and in a day; 0 is
    /// no limit.
    pub per_week: u8,
    pub per_day: u8,
    /// Planned switches of the fleet in a day.
    pub fleet_per_day: usize,
    /// Planned switches a pane may hold inside the pane horizon.  A
    /// schedule holds at most `HOPS` (two), so every value from two up
    /// plans alike; under two no pane is given a second switch.
    pub hops: usize,
    /// Notices of the fleet in an hour; of one session, the gap between
    /// two and the most in a day.
    pub notices_per_hour: usize,
    pub notice_gap_h: f64,
    pub notices_per_day: u8,
    /// Longest wait for a turn boundary after a notice, minutes.
    pub notice_wait_min: u16,
    /// What a planned switch is charged for the disturbance, beyond its
    /// rewrite.
    pub switch_extra: f64,
    /// What a refusal is charged beyond its rewrite: the turn it breaks.
    pub refusal_extra: f64,
    /// A new structure must be better by this share of the gain of each
    /// switch it changes.
    pub inertia: f64,
    /// A Fable point taken by other models costs a full point whenever
    /// Fable is short, rather than level 1's `pi`.
    pub pi_full: bool,
    pub fable: FableGuard,
    /// Working panes may be noticed and switched at a boundary, not only
    /// to avert a refusal within two hours.
    pub notice_working: bool,
}

impl Limits {
    pub const STEADY: Limits = Limits {
        profile: Profile::Steady,
        dwell_h: 6.0,
        lasts_h: 6.0,
        per_week: 6,
        per_day: 0,
        fleet_per_day: 8,
        hops: 2,
        notices_per_hour: 4,
        notice_gap_h: 3.0,
        notices_per_day: 2,
        notice_wait_min: 20,
        // the planner was proved without either charge, and steady keeps
        // its replays as they were
        switch_extra: 0.0,
        refusal_extra: 0.0,
        inertia: 0.25,
        pi_full: false,
        fable: FableGuard::Price,
        notice_working: false,
    };

    pub const BALANCED: Limits = Limits {
        profile: Profile::Balanced,
        dwell_h: 3.0,
        lasts_h: 3.0,
        per_week: 14,
        per_day: 3,
        fleet_per_day: 16,
        hops: 3,
        notices_per_hour: 6,
        notice_gap_h: 2.0,
        notices_per_day: 4,
        notice_wait_min: 10,
        // as steady: priced by the context a pane has, a switch of a
        // short session costs a few hundredths of a point, and any extra
        // charge holds back the cheap early switches that avert refusals
        switch_extra: 0.0,
        refusal_extra: 0.0,
        inertia: 0.25,
        pi_full: true,
        fable: FableGuard::Protect,
        notice_working: true,
    };

    pub const FULL: Limits = Limits {
        profile: Profile::Full,
        dwell_h: 1.0,
        lasts_h: 1.0,
        per_week: 0,
        per_day: 6,
        fleet_per_day: 32,
        hops: 6,
        notices_per_hour: 8,
        notice_gap_h: 1.0,
        notices_per_day: 8,
        notice_wait_min: 5,
        // as steady and balanced: with either charge, full let Fable
        // lapse where balanced did not and refused more
        switch_extra: 0.0,
        refusal_extra: 0.0,
        inertia: 0.10,
        pi_full: true,
        fable: FableGuard::Strict,
        notice_working: true,
    };

    pub fn of(p: Profile) -> Limits {
        match p {
            Profile::Steady => Limits::STEADY,
            Profile::Balanced => Limits::BALANCED,
            Profile::Full => Limits::FULL,
        }
    }

    /// The names a single line may override, as the settings file
    /// spells them after `sched.`.
    pub const KEYS: [&'static str; 12] = [
        "min_stay_h",
        "lasts_h",
        "per_week",
        "per_day",
        "fleet_per_day",
        "hops",
        "notices_per_hour",
        "notice_gap_h",
        "notices_per_day",
        "switch_extra_points",
        "refusal_extra_points",
        "inertia",
    ];

    /// `self` with `key` (one of `KEYS`) set to `v`; unchanged for any
    /// other key or a value out of range.
    pub fn with(mut self, key: &str, v: f64) -> Limits {
        let count = |lo: f64, hi: f64| (v.fract() == 0.0 && (lo..=hi).contains(&v)).then_some(v);
        let hours = (0.0..=168.0).contains(&v).then_some(v);
        match key {
            "min_stay_h" => self.dwell_h = hours.unwrap_or(self.dwell_h),
            "lasts_h" => self.lasts_h = hours.unwrap_or(self.lasts_h),
            "per_week" => self.per_week = count(0.0, 255.0).map_or(self.per_week, |v| v as u8),
            "per_day" => self.per_day = count(0.0, 255.0).map_or(self.per_day, |v| v as u8),
            "fleet_per_day" => self.fleet_per_day = count(0.0, 1000.0).map_or(self.fleet_per_day, |v| v as usize),
            // read up to six as the profiles are written; a schedule holds two
            "hops" => self.hops = count(0.0, 6.0).map_or(self.hops, |v| v as usize),
            "notices_per_hour" => self.notices_per_hour = count(0.0, 100.0).map_or(self.notices_per_hour, |v| v as usize),
            "notice_gap_h" => self.notice_gap_h = hours.unwrap_or(self.notice_gap_h),
            "notices_per_day" => self.notices_per_day = count(0.0, 255.0).map_or(self.notices_per_day, |v| v as u8),
            "switch_extra_points" if (0.0..=100.0).contains(&v) => self.switch_extra = v / 100.0,
            "refusal_extra_points" if (0.0..=100.0).contains(&v) => self.refusal_extra = v / 100.0,
            "inertia" if (0.0..=1.0).contains(&v) => self.inertia = v,
            _ => {}
        }
        self
    }

    /// Planned switches a pane that made `week` and `day` of them may
    /// still take.
    pub fn switches_left(&self, week: usize, day: usize) -> u8 {
        let left = |cap: u8, used: usize| if cap == 0 { u8::MAX } else { cap.saturating_sub(used.min(255) as u8) };
        left(self.per_week, week).min(left(self.per_day, day))
    }

    /// `profile=… limits=…`: the profile and a digest of every number,
    /// so two plans read under different numbers differ in their line.
    pub fn words(&self) -> String {
        format!("profile={} limits={:08x}", self.profile.word(), self.digest() as u32)
    }

    /// A digest that does not depend on the platform: integers only.
    pub fn digest(&self) -> u64 {
        let q = |x: f64| (x * 1e4).round() as i64 as u64;
        let fields = [
            q(self.dwell_h),
            q(self.lasts_h),
            u64::from(self.per_week),
            u64::from(self.per_day),
            self.fleet_per_day as u64,
            self.hops as u64,
            self.notices_per_hour as u64,
            q(self.notice_gap_h),
            u64::from(self.notices_per_day),
            u64::from(self.notice_wait_min),
            q(self.switch_extra * 100.0),
            q(self.refusal_extra * 100.0),
            q(self.inertia),
            u64::from(self.pi_full),
            self.fable as u64,
            u64::from(self.notice_working),
        ];
        // FNV-1a over the fields' bytes
        fields.iter().flat_map(|f| f.to_le_bytes()).fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
    }

    /// The price of a Fable point taken by other models, given level 1's.
    pub fn pi(&self, level1: f64) -> f64 {
        if self.pi_full && level1 > 0.0 { 1.0 } else { level1 }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Limits::BALANCED
    }
}

#[cfg(test)]
#[path = "limits_tests.rs"]
mod tests;

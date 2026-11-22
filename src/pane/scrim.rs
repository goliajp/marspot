//! The eased dim drawn over a pane that does not have the focus.

/// An eased dim: where it is, where it is going, and when it set off.
///
/// Deliberately not a timer.  The value is a pure function of the
/// clock, sampled when a frame is being built anyway, and it *ends* —
/// once the target is reached nothing here asks for another frame, and
/// the window goes back to drawing only when something changes
/// (idle CPU must be ~0 %, no animation timers).
#[derive(Debug, Clone, Copy)]
pub struct ScrimFade {
    from: f32,
    to: f32,
    since: Option<std::time::Instant>,
}

impl Default for ScrimFade {
    fn default() -> Self {
        Self { from: 0.0, to: 0.0, since: None }
    }
}

/// How long a dim takes to arrive.
///
/// Long enough to read as movement rather than a jump, short enough
/// that a pane you have just left has receded before you have
/// finished looking away.
pub const SCRIM_FADE: std::time::Duration = std::time::Duration::from_millis(220);

impl ScrimFade {
    /// Point the fade at `target`.
    ///
    /// Brightening to full is immediate: the pane you just clicked into
    /// must be *yours* the instant you click, and a fade there reads as
    /// lag in the click rather than as motion.  Everything else eases.
    pub fn aim(&mut self, target: f32, now: std::time::Instant) {
        if (self.to - target).abs() < f32::EPSILON {
            return;
        }
        let current = self.value(now);
        self.from = current;
        self.to = target;
        self.since = (target > current).then_some(now);
    }

    /// What to draw right now.
    pub fn value(&self, now: std::time::Instant) -> f32 {
        let Some(since) = self.since else { return self.to };
        let t = now.duration_since(since).as_secs_f32() / SCRIM_FADE.as_secs_f32();
        if t >= 1.0 {
            return self.to;
        }
        // Ease-out cubic: most of the change up front, settling gently.
        // A linear ramp reads as mechanical at this duration.
        let eased = 1.0 - (1.0 - t).powi(3);
        self.from + (self.to - self.from) * eased
    }

    /// Is it still moving?  The caller uses this to ask for one more
    /// frame — and to stop asking.
    pub fn is_moving(&self, now: std::time::Instant) -> bool {
        self.since
            .is_some_and(|s| now.duration_since(s) < SCRIM_FADE)
    }
}

#[cfg(test)]
mod scrim_fade_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Brightening is instant: the pane you just clicked into is
    /// *yours* the moment you click it.  A fade here reads as lag in
    /// the click, not as motion.
    #[test]
    fn focus_arrives_at_full_brightness_immediately() {
        let t0 = Instant::now();
        let mut f = ScrimFade::default();
        f.aim(0.25, t0);
        // …dimmed over time…
        assert!(f.value(t0 + SCRIM_FADE) > 0.2);
        // …and back to nothing the instant it is focused.
        f.aim(0.0, t0 + SCRIM_FADE);
        assert_eq!(f.value(t0 + SCRIM_FADE), 0.0);
        assert!(!f.is_moving(t0 + SCRIM_FADE), "nothing left to animate");
    }

    /// Dimming eases: it starts where it was, ends where it is going,
    /// and is somewhere between in between.
    #[test]
    fn dimming_travels_rather_than_jumps() {
        let t0 = Instant::now();
        let mut f = ScrimFade::default();
        f.aim(0.5, t0);
        assert_eq!(f.value(t0), 0.0, "starts where it was");
        let mid = f.value(t0 + SCRIM_FADE / 2);
        assert!(mid > 0.0 && mid < 0.5, "somewhere in between, got {mid}");
        assert_eq!(f.value(t0 + SCRIM_FADE), 0.5, "arrives");
        assert_eq!(f.value(t0 + SCRIM_FADE * 3), 0.5, "and stays");
    }

    /// **The animation has to end.**  Idle CPU is a hard constraint: a
    /// fade that kept reporting "still moving" would hold the render
    /// loop awake for the life of the process.
    #[test]
    fn a_fade_stops_asking_for_frames() {
        let t0 = Instant::now();
        let mut f = ScrimFade::default();
        f.aim(0.5, t0);
        assert!(f.is_moving(t0), "moving while it travels");
        assert!(f.is_moving(t0 + SCRIM_FADE / 2));
        assert!(!f.is_moving(t0 + SCRIM_FADE), "arrived — stop drawing");
        assert!(!f.is_moving(t0 + Duration::from_secs(3600)));
    }

    /// Aiming at where it is already going changes nothing — otherwise
    /// every frame would restart the fade and it would never arrive.
    #[test]
    fn re_aiming_at_the_same_target_does_not_restart_it() {
        let t0 = Instant::now();
        let mut f = ScrimFade::default();
        f.aim(0.5, t0);
        let quarter = t0 + SCRIM_FADE / 4;
        let at_quarter = f.value(quarter);
        f.aim(0.5, quarter); // the render loop, one frame later
        assert_eq!(f.value(quarter), at_quarter, "same target, same progress");
        assert!(!f.is_moving(t0 + SCRIM_FADE), "and it still arrives on time");
    }

    /// Changing target mid-flight continues from where the picture
    /// actually is, not from where the last fade started — otherwise a
    /// pane that dims and then dims further visibly jumps backwards.
    #[test]
    fn a_new_target_mid_fade_starts_from_what_is_on_screen() {
        let t0 = Instant::now();
        let mut f = ScrimFade::default();
        f.aim(0.25, t0);
        let half = t0 + SCRIM_FADE / 2;
        let shown = f.value(half);
        f.aim(0.75, half);
        assert_eq!(f.value(half), shown, "no jump at the moment of change");
        assert_eq!(f.value(half + SCRIM_FADE), 0.75);
    }
}

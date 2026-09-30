//! What a fresh install does before anyone touches a setting.
//!
//! These are the answers a new user gets, and a default is the one
//! decision that reaches everybody — including the people who never
//! open the settings panel.  Each was reviewed on 2026-10-01 and each
//! stands; the point of pinning them is that the next change to one
//! has to be deliberate rather than a value someone adjusted while
//! debugging.
//!
//! If you are here because this test failed: that is the test working.
//! Change the number here too, and say in the commit why the new
//! answer is better for someone who has never seen this program.

use marspot_term::settings::Settings;

#[test]
fn a_fresh_install_reclaims_idle_agent_panes_after_half_an_hour() {
    let d = Settings::default();
    // Only claudecode panes are touched, and only ones whose session
    // can be named — `reclaim_op` refuses without a uuid, because
    // taking claude down without a resume line would lose it.  The
    // pitch of this program is that twenty panes stay cheap, so
    // reclaiming idle ones is the product rather than a tweak.
    assert!(d.reclaim_enabled);
    assert_eq!(d.reclaim_idle_minutes, 30);
    // Wake them on return rather than on the click that wants one:
    // the wait is what the user would notice, and the work happens
    // either way.
    assert!(d.reclaim_prefetch);
}

#[test]
fn a_fresh_install_does_not_change_what_text_means() {
    let d = Settings::default();
    // `<u>…</u>` as underline is marspot's own extension.  On by
    // default, a program printing those four characters would have
    // them eaten.
    assert!(!d.render_u_tags);
    // The circled family stays one cell.  Widening moves the wrap
    // point, and a paragraph that scrolled past one comes back with
    // characters stranded in the margin — measured 2026-08-08,
    // shipped and reverted inside the hour.
    assert!(!d.appearance_circled_wide);
}

#[test]
fn the_attention_ladder_ships_at_full_strength() {
    // `dim_scale` is how loudly the ladder says which pane you are in,
    // not SGR 2.  Zero turns it off; one is the designed strength.
    assert_eq!(Settings::default().dim_scale, 1.0);
    assert_eq!(Settings::default().scroll_factor, 1.0);
}

#[test]
fn integrations_are_opt_in() {
    // Writing into someone's claude statusline config is not something
    // to do because they installed a terminal.
    assert!(!Settings::default().cc_statusline_hook);
}

/// Never is expressible two ways and they have to agree.
#[test]
fn the_switch_and_the_zero_both_mean_never() {
    let mut s = Settings::default();
    assert!(s.reclaim_after().is_some(), "the default is a real interval");

    s.reclaim_enabled = false;
    assert!(s.reclaim_after().is_none());

    s.reclaim_enabled = true;
    s.reclaim_idle_minutes = 0;
    assert!(s.reclaim_after().is_none(), "zero minutes is never, not immediately");
}

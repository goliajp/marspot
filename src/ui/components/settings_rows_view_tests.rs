//! A click in the settings panel rewrites the whole file, keys it has no
//! row for included: what such a key says must come back as written.

use super::settings_rows::rows;
use crate::settings::{self, Rebalance};

/// Every click the panel can make, on a file that sets the rebalance
/// mode by hand, writes the mode back as it was: `live` stays `live`,
/// `dryrun` stays `dryrun`, `off` stays `off`.
#[test]
fn settings_panel_writes_the_rebalance_mode_back_as_it_was() {
    for (word, want) in [("live", Rebalance::Live), ("dryrun", Rebalance::DryRun), ("off", Rebalance::Off)] {
        let line = format!("claudecode.rebalance = {word}");
        let file = format!("# by hand\n{line}\nreclaim.enabled = true\n");
        let read = settings::parse(&file);
        assert_eq!(read.cc_rebalance, want, "{word}");
        let mut clicks = 0;
        for spec in rows() {
            for seg in 0..8 {
                // what the panel's click handler writes: the settings in
                // force with the click applied, over the file on disk
                let Some(next) = spec.row.apply(&read, seg) else { continue };
                clicks += 1;
                let written = settings::render(&next, &file);
                assert!(written.lines().any(|l| l == line), "{:?} seg {seg} on {word}: {written}", spec.row);
                assert_eq!(written.matches("claudecode.rebalance").count(), 1, "{written}");
                assert_eq!(settings::parse(&written).cc_rebalance, want, "{:?} seg {seg} on {word}", spec.row);
            }
        }
        assert!(clicks > 10, "every row was clicked: {clicks}");
    }
}

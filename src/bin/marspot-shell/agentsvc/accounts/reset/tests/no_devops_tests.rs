//! A machine without devops: a claim confirmed on the accounts panel
//! goes ahead, one sent to the control socket is refused at once with
//! where to claim instead, and the log says nothing of devops.

use std::time::Instant;

use super::*;
use marspot_term::test_env::{EnvGuard, TempDir};

#[test]
fn reset_without_devops_the_panel_claims_and_the_socket_is_refused_at_once() {
    let dir = TempDir::new("reset-no-devops");
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut env = EnvGuard::new();
    env.set("HOME", &home).set("MARSPOT_STATE_DIR", dir.join("state"));
    let log = crate::test_log::open();
    let l = ledger(&dir);

    let mut acted = Vec::new();
    let started = Instant::now();
    let socket = run_with(&l, claim(Provider::Codex, 1, "nd-1"), TOTAL_WITHIN, socket_check("tok-1".into()), |c, _| {
        acted.push(c.request_id.clone());
        Outcome::of(Res::Reset, "used")
    });
    assert_eq!((socket.result, socket.detail.as_str()), (Res::Unavailable, verify::NO_DEVOPS));
    assert!(started.elapsed() < Duration::from_secs(1), "no wait for anything: {:?}", started.elapsed());

    let panel = Claim { by: By::MarspotUi, ..claim(Provider::Codex, 2, "nd-2") };
    let used = run_with(&l, panel, TOTAL_WITHIN, panel_check, |c, _| {
        acted.push(c.request_id.clone());
        Outcome::of(Res::Reset, "used")
    });
    assert_eq!(used.result, Res::Reset);
    assert_eq!(acted, ["nd-2"], "only the panel's claim went ahead");
    let rows = fact_rows(&dir);
    assert!(rows.iter().any(|r| r.contains("\tnd-2\tcodex\t2\t") && r.contains("\tmarspot-ui\treset\t")), "{rows:#?}");

    let text = std::fs::read_to_string(&log).unwrap();
    assert!(crate::test_log::devops_lines(&text).is_empty(), "{text}");
    assert!(!home.join(".config/devops").exists());
}

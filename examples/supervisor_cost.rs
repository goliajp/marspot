//! What the supervisor's per-wake work costs, against the machine as
//! it is right now.
//!
//! The supervisor wakes four times a second forever and, with
//! seventeen panes open, burns four times the CPU that all seventeen
//! panes together do. This measures the work it does on that path --
//! the registry read, the per-pane process observation, the whole
//! process table the plugins walk, the settings stat -- and scales
//! each by the cadence its caller actually uses.
//!
//! Run it while the app is open; it reads the live session registry.
//!
//! On 2026-10-01, with eighteen sessions and 2285 processes on the
//! machine, the answer was that all of it together comes to about a
//! tenth of one percent of a core. The supervisor was using 4.4%. So
//! the cost is not in the work -- it is in the waking, and the thing
//! to change is how often that happens rather than what it does.
fn main() {
    let entries = marspot_term::session_registry::list_session_entries();
    let pids: Vec<i32> = entries.iter().map(|e| e.shell_child_pid).collect();
    println!("sessions: {}", pids.len());

    let bench = |name: &str, n: u32, f: &mut dyn FnMut()| {
        f(); // warm
        let t = std::time::Instant::now();
        for _ in 0..n {
            f();
        }
        let us = t.elapsed().as_micros() as f64 / n as f64;
        println!("{name:34} {us:>9.0} us");
        us
    };

    let a = bench("list_session_entries", 50, &mut || {
        let _ = marspot_term::session_registry::list_session_entries();
    });
    let b = bench("observe_pane x all", 50, &mut || {
        for &p in &pids {
            let _ = marspot::pidtree::observe_pane(p);
        }
    });
    let c = bench("list_all_procs (whole table)", 20, &mut || {
        let _ = marspot::pidtree::list_all_procs();
    });
    let procs = marspot::pidtree::list_all_procs();
    println!("   (the table has {} processes)", procs.len());
    let d = bench("descendants_of x all", 20, &mut || {
        for &p in &pids {
            let _ = marspot::pidtree::descendants_of(p, &procs);
        }
    });
    let e = bench("settings stat", 200, &mut || {
        let _ = marspot_term::settings::reload_if_changed();
    });

    println!("\nif each ran at the cadence its caller uses:");
    for (name, us, hz) in [
        ("pane sweep (1 Hz)", a + b, 1.0),
        ("plugin scan (0.5 Hz)", c + d, 0.5),
        ("settings stat (4 Hz)", e, 4.0),
    ] {
        println!("  {name:26} {:>7.3} % of a core", us * hz / 1e6 * 100.0);
    }
}

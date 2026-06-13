//! Write a never-ending output loop into every live shelld session — the
//! workload driver for the core-side A1 soak (`bin/soak-l3-core-drift.sh`).
//!
//! It connects to shelld exactly as a client, lists the live sessions, and
//! writes `while :; do seq 1 200; done\n` to each one's PTY. The real L3
//! processes (already attached) then pump that flood and publish, and
//! marspot-core re-renders continuously — so the harness can watch *core's*
//! RSS for drift under sustained 9-session load (the renderer half of A1).
//!
//!     MARSPOT_STATE_DIR=/tmp/marspot-dev \
//!       cargo run -p marspot-session --example flood_sessions

use std::time::Duration;

use marspot_term::paths::shelld_socket;
use marspot_term::shelld_client::ShelldClient;

const FLOOD: &str = "while :; do seq 1 200; done\n";

fn main() {
    let sock = shelld_socket();
    let client = match ShelldClient::connect(&sock, || {}) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("flood: shelld connect at {} failed: {e}", sock.display());
            std::process::exit(1);
        }
    };
    let alive: Vec<u64> = client
        .list_sessions()
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.alive)
        .map(|s| s.session_id)
        .collect();
    if alive.is_empty() {
        eprintln!("flood: no live sessions to flood");
        std::process::exit(1);
    }
    let mut n = 0;
    // Keep the sessions in scope until after the writes flush, then drop
    // (detach) — the shells keep running the loop regardless of subscribers.
    let mut held = Vec::new();
    for id in &alive {
        match client.attach(*id, 80, 24) {
            Ok(mut s) => {
                if let Err(e) = s.write(FLOOD.as_bytes()) {
                    eprintln!("flood: write to session {id} failed: {e}");
                } else {
                    n += 1;
                }
                held.push(s);
            }
            Err(e) => eprintln!("flood: attach {id} failed: {e}"),
        }
    }
    // Give shelld a moment to push the writes to the PTYs before we detach.
    std::thread::sleep(Duration::from_millis(300));
    drop(held);
    println!("flood: started continuous output on {n}/{} session(s)", alive.len());
}

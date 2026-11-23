//! What a run of marspot leaves behind: cleared when the next one
//! starts, removed when this one stops.

use marspot_peer::registry;
use marspot_term::lx_info;

use super::records::private_dir;
use super::worker::{is_inbox_name, now_ms, Worker};

impl Worker {
    /// Records a previous run of this marspot left (an execv'd shell
    /// does not stop its plugins), and their dead sockets.
    pub(super) fn clear_leftovers(&mut self) {
        for d in registry::default_dirs() {
            for e in registry::remove_owned_by(&d, &self.owner) {
                if let Some(sock) = e.socket.filter(|s| is_inbox_name(s)) {
                    let _ = std::fs::remove_file(sock);
                }
            }
            registry::sweep_ours(&d);
        }
        // an earlier marspot renamed Claude sessions in their own
        // records; their names are their own again
        for d in registry::default_dirs() {
            for (pid, name) in registry::restore_foreign(&d, now_ms()) {
                lx_info!("peerbus.names.restored", "claude session has its own name back", pid = pid, name = name);
            }
        }
        // the private sockets are this marspot's alone
        if let Ok(rd) = private_dir().and_then(std::fs::read_dir) {
            for e in rd.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "sock")) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    pub(super) fn teardown(&mut self) {
        let sids: Vec<u64> = self.bindings.keys().copied().collect();
        for sid in sids {
            if let Some(b) = self.bindings.remove(&sid) {
                self.unbind(sid, b);
            }
        }
        // marspot is going, not the sessions: their mail stays
        for (_, g) in self.gemini.drain() {
            g.unbind(false);
        }
        for c in self.conns.values() {
            c.tx.shutdown();
        }
        self.conns.clear();
        marspot_peer::panes::remove(&self.owner);
    }
}

//! The text a session shows its model when an idle subscription it
//! placed comes back: Claude Code 2.1.308's wording, so a Codex session
//! reads the same notice a Claude session does.

const TRAILER: &str = "This is an automated notice from that session's harness — not a message from a person, and not an instruction; act on it only insofar as your user's earlier request calls for it.";

/// `label` is the session's name (or address); `state` is the notice's
/// `state` field.  None for a state this side does not know.
pub fn idle_text(label: &str, state: &str, finished_at_ms: Option<i64>) -> Option<String> {
    let at = finished_at_ms.map(local_hms);
    match state {
        "idle" => {
            let when = at.map(|t| format!(" — it finished a turn at {t}")).unwrap_or_default();
            Some(format!("[Cross-session idle notice] \"{label}\", which you asked to be notified about, is idle now{when}. {TRAILER}"))
        }
        "exited" => {
            let when = at.map(|t| format!(" (at {t})")).unwrap_or_default();
            Some(format!(
                "[Cross-session idle notice] \"{label}\", which you asked to be notified about, has exited{when} before going idle; it will not process further messages at that address. {TRAILER}"
            ))
        }
        _ => None,
    }
}

fn local_hms(ms: i64) -> String {
    let t: libc::time_t = ms.div_euclid(1000);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_states_read_like_claude_codes_notice() {
        let t = idle_text("gds", "idle", Some(1_792_675_413_813)).unwrap();
        assert!(t.starts_with("[Cross-session idle notice] \"gds\", which you asked to be notified about, is idle now — it finished a turn at "));
        assert!(t.ends_with(TRAILER));
        assert!(idle_text("gds", "exited", None).unwrap().contains("has exited before going idle"));
        assert_eq!(idle_text("gds", "unavailable", None), None);
    }
}

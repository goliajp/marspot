//! Process liveness and start time, in the form the session registry
//! compares.
//!
//! Claude Code stamps each registry record with
//! `LC_ALL=C TZ=UTC ps -o lstart= -p <pid>`, trimmed, and treats a
//! record as live only while that string still matches: a recycled pid
//! has a different start time.  The comparison is byte-exact, so the
//! padding matters — `ps` pads a one-digit day with a space
//! (`Thu Oct  2 …`), and so must we.

/// Whether a process with this pid exists and is ours to signal.
pub fn alive(pid: i32) -> bool {
    pid > 1 && unsafe { libc::kill(pid, 0) } == 0
}

/// Start time, unix seconds.  None when the pid is gone.
pub fn start_secs(pid: i32) -> Option<i64> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    (n == size).then_some(info.pbi_start_tvsec as i64)
}

/// The registry's start-time string for a live pid.
pub fn lstart(pid: i32) -> Option<String> {
    start_secs(pid).map(lstart_utc)
}

/// `%a %b %e %H:%M:%S %Y` in UTC, as BSD `ps -o lstart` prints it.
pub fn lstart_utc(secs: i64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{} {} {:>2} {:02}:{:02}:{:02} {}",
        DAYS[days.rem_euclid(7) as usize],
        MONTHS[(m - 1) as usize],
        d,
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        y
    )
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_dates_format_with_ps_padding() {
        assert_eq!(lstart_utc(0), "Thu Jan  1 00:00:00 1970");
        assert_eq!(lstart_utc(1_792_578_775), "Wed Oct 21 10:32:55 2026");
        assert_eq!(lstart_utc(951_782_400), "Tue Feb 29 00:00:00 2000");
        assert_eq!(lstart_utc(1_791_072_000), "Sun Oct  4 00:00:00 2026");
    }

    /// Against the tool Claude Code itself runs, for a live process.
    #[test]
    fn matches_ps_for_this_process() {
        let pid = std::process::id() as i32;
        let out = std::process::Command::new("ps")
            .args(["-o", "lstart=", "-p", &pid.to_string()])
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .output()
            .unwrap();
        let ps = String::from_utf8(out.stdout).unwrap();
        assert_eq!(lstart(pid).unwrap(), ps.trim());
    }

    #[test]
    fn a_dead_pid_has_no_start_time() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        child.wait().unwrap();
        assert_eq!(lstart(pid), None);
        assert!(!alive(pid));
        assert!(alive(std::process::id() as i32));
    }
}

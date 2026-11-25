//! Clock times in gemini's quota texts, and the Pacific midnight an
//! API key's daily quota renews at.

/// `4:30 PM PST` (how gemini formats the reset, in its own time zone):
/// the first such moment after `at`.  An abbreviation it does not know
/// is taken as this machine's zone, which is gemini's when it runs here.
pub(super) fn wall_clock(s: &str, at: i64) -> Option<i64> {
    let mut words = s.split_whitespace();
    let (h, m) = words.next()?.split_once(':')?;
    let (h, m): (i64, i64) = (h.parse().ok()?, m.parse().ok()?);
    let pm = match words.next()? {
        "AM" => false,
        "PM" => true,
        _ => return None,
    };
    if !(1..=12).contains(&h) || !(0..60).contains(&m) {
        return None;
    }
    let minutes = (h % 12 + if pm { 12 } else { 0 }) * 60 + m;
    let zone = words.next().map(|z| z.trim_end_matches('.'));
    match zone.and_then(offset_h) {
        Some(off) => {
            let local = at + off;
            let day = local.div_euclid(86_400) * 86_400;
            let t = day + minutes * 60 - off;
            Some(if t > at { t } else { t + 86_400 })
        }
        None => Some(local_wall(at, minutes)),
    }
}

/// Seconds east of UTC for a zone name gemini may print.
fn offset_h(z: &str) -> Option<i64> {
    let h = match z {
        "UTC" | "GMT" => 0,
        "PST" => -8,
        "PDT" | "MST" => -7,
        "MDT" | "CST" => -6,
        "CDT" | "EST" => -5,
        "EDT" => -4,
        _ => {
            let off = z.strip_prefix("GMT").or_else(|| z.strip_prefix("UTC"))?;
            let (sign, rest) = match off.as_bytes().first()? {
                b'+' => (1, &off[1..]),
                b'-' => (-1, &off[1..]),
                _ => return None,
            };
            let (h, m) = rest.split_once(':').unwrap_or((rest, "0"));
            return Some(sign * (h.parse::<i64>().ok()? * 3600 + m.parse::<i64>().ok()? * 60));
        }
    };
    Some(h * 3600)
}

fn local_wall(at: i64, minutes: i64) -> i64 {
    let t = at as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    tm.tm_hour = (minutes / 60) as i32;
    tm.tm_min = (minutes % 60) as i32;
    tm.tm_sec = 0;
    tm.tm_isdst = -1;
    let mut r = unsafe { libc::mktime(&mut tm) } as i64;
    if r <= at {
        tm.tm_mday += 1;
        tm.tm_isdst = -1;
        r = unsafe { libc::mktime(&mut tm) } as i64;
    }
    r
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// The `n`th Sunday (1-based) of month `m` of year `y`, as a day number.
fn nth_sunday(y: i64, m: i64, n: i64) -> i64 {
    let first = days_from_civil(y, m, 1);
    let to_sunday = (7 - (first + 4).rem_euclid(7)) % 7;
    first + to_sunday + 7 * (n - 1)
}

/// The Pacific zone's offset at `t`: daylight time from 2 a.m. on the
/// second Sunday of March to 2 a.m. on the first Sunday of November.
fn pacific_offset(t: i64) -> i64 {
    let year = 1970 + t.div_euclid(86_400 * 365 + 21_600).clamp(0, 400);
    let starts = nth_sunday(year, 3, 2) * 86_400 + 10 * 3600;
    let ends = nth_sunday(year, 11, 1) * 86_400 + 9 * 3600;
    if (starts..ends).contains(&t) { -7 * 3600 } else { -8 * 3600 }
}

pub(crate) fn pacific_midnight_after(at: i64) -> i64 {
    let off = pacific_offset(at);
    let midnight = ((at + off).div_euclid(86_400) + 1) * 86_400;
    midnight - pacific_offset(midnight - off)
}

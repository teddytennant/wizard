// OWNER: dialogs
//! Wall-clock text the way opencode prints it (`Locale.time`, `toDateString`), in local time.
//! The offset is passed in so tests do not depend on the machine's zone.

const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Seconds east of UTC at `now` in the process's time zone.
pub fn local_offset(now: i64) -> i64 {
    #[cfg(unix)]
    {
        // SAFETY: `localtime_r` writes only into the `tm` we pass and reads `now` by value.
        unsafe {
            let t: libc::time_t = now as libc::time_t;
            let mut tm: libc::tm = std::mem::zeroed();
            if !libc::localtime_r(&t, &mut tm).is_null() {
                return tm.tm_gmtoff as i64;
            }
        }
    }
    0
}

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Days since the epoch to (year, month, day), Howard Hinnant's algorithm.
fn civil(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `Mon Oct 05 2026` (JS `toDateString`).
pub fn date_string(secs: i64, off: i64) -> String {
    let days = (secs + off).div_euclid(86_400);
    let (y, m, d) = civil(days);
    format!(
        "{} {} {:02} {}",
        DAYS[days.rem_euclid(7) as usize],
        MONTHS[(m - 1) as usize],
        d,
        y
    )
}

/// `1:30 PM`.
pub fn time(secs: i64, off: i64) -> String {
    let s = (secs + off).rem_euclid(86_400);
    let (h, m) = (s / 3600, (s % 3600) / 60);
    let h12 = match h % 12 {
        0 => 12,
        x => x,
    };
    format!("{h12}:{m:02} {}", if h < 12 { "AM" } else { "PM" })
}

/// `10:04 PM · 10/5/2026`.
pub fn datetime(secs: i64, off: i64) -> String {
    let (y, m, d) = civil((secs + off).div_euclid(86_400));
    format!("{} · {m}/{d}/{y}", time(secs, off))
}

/// `just now`, `5m ago`, `3h ago`, `2d ago`, else the full date.
pub fn relative(now: i64, then: i64, off: i64) -> String {
    let secs = (now - then).max(0);
    let (mins, hours, days) = (secs / 60, secs / 3600, secs / 86_400);
    if secs < 60 {
        "just now".into()
    } else if mins < 60 {
        format!("{mins}m ago")
    } else if hours < 24 {
        format!("{hours}h ago")
    } else if days < 7 {
        format!("{days}d ago")
    } else {
        datetime(then, off)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_js_formats() {
        assert_eq!(date_string(0, 0), "Thu Jan 01 1970");
        assert_eq!(date_string(1_791_158_400, 0), "Mon Oct 05 2026");
        assert_eq!(time(13 * 3600 + 30 * 60, 0), "1:30 PM");
        assert_eq!(time(0, 0), "12:00 AM");
        assert_eq!(time(12 * 3600, 0), "12:00 PM");
        assert_eq!(
            datetime(1_791_158_400 + 22 * 3600 + 4 * 60, 0),
            "10:04 PM · 10/5/2026"
        );
    }

    #[test]
    fn offset_moves_the_day() {
        // 23:30 UTC on Oct 5 is already Oct 6 at +1h
        let t = 1_791_158_400 + 23 * 3600 + 1800;
        assert_eq!(date_string(t, 0), "Mon Oct 05 2026");
        assert_eq!(date_string(t, 3600), "Tue Oct 06 2026");
    }

    #[test]
    fn relative_buckets() {
        assert_eq!(relative(1000, 990, 0), "just now");
        assert_eq!(relative(1000, 1000 - 300, 0), "5m ago");
        assert_eq!(relative(100_000, 100_000 - 3 * 3600, 0), "3h ago");
        assert_eq!(relative(1_000_000, 1_000_000 - 2 * 86_400, 0), "2d ago");
    }
}

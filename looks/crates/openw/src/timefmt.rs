//! Wall-clock text for the transcript: opencode's `Locale.todayTimeOrDateTime`, in the local
//! zone (`1:26 PM` for today, `1:26 PM · 10/5/2026` otherwise).

use std::time::SystemTime;

/// Local broken-down time of a unix timestamp: `(year, month 1..=12, day, hour, minute)`.
fn local(secs: i64) -> Option<(i32, u32, u32, u32, u32)> {
    // SAFETY: localtime_r only writes into the zeroed tm we hand it.
    unsafe {
        let t: libc::time_t = secs as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return None;
        }
        Some((
            tm.tm_year + 1900,
            (tm.tm_mon + 1) as u32,
            tm.tm_mday as u32,
            tm.tm_hour as u32,
            tm.tm_min as u32,
        ))
    }
}

fn unix(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// `h:mm AM/PM` of a local hour and minute.
fn clock(h: u32, m: u32) -> String {
    let (h12, ap) = match h {
        0 => (12, "AM"),
        1..=11 => (h, "AM"),
        12 => (12, "PM"),
        _ => (h - 12, "PM"),
    };
    format!("{h12}:{m:02} {ap}")
}

/// Today's messages show the time; older ones add the date.
pub fn today_time_or_date_time(at: SystemTime, now: SystemTime) -> String {
    let (Some((y, mo, d, h, mi)), Some(today)) = (local(unix(at)), local(unix(now))) else {
        return String::new();
    };
    if (y, mo, d) == (today.0, today.1, today.2) {
        clock(h, mi)
    } else {
        format!("{} · {mo}/{d}/{y}", clock(h, mi))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_is_twelve_hour() {
        assert_eq!(clock(0, 5), "12:05 AM");
        assert_eq!(clock(13, 26), "1:26 PM");
        assert_eq!(clock(12, 0), "12:00 PM");
        assert_eq!(clock(9, 30), "9:30 AM");
    }

    #[test]
    fn same_day_shows_only_the_time() {
        let now = SystemTime::now();
        let s = today_time_or_date_time(now, now);
        assert!(s.ends_with("AM") || s.ends_with("PM"), "{s}");
        assert!(!s.contains('·'));
        let old = now - std::time::Duration::from_secs(5 * 86_400);
        assert!(today_time_or_date_time(old, now).contains(" · "));
    }
}

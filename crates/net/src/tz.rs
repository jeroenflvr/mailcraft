//! The computer's local time zone, for calendar times: its offset from UTC at every instant
//! (daylight saving included) and its name.

use chrono::Local;
use mailcraft_model::dates::Zone;

/// Years before and after now whose changes of clocks are read.
const YEARS: i64 = 30;

/// The zone's IANA name (`Europe/Brussels`), when the system says it: `TZ` if it names a zone,
/// else the platform's setting.
fn name() -> String {
    std::env::var("TZ")
        .ok()
        .map(|t| t.trim().trim_start_matches(':').to_string())
        .filter(|t| t.contains('/') && !t.starts_with('/') && !t.contains(','))
        .or_else(|| iana_time_zone::get_timezone().ok())
        .unwrap_or_default()
}

/// The local offset (seconds east of UTC) at a Unix time.
fn offset_at(t: i64) -> i32 {
    use chrono::TimeZone;
    Local.timestamp_opt(t, 0).single().map_or(0, |d| d.offset().local_minus_utc())
}

/// The local zone, read from the system by asking what its offset was at every week from 30 years
/// ago to 30 years ahead (each change then pinned to the second). Takes a few milliseconds.
pub fn local_zone() -> Zone {
    let now = Local::now().timestamp();
    Zone::probe(name(), offset_at, now - YEARS * 366 * 86_400, now + YEARS * 366 * 86_400)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_local_zone_agrees_with_the_system() {
        let began = std::time::Instant::now();
        let z = local_zone();
        assert!(began.elapsed().as_millis() < 500, "{:?}", began.elapsed());
        let now = Local::now().timestamp();
        for t in [now, now - 100 * 86_400, now + 200 * 86_400, now + 5 * 365 * 86_400] {
            assert_eq!(z.offset_at(t), offset_at(t), "at {t} in {}", z.id);
        }
        assert!(z.transitions.len() < 400);
    }
}

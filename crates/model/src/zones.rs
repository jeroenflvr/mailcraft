//! Common time zones by name, for calendar files that say `TZID=Europe/Brussels` (or a Windows
//! name such as `W. Europe Standard Time`) without carrying a `VTIMEZONE`.
//!
//! Each zone is a standard offset plus one of five daylight-saving rule sets. The rules are the
//! current ones and are applied to every year (the US before 2007 and Australia before 2008 had
//! other dates; a series that old reads a few weeks wrong at most). The Windows names follow the
//! Unicode CLDR `windowsZones` mapping, limited to the zones listed here.

use crate::dates::{Zone, ZoneRule};

#[derive(Clone, Copy)]
enum Dst {
    None,
    /// Last Sunday of March to last Sunday of October, at 01:00 UTC.
    Eu,
    /// Second Sunday of March to first Sunday of November, at 02:00 local.
    Us,
    /// First Sunday of October to first Sunday of April, at 02:00 / 03:00 local.
    AuSouth,
    /// Last Sunday of September to first Sunday of April, at 02:00 / 03:00 local.
    Nz,
}

const H: i32 = 3600;

/// (standard offset, daylight rules, names). The first name is the IANA one.
const ZONES: &[(i32, Dst, &[&str])] = &[
    (0, Dst::None, &["UTC", "Etc/UTC", "GMT", "Etc/GMT", "Z", "Zulu", "Universal"]),
    (0, Dst::None, &["Atlantic/Reykjavik", "Greenwich Standard Time"]),
    (0, Dst::Eu, &["Europe/London", "Europe/Dublin", "Europe/Lisbon", "GB", "Europe/Belfast", "GMT Standard Time"]),
    (
        H,
        Dst::Eu,
        &[
            "Europe/Brussels",
            "Europe/Paris",
            "Europe/Madrid",
            "Europe/Copenhagen",
            "Romance Standard Time",
            "Europe/Amsterdam",
            "Europe/Berlin",
            "Europe/Rome",
            "Europe/Stockholm",
            "Europe/Vienna",
            "Europe/Zurich",
            "Europe/Oslo",
            "Europe/Luxembourg",
            "Europe/Monaco",
            "Europe/Malta",
            "Europe/Andorra",
            "Europe/Vaduz",
            "Europe/San_Marino",
            "Europe/Vatican",
            "Europe/Gibraltar",
            "Europe/Busingen",
            "Arctic/Longyearbyen",
            "W. Europe Standard Time",
            "Europe/Prague",
            "Europe/Budapest",
            "Europe/Bratislava",
            "Europe/Ljubljana",
            "Europe/Belgrade",
            "Europe/Podgorica",
            "Europe/Tirane",
            "Central Europe Standard Time",
            "Europe/Warsaw",
            "Europe/Zagreb",
            "Europe/Sarajevo",
            "Europe/Skopje",
            "Central European Standard Time",
            "Africa/Ceuta",
            "CET",
            "MET",
        ],
    ),
    (
        2 * H,
        Dst::Eu,
        &[
            "Europe/Helsinki",
            "Europe/Kiev",
            "Europe/Kyiv",
            "Europe/Riga",
            "Europe/Tallinn",
            "Europe/Vilnius",
            "Europe/Sofia",
            "FLE Standard Time",
            "Europe/Athens",
            "Europe/Bucharest",
            "GTB Standard Time",
            "EET",
        ],
    ),
    (-5 * H, Dst::Us, &["America/New_York", "America/Toronto", "America/Detroit", "US/Eastern", "EST5EDT", "Eastern Standard Time"]),
    (-6 * H, Dst::Us, &["America/Chicago", "America/Winnipeg", "US/Central", "CST6CDT", "Central Standard Time"]),
    (-7 * H, Dst::Us, &["America/Denver", "America/Edmonton", "US/Mountain", "MST7MDT", "Mountain Standard Time"]),
    (-8 * H, Dst::Us, &["America/Los_Angeles", "America/Vancouver", "US/Pacific", "PST8PDT", "Pacific Standard Time"]),
    (-9 * H, Dst::Us, &["America/Anchorage", "US/Alaska", "Alaskan Standard Time"]),
    (-4 * H, Dst::Us, &["America/Halifax", "Canada/Atlantic", "Atlantic Standard Time"]),
    (-7 * H, Dst::None, &["America/Phoenix", "US/Arizona", "US Mountain Standard Time"]),
    (-10 * H, Dst::None, &["Pacific/Honolulu", "US/Hawaii", "Hawaiian Standard Time"]),
    (-6 * H, Dst::None, &["America/Mexico_City", "Central Standard Time (Mexico)"]),
    (
        -3 * H,
        Dst::None,
        &["America/Sao_Paulo", "America/Buenos_Aires", "America/Argentina/Buenos_Aires", "E. South America Standard Time", "Argentina Standard Time"],
    ),
    (3 * H, Dst::None, &["Europe/Moscow", "Europe/Istanbul", "Asia/Istanbul", "Russian Standard Time", "Turkey Standard Time"]),
    (2 * H, Dst::None, &["Africa/Johannesburg", "Africa/Cairo", "South Africa Standard Time"]),
    (4 * H, Dst::None, &["Asia/Dubai", "Arabian Standard Time"]),
    (5 * H + 1800, Dst::None, &["Asia/Kolkata", "Asia/Calcutta", "India Standard Time"]),
    (
        8 * H,
        Dst::None,
        &[
            "Asia/Shanghai",
            "Asia/Hong_Kong",
            "China Standard Time",
            "Asia/Singapore",
            "Singapore Standard Time",
            "Australia/Perth",
            "W. Australia Standard Time",
        ],
    ),
    (9 * H, Dst::None, &["Asia/Tokyo", "Tokyo Standard Time", "Asia/Seoul", "Korea Standard Time"]),
    (
        10 * H,
        Dst::AuSouth,
        &["Australia/Sydney", "Australia/Melbourne", "Australia/Canberra", "Australia/Hobart", "AUS Eastern Standard Time", "Tasmania Standard Time"],
    ),
    (10 * H, Dst::None, &["Australia/Brisbane", "E. Australia Standard Time"]),
    (9 * H + 1800, Dst::AuSouth, &["Australia/Adelaide", "Cen. Australia Standard Time"]),
    (12 * H, Dst::Nz, &["Pacific/Auckland", "New Zealand Standard Time"]),
];

fn rules(std: i32, dst: Dst) -> Vec<ZoneRule> {
    let up = std + H;
    let yearly = |from, to, month, nth, local| ZoneRule { from, to, yearly: Some((month, nth, 6)), local, until: None };
    match dst {
        Dst::None => Vec::new(),
        Dst::Eu => vec![yearly(std, up, 3, -1, i64::from(H + std)), yearly(up, std, 10, -1, i64::from(H + up))],
        Dst::Us => vec![yearly(std, up, 3, 2, 2 * 3600), yearly(up, std, 11, 1, 2 * 3600)],
        Dst::AuSouth => vec![yearly(std, up, 10, 1, 2 * 3600), yearly(up, std, 4, 1, 3 * 3600)],
        Dst::Nz => vec![yearly(std, up, 9, -1, 2 * 3600), yearly(up, std, 4, 1, 3 * 3600)],
    }
}

/// The name with any leading path trimmed (`/mozilla.org/20070129_1/Europe/Brussels`).
fn tails(name: &str) -> impl Iterator<Item = &str> {
    let name = name.trim().trim_matches('"');
    std::iter::once(name).chain(name.match_indices('/').map(move |(i, _)| name.get(i + 1..).unwrap_or("")))
}

impl Zone {
    /// A common zone by IANA or Windows name (case-insensitive); the zone keeps the name as
    /// given. `None` for names this table doesn't know.
    pub fn named(name: &str) -> Option<Zone> {
        for tail in tails(name) {
            if tail.is_empty() {
                continue;
            }
            let found = ZONES.iter().find(|(_, _, names)| names.iter().any(|n| n.eq_ignore_ascii_case(tail)));
            if let Some(&(std, dst, _)) = found {
                let mut zone = Zone::from_rules(name.trim().trim_matches('"'), rules(std, dst));
                if matches!(dst, Dst::None) {
                    zone.default = std;
                }
                return Some(zone);
            }
        }
        None
    }
}

/// The Windows name of a zone (`W. Europe Standard Time`), which Exchange's EWS wants, for a
/// zone this table knows by any name. A Windows name comes back as it is; for an IANA name it is
/// the first Windows name listed for the same rules (equivalent: same offsets and changes).
pub fn windows_name(name: &str) -> Option<&'static str> {
    tails(name).filter(|t| !t.is_empty()).find_map(|tail| {
        let (_, _, names) = ZONES.iter().find(|(_, _, names)| names.iter().any(|n| n.eq_ignore_ascii_case(tail)))?;
        let windows = |n: &&&str| n.contains(' ') || n.eq_ignore_ascii_case("UTC");
        names.iter().filter(windows).find(|n| n.eq_ignore_ascii_case(tail)).or_else(|| names.iter().find(windows)).copied()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dates::days_from_civil;

    fn at(y: i64, m: u32, d: u32, h: i64) -> i64 {
        days_from_civil(y, m, d) * 86_400 + h * 3600
    }

    #[test]
    fn europe_changes_on_the_last_sundays() {
        let z = Zone::named("Europe/Brussels").unwrap_or_default();
        assert_eq!(z.offset_at(at(2026, 3, 29, 0) + 3599), 3600);
        assert_eq!(z.offset_at(at(2026, 3, 29, 1)), 7200);
        assert_eq!(z.offset_at(at(2026, 10, 25, 1) - 1), 7200);
        assert_eq!(z.offset_at(at(2026, 10, 25, 1)), 3600);
        // Windows names, other case and a path prefix find the same rules.
        for n in ["W. Europe Standard Time", "europe/berlin", "/mozilla.org/20070129_1/Europe/Brussels", "\"Romance Standard Time\""] {
            assert_eq!(Zone::named(n).map(|z| z.offset_at(at(2026, 7, 1, 12))), Some(7200), "{n}");
        }
    }

    #[test]
    fn windows_names_for_exchange() {
        assert_eq!(windows_name("W. Europe Standard Time"), Some("W. Europe Standard Time"));
        assert_eq!(windows_name("Europe/Brussels"), Some("Romance Standard Time"));
        assert_eq!(windows_name("America/New_York"), Some("Eastern Standard Time"));
        assert_eq!(windows_name("Etc/UTC"), Some("UTC"));
        assert_eq!(windows_name("Mars/Olympus"), None);
    }

    #[test]
    fn other_regions() {
        let ny = Zone::named("America/New_York").unwrap_or_default();
        assert_eq!(ny.offset_at(at(2026, 7, 1, 12)), -4 * 3600);
        assert_eq!(ny.offset_at(at(2026, 1, 1, 12)), -5 * 3600);
        let sy = Zone::named("Australia/Sydney").unwrap_or_default();
        assert_eq!(sy.offset_at(at(2026, 1, 1, 0)), 11 * 3600);
        assert_eq!(sy.offset_at(at(2026, 7, 1, 0)), 10 * 3600);
        let tokyo = Zone::named("Asia/Tokyo").unwrap_or_default();
        assert_eq!((tokyo.offset_at(0), tokyo.offset_at(at(2026, 7, 1, 0))), (9 * 3600, 9 * 3600));
        assert_eq!(Zone::named("Mars/Olympus"), None);
        assert_eq!(Zone::named(""), None);
        assert_eq!(Zone::named("/"), None);
    }
}

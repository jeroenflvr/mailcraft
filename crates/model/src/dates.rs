//! Calendar arithmetic for the message list: date groups (Today, Yesterday, Last week…) and the
//! short and long times shown for a message. Pure integer maths on Unix seconds and a time zone
//! (a list of UTC-offset changes), so it works the same everywhere, wasm included.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// The first and last year zone rules are expanded for.
const FIRST_YEAR: i64 = 1970;
const LAST_YEAR: i64 = 2100;
/// Most rules read from one zone definition, and most transitions kept (hostile input).
const MAX_RULES: usize = 64;
const MAX_TRANSITIONS: usize = 2000;
const MAX_OFFSET: i32 = 18 * 3600;

/// One change of UTC offset, once or every year (what a `VTIMEZONE` `STANDARD`/`DAYLIGHT` says).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZoneRule {
    /// Offset (seconds east of UTC) before and after the change.
    pub from: i32,
    pub to: i32,
    /// `Some((month, nth, weekday))`: every year, on the `nth` (negative: from the end) weekday
    /// (0 = Monday) of the month; `None`: just once.
    #[serde(default)]
    pub yearly: Option<(u32, i32, u32)>,
    /// Local wall-clock time of the first (or only) change, as seconds since 1970-01-01 00:00
    /// read in the `from` offset.
    pub local: i64,
    /// The last change, in UTC, of a yearly rule that stops.
    #[serde(default)]
    pub until: Option<i64>,
}

/// A time zone: the offset in force at every instant, as a list of changes. Pure data (no tz
/// database, no clock), so it works in the browser too.
///
/// Zones come from three places: a `VTIMEZONE` ([`Zone::from_rules`]), the built-in table of
/// common names (`Zone::named`) and the platform's local zone ([`Zone::probe`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ZoneRepr", into = "ZoneRepr")]
pub struct Zone {
    /// The name as the platform or a file gives it (`Europe/Brussels`); empty when unknown.
    pub id: String,
    /// The offset before the first change.
    pub default: i32,
    /// (UTC second, offset from then on), ascending.
    pub transitions: Vec<(i64, i32)>,
    /// The rules the transitions were built from, when there are any (kept to write them back).
    pub rules: Vec<ZoneRule>,
}

/// What is stored: rules when there are some (compact), else the transitions.
#[derive(Serialize, Deserialize)]
struct ZoneRepr {
    #[serde(default)]
    id: String,
    #[serde(default)]
    default: i32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    transitions: Vec<(i64, i32)>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    rules: Vec<ZoneRule>,
}

impl From<Zone> for ZoneRepr {
    fn from(z: Zone) -> ZoneRepr {
        let transitions = if z.rules.is_empty() { z.transitions } else { Vec::new() };
        ZoneRepr { id: z.id, default: z.default, transitions, rules: z.rules }
    }
}

impl From<ZoneRepr> for Zone {
    fn from(r: ZoneRepr) -> Zone {
        if !r.rules.is_empty() {
            return Zone::from_rules(r.id, r.rules);
        }
        let mut transitions = r.transitions;
        transitions.truncate(MAX_TRANSITIONS);
        transitions.sort_by_key(|t| t.0);
        Zone { id: r.id, default: r.default.clamp(-MAX_OFFSET, MAX_OFFSET), transitions, rules: Vec::new() }
    }
}

impl Zone {
    /// A zone that never changes.
    pub fn fixed(offset: i32) -> Zone {
        Zone { default: offset.clamp(-14 * 3600, 14 * 3600), ..Zone::default() }
    }

    pub fn utc() -> Zone {
        Zone { id: "UTC".into(), ..Zone::default() }
    }

    /// The zone the rules describe, expanded for the years 1970–2100. When two rules change at
    /// the same instant the later one wins.
    pub fn from_rules(id: impl Into<String>, rules: Vec<ZoneRule>) -> Zone {
        let mut all: Vec<(i64, i32, i32)> = Vec::new();
        for r in rules.iter().take(MAX_RULES) {
            let (from, to) = (r.from.clamp(-MAX_OFFSET, MAX_OFFSET), r.to.clamp(-MAX_OFFSET, MAX_OFFSET));
            let first = r.local.saturating_sub(i64::from(from));
            match r.yearly {
                None => all.push((first, from, to)),
                Some((month, nth, weekday)) => {
                    let time = r.local.rem_euclid(86_400);
                    let start = civil_from_days(r.local.div_euclid(86_400)).0.max(FIRST_YEAR);
                    for y in start..=LAST_YEAR {
                        let Some(d) = nth_weekday(y, month, nth, weekday) else { continue };
                        let t = (d * 86_400 + time).saturating_sub(i64::from(from));
                        if t >= first && r.until.is_none_or(|u| t <= u) {
                            all.push((t, from, to));
                        }
                    }
                }
            }
        }
        all.sort_by_key(|a| a.0);
        let default = all.first().map_or(0, |a| a.1);
        let mut transitions: Vec<(i64, i32)> = Vec::new();
        for (t, _, to) in all {
            match transitions.last_mut() {
                Some(last) if last.0 == t => last.1 = to,
                _ => transitions.push((t, to)),
            }
        }
        transitions.truncate(MAX_TRANSITIONS);
        Zone { id: id.into(), default, transitions, rules }
    }

    /// The zone a platform reports through `offset_at` (seconds east of UTC for a Unix time),
    /// read over `[from, to]`: probed a week at a time, each change then pinned to the second.
    pub fn probe(id: impl Into<String>, offset_at: impl Fn(i64) -> i32, from: i64, to: i64) -> Zone {
        const STEP: i64 = 7 * 86_400;
        let clamp = |o: i32| o.clamp(-MAX_OFFSET, MAX_OFFSET);
        let default = clamp(offset_at(from));
        let (mut prev, mut t) = (default, from);
        let mut transitions = Vec::new();
        while t < to && transitions.len() < MAX_TRANSITIONS {
            let next = t.saturating_add(STEP).min(to);
            if clamp(offset_at(next)) == prev {
                t = next;
                continue;
            }
            let (mut lo, mut hi) = (t, next);
            while hi - lo > 1 {
                let mid = lo + (hi - lo) / 2;
                if clamp(offset_at(mid)) == prev {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            prev = clamp(offset_at(hi));
            transitions.push((hi, prev));
            t = hi;
        }
        Zone { id: id.into(), default, transitions, rules: Vec::new() }
    }

    /// The offset (seconds east of UTC) in force at a Unix time.
    pub fn offset_at(&self, utc: i64) -> i32 {
        let i = self.transitions.partition_point(|&(t, _)| t <= utc);
        let o = match i.checked_sub(1).and_then(|i| self.transitions.get(i)) {
            Some(&(_, o)) => o,
            None => self.default,
        };
        o.clamp(-MAX_OFFSET, MAX_OFFSET)
    }

    /// The Unix time of a local wall-clock time (`local`: seconds since 1970-01-01 00:00 read as
    /// local). RFC 5545 §3.3.5 decides the two odd cases: a time that happens twice (the clocks go
    /// back) is its first occurrence; one that never happens (the clocks go forward) is read with
    /// the offset from before the change.
    pub fn local_to_utc(&self, local: i64) -> i64 {
        let before = self.offset_at(local.saturating_sub(86_400));
        let after = self.offset_at(local.saturating_add(86_400));
        let holds = |o: i32| self.offset_at(local.saturating_sub(i64::from(o))) == o;
        let o = match (holds(before), holds(after)) {
            (true, true) => before.max(after),
            (false, true) => after,
            _ => before,
        };
        local.saturating_sub(i64::from(o))
    }
}

/// "Now" and the local time zone, as the app reports them each frame.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Clock {
    /// Unix seconds, UTC.
    pub now: i64,
    /// The local zone: its offset differs between summer and winter, so ask it per instant.
    pub zone: Arc<Zone>,
}

impl Clock {
    /// A clock in a zone that never changes (tests, the sample mailbox).
    pub fn new(now: i64, offset: i32) -> Clock {
        Clock { now, zone: Arc::new(Zone::fixed(offset)) }
    }

    pub fn with_zone(now: i64, zone: Arc<Zone>) -> Clock {
        Clock { now, zone }
    }

    /// The local offset (seconds east of UTC) at a Unix time.
    pub fn offset_at(&self, t: i64) -> i32 {
        self.zone.offset_at(t)
    }

    /// Unix seconds of a local wall-clock time (see [`Zone::local_to_utc`]).
    pub fn local_to_utc(&self, local: i64) -> i64 {
        self.zone.local_to_utc(local)
    }

    /// The local day (days since 1970-01-01) of a Unix time.
    pub fn local_day(&self, t: i64) -> i64 {
        t.saturating_add(i64::from(self.offset_at(t))).div_euclid(86_400)
    }

    /// The Unix time local day `day` starts.
    pub fn local_midnight(&self, day: i64) -> i64 {
        self.local_to_utc(day.saturating_mul(86_400))
    }
}

/// Days since the epoch of the `nth` (1-based; negative from the end) `weekday` (0 Monday) of a
/// month.
pub fn nth_weekday(year: i64, month: u32, nth: i32, weekday: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || nth == 0 {
        return None;
    }
    let first = days_from_civil(year, month, 1);
    let next = if month == 12 { days_from_civil(year + 1, 1, 1) } else { days_from_civil(year, month + 1, 1) };
    let wd = |d: i64| (d + 3).rem_euclid(7) as u32;
    if nth > 0 {
        let offset = (i64::from(weekday) - i64::from(wd(first))).rem_euclid(7);
        let d = first + offset + 7 * i64::from(nth - 1);
        (d < next).then_some(d)
    } else {
        let last = next - 1;
        let offset = (i64::from(wd(last)) - i64::from(weekday)).rem_euclid(7);
        let d = last - offset - 7 * i64::from(-nth - 1);
        (d >= first).then_some(d)
    }
}

/// A date split into its parts (proleptic Gregorian).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Civil {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    /// 0 = Monday … 6 = Sunday.
    pub weekday: u32,
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's `civil_from_days`.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z.saturating_add(719_468);
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (year, month, day) → days since 1970-01-01.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m.clamp(1, 12));
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d.clamp(1, 31)) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Split a time into local calendar parts.
pub fn civil(t: i64, clock: &Clock) -> Civil {
    let local = t.saturating_add(i64::from(clock.offset_at(t)));
    let days = local.div_euclid(86_400);
    let secs = local.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    // 1970-01-01 was a Thursday (weekday 3 when Monday is 0).
    let weekday = (days + 3).rem_euclid(7) as u32;
    Civil { year, month, day, hour: (secs / 3600) as u32, minute: ((secs % 3600) / 60) as u32, weekday }
}

const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const WEEKDAYS_LONG: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

fn weekday_name(c: &Civil) -> &'static str {
    WEEKDAYS.get(c.weekday as usize).copied().unwrap_or("")
}

fn month_name(c: &Civil) -> &'static str {
    MONTHS.get(c.month.saturating_sub(1) as usize).copied().unwrap_or("")
}

/// The headings the message list groups by, newest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DateGroup {
    /// Dated in the future (a sender's clock is off): shown on top.
    Future,
    Today,
    Yesterday,
    /// Earlier this week (Monday-based); the weekday 0–6.
    ThisWeek(u32),
    LastWeek,
    TwoWeeksAgo,
    ThreeWeeksAgo,
    LastMonth,
    Older,
}

impl DateGroup {
    pub fn of(t: i64, clock: &Clock) -> DateGroup {
        let today = clock.local_day(clock.now);
        let day = clock.local_day(t);
        let diff = today - day;
        if diff < 0 {
            return DateGroup::Future;
        }
        if diff == 0 {
            return DateGroup::Today;
        }
        if diff == 1 {
            return DateGroup::Yesterday;
        }
        let today_wd = (today + 3).rem_euclid(7);
        let week_start = today - today_wd;
        if day >= week_start {
            return DateGroup::ThisWeek((day + 3).rem_euclid(7) as u32);
        }
        if day >= week_start - 7 {
            return DateGroup::LastWeek;
        }
        if day >= week_start - 14 {
            return DateGroup::TwoWeeksAgo;
        }
        if day >= week_start - 21 {
            return DateGroup::ThreeWeeksAgo;
        }
        let (ty, tm, _) = civil_from_days(today);
        let (y, m, _) = civil_from_days(day);
        let months_back = (ty * 12 + i64::from(tm)) - (y * 12 + i64::from(m));
        if months_back <= 1 { DateGroup::LastMonth } else { DateGroup::Older }
    }

    pub fn label(self) -> &'static str {
        match self {
            DateGroup::Future => "Upcoming",
            DateGroup::Today => "Today",
            DateGroup::Yesterday => "Yesterday",
            DateGroup::ThisWeek(wd) => WEEKDAYS_LONG.get(wd as usize).copied().unwrap_or("This week"),
            DateGroup::LastWeek => "Last week",
            DateGroup::TwoWeeksAgo => "Two weeks ago",
            DateGroup::ThreeWeeksAgo => "Three weeks ago",
            DateGroup::LastMonth => "Last month",
            DateGroup::Older => "Older",
        }
    }
}

/// The compact time in the message list: `14:05` today, `Mon 14:05` this week, `Oct 3` this
/// year, `2025-10-03` before that.
pub fn list_time(t: i64, clock: &Clock) -> String {
    let c = civil(t, clock);
    let now = civil(clock.now, clock);
    let days = clock.local_day(clock.now) - clock.local_day(t);
    if days == 0 {
        format!("{:02}:{:02}", c.hour, c.minute)
    } else if (1..7).contains(&days) {
        format!("{} {:02}:{:02}", weekday_name(&c), c.hour, c.minute)
    } else if c.year == now.year {
        format!("{} {}", month_name(&c), c.day)
    } else {
        format!("{:04}-{:02}-{:02}", c.year, c.month, c.day)
    }
}

/// The full date in the reading pane: `Wed 2026-10-14 14:05`.
pub fn full_time(t: i64, clock: &Clock) -> String {
    let c = civil(t, clock);
    format!("{} {:04}-{:02}-{:02} {:02}:{:02}", weekday_name(&c), c.year, c.month, c.day, c.hour, c.minute)
}

/// When a flagged message is due, as Outlook's Follow Up menu offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FollowUp {
    Today,
    Tomorrow,
    /// The last working day of this week (Friday), or today from Friday on.
    ThisWeek,
    /// Friday next week.
    NextWeek,
}

impl FollowUp {
    pub const ALL: [FollowUp; 4] = [FollowUp::Today, FollowUp::Tomorrow, FollowUp::ThisWeek, FollowUp::NextWeek];

    pub fn code(self) -> &'static str {
        match self {
            FollowUp::Today => "today",
            FollowUp::Tomorrow => "tomorrow",
            FollowUp::ThisWeek => "thisWeek",
            FollowUp::NextWeek => "nextWeek",
        }
    }

    pub fn from_code(s: &str) -> Option<FollowUp> {
        FollowUp::ALL.into_iter().find(|f| f.code().eq_ignore_ascii_case(s))
    }

    pub fn label(self) -> &'static str {
        match self {
            FollowUp::Today => "Today",
            FollowUp::Tomorrow => "Tomorrow",
            FollowUp::ThisWeek => "This week",
            FollowUp::NextWeek => "Next week",
        }
    }

    /// The local day it's due (days since 1970-01-01).
    pub fn due(self, clock: &Clock) -> i64 {
        let today = clock.local_day(clock.now);
        // 0 = Monday … 6 = Sunday; Friday is 4.
        let weekday = (today + 3).rem_euclid(7);
        match self {
            FollowUp::Today => today,
            FollowUp::Tomorrow => today + 1,
            FollowUp::ThisWeek => today + (4 - weekday).max(0),
            FollowUp::NextWeek => today + (7 - weekday) + 4,
        }
    }
}

/// Today, as a local day (days since 1970-01-01).
pub fn today(clock: &Clock) -> i64 {
    clock.local_day(clock.now)
}

/// A local day as `YYYY-MM-DD`.
pub fn day_string(day: i64) -> String {
    let (y, m, d) = civil_from_days(day);
    format!("{y:04}-{m:02}-{d:02}")
}

/// A local day as the reading pane shows it: `Fri 2026-10-16`.
pub fn day_label(day: i64) -> String {
    let (y, m, d) = civil_from_days(day);
    let weekday = WEEKDAYS.get((day + 3).rem_euclid(7) as usize).copied().unwrap_or("");
    format!("{weekday} {y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follow_up_days() {
        // NOW is a Wednesday, 2026-10-14.
        let clock = Clock::new(NOW, 0);
        let day = |f: FollowUp| day_string(f.due(&clock));
        assert_eq!(day(FollowUp::Today), "2026-10-14");
        assert_eq!(day(FollowUp::Tomorrow), "2026-10-15");
        assert_eq!(day(FollowUp::ThisWeek), "2026-10-16");
        assert_eq!(day(FollowUp::NextWeek), "2026-10-23");
        // On a Saturday, "this week" is today.
        let sat = Clock::new(NOW + 3 * 86_400, 0);
        assert_eq!(day_string(FollowUp::ThisWeek.due(&sat)), "2026-10-17");
        assert_eq!(day_string(FollowUp::NextWeek.due(&sat)), "2026-10-23");
        assert_eq!(day_label(FollowUp::ThisWeek.due(&clock)), "Fri 2026-10-16");
        assert_eq!(FollowUp::from_code("nextweek"), Some(FollowUp::NextWeek));
    }

    // 2026-10-14 12:00:00 UTC, a Wednesday.
    const NOW: i64 = 1_791_979_200;

    #[test]
    fn civil_round_trip() {
        for d in [-800_000i64, -1, 0, 1, 365, 20_000, 2_000_000] {
            let (y, m, dd) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m, dd), d);
        }
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        let c = civil(NOW, &Clock::new(NOW, 0));
        assert_eq!((c.year, c.month, c.day, c.hour, c.weekday), (2026, 10, 14, 12, 2));
    }

    #[test]
    fn groups() {
        let clock = Clock::new(NOW, 0);
        let day = 86_400;
        assert_eq!(DateGroup::of(NOW - 60, &clock), DateGroup::Today);
        assert_eq!(DateGroup::of(NOW + day, &clock), DateGroup::Future);
        assert_eq!(DateGroup::of(NOW - day, &clock), DateGroup::Yesterday);
        assert_eq!(DateGroup::of(NOW - 2 * day, &clock), DateGroup::ThisWeek(0));
        assert_eq!(DateGroup::of(NOW - 4 * day, &clock), DateGroup::LastWeek);
        assert_eq!(DateGroup::of(NOW - 12 * day, &clock), DateGroup::TwoWeeksAgo);
        assert_eq!(DateGroup::of(NOW - 30 * day, &clock), DateGroup::LastMonth);
        assert_eq!(DateGroup::of(NOW - 400 * day, &clock), DateGroup::Older);
    }

    #[test]
    fn offsets_move_the_day_boundary() {
        // 23:30 UTC on the 13th is already the 14th in UTC+2.
        let t = NOW - 12 * 3600 - 1800;
        assert_eq!(DateGroup::of(t, &Clock::new(NOW, 0)), DateGroup::Yesterday);
        assert_eq!(DateGroup::of(t, &Clock::new(NOW, 7200)), DateGroup::Today);
    }

    #[test]
    fn formats() {
        let clock = Clock::new(NOW, 0);
        assert_eq!(list_time(NOW - 3600, &clock), "11:00");
        assert_eq!(list_time(NOW - 2 * 86_400, &clock), "Mon 12:00");
        assert_eq!(list_time(NOW - 30 * 86_400, &clock), "Sep 14");
        assert_eq!(list_time(NOW - 400 * 86_400, &clock), "2025-09-09");
        assert_eq!(full_time(NOW, &clock), "Wed 2026-10-14 12:00");
        // Hostile values never panic.
        let _ = full_time(i64::MAX, &clock);
        let _ = list_time(i64::MIN, &clock);
        let _ = DateGroup::of(i64::MIN, &Clock::new(i64::MAX, i32::MAX));
    }

    fn brussels() -> Zone {
        Zone::named("Europe/Brussels").unwrap_or_default()
    }

    fn at(y: i64, m: u32, d: u32, h: i64, mi: i64) -> i64 {
        days_from_civil(y, m, d) * 86_400 + h * 3600 + mi * 60
    }

    /// The same clock reads 08:00 on both sides of the change when it is 07:00Z and 08:00Z by turns.
    #[test]
    fn the_offset_follows_the_instant() {
        let clock = Clock::with_zone(at(2026, 10, 24, 12, 0), Arc::new(brussels()));
        // Today (24 Oct) is still summer time; Monday 26 Oct 07:00Z is 08:00 winter time.
        assert_eq!(clock.offset_at(clock.now), 7200);
        assert_eq!(civil(at(2026, 10, 26, 7, 0), &clock).hour, 8);
        assert_eq!(civil(at(2026, 10, 23, 6, 0), &clock).hour, 8);
        assert_eq!(civil(at(2026, 10, 25, 0, 59), &clock).hour, 2);
        assert_eq!(civil(at(2026, 10, 25, 1, 0), &clock).hour, 2, "the repeated hour");
        // Local midnight of Monday is 23:00Z on Sunday, an hour later than on Saturday.
        let sun = days_from_civil(2026, 10, 25);
        assert_eq!(clock.local_midnight(sun), at(2026, 10, 24, 22, 0));
        assert_eq!(clock.local_midnight(sun + 1), at(2026, 10, 25, 23, 0));
        assert_eq!(clock.local_day(at(2026, 10, 25, 23, 0)), sun + 1);
    }

    #[test]
    fn gaps_and_repeats() {
        let z = brussels();
        // 02:30 on 2026-03-29 never happens: it is read with the offset from before (+1).
        let gap = at(2026, 3, 29, 2, 30);
        assert_eq!(z.local_to_utc(gap), at(2026, 3, 29, 1, 30));
        // 02:30 on 2026-10-25 happens twice: the first one (+2) counts.
        let twice = at(2026, 10, 25, 2, 30);
        assert_eq!(z.local_to_utc(twice), at(2026, 10, 25, 0, 30));
        // Ordinary times, either side.
        assert_eq!(z.local_to_utc(at(2026, 7, 1, 8, 0)), at(2026, 7, 1, 6, 0));
        assert_eq!(z.local_to_utc(at(2026, 1, 1, 8, 0)), at(2026, 1, 1, 7, 0));
        for t in [i64::MIN, i64::MAX, 0] {
            let _ = (z.offset_at(t), z.local_to_utc(t));
        }
    }

    /// A zone read from a platform's offsets finds each change to the second.
    #[test]
    fn probing_finds_the_changes() {
        let truth = brussels();
        let probed = Zone::probe("Europe/Brussels", |t| truth.offset_at(t), at(2024, 1, 1, 0, 0), at(2028, 1, 1, 0, 0));
        assert_eq!(probed.transitions.len(), 8);
        assert!(probed.transitions.contains(&(at(2026, 3, 29, 1, 0), 7200)));
        assert!(probed.transitions.contains(&(at(2026, 10, 25, 1, 0), 3600)));
        for t in [at(2025, 6, 1, 0, 0), at(2027, 2, 1, 0, 0)] {
            assert_eq!(probed.offset_at(t), truth.offset_at(t));
        }
        assert!(Zone::probe("", |_| 3600, 0, 10 * 365 * 86_400).transitions.is_empty());
    }

    /// Rules are what is stored; a zone read back is the same zone. Damaged data is harmless.
    #[test]
    fn zones_round_trip_and_survive_junk() {
        let z = brussels();
        let json = serde_json::to_string(&z).unwrap_or_default();
        assert!(json.len() < 600, "stored as rules, not 260 transitions: {}", json.len());
        assert_eq!(serde_json::from_str::<Zone>(&json).ok(), Some(z));
        let probed = Zone::probe("X", |t| if t > 100 { 7200 } else { 3600 }, 0, 10_000);
        let json = serde_json::to_string(&probed).unwrap_or_default();
        assert_eq!(serde_json::from_str::<Zone>(&json).ok(), Some(probed));
        let damaged: Zone = serde_json::from_str(r#"{"default":99999999,"transitions":[[5,1],[1,2]]}"#).unwrap_or_default();
        assert_eq!((damaged.offset_at(0), damaged.offset_at(3)), (18 * 3600, 2), "clamped, and sorted");
        assert!(serde_json::from_str::<Zone>("{}").is_ok());
        let wild = ZoneRule { from: i32::MAX, to: i32::MIN, yearly: Some((13, 99, 9)), local: i64::MIN, until: Some(i64::MAX) };
        let z = Zone::from_rules("w", vec![wild.clone(), ZoneRule { yearly: None, local: i64::MAX, ..wild }]);
        let _ = (z.offset_at(0), z.local_to_utc(i64::MAX));
    }
}

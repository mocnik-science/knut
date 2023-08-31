//! Date literals, spans and calendar arithmetic (KNUT_LANG.md §5.4). All times are UTC
//! Unix seconds.

use knut_format::time::{civil_from_days, days_from_civil};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Precision {
    Year,
    Month,
    Week,
    Day,
    Instant,
}

/// A date literal: the half-open span `[start, end)` it denotes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DateVal {
    pub start: i64,
    pub end: i64,
    pub prec: Precision,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TimeUnit {
    Day,
    Week,
    Month,
    Year,
}

impl TimeUnit {
    pub fn parse(s: &str) -> Option<TimeUnit> {
        Some(match s {
            "day" | "days" => TimeUnit::Day,
            "week" | "weeks" => TimeUnit::Week,
            "month" | "months" => TimeUnit::Month,
            "year" | "years" => TimeUnit::Year,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            TimeUnit::Day => "day",
            TimeUnit::Week => "week",
            TimeUnit::Month => "month",
            TimeUnit::Year => "year",
        }
    }

    pub fn precision(self) -> Precision {
        match self {
            TimeUnit::Day => Precision::Day,
            TimeUnit::Week => Precision::Week,
            TimeUnit::Month => Precision::Month,
            TimeUnit::Year => Precision::Year,
        }
    }
}

pub fn month_name(s: &str) -> Option<u32> {
    const M: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    const L: [&str; 12] = [
        "january", "february", "march", "april", "may", "june", "july", "august", "september", "october", "november",
        "december",
    ];
    let l = s.to_ascii_lowercase();
    M.iter().position(|m| *m == l).or_else(|| L.iter().position(|m| *m == l)).map(|i| i as u32 + 1)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

pub fn year(y: i64) -> DateVal {
    DateVal { start: days_from_civil(y, 1, 1) * 86_400, end: days_from_civil(y + 1, 1, 1) * 86_400, prec: Precision::Year }
}

pub fn month(y: i64, m: u32) -> Option<DateVal> {
    if !(1..=12).contains(&m) {
        return None;
    }
    let s = days_from_civil(y, m, 1) * 86_400;
    Some(DateVal { start: s, end: add_months(s, 1), prec: Precision::Month })
}

pub fn day(y: i64, m: u32, d: u32) -> Option<DateVal> {
    if !(1..=12).contains(&m) || d == 0 || d > days_in_month(y, m) {
        return None;
    }
    let s = days_from_civil(y, m, d) * 86_400;
    Some(DateVal { start: s, end: s + 86_400, prec: Precision::Day })
}

/// The UTC day containing `t`.
pub fn day_of(t: i64) -> DateVal {
    let s = t.div_euclid(86_400) * 86_400;
    DateVal { start: s, end: s + 86_400, prec: Precision::Day }
}

pub fn instant(t: i64) -> DateVal {
    DateVal { start: t, end: t + 1, prec: Precision::Instant }
}

/// Parses `YYYY-M[-D][Thh:mm[:ss][Z|±hh:mm]]`.
pub fn parse_iso(s: &str) -> Option<DateVal> {
    let (date, time) = match s.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut it = date.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: u32 = it.next()?.parse().ok()?;
    let d: Option<u32> = match it.next() {
        Some(v) => Some(v.parse().ok()?),
        None => None,
    };
    let Some(d) = d else { return month(y, m) };
    let dv = day(y, m, d)?;
    let Some(t) = time else { return Some(dv) };
    let (clock, off) = if let Some(c) = t.strip_suffix('Z') {
        (c, 0)
    } else if let Some(i) = t.rfind(['+', '-']) {
        let (c, o) = t.split_at(i);
        let sign = if o.starts_with('-') { -1 } else { 1 };
        let mut op = o[1..].split(':');
        let oh: i64 = op.next()?.parse().ok()?;
        let om: i64 = op.next().map_or(Some(0), |v| v.parse().ok())?;
        (c, sign * (oh * 3600 + om * 60))
    } else {
        (t, 0)
    };
    let mut cp = clock.split(':');
    let h: i64 = cp.next()?.parse().ok()?;
    let mi: i64 = cp.next()?.parse().ok()?;
    let se: i64 = cp.next().map_or(Some(0), |v| v.parse().ok())?;
    if h > 24 || mi > 59 || se > 60 {
        return None;
    }
    Some(instant(dv.start + h * 3600 + mi * 60 + se - off))
}

/// Adds calendar months, clamping the day to the target month's length.
pub fn add_months(t: i64, n: i64) -> i64 {
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let total = y * 12 + (m as i64 - 1) + n;
    let ny = total.div_euclid(12);
    let nm = (total.rem_euclid(12) + 1) as u32;
    let nd = d.min(days_in_month(ny, nm));
    days_from_civil(ny, nm, nd) * 86_400 + secs
}

/// The bucket of unit `u` containing `t` (weeks are ISO weeks starting Monday).
pub fn bucket(t: i64, u: TimeUnit) -> DateVal {
    let days = t.div_euclid(86_400);
    let (y, m, _) = civil_from_days(days);
    match u {
        TimeUnit::Day => DateVal { start: days * 86_400, end: (days + 1) * 86_400, prec: Precision::Day },
        TimeUnit::Week => {
            // 1970-01-01 was a Thursday; Monday-based week start
            let wd = (days + 3).rem_euclid(7);
            let s = (days - wd) * 86_400;
            DateVal { start: s, end: s + 7 * 86_400, prec: Precision::Week }
        }
        TimeUnit::Month => month(y, m).unwrap(),
        TimeUnit::Year => year(y),
    }
}

/// Next bucket start after the bucket starting at `start`.
pub fn next_bucket(start: i64, u: TimeUnit) -> i64 {
    match u {
        TimeUnit::Day => start + 86_400,
        TimeUnit::Week => start + 7 * 86_400,
        TimeUnit::Month => add_months(start, 1),
        TimeUnit::Year => add_months(start, 12),
    }
}

const MON: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

pub fn format(d: &DateVal) -> String {
    let days = d.start.div_euclid(86_400);
    let (y, m, dd) = civil_from_days(days);
    match d.prec {
        Precision::Year => format!("{y}"),
        Precision::Month => format!("{y}-{m:02}"),
        Precision::Week => {
            // ISO week number
            let thursday = days + 3;
            let (ty, _, _) = civil_from_days(thursday);
            let w = (thursday - days_from_civil(ty, 1, 1)) / 7 + 1;
            format!("{ty}-W{w:02}")
        }
        Precision::Day => format!("{y}-{m:02}-{dd:02}"),
        Precision::Instant => format_instant(d.start),
    }
}

pub fn format_instant(t: i64) -> String {
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    // always the full instant, so that it cannot be taken for a day
    format!("{y}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600, (secs / 60) % 60, secs % 60)
}

/// `7 Mar 2020` style for messages.
pub fn format_long(y: i64, m: u32, d: u32) -> String {
    format!("{d} {} {y}", MON[(m - 1) as usize])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forms() {
        let y = year(2020);
        assert_eq!(knut_format::time::format_time(y.start), "2020-01-01T00:00:00Z");
        assert_eq!(knut_format::time::format_time(y.end), "2021-01-01T00:00:00Z");
        assert_eq!(parse_iso("2020-3").unwrap(), month(2020, 3).unwrap());
        assert_eq!(parse_iso("2020-03-07").unwrap(), day(2020, 3, 7).unwrap());
        assert_eq!(parse_iso("2020-03-07T14:30+02:00").unwrap().start, day(2020, 3, 7).unwrap().start + 12 * 3600 + 1800);
        assert!(parse_iso("2020-02-30").is_none());
        assert_eq!(add_months(day(2020, 1, 31).unwrap().start, 1), day(2020, 2, 29).unwrap().start);
        assert_eq!(format(&bucket(day(2024, 1, 3).unwrap().start, TimeUnit::Week)), "2024-W01");
    }
}

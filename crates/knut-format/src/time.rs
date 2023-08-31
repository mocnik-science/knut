//! Minimal UTC timestamp parsing/formatting (ISO 8601 subset, no external deps).

/// Days since 1970-01-01 for a proleptic Gregorian date.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Parses `YYYY-MM-DD`, `YYYY-MM-DDTHH:MM[:SS][Z]` or a plain integer (Unix seconds).
pub fn parse_time(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok(v) = s.parse::<i64>() {
        return Some(v);
    }
    let s = s.strip_suffix('Z').unwrap_or(s);
    let (date, time) = match s.split_once(['T', ' ']) {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut dp = date.split('-');
    let y: i64 = dp.next()?.parse().ok()?;
    let m: u32 = dp.next().map_or(Some(1), |v| v.parse().ok())?;
    let d: u32 = dp.next().map_or(Some(1), |v| v.parse().ok())?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let mut secs = 0i64;
    if let Some(t) = time {
        let mut tp = t.split(':');
        let h: i64 = tp.next()?.parse().ok()?;
        let mi: i64 = tp.next().map_or(Some(0), |v| v.parse().ok())?;
        let se: i64 = tp.next().map_or(Some(0), |v| v.split('.').next()?.parse().ok())?;
        secs = h * 3600 + mi * 60 + se;
    }
    Some(days_from_civil(y, m, d) * 86_400 + secs)
}

pub fn format_time(t: i64) -> String {
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600, (secs / 60) % 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        assert_eq!(parse_time("1970-01-01"), Some(0));
        assert_eq!(parse_time("2020-02-29T12:34:56Z"), Some(1_582_979_696));
        for t in [0i64, 1_582_979_696, 1_104_537_600, 4_102_444_800] {
            assert_eq!(parse_time(&format_time(t)), Some(t));
        }
    }
}

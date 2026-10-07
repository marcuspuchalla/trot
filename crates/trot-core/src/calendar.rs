//! Reporting calendar. The stored IANA zone, not the current OS offset, decides
//! day boundaries. An ambiguous wall time uses its first occurrence; a missing
//! wall time advances to the first valid second after the DST gap.
use chrono::{Datelike, NaiveDate, TimeZone, Timelike};
use chrono_tz::Tz;
static ZONE: std::sync::OnceLock<std::sync::RwLock<Tz>> = std::sync::OnceLock::new();
pub fn zone() -> Tz {
    *ZONE
        .get_or_init(|| {
            std::sync::RwLock::new(
                crate::config::load_settings()
                    .reporting_timezone
                    .parse()
                    .unwrap_or(chrono_tz::UTC),
            )
        })
        .read()
        .unwrap_or_else(|e| e.into_inner())
}
pub fn set_zone(name: &str) -> bool {
    let Ok(tz) = name.parse::<Tz>() else {
        return false;
    };
    let _ = zone();
    *ZONE
        .get()
        .unwrap()
        .write()
        .unwrap_or_else(|e| e.into_inner()) = tz;
    true
}
pub fn date(ts: f64) -> String {
    date_in(ts, zone())
}
pub fn date_in(ts: f64, tz: Tz) -> String {
    tz.timestamp_opt(ts as i64, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}
pub fn hour(ts: f64) -> usize {
    zone()
        .timestamp_opt(ts as i64, 0)
        .single()
        .map(|d| d.hour() as usize)
        .unwrap_or(0)
}
pub fn cutoff(date: &str, sod: i64) -> Option<f64> {
    cutoff_in(date, sod, zone())
}
pub fn cutoff_in(date: &str, sod: i64, tz: Tz) -> Option<f64> {
    if !(0..=86400).contains(&sod) {
        return None;
    }
    let day = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let start = day.and_hms_opt(0, 0, 0)? + chrono::Duration::seconds(sod);
    for offset in 0..=86400 {
        let local = start + chrono::Duration::seconds(offset);
        if let Some(dt) = tz.from_local_datetime(&local).earliest() {
            return Some(dt.timestamp() as f64);
        }
    }
    None
}
pub fn next_midnight(ts: f64) -> Option<f64> {
    let d = zone()
        .timestamp_opt(ts as i64, 0)
        .single()?
        .date_naive()
        .succ_opt()?;
    cutoff(
        &format!("{:04}-{:02}-{:02}", d.year(), d.month(), d.day()),
        0,
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn berlin_noon_is_noon_across_dst() {
        for day in ["2026-03-29", "2026-10-25"] {
            let ts = cutoff_in(day, 43200, chrono_tz::Europe::Berlin).unwrap();
            assert_eq!(
                chrono_tz::Europe::Berlin
                    .timestamp_opt(ts as i64, 0)
                    .unwrap()
                    .hour(),
                12
            );
        }
    }
    #[test]
    fn short_long_days_and_missing_time() {
        let t = chrono_tz::Europe::Berlin;
        assert_eq!(
            cutoff_in("2026-03-29", 86400, t).unwrap() - cutoff_in("2026-03-29", 0, t).unwrap(),
            23. * 3600.
        );
        assert_eq!(
            cutoff_in("2026-10-25", 86400, t).unwrap() - cutoff_in("2026-10-25", 0, t).unwrap(),
            25. * 3600.
        );
        let gap = cutoff_in("2026-03-29", 9000, t).unwrap();
        assert_eq!(t.timestamp_opt(gap as i64, 0).unwrap().hour(), 3);
    }
}

//! Binds the pure schedule grammar in [`gns_core::routine`] to real clocks:
//! `computeNextRunAt` (`shared/automation-schedule.ts`) against the machine's
//! local zone, a named IANA zone, or UTC (a test hook).

use gns_core::routine::{Trigger, compile_cron_matcher, format_timestamp, next_cron_run, parse_every_interval_ms};
pub use gns_core::routine::{WallClock, ZoneChoice, automation_anchor, describe_schedule, normalize_schedule};

/// Whether `schedule` is something the scheduler can fire.
pub fn is_valid_schedule(schedule: &str) -> bool {
    let normalized = normalize_schedule(schedule);
    parse_every_interval_ms(&normalized).is_some() || compile_cron_matcher(&normalized).is_some()
}

/// `computeNextRunAt`: the next fire time (ms) strictly after `after_ms`.
/// `@every` intervals are anchor + interval; cron expressions are searched
/// minute by minute in the schedule's pinned zone, else `time_zone`, else
/// `fallback` (local time in production, UTC in tests).
pub fn compute_next_run_at(schedule: &str, after_ms: i64, time_zone: Option<&str>, fallback: &ZoneChoice) -> Option<i64> {
    let normalized = normalize_schedule(schedule);
    if let Some(interval) = parse_every_interval_ms(&normalized) {
        return Some(after_ms + interval);
    }
    let matcher = compile_cron_matcher(&normalized)?;
    let zone = ZoneChoice::resolve(matcher.time_zone.as_deref().or(time_zone), fallback.clone());
    next_cron_run(&matcher, after_ms, |ms| zone.wall_clock(ms))
}

/// `earliestNextRunAt`: the soonest next run over a trigger's cron members.
pub fn earliest_next_run_at(trigger: &Trigger, anchor_ms: i64, time_zone: Option<&str>, fallback: &ZoneChoice) -> Option<i64> {
    trigger.cron_schedules().into_iter().filter_map(|schedule| compute_next_run_at(schedule, anchor_ms, time_zone, fallback)).min()
}

/// `formatTimestamp` in the user's zone (local time when unknown).
pub fn format_in_zone(ms: i64, time_zone: Option<&str>, fallback: &ZoneChoice) -> String {
    match time_zone {
        Some(_) => format_timestamp(Some(ms), time_zone),
        None => fallback.format_timestamp(Some(ms)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Datelike, TimeZone, Utc};

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp_millis()
    }

    #[test]
    fn five_field_with_weekdays() {
        // 2026-09-19 is a Saturday; the next weekday 09:00 UTC is Monday the 21st.
        let next = compute_next_run_at("0 9 * * 1-5", utc(2026, 9, 19, 0, 0), None, &ZoneChoice::Utc).unwrap();
        let dt = DateTime::<Utc>::from_timestamp_millis(next).unwrap();
        assert_eq!((dt.day(), dt.weekday()), (21, chrono::Weekday::Mon));
        assert_eq!(next, utc(2026, 9, 21, 9, 0));
    }

    #[test]
    fn sunday_zero_and_seven() {
        for dow in ["0", "7"] {
            let next = compute_next_run_at(&format!("0 8 * * {dow}"), utc(2026, 9, 19, 0, 0), None, &ZoneChoice::Utc).unwrap();
            assert_eq!(DateTime::<Utc>::from_timestamp_millis(next).unwrap().weekday(), chrono::Weekday::Sun);
        }
        // 5-7 = Friday..Sunday.
        let next = compute_next_run_at("0 8 * * 5-7", utc(2026, 9, 21, 0, 0), None, &ZoneChoice::Utc).unwrap();
        assert_eq!(DateTime::<Utc>::from_timestamp_millis(next).unwrap().weekday(), chrono::Weekday::Fri);
    }

    #[test]
    fn dom_and_dow_are_or_when_both_restricted() {
        // The 15th OR a Monday. 2026-09-14 is a Monday, the 15th a Tuesday.
        let after = utc(2026, 9, 14, 9, 0);
        assert_eq!(compute_next_run_at("0 9 15 * 1", after, None, &ZoneChoice::Utc), Some(utc(2026, 9, 15, 9, 0)));
        assert_eq!(compute_next_run_at("0 9 15 * 1", utc(2026, 9, 15, 9, 0), None, &ZoneChoice::Utc), Some(utc(2026, 9, 21, 9, 0)));
    }

    #[test]
    fn shorthands_and_every() {
        assert_eq!(compute_next_run_at("@every 30m", 1_000, None, &ZoneChoice::Utc), Some(1_000 + 1_800_000));
        assert_eq!(compute_next_run_at("@hourly", utc(2026, 9, 19, 0, 10), None, &ZoneChoice::Utc), Some(utc(2026, 9, 19, 1, 0)));
        assert_eq!(compute_next_run_at("@weekly", utc(2026, 9, 19, 0, 0), None, &ZoneChoice::Utc), Some(utc(2026, 9, 20, 0, 0)));
        assert!(compute_next_run_at("@every 0s", 0, None, &ZoneChoice::Utc).is_none());
        assert!(compute_next_run_at("bogus", 0, None, &ZoneChoice::Utc).is_none());
        assert!(is_valid_schedule("@every 5m") && is_valid_schedule("TZ=UTC 0 9 * * *") && !is_valid_schedule("0 9 * *"));
    }

    #[test]
    fn zones() {
        // CRON_TZ pins the zone: 09:30 EST == 14:30 UTC.
        let next = compute_next_run_at("CRON_TZ=America/New_York 30 9 * * *", utc(2026, 1, 5, 0, 0), Some("Asia/Tokyo"), &ZoneChoice::Utc)
            .unwrap();
        assert_eq!(DateTime::<Utc>::from_timestamp_millis(next).unwrap().format("%H:%M").to_string(), "14:30");
        // Otherwise the user's zone applies: 09:30 JST == 00:30 UTC.
        let next = compute_next_run_at("30 9 * * *", utc(2026, 1, 5, 0, 0), Some("Asia/Tokyo"), &ZoneChoice::Utc).unwrap();
        assert_eq!(DateTime::<Utc>::from_timestamp_millis(next).unwrap().format("%H:%M").to_string(), "00:30");
        // An unknown user zone falls back to the configured default (UTC here).
        let next = compute_next_run_at("30 9 * * *", utc(2026, 1, 5, 0, 0), Some("Nowhere/Land"), &ZoneChoice::Utc).unwrap();
        assert_eq!(next, utc(2026, 1, 5, 9, 30));
        // An unknown pinned zone makes the schedule invalid.
        assert!(compute_next_run_at("CRON_TZ=Nowhere/Land 30 9 * * *", 0, None, &ZoneChoice::Utc).is_none());
        // Earliest across a group's cron members.
        let group = Trigger::from_members(vec![Trigger::cron("0 12 * * *"), Trigger::cron("@every 10m")]).unwrap();
        assert_eq!(earliest_next_run_at(&group, utc(2026, 1, 5, 0, 0), None, &ZoneChoice::Utc), Some(utc(2026, 1, 5, 0, 10)));
        assert_eq!(format_in_zone(utc(2026, 1, 5, 14, 30), Some("America/New_York"), &ZoneChoice::Utc), "1/5/2026, 9:30:00 AM");
        assert_eq!(format_in_zone(utc(2026, 1, 5, 14, 30), None, &ZoneChoice::Utc), "1/5/2026, 2:30:00 PM");
    }
}

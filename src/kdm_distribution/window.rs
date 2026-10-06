use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, Timelike, Utc};
use jiff::tz::{AmbiguousOffset, TimeZone};
use serde::{Deserialize, Serialize};

// ST 430-1:2023 wants 25 characters with an explicit offset, never Z
const KDM_TIMESTAMP_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%:z";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalWindow {
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdmWindowTimes {
    pub not_valid_before: String,
    pub not_valid_after: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowEdge {
    Start,
    End,
}

fn civil(local: NaiveDateTime) -> Result<jiff::civil::DateTime, String> {
    let narrow = |value: u32| i8::try_from(value).map_err(|e| e.to_string());
    jiff::civil::DateTime::new(
        i16::try_from(local.year()).map_err(|e| e.to_string())?,
        narrow(local.month())?,
        narrow(local.day())?,
        narrow(local.hour())?,
        narrow(local.minute())?,
        narrow(local.second())?,
        0,
    )
    .map_err(|e| format!("{local} is not a valid date and time: {e}"))
}

// a time repeated by a clock change opens at its first and closes at its second occurrence
fn zoned(
    local: NaiveDateTime,
    time_zone: &TimeZone,
    zone_name: &str,
    edge: WindowEdge,
) -> Result<jiff::Zoned, String> {
    let ambiguous = time_zone.to_ambiguous_zoned(civil(local)?);
    if let AmbiguousOffset::Gap { .. } = ambiguous.offset() {
        return Err(format!(
            "{local} does not exist in {zone_name}: the clocks skip it, pick a time outside the change"
        ));
    }
    let resolved = match edge {
        WindowEdge::Start => ambiguous.earlier(),
        WindowEdge::End => ambiguous.later(),
    };
    resolved.map_err(|e| format!("{local} in {zone_name}: {e}"))
}

fn utc(zoned: &jiff::Zoned) -> Result<DateTime<Utc>, String> {
    DateTime::from_timestamp(zoned.timestamp().as_second(), 0)
        .ok_or_else(|| format!("{zoned} is out of range"))
}

pub fn kdm_window_in_time_zone(
    window: &LocalWindow,
    zone_name: &str,
) -> Result<KdmWindowTimes, String> {
    if window.end <= window.start {
        return Err(format!(
            "the window ends at {} before it starts at {}",
            window.end, window.start
        ));
    }
    let time_zone = TimeZone::get(zone_name)
        .map_err(|e| format!("'{zone_name}' is not an IANA time zone: {e}"))?;
    let start = zoned(window.start, &time_zone, zone_name, WindowEdge::Start)?;
    let end = zoned(window.end, &time_zone, zone_name, WindowEdge::End)?;
    Ok(KdmWindowTimes {
        not_valid_before: start.strftime(KDM_TIMESTAMP_FORMAT).to_string(),
        not_valid_after: end.strftime(KDM_TIMESTAMP_FORMAT).to_string(),
        start: utc(&start)?,
        end: utc(&end)?,
        start_date: window.start.date(),
        end_date: window.end.date(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KDM_TIMESTAMP_LENGTH: usize = 25;

    fn local(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S").unwrap()
    }

    fn window(start: &str, end: &str) -> LocalWindow {
        LocalWindow {
            start: local(start),
            end: local(end),
        }
    }

    #[test]
    fn a_window_across_the_october_change_carries_both_offsets() {
        let times = kdm_window_in_time_zone(
            &window("2026-10-23T18:00:00", "2026-10-26T23:30:00"),
            "Europe/London",
        )
        .unwrap();
        assert_eq!(times.not_valid_before, "2026-10-23T18:00:00+01:00");
        assert_eq!(times.not_valid_after, "2026-10-26T23:30:00+00:00");
        assert_eq!(times.start.to_rfc3339(), "2026-10-23T17:00:00+00:00");
        for timestamp in [&times.not_valid_before, &times.not_valid_after] {
            assert_eq!(timestamp.len(), KDM_TIMESTAMP_LENGTH, "{timestamp}");
            assert!(!timestamp.ends_with('Z'), "{timestamp}");
        }
    }

    #[test]
    fn a_window_across_the_march_change_south_of_the_equator() {
        let times = kdm_window_in_time_zone(
            &window("2027-04-01T09:00:00", "2027-04-06T09:00:00"),
            "Australia/Melbourne",
        )
        .unwrap();
        assert_eq!(times.not_valid_before, "2027-04-01T09:00:00+11:00");
        assert_eq!(times.not_valid_after, "2027-04-06T09:00:00+10:00");
    }

    #[test]
    fn utc_is_written_as_a_zero_offset() {
        let times =
            kdm_window_in_time_zone(&window("2026-11-01T00:00:00", "2026-11-02T00:00:00"), "UTC")
                .unwrap();
        assert_eq!(times.not_valid_before, "2026-11-01T00:00:00+00:00");
    }

    #[test]
    fn a_repeated_hour_opens_at_its_first_pass_and_closes_at_its_second() {
        let times = kdm_window_in_time_zone(
            &window("2026-10-25T01:30:00", "2026-11-01T01:30:00"),
            "Europe/London",
        )
        .unwrap();
        assert_eq!(times.not_valid_before, "2026-10-25T01:30:00+01:00");
        let end_in_fold = kdm_window_in_time_zone(
            &window("2026-10-24T01:30:00", "2026-10-25T01:30:00"),
            "Europe/London",
        )
        .unwrap();
        assert_eq!(end_in_fold.not_valid_after, "2026-10-25T01:30:00+00:00");
    }

    #[test]
    fn a_skipped_hour_unknown_zone_and_backwards_window_are_refused() {
        let skipped = kdm_window_in_time_zone(
            &window("2027-03-28T01:30:00", "2027-03-29T01:30:00"),
            "Europe/London",
        )
        .unwrap_err();
        assert!(skipped.contains("does not exist"), "{skipped}");
        assert!(
            kdm_window_in_time_zone(
                &window("2026-11-01T00:00:00", "2026-11-02T00:00:00"),
                "Mars/Olympus_Mons"
            )
            .is_err()
        );
        assert!(
            kdm_window_in_time_zone(&window("2026-11-02T00:00:00", "2026-11-01T00:00:00"), "UTC")
                .is_err()
        );
    }
}

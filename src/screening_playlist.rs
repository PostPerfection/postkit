use chrono::{NaiveDateTime, TimeDelta};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub const PLAYLIST_FORMAT_VERSION: u32 = 2;
// format 1 has no in and out frames and reads as format 2 without them
const OLDEST_READ_FORMAT_VERSION: u32 = 1;
const LOCAL_TIME_FORMAT: &str = "%Y-%m-%dT%H:%M:%S";
// what an html datetime-local input sends when the seconds are zero
const LOCAL_TIME_WITHOUT_SECONDS_FORMAT: &str = "%Y-%m-%dT%H:%M";
const MILLISECONDS_PER_SECOND: f64 = 1000.0;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreeningPlaylist {
    pub version: u32,
    pub name: String,
    pub rows: Vec<PlaylistRow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistRow {
    // local wall clock time, the row waits for it after the row before ends
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_local_time"
    )]
    pub start_time: Option<NaiveDateTime>,
    #[serde(flatten)]
    pub item: RowItem,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RowItem {
    Composition {
        package_directory: PathBuf,
        cpl_id: Uuid,
        title: String,
        // the first composition frame played, None for the first frame
        #[serde(default, skip_serializing_if = "Option::is_none")]
        in_frame: Option<u64>,
        // the composition frame playback stops before, None for the end
        #[serde(default, skip_serializing_if = "Option::is_none")]
        out_frame: Option<u64>,
    },
    Intermission {
        seconds: u32,
        // None holds black
        #[serde(default, skip_serializing_if = "Option::is_none")]
        still_image: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RowPlan {
    pub row: usize,
    #[serde(serialize_with = "serialize_local_time")]
    pub expected_start: NaiveDateTime,
    #[serde(serialize_with = "serialize_local_time")]
    pub expected_end: NaiveDateTime,
    // black on screen between the row before and a scheduled start
    pub wait_before_seconds: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum PlaylistWarning {
    MissingComposition {
        row: usize,
        package_directory: PathBuf,
        cpl_id: Uuid,
    },
    // the row plays the part of the range inside the composition
    RangeOutsideComposition {
        row: usize,
        in_frame: u64,
        out_frame: u64,
        frame_count: u64,
    },
    // the row starts late, at the end of the row before or at once for the first row
    StartsBeforeItCan {
        row: usize,
        #[serde(serialize_with = "serialize_local_time")]
        start_time: NaiveDateTime,
        #[serde(serialize_with = "serialize_local_time")]
        earliest_start: NaiveDateTime,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompositionLength {
    pub frame_count: u64,
    pub frames_per_second: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistPlan {
    pub rows: Vec<RowPlan>,
    pub warnings: Vec<PlaylistWarning>,
}

impl ScreeningPlaylist {
    pub fn new(name: &str) -> ScreeningPlaylist {
        ScreeningPlaylist {
            version: PLAYLIST_FORMAT_VERSION,
            name: name.to_string(),
            rows: Vec::new(),
        }
    }

    pub fn read(path: &Path) -> Result<ScreeningPlaylist, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let mut playlist: ScreeningPlaylist =
            serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
        if !(OLDEST_READ_FORMAT_VERSION..=PLAYLIST_FORMAT_VERSION).contains(&playlist.version) {
            return Err(format!(
                "{} is playlist format {}, this build reads formats {OLDEST_READ_FORMAT_VERSION} to {PLAYLIST_FORMAT_VERSION}",
                path.display(),
                playlist.version
            ));
        }
        playlist.version = PLAYLIST_FORMAT_VERSION;
        Ok(playlist)
    }

    pub fn write(&self, path: &Path) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self).map_err(|error| error.to_string())?;
        crate::fs::write_atomic(path, json.as_bytes())
    }
}

pub fn parse_local_time(text: &str) -> Result<NaiveDateTime, String> {
    NaiveDateTime::parse_from_str(text, LOCAL_TIME_FORMAT)
        .or_else(|_| NaiveDateTime::parse_from_str(text, LOCAL_TIME_WITHOUT_SECONDS_FORMAT))
        .map_err(|_| format!("{text} is not a local date and time such as 2026-10-06T20:30:00"))
}

pub fn format_local_time(time: NaiveDateTime) -> String {
    time.format(LOCAL_TIME_FORMAT).to_string()
}

fn seconds_delta(seconds: f64) -> TimeDelta {
    TimeDelta::milliseconds((seconds * MILLISECONDS_PER_SECOND).round() as i64)
}

fn delta_seconds(delta: TimeDelta) -> f64 {
    delta.num_milliseconds() as f64 / MILLISECONDS_PER_SECOND
}

// what plays when from first_row on, if it starts now, with a missing composition counted as zero long
pub fn plan(
    rows: &[PlaylistRow],
    first_row: usize,
    now: NaiveDateTime,
    composition_length: impl Fn(&Path, Uuid) -> Option<CompositionLength>,
) -> PlaylistPlan {
    let mut planned = Vec::new();
    let mut warnings = Vec::new();
    let mut earliest_start = now;
    for (row, entry) in rows.iter().enumerate().skip(first_row) {
        let expected_start = match entry.start_time {
            Some(start_time) if start_time >= earliest_start => start_time,
            Some(start_time) => {
                warnings.push(PlaylistWarning::StartsBeforeItCan {
                    row,
                    start_time,
                    earliest_start,
                });
                earliest_start
            }
            None => earliest_start,
        };
        let length_seconds = match &entry.item {
            RowItem::Intermission { seconds, .. } => f64::from(*seconds),
            RowItem::Composition {
                package_directory,
                cpl_id,
                in_frame,
                out_frame,
                ..
            } => match composition_length(package_directory, *cpl_id) {
                Some(length) => {
                    let played = played_frames(row, *in_frame, *out_frame, length, &mut warnings);
                    played as f64 / length.frames_per_second
                }
                None => {
                    warnings.push(PlaylistWarning::MissingComposition {
                        row,
                        package_directory: package_directory.clone(),
                        cpl_id: *cpl_id,
                    });
                    0.0
                }
            },
        };
        let expected_end = expected_start + seconds_delta(length_seconds);
        planned.push(RowPlan {
            row,
            expected_start,
            expected_end,
            wait_before_seconds: delta_seconds(expected_start - earliest_start),
        });
        earliest_start = expected_end;
    }
    PlaylistPlan {
        rows: planned,
        warnings,
    }
}

// the frames of the range inside the composition
fn played_frames(
    row: usize,
    in_frame: Option<u64>,
    out_frame: Option<u64>,
    length: CompositionLength,
    warnings: &mut Vec<PlaylistWarning>,
) -> u64 {
    let in_frame = in_frame.unwrap_or(0);
    let out_frame = out_frame.unwrap_or(length.frame_count);
    if in_frame >= out_frame || out_frame > length.frame_count {
        warnings.push(PlaylistWarning::RangeOutsideComposition {
            row,
            in_frame,
            out_frame,
            frame_count: length.frame_count,
        });
    }
    let out_frame = out_frame.min(length.frame_count);
    out_frame.saturating_sub(in_frame)
}

fn serialize_local_time<S: serde::Serializer>(
    time: &NaiveDateTime,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&format_local_time(*time))
}

mod optional_local_time {
    use chrono::NaiveDateTime;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(
        time: &Option<NaiveDateTime>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match time {
            Some(time) => serializer.serialize_str(&super::format_local_time(*time)),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<NaiveDateTime>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|text| super::parse_local_time(&text).map_err(serde::de::Error::custom))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAMES_PER_SECOND: f64 = 24.0;
    const FEATURE_SECONDS: f64 = 5400.0;
    const TRAILER_SECONDS: f64 = 150.5;

    fn time(text: &str) -> NaiveDateTime {
        parse_local_time(text).unwrap()
    }

    fn composition(package: &str, start_time: Option<&str>) -> PlaylistRow {
        PlaylistRow {
            start_time: start_time.map(time),
            item: RowItem::Composition {
                package_directory: PathBuf::from(package),
                cpl_id: Uuid::nil(),
                title: package.trim_start_matches('/').to_string(),
                in_frame: None,
                out_frame: None,
            },
        }
    }

    fn ranged(package: &str, in_frame: Option<u64>, out_frame: Option<u64>) -> PlaylistRow {
        let mut row = composition(package, None);
        if let RowItem::Composition {
            in_frame: row_in,
            out_frame: row_out,
            ..
        } = &mut row.item
        {
            *row_in = in_frame;
            *row_out = out_frame;
        }
        row
    }

    fn intermission(seconds: u32, start_time: Option<&str>) -> PlaylistRow {
        PlaylistRow {
            start_time: start_time.map(time),
            item: RowItem::Intermission {
                seconds,
                still_image: None,
            },
        }
    }

    fn library_length(package: &Path, _cpl_id: Uuid) -> Option<CompositionLength> {
        let seconds = match package.to_str()? {
            "/trailer" => TRAILER_SECONDS,
            "/feature" => FEATURE_SECONDS,
            _ => return None,
        };
        Some(CompositionLength {
            frame_count: (seconds * FRAMES_PER_SECOND) as u64,
            frames_per_second: FRAMES_PER_SECOND,
        })
    }

    #[test]
    fn a_playlist_round_trips_through_its_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("playlists").join("evening.json");
        let mut playlist = ScreeningPlaylist::new("Evening");
        playlist.rows = vec![
            composition("/trailer", None),
            PlaylistRow {
                start_time: None,
                item: RowItem::Intermission {
                    seconds: 600,
                    still_image: Some(PathBuf::from("/stills/interval.png")),
                },
            },
            composition("/feature", Some("2026-10-06T20:30:00")),
            ranged("/trailer", Some(48), Some(240)),
        ];

        playlist.write(&path).unwrap();

        assert_eq!(ScreeningPlaylist::read(&path).unwrap(), playlist);
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(json["version"], 2);
        assert_eq!(json["rows"][0]["kind"], "composition");
        assert_eq!(json["rows"][0]["packageDirectory"], "/trailer");
        assert_eq!(json["rows"][0].get("startTime"), None);
        assert_eq!(json["rows"][1]["kind"], "intermission");
        assert_eq!(json["rows"][1]["stillImage"], "/stills/interval.png");
        assert_eq!(json["rows"][2]["startTime"], "2026-10-06T20:30:00");
        assert_eq!(json["rows"][2].get("inFrame"), None);
        assert_eq!(json["rows"][3]["inFrame"], 48);
        assert_eq!(json["rows"][3]["outFrame"], 240);
    }

    #[test]
    fn a_format_1_playlist_reads_as_format_2_without_ranges() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("before_ranges.json");
        std::fs::write(
            &path,
            r#"{"version": 1, "name": "Old", "rows": [
                {"kind": "composition", "packageDirectory": "/trailer",
                 "cplId": "00000000-0000-0000-0000-000000000000", "title": "trailer"}
            ]}"#,
        )
        .unwrap();

        let playlist = ScreeningPlaylist::read(&path).unwrap();

        assert_eq!(playlist.version, PLAYLIST_FORMAT_VERSION);
        assert_eq!(playlist.rows, [composition("/trailer", None)]);
    }

    #[test]
    fn a_start_time_without_seconds_reads_as_the_whole_minute() {
        let row: PlaylistRow = serde_json::from_str(
            r#"{"kind": "intermission", "seconds": 5, "startTime": "2026-10-06T20:30"}"#,
        )
        .unwrap();
        assert_eq!(row.start_time, Some(time("2026-10-06T20:30:00")));
        assert_eq!(
            row.item,
            RowItem::Intermission {
                seconds: 5,
                still_image: None
            }
        );
    }

    #[test]
    fn a_start_time_that_is_not_a_time_is_refused() {
        let error = serde_json::from_str::<PlaylistRow>(
            r#"{"kind": "intermission", "seconds": 5, "startTime": "tonight"}"#,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("tonight is not a local date and time"),
            "{error}"
        );
    }

    #[test]
    fn a_newer_format_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("later.json");
        std::fs::write(&path, r#"{"version": 3, "name": "Later", "rows": []}"#).unwrap();

        let error = ScreeningPlaylist::read(&path).unwrap_err();

        assert!(
            error.contains("is playlist format 3, this build reads formats 1 to 2"),
            "{error}"
        );
    }

    #[test]
    fn rows_without_start_times_follow_each_other() {
        let now = time("2026-10-06T19:00:00");
        let rows = [
            composition("/trailer", None),
            intermission(600, None),
            composition("/feature", None),
        ];

        let plan = plan(&rows, 0, now, library_length);

        assert_eq!(plan.warnings, []);
        let starts: Vec<_> = plan.rows.iter().map(|row| row.expected_start).collect();
        assert_eq!(
            starts,
            [
                now,
                time("2026-10-06T19:02:30") + TimeDelta::milliseconds(500),
                time("2026-10-06T19:12:30") + TimeDelta::milliseconds(500),
            ]
        );
        assert!(plan.rows.iter().all(|row| row.wait_before_seconds == 0.0));
        assert_eq!(
            plan.rows[2].expected_end,
            time("2026-10-06T20:42:30") + TimeDelta::milliseconds(500)
        );
    }

    #[test]
    fn a_scheduled_row_waits_for_its_start_time() {
        let now = time("2026-10-06T19:00:00");
        let rows = [
            composition("/trailer", None),
            composition("/feature", Some("2026-10-06T20:00:00")),
        ];

        let plan = plan(&rows, 0, now, library_length);

        assert_eq!(plan.warnings, []);
        assert_eq!(plan.rows[1].expected_start, time("2026-10-06T20:00:00"));
        assert_eq!(plan.rows[1].wait_before_seconds, 3600.0 - TRAILER_SECONDS);
    }

    #[test]
    fn a_start_time_before_the_row_before_ends_is_a_warning_and_the_row_starts_late() {
        let now = time("2026-10-06T19:00:00");
        let rows = [
            composition("/feature", None),
            composition("/trailer", Some("2026-10-06T20:00:00")),
        ];

        let plan = plan(&rows, 0, now, library_length);

        assert_eq!(
            plan.warnings,
            [PlaylistWarning::StartsBeforeItCan {
                row: 1,
                start_time: time("2026-10-06T20:00:00"),
                earliest_start: time("2026-10-06T20:30:00"),
            }]
        );
        assert_eq!(plan.rows[1].expected_start, time("2026-10-06T20:30:00"));
        assert_eq!(plan.rows[1].wait_before_seconds, 0.0);
    }

    #[test]
    fn a_missing_composition_is_a_warning_counted_as_zero_long() {
        let now = time("2026-10-06T19:00:00");
        let rows = [composition("/gone", None), intermission(60, None)];

        let plan = plan(&rows, 0, now, library_length);

        assert_eq!(
            plan.warnings,
            [PlaylistWarning::MissingComposition {
                row: 0,
                package_directory: PathBuf::from("/gone"),
                cpl_id: Uuid::nil(),
            }]
        );
        assert_eq!(plan.rows[1].expected_start, now);
    }

    #[test]
    fn a_ranged_row_lasts_as_long_as_its_range() {
        let now = time("2026-10-06T19:00:00");
        let rows = [
            ranged("/feature", Some(240), Some(720)),
            ranged("/trailer", Some(3600), None),
            composition("/trailer", None),
        ];

        let plan = plan(&rows, 0, now, library_length);

        assert_eq!(plan.warnings, []);
        // 480 frames is 20 s, then the last 12 frames of the trailer, half a second
        assert_eq!(plan.rows[1].expected_start, time("2026-10-06T19:00:20"));
        assert_eq!(
            plan.rows[2].expected_start,
            time("2026-10-06T19:00:20") + TimeDelta::milliseconds(500)
        );
    }

    #[test]
    fn a_range_outside_the_composition_is_a_warning_and_plays_the_part_inside() {
        let now = time("2026-10-06T19:00:00");
        let trailer_frames = (TRAILER_SECONDS * FRAMES_PER_SECOND) as u64;
        let rows = [
            ranged(
                "/trailer",
                Some(trailer_frames - 24),
                Some(trailer_frames + 240),
            ),
            ranged("/trailer", Some(100), Some(100)),
            intermission(60, None),
        ];

        let plan = plan(&rows, 0, now, library_length);

        assert_eq!(
            plan.warnings,
            [
                PlaylistWarning::RangeOutsideComposition {
                    row: 0,
                    in_frame: trailer_frames - 24,
                    out_frame: trailer_frames + 240,
                    frame_count: trailer_frames,
                },
                PlaylistWarning::RangeOutsideComposition {
                    row: 1,
                    in_frame: 100,
                    out_frame: 100,
                    frame_count: trailer_frames,
                },
            ]
        );
        assert_eq!(plan.rows[1].expected_start, time("2026-10-06T19:00:01"));
        assert_eq!(plan.rows[2].expected_start, time("2026-10-06T19:00:01"));
        assert_eq!(
            serde_json::to_value(&plan.warnings[0]).unwrap()["kind"],
            "rangeOutsideComposition"
        );
    }

    #[test]
    fn playing_from_a_later_row_plans_only_that_row_on() {
        let now = time("2026-10-06T19:00:00");
        let rows = [composition("/gone", None), composition("/trailer", None)];

        let plan = plan(&rows, 1, now, library_length);

        assert_eq!(plan.warnings, []);
        assert_eq!(plan.rows.len(), 1);
        assert_eq!(plan.rows[0].row, 1);
        assert_eq!(plan.rows[0].expected_start, now);
    }

    #[test]
    fn the_plan_reads_as_local_times_for_the_page() {
        let now = time("2026-10-06T19:00:00");
        let plan = plan(
            &[composition("/trailer", Some("2026-10-06T18:00:00"))],
            0,
            now,
            library_length,
        );

        let json = serde_json::to_value(&plan).unwrap();

        assert_eq!(json["rows"][0]["expectedStart"], "2026-10-06T19:00:00");
        assert_eq!(json["warnings"][0]["kind"], "startsBeforeItCan");
        assert_eq!(json["warnings"][0]["startTime"], "2026-10-06T18:00:00");
    }
}
